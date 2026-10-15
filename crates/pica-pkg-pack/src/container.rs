//! Unpacking of the pre-built containers a package ships in `pkgs/`.
//!
//! Containers are expanded **at build time**, not on the device. `pica-pack`
//! calls [`unpack_into`] to flatten a `.ipk` or `.apk` into the artifact's
//! `root/`, so the router only ever copies plain files and never has to spare
//! room for a second copy of a package it is about to lay down.
//!
//! Two layouts are understood:
//!
//! - `.ipk` — an outer tar holding `control` and a `data.tar.<compression>`
//!   member; the data member holds the files.
//! - `.apk` — three gzip streams concatenated (signature, control, data), so the
//!   payload shares one archive with the control entries and is told apart by
//!   name afterwards: apk keeps every control entry as a dot-file at the archive
//!   root. This is the layout of apk-tools 2.x and earlier.
//!
//! apk-tools 3.0 replaced that layout with ADB (the apk database format), which
//! opens with the `ADBd` signature block instead of a gzip stream and cannot be
//! unpacked by any tar. Such a container is reported rather than passed to tar,
//! which would fail without saying why.
//!
//! Dependency metadata inside a container is deliberately ignored. Pica installs
//! files; it does not solve dependency graphs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::{ensure_dir, make_temp_dir};

/// The signature block of the apk-tools 3.x (ADB) format, which tar cannot open.
const ADB_SIGNATURE: &[u8; 4] = b"ADBd";

/// A container directory removed when the value goes out of scope.
struct Scratch(PathBuf);

impl Scratch {
  fn new(prefix: &str) -> PicaResult<Self> {
    Ok(Self(make_temp_dir(prefix)?))
  }

