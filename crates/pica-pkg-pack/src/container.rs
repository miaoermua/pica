//! Reading a backend container, only to tell how much it installs.
//!
//! Pica does not unpack the containers an `opkg`/`apk` artifact ships. They are
//! installed by the device's own package manager, which is the only thing that
//! keeps that backend's database and dependency solver in charge of the files.
//! The one question pica-pack still asks of a container is how many bytes it puts
//! on a device, for the `size` the index advertises — and the answer has to come
//! from the payload, not from the size of the archive around it.
//!
//! Two layouts can be read:
//!
//! - `.ipk` — an outer tar holding a `control` and a `data.tar.<compression>`
//!   member; the data member holds the files.
//! - `.apk` — three gzip streams concatenated (signature, control, data), so the
//!   payload shares one archive with the control entries and is told apart by
//!   name afterwards: apk keeps every control entry as a dot-file at the archive
//!   root. This is the layout of apk-tools 2.x and earlier.
//!
//! apk-tools 3.0 replaced that layout with ADB (the apk database format), which
//! opens with the `ADBd` signature block instead of a gzip stream and cannot be
//! unpacked by any tar. Such a container is named as unmeasured rather than
//! passed to tar, which would fail without saying why.
//!
//! Nothing else about a container is interpreted: not its dependency metadata,
//! not its maintainer scripts, not its package name.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::make_temp_dir;

/// The signature block of the apk-tools 3.x (ADB) format, which tar cannot open.
const ADB_SIGNATURE: &[u8; 4] = b"ADBd";

/// A temporary directory removed when the value goes out of scope.
#[derive(Debug)]
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
///
/// The directory belongs to the value: dropping it removes the tree, including
/// the one an `.ipk`'s data member was extracted into. It exists only to be
/// measured.
#[derive(Debug)]
pub struct UnpackedContainer {
  payload_root: PathBuf,
  /// Held so that `payload_root` stays valid for as long as this value does.
  ///
  /// Never read: what this field does is happen, and then stop happening. An
  /// `.apk`'s payload is this directory, and an `.ipk`'s was extracted into it.
  #[allow(dead_code, reason = "held for its Drop, which removes the tree")]
  scratch: Scratch,
}

impl UnpackedContainer {
  /// The directory holding the files the payload carried.
  #[must_use]
  pub fn payload_root(&self) -> &Path {
    &self.payload_root
  }

  fn new(scratch: Scratch) -> Self {
    let payload_root = scratch.path().to_path_buf();
    Self { payload_root, scratch }
  }
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
      "{} is an apk-tools 3.x (ADB) container, which tar cannot unpack and Pica does not \
       measure: the device's apk is what installs it, so its bytes are left out of the size",
      container.display()
    )));
  }

  let outer = Scratch::new("pica-container")?;
  // `--ignore-zeros` matters for a legacy `.apk`: without it tar stops at the
  // first end-of-archive marker and would unpack the signature member only.
  run_tar(&["-xzf"], &["--ignore-zeros"], container, outer.path())?;

  if extension == "ipk" {
    let member = find_data_member(outer.path()).ok_or_else(|| {
      PicaError::msg(format!("container carries no data.tar member: {}", container.display()))
    })?;
    let data = Scratch::new("pica-container-data")?;
    // An ipk's data member names its own compression, which the builder chose.
    run_tar(&compression_flags(&member), &[], &member, data.path())?;
    // `outer` goes out of scope here and takes the container's own members with
    // it; the payload is in `data`, which the returned value keeps alive.
    return Ok(UnpackedContainer::new(data));
  }

  strip_apk_control(outer.path())?;
  Ok(UnpackedContainer::new(outer))
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

/// The `data.tar.<compression>` member of an `.ipk`, if the container has one.
fn find_data_member(outer: &Path) -> Option<PathBuf> {
  let entries = fs::read_dir(outer).ok()?;
  let mut candidates: Vec<PathBuf> = entries
    .flatten()
    .map(|entry| entry.path())
    .filter(|path| {
      let named = path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("data.tar"));
      named && is_real_file(path)
    })
    .collect();
  candidates.sort();
  candidates.into_iter().next()
}

/// Whether `path` is a regular file rather than a link to one.
///
/// A container that names a member through a symlink is asking tar to read
/// whatever the link points at, which is not part of the container and may be a
/// file on the machine doing the build.
fn is_real_file(path: &Path) -> bool {
  fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

/// The tar flags implied by an `data.tar.<compression>` member's name.
///
/// The name is folded above, so the `.tgz`-style shortenings are matched
/// case-insensitively already; the lint that suggests folding them does not
/// follow the binding it is looking at.
#[allow(clippy::case_sensitive_file_extension_comparisons, reason = "the name is folded above")]
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
/// They are not payload, and counting them would make every apk report a few
/// bytes more than it installs.
fn strip_apk_control(outer: &Path) -> PicaResult<()> {
  let entries = fs::read_dir(outer).map_err(|err| PicaError::msg(format!("{err}")))?;

  for entry in entries.flatten() {
    let path = entry.path();
    // Not `is_file()`: that follows links, and an entry that is a link to a file
    // elsewhere is a control entry all the same. `remove_file` drops the link
    // itself, never its target.
    let named =
      path.file_name().and_then(|value| value.to_str()).is_some_and(|name| name.starts_with('.'));
    let Ok(metadata) = fs::symlink_metadata(&path) else { continue };
    if named && !metadata.is_dir() {
      let _ = fs::remove_file(&path);
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{compression_flags, is_adb_container, unpack_container};
  use pica_pkg_core::io::make_temp_dir;
  use pretty_assertions::assert_eq;
  use std::fs;
  use std::io::Write;
  use std::path::Path;

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
    assert!(
      message.contains("left out of the size"),
      "the error must say the bytes are not counted, not offer to build for another target: \
       {message}"
    );
    assert!(!message.contains("targets ="), "there is no other target to build for: {message}");
  }

  #[test]
  fn an_ipk_maintainer_scripts_are_not_payload() {
    // `control.tar.gz` carries `preinst`, `postinst`, `prerm`, `postrm` and
    // `control`. They belong to the package manager, not to the rootfs, and
    // counting them would report bytes the device never receives.
    let dir = make_temp_dir("pica-ipk-scripts").expect("temp dir");
    let container = build_ipk_with_scripts(&dir, "demo-lib");

    let unpacked = unpack_container(&container).expect("unpack");
    let leaked: Vec<String> = ["control", "preinst", "postinst", "prerm", "postrm"]
      .into_iter()
      .filter(|name| unpacked.payload_root().join(name).exists())
      .map(ToString::to_string)
      .collect();
    assert!(leaked.is_empty(), "maintainer scripts must not be payload: {leaked:?}");

    assert!(unpacked.payload_root().join("etc/demo").is_dir(), "the payload is still there");
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
}
