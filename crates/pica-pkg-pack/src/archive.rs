use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::{sha256_file, write_atomic};
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn run_tar_create(base_dir: &Path, output: &Path, items: &[String]) -> PicaResult<()> {
  let mut command = Command::new("tar");
  command.arg("-C");
  command.arg(base_dir);
  command.arg("-czf");
  command.arg(output);
  for item in items {
    command.arg(item);
  }

  let result = command.output()?;
  if result.status.success() {
    Ok(())
  } else {
    let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
    Err(PicaError::msg(format!(
      "tar create failed: {}",
      if stderr.is_empty() { "unknown error" } else { &stderr }
    )))
  }
}

pub(crate) fn write_sha256sum_entry(path: &Path, filename: &str, sha256: &str) -> PicaResult<()> {
  let mut lines: Vec<String> = if path.is_file() {
    fs::read_to_string(path)?.lines().map(ToString::to_string).collect()
  } else {
    Vec::new()
  };

  lines.retain(|line| {
    let trimmed = line.trim_end();
    !trimmed.ends_with(&format!("  {filename}"))
  });
  lines.push(format!("{sha256}  {filename}"));
  lines.sort();

  let mut content = lines.join("\n");
  content.push('\n');
  write_atomic(path, content.as_bytes())?;
  Ok(())
}

pub(crate) fn compute_sha256(path: &Path) -> PicaResult<String> {
  sha256_file(path)
}

/// Total size of regular files under `dir`, i.e. the installed size of a
/// package's `root/` payload. Symlinks contribute nothing, matching the way
/// pacman reports `size`.
pub(crate) fn unpacked_size(dir: &Path) -> u64 {
  let Ok(entries) = fs::read_dir(dir) else { return 0 };

  let mut total = 0u64;
  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(metadata) = fs::symlink_metadata(&path) else { continue };
    if metadata.is_dir() {
      total += unpacked_size(&path);
    } else if metadata.is_file() {
      total += metadata.len();
    }
  }
  total
}

#[cfg(test)]
mod tests {
  use super::{unpacked_size, write_sha256sum_entry};
  use std::fs;

  #[test]
  fn unpacked_size_sums_regular_files_only() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("usr/bin")).expect("mkdir");
    fs::write(root.join("usr/bin/app"), b"0123456789").expect("write bin");
    fs::write(root.join("etc-config"), b"abc").expect("write config");
    #[cfg(unix)]
    std::os::unix::fs::symlink("usr/bin/app", root.join("link")).expect("symlink");

    assert_eq!(unpacked_size(&root), 13);
    assert_eq!(unpacked_size(&root.join("missing")), 0);
  }

  #[test]
  fn sha256sums_replaces_existing_entry_and_stays_sorted() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let sums = tmp.path().join("SHA256SUMS");
    write_sha256sum_entry(&sums, "b.pkg.tar.gz", &"2".repeat(64)).expect("write b");
    write_sha256sum_entry(&sums, "a.pkg.tar.gz", &"1".repeat(64)).expect("write a");
    write_sha256sum_entry(&sums, "b.pkg.tar.gz", &"3".repeat(64)).expect("rewrite b");

    let lines: Vec<String> =
      fs::read_to_string(&sums).expect("read sums").lines().map(ToString::to_string).collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with("  a.pkg.tar.gz"));
    assert!(lines[1].starts_with(&"3".repeat(64)));
  }
}