  fn path(&self) -> &Path {
    &self.0
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

/// Runs tar, tolerating builds whose tar lacks `--ignore-zeros`.
///
/// The flag goes after the archive because `-f` consumes the argument that
/// follows it: `tar -xzf --ignore-zeros ARCHIVE` asks tar to open a member
/// called `--ignore-zeros`. Where it is unknown the plain form is retried,
/// which still unpacks single-member containers.
fn run_tar(flags: &[&str], extra: &[&str], archive: &Path, target: &Path) -> PicaResult<()> {
  let mut last = None;
  for suffix in [extra, &[]] {
    let mut command = Command::new("tar");
    // The order matters: `-f` consumes the argument that follows it, so
    // `tar -xzf --ignore-zeros ARCHIVE` would ask tar to open a member called
    // `--ignore-zeros`. The extra flags therefore follow the archive.
    command.args(flags).arg(archive).args(suffix).arg("-C").arg(target);
    match command.output() {
      Ok(output) if output.status.success() => return Ok(()),
      Ok(output) => {
        last = Some(
          String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("tar failed")
            .to_string(),
        );
      }
      Err(err) => return Err(PicaError::msg(format!("tar could not run: {err}"))),
    }
  }
  Err(PicaError::msg(format!(
    "{}: {}",
    archive.display(),
    last.unwrap_or_else(|| "tar failed".to_string())
  )))
}

/// A container's payload, unpacked somewhere on disk.
#[derive(Debug)]
pub struct UnpackedContainer {
  /// Name recorded for the container, taken from its control metadata.
  pub pkgname: String,
  /// Directory holding the files the payload carried.
  pub payload_root: PathBuf,
}

/// Unpacks one container and returns where its files landed.
///
/// # Errors
/// Returns an error for an unknown container format, an ADB (apk-tools 3.x)
/// container, a container whose payload cannot be found, and a failed extract.
pub fn unpack_container(container: &Path) -> PicaResult<UnpackedContainer> {
  let extension =
    container.extension().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
  if extension != "ipk" && extension != "apk" {
    return Err(PicaError::msg(format!(
      "unsupported container format: {} (expected .ipk or .apk)",
      container.display()
    )));
  }

  if extension == "apk" && is_adb_container(container) {
    return Err(PicaError::msg(format!(
      "{} is an apk-tools 3.x (ADB) container, which tar cannot unpack. \
       Repack it as a legacy concatenated-gzip .apk, or build the artifact with \
       `targets = apk` so the device's own apk installs it instead of Pica.",
      container.display()
    )));
  }

  let outer = Scratch::new("pica-container")?;
  // `--ignore-zeros` matters for a legacy `.apk`: without it tar stops at the
  // first end-of-archive marker and would unpack the signature member only.
  run_tar(&["-xzf"], &["--ignore-zeros"], container, outer.path())?;

  let data = Scratch::new("pica-container-data")?;
  let (payload_root, control) = if extension == "ipk" {
    let member = find_data_member(outer.path()).ok_or_else(|| {
      PicaError::msg(format!("container carries no data.tar member: {}", container.display()))
    })?;
    // An ipk's data member names its own compression, which the builder chose.
    run_tar(&compression_flags(&member), &[], &member, data.path())?;
    // `control` lives inside `control.tar.gz`, not loose in the outer archive.
    (data.path().to_path_buf(), read_ipk_control(outer.path()))
  } else {
    strip_apk_control(outer.path())?;
    (outer.path().to_path_buf(), read_text(&outer.path().join(".PKGINFO")))
  };

  // The payload is copied out before this returns: `data` is a `Scratch`, so its
  // directory is removed when this function returns and a path handed back from
  // it would already be gone by the time the caller looked.
  let owned = make_temp_dir("pica-container-out")?;
  copy_tree(&payload_root, &owned)?;
  std::mem::forget(data);

  Ok(UnpackedContainer { pkgname: package_name(&control, container), payload_root: owned })
}

/// The `control` text of an `.ipk`: the outer archive holds `control.tar.gz`, and
/// the `control` file is a member of that.
/// The `control` text of an `.ipk`: the outer archive holds `control.tar.gz`, and
/// the `control` file is a member of that.
///
/// It is unpacked into a directory of its own rather than beside the payload:
/// a `control.tar.gz` also carries the maintainer scripts (`preinst`,
/// `postinst`, `prerm`, `postrm`, `control`), and those are not payload. Sharing
/// the payload's directory would hand them to the device's root.
fn read_ipk_control(outer: &Path) -> String {
  let Some(member) = find_control_member(outer) else {
    // Some builders put a plain `control` file loose in the outer archive.
    return read_text(&outer.join("control"));
  };
  let control_dir = Scratch::new("pica-container-control");
  let Ok(scratch) = control_dir else { return read_text(&outer.join("control")) };
  if run_tar(&["-xzf"], &[], &member, scratch.path()).is_err() {
    return read_text(&outer.join("control"));
  }
  read_text(&scratch.path().join("control"))
}

/// The `control.tar.*` member of an `.ipk`, if it has one.
fn find_control_member(outer: &Path) -> Option<PathBuf> {
  let entries = fs::read_dir(outer).ok()?;
  let mut candidates: Vec<PathBuf> = entries
    .flatten()
    .map(|entry| entry.path())
    .filter(|path| {
      path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("control.tar"))
    })
    .collect();
  candidates.sort();
  candidates.into_iter().next()
}

/// Whether `path` starts with the apk-tools 3.x signature block.
///
/// The ADB formats introduced in apk-tools 3.0 open with `ADBd`. A legacy
/// container opens with a gzip stream (`1f 8b`) or a tar header (`ustar`), so
/// the four bytes decide it without reading the whole file.
pub fn is_adb_container(path: &Path) -> bool {
  use std::io::Read;

  let Ok(mut file) = fs::File::open(path) else { return false };
  let mut magic = [0u8; 4];
  matches!(file.read_exact(&mut magic), Ok(())) && &magic == ADB_SIGNATURE
}

/// Unpacks every container under `pkgs/` into `payload`, returning the names
/// that were laid down.
///
/// This is what a `pica`-backend artifact needs at build time: the device then
/// only copies files, and `-R` can take exactly those away again.
pub fn unpack_into(pkgs_dir: &Path, payload: &Path) -> PicaResult<Vec<String>> {
  if !pkgs_dir.is_dir() {
    return Ok(Vec::new());
  }
  ensure_dir(payload)?;

  let mut containers: Vec<PathBuf> = Vec::new();
  collect_files(pkgs_dir, &mut containers)?;
  containers.sort();
  let mut names = Vec::new();
  for container in &containers {
    let unpacked = unpack_container(container)?;
    copy_tree(&unpacked.payload_root, payload)?;
    names.push(unpacked.pkgname);
  }
  Ok(names)
}

/// Recursively lists the files under `dir`, sorted so builds are reproducible.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> PicaResult<()> {
  let mut entries: Vec<PathBuf> = fs::read_dir(dir)
    .map_err(|err| PicaError::msg(format!("read {} failed: {err}", dir.display())))?
    .flatten()
    .map(|entry| entry.path())
    .collect();
  entries.sort();

