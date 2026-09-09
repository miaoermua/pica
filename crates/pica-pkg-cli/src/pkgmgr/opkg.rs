use super::{PackageManager, PackageManagerKind};
use crate::app::{CliError, CliResult, E_NO_SPACE, E_OPKG_INSTALL, E_OPKG_REMOVE};
use crate::system::{has_command, run_command_capture_output, stderr_or_stdout};
use std::fs;
use std::path::Path;
use std::process::Command;

const OPKG_LISTS_DIRS: [&str; 2] = ["/var/opkg-lists", "/tmp/opkg-lists"];
const OPKG_LOCK_FILES: [&str; 2] = ["/var/lock/opkg.lock", "/tmp/lock/opkg.lock"];

pub struct OpkgBackend;

impl OpkgBackend {
  fn update_ignore() {
    if !has_command("opkg") || opkg_lists_ready() {
      return;
    }

    let Ok(output) = Command::new("opkg").arg("update").output() else {
      return;
    };
    if output.status.success() {
      return;
    }

    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    if is_opkg_lock_error(&detail) && clear_opkg_lock_files_if_stale() {
      let _ = Command::new("opkg").arg("update").output();
    }
  }
}

impl PackageManager for OpkgBackend {
  fn kind(&self) -> PackageManagerKind {
    PackageManagerKind::Opkg
  }

  fn update_index(&self) {
    Self::update_ignore();
  }

  fn package_exists(&self, name: &str) -> bool {
    let Ok(output) = Command::new("opkg").arg("info").arg(name).output() else {
      return false;
    };
    let text = format!(
      "{}{}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
    !text.trim().is_empty() && !text.to_ascii_lowercase().contains("unknown package")
  }

  fn is_installed(&self, name: &str) -> bool {
    let Ok(output) = Command::new("opkg").arg("status").arg(name).output() else {
      return false;
    };
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.contains("status:") && text.contains("installed")
  }

  fn installed_version(&self, name: &str) -> Option<String> {
    let output = Command::new("opkg").arg("status").arg(name).output().ok()?;
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .find_map(|line| line.trim().strip_prefix("Version: ").map(str::trim).map(str::to_string))
  }

  fn install(&self, label: &str, target: &str) -> CliResult<()> {
    Self::update_ignore();
    let output = Command::new("opkg")
      .arg("install")
      .arg(target)
      .output()
      .map_err(|err| CliError::new(E_OPKG_INSTALL, format!("opkg install failed: {err}")))?;
    if output.status.success() {
      return Ok(());
    }
    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    if detail.to_ascii_lowercase().contains("no space left on device") {
      return Err(CliError::new(E_NO_SPACE, format!("{label} install failed: {target} (storage-full). detail=[{detail}]")));
    }
    Err(CliError::new(E_OPKG_INSTALL, format!("{label} install failed: {target} detail=[{detail}]")))
  }

  fn remove(&self, target: &str) -> CliResult<()> {
    let output = Command::new("opkg")
      .arg("remove")
      .arg(target)
      .output()
      .map_err(|err| CliError::new(E_OPKG_REMOVE, format!("opkg remove failed: {err}")))?;
    if output.status.success() {
      Ok(())
    } else {
      let detail = stderr_or_stdout(&output.stdout, &output.stderr);
      Err(CliError::new(E_OPKG_REMOVE, format!("opkg remove failed: {target} detail=[{detail}]")))
    }
  }

  fn snapshot_installed(&self) -> Vec<String> {
    let Ok(output) = Command::new("opkg").arg("list-installed").output() else {
      return Vec::new();
    };
    let mut packages: Vec<String> = String::from_utf8_lossy(&output.stdout)
      .lines()
      .filter_map(|line| line.split_once(" - ").map(|(name, _)| name.trim().to_string()))
      .filter(|name| !name.is_empty())
      .collect();
    packages.sort();
    packages.dedup();
    packages
  }

  fn architectures(&self) -> Vec<String> {
    let Ok(output) = run_command_capture_output("opkg", &["print-architecture"]) else {
      return Vec::new();
    };
    String::from_utf8_lossy(&output)
      .lines()
      .filter_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("arch")).then(|| fields.next()).flatten()
      })
      .map(ToString::to_string)
      .collect()
  }
}

fn opkg_lists_ready() -> bool {
  OPKG_LISTS_DIRS.iter().map(Path::new).any(opkg_list_dir_has_files)
}

fn opkg_list_dir_has_files(dir: &Path) -> bool {
  let Ok(entries) = fs::read_dir(dir) else { return false; };
  entries.flatten().any(|entry| entry.path().is_file())
}

fn is_opkg_lock_error(detail: &str) -> bool {
  let text = detail.to_ascii_lowercase();
  if text.contains("opkg.lock") { return true; }
  let mentions_lock = text.contains(" lock") || text.starts_with("lock") || text.contains("locked");
  let lock_failure = ["resource temporarily unavailable", "could not lock", "failed to lock", "cannot lock"]
    .iter().any(|pattern| text.contains(pattern));
  mentions_lock && lock_failure
}

fn clear_opkg_lock_files_if_stale() -> bool {
  let mut cleared_any = false;
  for lock_path in OPKG_LOCK_FILES {
    let path = Path::new(lock_path);
    if !path.exists() { continue; }
    match read_opkg_lock_pid(path) {
      Some(pid) if Path::new("/proc").join(pid.to_string()).exists() => return false,
      Some(_) | None => {
        if fs::remove_file(path).is_ok() { cleared_any = true; }
      }
    }
  }
  cleared_any
}

fn read_opkg_lock_pid(path: &Path) -> Option<u32> {
  let text = fs::read_to_string(path).ok()?;
  text.split(|ch: char| !ch.is_ascii_digit())
    .filter(|token| !token.is_empty())
    .filter_map(|token| token.parse::<u32>().ok())
    .find(|pid| *pid > 0)
}

#[cfg(test)]
mod tests {
  use super::{clear_opkg_lock_files_if_stale, is_opkg_lock_error, opkg_list_dir_has_files, read_opkg_lock_pid};
  use pretty_assertions::assert_eq;
  use std::fs;

  #[test]
  fn detects_opkg_lock_errors() {
    assert!(is_opkg_lock_error("Could not lock /var/lock/opkg.lock"));
    assert!(is_opkg_lock_error("Cannot lock package database"));
    assert!(!is_opkg_lock_error("wget: bad address"));
  }

  #[test]
  fn list_dir_check_requires_regular_files() {
    let dir = tempfile::tempdir().expect("create temp dir");
    assert!(!opkg_list_dir_has_files(dir.path()));
    fs::create_dir_all(dir.path().join("subdir")).expect("create nested dir");
    assert!(!opkg_list_dir_has_files(dir.path()));
    fs::write(dir.path().join("generic"), b"Package: test\n").expect("write list file");
    assert!(opkg_list_dir_has_files(dir.path()));
  }

  #[test]
  fn read_opkg_lock_pid_extracts_numeric_token() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let lock_file = dir.path().join("opkg.lock");
    fs::write(&lock_file, "pid: 1234\n").expect("write lock file");
    assert_eq!(read_opkg_lock_pid(&lock_file), Some(1234));
    fs::write(&lock_file, "nonsense").expect("write non pid lock file");
    assert_eq!(read_opkg_lock_pid(&lock_file), None);
  }

  #[test]
  fn stale_lock_cleanup_skips_missing_default_paths() {
    assert!(!clear_opkg_lock_files_if_stale());
  }
}
