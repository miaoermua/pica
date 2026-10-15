use super::{PackageManager, PackageManagerKind};
use crate::app::{CliError, CliResult, E_NO_SPACE, E_OPKG_INSTALL, E_OPKG_REMOVE};
use crate::system::{has_command, run_command_capture_output, stderr_or_stdout};
use std::fs;
use std::path::Path;
use std::process::Command;

const OPKG_LISTS_DIRS: [&str; 2] = ["/var/opkg-lists", "/tmp/opkg-lists"];
const OPKG_LOCK_FILES: [&str; 2] = ["/var/lock/opkg.lock", "/tmp/lock/opkg.lock"];
const OPKG_INFO_DIR: &str = "/usr/lib/opkg/info";

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
      return Err(CliError::new(
        E_NO_SPACE,
        format!("{label} install failed: {target} (storage-full). detail=[{detail}]"),
      ));
    }
    Err(CliError::new(
      E_OPKG_INSTALL,
      format!("{label} install failed: {target} detail=[{detail}]"),
    ))
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

  fn installed(&self) -> Vec<(String, String)> {
    let Ok(output) = Command::new("opkg").arg("list-installed").output() else {
      return Vec::new();
    };
    let mut packages = parse_installed_list(&String::from_utf8_lossy(&output.stdout));
    packages.sort();
    packages.dedup();
    packages
  }

  fn depends(&self, name: &str) -> Vec<String> {
    let Ok(text) = fs::read_to_string(Path::new(OPKG_INFO_DIR).join(format!("{name}.control")))
    else {
      return Vec::new();
    };
    parse_depends_field(&text)
  }

  fn owns_file(&self, path: &str) -> Option<String> {
    let entries = fs::read_dir(OPKG_INFO_DIR).ok()?;
    let mut owners: Vec<String> = entries
      .flatten()
      .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "list"))
      .filter_map(|entry| {
        let name = entry.path().file_stem()?.to_string_lossy().into_owned();
        let text = fs::read_to_string(entry.path()).ok()?;
        list_owns_path(&text, path).then_some(name)
      })
      .collect();
    owners.sort();
    owners.into_iter().next()
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

/// `opkg list-installed` prints `name - version`, one package per line.
fn parse_installed_list(text: &str) -> Vec<(String, String)> {
  text
    .lines()
    .filter_map(|line| {
      let (name, version) = line.split_once(" - ")?;
      let name = name.trim();
      let version = version.trim();
      (!name.is_empty() && !version.is_empty()).then(|| (name.to_string(), version.to_string()))
    })
    .collect()
}

/// `Depends: libc (>= 1.0), libubox` -> `["libc", "libubox"]`.
fn parse_depends_field(control: &str) -> Vec<String> {
  let mut values: Vec<String> = Vec::new();
  let mut collecting = false;
  for line in control.lines() {
    if collecting && line.starts_with([' ', '\t']) {
      values.extend(split_dependency_list(line));
      continue;
    }
    collecting = false;
    if line.starts_with("Depends:") {
      collecting = true;
      values.extend(split_dependency_list(line.trim_start_matches("Depends:")));
    }
  }
  values.sort();
  values.dedup();
  values
}

fn split_dependency_list(value: &str) -> Vec<String> {
  value
    .split(',')
    .filter_map(|item| {
      let name = item.trim().split(['(', ' ']).next()?.trim();
      (!name.is_empty()).then(|| name.to_string())
    })
    .collect()
}

/// Ownership lists hold one absolute path per line.
fn list_owns_path(list: &str, path: &str) -> bool {
  list.lines().any(|line| line.trim() == path)
}

fn opkg_lists_ready() -> bool {
  OPKG_LISTS_DIRS.iter().map(Path::new).any(opkg_list_dir_has_files)
}

fn opkg_list_dir_has_files(dir: &Path) -> bool {
  let Ok(entries) = fs::read_dir(dir) else {
    return false;
  };
  entries.flatten().any(|entry| entry.path().is_file())
}

fn is_opkg_lock_error(detail: &str) -> bool {
  let text = detail.to_ascii_lowercase();
  if text.contains("opkg.lock") {
    return true;
  }
  let mentions_lock = text.contains(" lock") || text.starts_with("lock") || text.contains("locked");
  let lock_failure =
    ["resource temporarily unavailable", "could not lock", "failed to lock", "cannot lock"]
      .iter()
      .any(|pattern| text.contains(pattern));
  mentions_lock && lock_failure
}

fn clear_opkg_lock_files_if_stale() -> bool {
  let mut cleared_any = false;
  for lock_path in OPKG_LOCK_FILES {
    let path = Path::new(lock_path);
    if !path.exists() {
      continue;
    }
    match read_opkg_lock_pid(path) {
      Some(pid) if Path::new("/proc").join(pid.to_string()).exists() => return false,
      Some(_) | None => {
        if fs::remove_file(path).is_ok() {
          cleared_any = true;
        }
      }
    }
  }
  cleared_any
}

fn read_opkg_lock_pid(path: &Path) -> Option<u32> {
  let text = fs::read_to_string(path).ok()?;
  text
    .split(|ch: char| !ch.is_ascii_digit())
    .filter(|token| !token.is_empty())
    .filter_map(|token| token.parse::<u32>().ok())
    .find(|pid| *pid > 0)
}

#[cfg(test)]
mod tests {
  use super::{
    clear_opkg_lock_files_if_stale, is_opkg_lock_error, list_owns_path, opkg_list_dir_has_files,
    parse_depends_field, parse_installed_list, read_opkg_lock_pid,
  };
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

  #[test]
  fn parses_list_installed_output() {
    let text = "busybox - 1.36.1-1\nluci-app-demo - 2:1.0.0-r0\nbroken line\n";
    assert_eq!(
      parse_installed_list(text),
      vec![
        ("busybox".to_string(), "1.36.1-1".to_string()),
        ("luci-app-demo".to_string(), "2:1.0.0-r0".to_string()),
      ]
    );
  }

  #[test]
  fn parses_depends_field_with_versions_and_continuations() {
    let control = "Package: demo\nDepends: libc (>= 1.0), libubox\n  , luci-base\n";
    assert_eq!(
      parse_depends_field(control),
      vec!["libc".to_string(), "libubox".to_string(), "luci-base".to_string()]
    );
    assert!(parse_depends_field("Package: demo\n").is_empty());
  }

  #[test]
  fn matches_file_ownership_by_exact_path() {
    let list = "/usr/bin/demo\n/usr/lib/demo/lib.so\n";
    assert!(list_owns_path(list, "/usr/bin/demo"));
    assert!(!list_owns_path(list, "/usr/bin/demo2"));
  }
}