  for path in entries {
    if path.is_dir() {
      collect_files(&path, out)?;
    } else if path.is_file() {
      out.push(path);
    }
  }
  Ok(())
}

/// Merges `src` into `dst`, overwriting files that are already there.
fn copy_tree(src: &Path, dst: &Path) -> PicaResult<()> {
  for entry in fs::read_dir(src).map_err(|err| PicaError::msg(format!("{err}")))?.flatten() {
    let path = entry.path();
    let target = dst.join(entry.file_name());
    if path.is_dir() {
      ensure_dir(&target)?;
      copy_tree(&path, &target)?;
    } else if path.is_file() {
      if let Some(parent) = target.parent() {
        ensure_dir(parent)?;
      }
      fs::copy(&path, &target)
        .map_err(|err| PicaError::msg(format!("copy {} failed: {err}", path.display())))?;
    }
  }
  Ok(())
}

/// The `data.tar.<compression>` member of an `.ipk`, if the container has one.
fn find_data_member(outer: &Path) -> Option<PathBuf> {
  let entries = fs::read_dir(outer).ok()?;
  let mut candidates: Vec<PathBuf> = entries
    .flatten()
    .map(|entry| entry.path())
    .filter(|path| {
      path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("data.tar"))
    })
    .collect();
  candidates.sort();
  candidates.into_iter().next()
}

/// The tar flags implied by an `data.tar.<compression>` member's name.
fn compression_flags(member: &Path) -> Vec<&'static str> {
  let name = member.file_name().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
  if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
    vec!["-xzf"]
  } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
    vec!["-xJf"]
  } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
    vec!["-xjf"]
  } else {
    vec!["-xf"]
  }
}

/// Removes apk's control entries from an unpacked container.
///
/// apk keeps `.PKGINFO`, `.SIGN.*` and the lifecycle scripts it declares as
/// dot-files at the archive root, while the payload always lives under a real
/// directory. Only top-level dot *files* are dropped, so payload such as
/// `root/.profile` stays in place — that entry is `root/`, not `.profile`.
fn strip_apk_control(outer: &Path) -> PicaResult<()> {
  let entries = fs::read_dir(outer).map_err(|err| PicaError::msg(format!("{err}")))?;

  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_file()
      && path.file_name().and_then(|value| value.to_str()).is_some_and(|name| name.starts_with('.'))
    {
      let _ = fs::remove_file(&path);
    }
  }
  Ok(())
}

fn read_text(path: &Path) -> String {
  fs::read_to_string(path).unwrap_or_default()
}

/// The package's own name: the control field, or the file name as a fallback.
fn package_name(control: &str, container: &Path) -> String {
  // An ipk uses `Package:`, apk v2 `.PKGINFO` uses `pkgname =`.
  for line in control.lines() {
    let line = line.trim();
    if let Some(value) = line.strip_prefix("Package:") {
      let value = value.trim();
      if !value.is_empty() {
        return value.to_string();
      }
    }
    if let Some(value) = line.strip_prefix("pkgname") {
      if let Some(value) = value.trim_start().strip_prefix('=') {
        let value = value.trim();
        if !value.is_empty() {
          return value.to_string();
        }
      }
    }
  }
  container.file_stem().and_then(|value| value.to_str()).unwrap_or("unknown").to_string()
}

