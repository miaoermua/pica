use pica_pkg_core::error::PicaResult;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

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
    let Ok(arch_entries) = fs::read_dir(platform_path) else {
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
    let root = tmp.path().to_path_buf();
    let binary = root.join("binary");
    fs::create_dir_all(binary.join("mt7621").join("all")).expect("mkdir binary mt7621/all");
    fs::create_dir_all(binary.join("mt7621").join("arm")).expect("mkdir binary mt7621/arm");
    fs::create_dir_all(binary.join("x86").join("all")).expect("mkdir binary x86/all");

    assert!(has_platform_arch_matrix(&binary));

    let platforms = collect_platforms(&[binary.as_path()]).expect("collect platforms");
    assert_eq!(platforms, vec!["mt7621".to_string(), "x86".to_string()]);

    let archs = collect_archs(&[binary.as_path()], "mt7621").expect("collect archs");
    assert_eq!(archs, vec!["all".to_string(), "arm".to_string()]);
  }
}
