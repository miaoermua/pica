use pica_pkg_core::error::PicaResult;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Scans `pkgs/<backend>/` for the optional `<platform>/<arch>` matrix layout.
///
/// Detection is per backend, so an arch-specific opkg build and a noarch-only
/// apk build can coexist in one pkgdir.
pub(crate) fn has_platform_arch_matrix(root: &Path) -> bool {
  if !root.is_dir() {
    return false;
  }

  let Ok(platform_entries) = fs::read_dir(root) else {
    return false;
  };
  for platform_entry in platform_entries.flatten() {
    let platform_path = platform_entry.path();
    if !platform_path.is_dir() {
      continue;
    }
    let Ok(arch_entries) = fs::read_dir(&platform_path) else {
      continue;
    };
    for arch_entry in arch_entries.flatten() {
      if arch_entry.path().is_dir() {
        return true;
      }
    }
  }

  false
}

pub(crate) fn collect_platforms(roots: &[&Path]) -> PicaResult<Vec<String>> {
  let mut platforms = BTreeSet::new();
  for root in roots {
    for entry in fs::read_dir(root)? {
      let entry = entry?;
      if entry.path().is_dir() {
        platforms.insert(entry.file_name().to_string_lossy().to_string());
      }
    }
  }
  Ok(platforms.into_iter().collect())
}

pub(crate) fn collect_archs(roots: &[&Path], platform: &str) -> PicaResult<Vec<String>> {
  let mut archs = BTreeSet::new();
  for root in roots {
    let platform_path = root.join(platform);
    if !platform_path.is_dir() {
      continue;
    }
    for entry in fs::read_dir(platform_path)? {
      let entry = entry?;
      if entry.path().is_dir() {
        archs.insert(entry.file_name().to_string_lossy().to_string());
      }
    }
  }
  Ok(archs.into_iter().collect())
}

#[cfg(test)]
mod tests {
  use super::{collect_archs, collect_platforms, has_platform_arch_matrix};
  use pretty_assertions::assert_eq;
  use std::fs;

  #[test]
  fn matrix_detection_and_collection_work() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pkgs = tmp.path().join("pkgs").join("opkg");
    fs::create_dir_all(pkgs.join("mt7621/all")).expect("mkdir mt7621/all");
    fs::create_dir_all(pkgs.join("mt7621/arm")).expect("mkdir mt7621/arm");
    fs::create_dir_all(pkgs.join("x86/all")).expect("mkdir x86/all");

    assert!(has_platform_arch_matrix(&pkgs));

    let roots = [pkgs.as_path()];
    assert_eq!(collect_platforms(&roots).expect("collect platforms"), vec!["mt7621", "x86"]);
    assert_eq!(collect_archs(&roots, "mt7621").expect("collect archs"), vec!["all", "arm"]);
  }

  #[test]
  fn flat_layout_is_not_a_matrix() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pkgs = tmp.path().join("pkgs").join("apk");
    fs::create_dir_all(&pkgs).expect("mkdir");
    fs::write(pkgs.join("demo-1.0.0-r1.apk"), b"apk").expect("write package");

    assert!(!has_platform_arch_matrix(&pkgs));
    assert!(!has_platform_arch_matrix(&tmp.path().join("missing")));
  }
}