#[cfg(test)]
mod tests {
  use super::{compression_flags, is_adb_container, package_name, unpack_container, unpack_into};
  use pica_pkg_core::io::make_temp_dir;
  use pretty_assertions::assert_eq;
  use std::fs;
  use std::io::Write;
  use std::path::Path;

  #[test]
  fn reads_the_ipk_package_field() {
    let control = "Package: luci-app-demo\nVersion: 1.0.0\nDescription: a: b\n";
    assert_eq!(package_name(control, Path::new("demo.ipk")), "luci-app-demo");
  }

  #[test]
  fn reads_the_apk_pkgname_field() {
    let control = "pkgname = demo\npkgver = 1.0.0\n";
    assert_eq!(package_name(control, Path::new("demo.apk")), "demo");
  }

  #[test]
  fn falls_back_to_the_container_file_name() {
    assert_eq!(package_name("", Path::new("demo_1.0_all.ipk")), "demo_1.0_all");
    assert_eq!(package_name("pkgname =\n", Path::new("/tmp/x/demo.apk")), "demo");
  }

  #[test]
  fn data_member_compression_is_taken_from_its_name() {
    assert_eq!(compression_flags(Path::new("data.tar.gz")), vec!["-xzf"]);
    assert_eq!(compression_flags(Path::new("data.tar.xz")), vec!["-xJf"]);
    assert_eq!(compression_flags(Path::new("data.tar.bz2")), vec!["-xjf"]);
    assert_eq!(compression_flags(Path::new("data.tar")), vec!["-xf"]);
  }

  #[test]
  fn recognises_the_apk_tools_3x_signature_block() {
    let dir = pica_pkg_core::io::make_temp_dir("pica-adb-test").expect("temp dir");
    let path = dir.join("demo.apk");

    let mut file = fs::File::create(&path).expect("create");
    file.write_all(b"ADBd\x8c\xbc\x07XSI").expect("write adb magic");
    drop(file);
    assert!(is_adb_container(&path), "an apk-tools 3.x container starts with ADBd");

    // A legacy container opens with a gzip stream instead.
    let legacy = dir.join("legacy.apk");
    fs::write(&legacy, b"\x1f\x8b\x08\x00\x00\x00\x00\x00").expect("write gzip magic");
    assert!(!is_adb_container(&legacy), "a legacy apk opens with gzip");
  }

  #[test]
  fn an_adb_container_reports_the_cause_instead_of_a_tar_failure() {
    let dir = pica_pkg_core::io::make_temp_dir("pica-adb-err").expect("temp dir");
    let path = dir.join("demo.apk");
    fs::write(&path, b"ADBd\x8c\xbc\x07XSImore bytes that are not a gzip stream").expect("write");

    let err = unpack_container(&path).expect_err("must refuse the ADB container");
    let message = err.to_string();
    assert!(message.contains("apk-tools 3.x"), "the error must name the format: {message}");
    assert!(message.contains("targets = apk"), "the error must offer a way forward: {message}");
  }

  #[test]
  fn an_ipk_maintainer_scripts_are_not_payload() {
    // `control.tar.gz` carries `preinst`, `postinst`, `prerm`, `postrm` and
    // `control`. They belong to the package manager, not to the rootfs, and
    // unpacking them beside the payload would hand the device a `/postinst`.
    let dir = make_temp_dir("pica-ipk-scripts").expect("temp dir");
    let container = build_ipk_with_scripts(&dir, "demo-lib");

    let unpacked = unpack_container(&container).expect("unpack");
    let leaked: Vec<String> = ["control", "preinst", "postinst", "prerm", "postrm"]
      .into_iter()
      .filter(|name| unpacked.payload_root.join(name).exists())
      .map(ToString::to_string)
      .collect();
    assert!(leaked.is_empty(), "maintainer scripts must not be payload: {leaked:?}");

    assert_eq!(unpacked.pkgname, "demo-lib");
    assert!(unpacked.payload_root.join("etc/demo").is_dir(), "the payload is still there");
  }

