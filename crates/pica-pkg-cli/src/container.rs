//! Unpacking of the pre-built containers a `pica` package ships in `pkgs/`.
//!
//! The `opkg` and `apk` backends hand their containers to the system package
//! manager on the target. The `pica` backend has nobody to hand them to, so it
//! unpacks them itself and records the files it created — which is what lets
//! `-R` take those files away again later.
//!
//! Two formats are understood:
//!
//! - `.ipk` — an outer tar holding `control` and a `data.tar.<compression>`
//!   member; the data member holds the files.
//! - `.apk` — three gzip streams concatenated (signature, control, data), so the
//!   payload shares one archive with the control entries and is told apart by
//!   name afterwards: apk keeps every control entry as a dot-file at the archive
//!   root.
//!
//! Dependency metadata inside a container is deliberately ignored. This backend
//! installs files; it does not solve dependency graphs.

use std::fs;
use std::path::{Path, PathBuf};

use pica_pkg_core::io::{install_payload, make_temp_dir, PayloadInstall};

use crate::app::{CliError, CliResult, E_RUNTIME};
use crate::system::{run_tar_extract_auto, run_tar_extract_members};

/// A container that has been unpacked into the target root.
#[derive(Debug)]
pub struct UnpackedContainer {
  /// Name recorded for the container, taken from its control metadata.
  pub pkgname: String,
  /// Files and directories this container put on the target.
  pub install: PayloadInstall,
}

/// Unpacks one container and installs the files it carries into `target_root`.
///
/// # Errors
/// Returns an error for an unknown container format, a container whose payload
/// cannot be found or extracted, and an install that fails on the target.
pub fn unpack_container(container: &Path, target_root: &Path) -> CliResult<UnpackedContainer> {
  let extension = container
    .extension()
    .and_then(|value| value.to_str())
    .unwrap_or("")
    .to_ascii_lowercase();
  if extension != "ipk" && extension != "apk" {
    return Err(CliError::new(
      E_RUNTIME,
      format!(
        "unsupported container format in pkgs/: {} (expected .ipk or .apk)",
        container.display()
      ),
    ));
  }

  // `--ignore-zeros` matters for `.apk`: without it tar stops at the first
  // end-of-archive marker and would unpack the signature member only.
  let outer = Scratch::new("pica-container")?;
  run_tar_extract_members(container, outer.path())?;

  let data = Scratch::new("pica-container-data")?;
  let (payload_root, control) = if extension == "ipk" {
    let member = find_data_member(outer.path()).ok_or_else(|| {
      CliError::new(
        E_RUNTIME,
        format!("container carries no data.tar member: {}", container.display()),
      )
    })?;
    run_tar_extract_auto(&member, data.path())?;
    (data.path().to_path_buf(), read_text(&outer.path().join("control")))
  } else {
    strip_apk_control(outer.path())?;
    (outer.path().to_path_buf(), read_text(&outer.path().join(".PKGINFO")))
  };

  let install = install_payload(&payload_root, target_root).map_err(CliError::from)?;
  if install.files.is_empty() {
    return Err(CliError::new(
      E_RUNTIME,
      format!("container carries no files: {}", container.display()),
    ));
  }

  Ok(UnpackedContainer {
    pkgname: package_name(&control, container),
    install,
  })
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

/// Removes apk's control entries from an unpacked container.
///
/// apk keeps `.PKGINFO`, `.SIGN.*` and the lifecycle scripts it declares as
/// dot-files at the archive root, while the payload always lives under a real
/// directory. Only top-level dot *files* are dropped, so payload such as
/// `root/.profile` stays in place — that entry is `root/`, not `.profile`.
fn strip_apk_control(outer: &Path) -> CliResult<()> {
  let entries = fs::read_dir(outer).map_err(|err| CliError::new(E_RUNTIME, format!("{err}")))?;

  for entry in entries.flatten() {
    let path = entry.path();
    let is_control = path.is_file()
      && entry.file_name().to_string_lossy().starts_with('.');
    if is_control {
      fs::remove_file(&path).map_err(|err| CliError::new(E_RUNTIME, format!("{err}")))?;
    }
  }

  Ok(())
}

/// Reads one field out of a `control` (`.ipk`) or `.PKGINFO` (`.apk`) file.
///
/// Both formats are `key <separator> value` lines — `Package: foo` for ipk,
/// `pkgname = foo` for apk — so one parser serves both. A missing file yields an
/// empty string, which makes every lookup miss and fall back to the file name.
fn package_name(control: &str, container: &Path) -> String {
  for key in ["Package", "pkgname"] {
    for line in control.lines() {
      let Some((name, value)) = line.split_once([':', '=']) else {
        continue;
      };
      if name.trim().eq_ignore_ascii_case(key) {
        let value = value.trim().trim_matches('"').to_string();
        if !value.is_empty() {
          return value;
        }
      }
    }
  }

  container.file_stem().map_or_else(
    || "unknown".to_string(),
    |value| value.to_string_lossy().to_string(),
  )
}

fn read_text(path: &Path) -> String {
  fs::read_to_string(path).unwrap_or_default()
}

/// A temp directory that deletes itself, on the error path included.
struct Scratch {
  path: PathBuf,
}

impl Scratch {
  fn new(prefix: &str) -> CliResult<Self> {
    let path = make_temp_dir(prefix).map_err(CliError::from)?;
    Ok(Self { path })
  }

  fn path(&self) -> &Path {
    &self.path
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.path);
  }
}

#[cfg(test)]
mod tests {
  use super::package_name;

  use std::path::Path;

  use pretty_assertions::assert_eq;

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
}