  /// An `.ipk` whose `control.tar.gz` also holds the maintainer scripts a real
  /// package carries, which is what must not be mistaken for payload.
  fn build_ipk_with_scripts(dir: &Path, pkgname: &str) -> std::path::PathBuf {
    let data = dir.join("data/etc/demo");
    fs::create_dir_all(&data).expect("mkdir data");
    fs::write(data.join("config"), "payload\n").expect("write payload");
    let data_tar = dir.join("data.tar.gz");
    run(&[
      "-czf",
      data_tar.to_str().expect("path"),
      "-C",
      dir.join("data").to_str().expect("path"),
      ".",
    ]);

    let control = dir.join("ctl");
    fs::create_dir_all(&control).expect("mkdir control");
    fs::write(control.join("control"), format!("Package: {pkgname}\nVersion: 1.0\n"))
      .expect("write control");
    for script in ["preinst", "postinst", "prerm", "postrm"] {
      fs::write(control.join(script), "#!/bin/sh\nexit 0\n").expect("write script");
    }
    let control_tar = dir.join("control.tar.gz");
    run(&[
      "-czf",
      control_tar.to_str().expect("path"),
      "-C",
      control.to_str().expect("path"),
      "control",
      "preinst",
      "postinst",
      "prerm",
      "postrm",
    ]);

    let ipk = dir.join("demo_1.0_all.ipk");
    run(&[
      "-czf",
      ipk.to_str().expect("path"),
      "-C",
      dir.to_str().expect("path"),
      "control.tar.gz",
      "data.tar.gz",
    ]);
    ipk
  }

  fn run(args: &[&str]) {
    let status = std::process::Command::new("tar").args(args).status().expect("run tar");
    assert!(status.success(), "tar {args:?} failed");
  }

  #[test]
  fn an_ipk_payload_is_flattened_into_the_target() {
    let dir = pica_pkg_core::io::make_temp_dir("pica-unpack-test").expect("temp dir");
    let ipk = dir.join("demo_1.0_all.ipk");

    // Build a real .ipk: an outer tar holding control.tar.gz and data.tar.gz.
    let data_dir = dir.join("data/etc/init.d");
    fs::create_dir_all(&data_dir).expect("mkdir data");
    fs::write(data_dir.join("demo"), "#!/bin/sh\n").expect("write payload");
    let data_tar = dir.join("data.tar.gz");
    let status = std::process::Command::new("tar")
      .args(["-czf"])
      .arg(&data_tar)
      .arg("-C")
      .arg(dir.join("data"))
      .arg(".")
      .status()
      .expect("run tar");
    assert!(status.success(), "tar must create the data member");

    let control_dir = dir.join("control");
    fs::create_dir_all(&control_dir).expect("mkdir control");
    fs::write(control_dir.join("control"), "Package: demo\nVersion: 1.0\n").expect("write control");
    let control_tar = dir.join("control.tar.gz");
    let status = std::process::Command::new("tar")
      .args(["-czf"])
      .arg(&control_tar)
      .arg("-C")
      .arg(&control_dir)
      .arg("control")
      .status()
      .expect("run tar");
    assert!(status.success(), "tar must create the control member");

    let status = std::process::Command::new("tar")
      .args(["-czf"])
      .arg(&ipk)
      .arg("-C")
      .arg(&dir)
      .arg("control.tar.gz")
      .arg("data.tar.gz")
      .status()
      .expect("run tar");
    assert!(status.success(), "tar must create the ipk");

    let pkgs = dir.join("pkgs");
    fs::create_dir_all(&pkgs).expect("mkdir pkgs");
    fs::copy(&ipk, pkgs.join("demo_1.0_all.ipk")).expect("copy in");

    let payload = dir.join("payload");
    let names = unpack_into(&pkgs, &payload).expect("unpack");

    assert_eq!(names, vec!["demo".to_string()]);
    assert_eq!(fs::read_to_string(payload.join("etc/init.d/demo")).expect("read"), "#!/bin/sh\n");
  }
}
