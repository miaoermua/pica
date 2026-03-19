use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_IO, E_MISSING_COMMAND, E_NO_SPACE, E_OPKG_INSTALL,
  E_OPKG_REMOVE, E_RUNTIME,
};
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

const OPKG_LISTS_DIRS: [&str; 2] = ["/var/opkg-lists", "/tmp/opkg-lists"];
const OPKG_LOCK_FILES: [&str; 2] = ["/var/lock/opkg.lock", "/tmp/lock/opkg.lock"];

pub fn fetch_url(
  url: &str,
  is_supported_url: fn(&str) -> bool,
  timeout_secs: u64,
  retry: u32,
  retry_delay_secs: u64,
) -> CliResult<Vec<u8>> {
  if !is_supported_url(url) {
    return Err(CliError::new(E_RUNTIME, format!("unsupported URL: {url}")));
  }

  if timeout_secs == 0 {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("invalid --fetch-timeout: {timeout_secs}"),
    ));
  }

  if let Some(path) = url.strip_prefix("file://") {
    return fs::read(path)
      .map_err(|err| CliError::new(E_IO, format!("read file url failed: {err}")));
  }

  let max_attempts = retry.saturating_add(1);
  let timeout_text = timeout_secs.to_string();

  let fetchers: &[(&str, &[&str])] = &[
    ("uclient-fetch", &["-T", &timeout_text, "-O", "-", url]),
    ("wget", &["-T", &timeout_text, "-qO-", url]),
    ("curl", &["--connect-timeout", &timeout_text, "--max-time", &timeout_text, "-fsSL", url]),
  ];

  for &(cmd, args) in fetchers {
    if !has_command(cmd) {
      continue;
    }
    return try_fetch_with_retry(cmd, args, max_attempts, retry_delay_secs, timeout_secs, url);
  }

  Err(CliError::new(E_MISSING_COMMAND, "no fetch tool found (need uclient-fetch, wget, or curl)"))
}

fn try_fetch_with_retry(
  cmd: &str,
  args: &[&str],
  max_attempts: u32,
  retry_delay_secs: u64,
  timeout_secs: u64,
  url: &str,
) -> CliResult<Vec<u8>> {
  let mut last_error = String::new();
  for attempt in 1..=max_attempts {
    match run_fetch(cmd, args) {
      Ok(output) => return Ok(output),
      Err(err) => {
        last_error = err.message;
        if attempt < max_attempts {
          std::thread::sleep(std::time::Duration::from_secs(retry_delay_secs));
        }
      }
    }
  }
  Err(CliError::new(
    E_RUNTIME,
    format!(
      "download failed after {max_attempts} attempts (timeout={timeout_secs}s): {url} detail=[{last_error}]"
    ),
  ))
}

pub fn need_cmd(name: &str) -> CliResult<()> {
  if has_command(name) {
    Ok(())
  } else {
    Err(CliError::new(E_MISSING_COMMAND, format!("missing required command: {name}")))
  }
}

pub fn has_command(name: &str) -> bool {
  if name.contains('/') {
    return Path::new(name).is_file();
  }

  let Some(path_env) = env::var_os("PATH") else {
    return false;
  };

  env::split_paths(&path_env).any(|dir| {
    let full = dir.join(name);
    full.is_file()
  })
}

pub fn opkg_update_ignore() {
  if !has_command("opkg") {
    return;
  }

  if opkg_lists_ready() {
    return;
  }

  let Ok(output) = Command::new("opkg").arg("update").output() else {
    return;
  };

  if output.status.success() {
    return;
  }

  let detail = stderr_or_stdout(&output.stdout, &output.stderr);
  if !is_opkg_lock_error(&detail) {
    return;
  }

  if !clear_opkg_lock_files_if_stale() {
    return;
  }
  let _ = Command::new("opkg").arg("update").output();
}

pub fn opkg_has_package(name: &str) -> bool {
  let Ok(output) = Command::new("opkg").arg("info").arg(name).output() else {
    return false;
  };

  let text = format!(
    "{}{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  if text.trim().is_empty() {
    return false;
  }

  !text.to_ascii_lowercase().contains("unknown package")
}

pub fn opkg_installed_version(name: &str) -> Option<String> {
  let output = Command::new("opkg").arg("status").arg(name).output().ok()?;

  let content = String::from_utf8_lossy(&output.stdout);
  for line in content.lines() {
    let trimmed = line.trim();
    if let Some(value) = trimmed.strip_prefix("Version: ") {
      return Some(value.trim().to_string());
    }
  }

  None
}

pub fn opkg_install_pkg(label: &str, target: &str) -> CliResult<()> {
  opkg_update_ignore();

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

  Err(CliError::new(E_OPKG_INSTALL, format!("{label} install failed: {target} detail=[{detail}]")))
}

pub fn opkg_remove_pkg(target: &str) -> CliResult<()> {
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

pub fn opkg_is_installed(name: &str) -> bool {
  let Ok(output) = Command::new("opkg").arg("status").arg(name).output() else {
    return false;
  };

  let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
  text.contains("status:") && text.contains("installed")
}

pub fn opkg_snapshot_installed() -> Vec<String> {
  let Ok(output) = Command::new("opkg").arg("list-installed").output() else {
    return Vec::new();
  };

  let text = String::from_utf8_lossy(&output.stdout);
  let mut out = Vec::new();
  for line in text.lines() {
    let Some((name, _)) = line.split_once(" - ") else {
      continue;
    };
    let trimmed = name.trim();
    if !trimmed.is_empty() {
      out.push(trimmed.to_string());
    }
  }

  out.sort();
  out.dedup();
  out
}

pub fn run_command_text(program: &str, args: &[&str]) -> CliResult<String> {
  let output = Command::new(program)
    .args(args)
    .output()
    .map_err(|err| CliError::new(E_IO, format!("{program} failed: {err}")))?;

  if !output.status.success() {
    return Err(CliError::new(
      E_RUNTIME,
      format!("{program} exited with status {}", output.status),
    ));
  }

  Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn run_command_capture_output(program: &str, args: &[&str]) -> CliResult<Vec<u8>> {
  let output = Command::new(program)
    .args(args)
    .output()
    .map_err(|err| CliError::new(E_IO, format!("{program} failed: {err}")))?;

  if output.status.success() {
    Ok(output.stdout)
  } else {
    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    Err(CliError::new(E_RUNTIME, format!("{program} failed: {detail}")))
  }
}

pub fn run_tar_extract(pkgfile: &Path, target_dir: &Path) -> CliResult<()> {
  let output = Command::new("tar")
    .arg("-xzf")
    .arg(pkgfile)
    .arg("-C")
    .arg(target_dir)
    .output()
    .map_err(|err| CliError::new(E_IO, format!("tar extract failed: {err}")))?;

  if output.status.success() {
    Ok(())
  } else {
    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    Err(CliError::new(E_RUNTIME, format!("tar extract failed: {detail}")))
  }
}

fn run_fetch(command: &str, args: &[&str]) -> CliResult<Vec<u8>> {
  let output = Command::new(command)
    .args(args)
    .output()
    .map_err(|err| CliError::new(E_IO, format!("{command} failed: {err}")))?;

  if output.status.success() {
    Ok(output.stdout)
  } else {
    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    Err(CliError::new(E_RUNTIME, format!("{command} download failed: {detail}")))
  }
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
  let lock_failure = [
    "resource temporarily unavailable",
    "could not lock",
    "failed to lock",
    "cannot lock",
  ]
  .iter()
  .any(|&pat| text.contains(pat));

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
      Some(pid) if pid_is_running(pid) => {
        return false;
      }
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

  for token in text.split(|ch: char| !ch.is_ascii_digit()) {
    if token.is_empty() {
      continue;
    }
    if let Ok(pid) = token.parse::<u32>() {
      if pid > 0 {
        return Some(pid);
      }
    }
  }

  None
}

fn pid_is_running(pid: u32) -> bool {
  if pid == 0 {
    return false;
  }

  Path::new("/proc").join(pid.to_string()).exists()
}

pub fn run_hook(app: &mut App, tmpdir: &Path, hook_rel: &str, label: &str) -> CliResult<()> {
  if hook_rel.is_empty() {
    return Ok(());
  }

  let hook_path = tmpdir.join(hook_rel);
  if !hook_path.is_file() {
    return Err(CliError::new(E_CONFIG_INVALID, format!("{label} hook not found: {hook_rel}")));
  }

  app.log_info(format!("Running {label} hook: {hook_rel}"));
  run_command_capture_output("sh", &[hook_path.to_string_lossy().as_ref()]).map(|_| ())
}

fn stderr_or_stdout(stdout: &[u8], stderr: &[u8]) -> String {
  let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
  if !stderr_text.is_empty() {
    return stderr_text;
  }

  let stdout_text = String::from_utf8_lossy(stdout).trim().to_string();
  if stdout_text.is_empty() {
    "unknown error".to_string()
  } else {
    stdout_text
  }
}

#[cfg(test)]
mod tests {
  use super::{
    clear_opkg_lock_files_if_stale, is_opkg_lock_error, opkg_list_dir_has_files, read_opkg_lock_pid,
  };
  use pretty_assertions::assert_eq;
  use std::fs;

  #[test]
  fn lock_error_detection_handles_common_messages() {
    assert!(is_opkg_lock_error(
      "Could not lock /var/lock/opkg.lock: Resource temporarily unavailable"
    ));
    assert!(is_opkg_lock_error("Cannot lock package database"));
    assert!(!is_opkg_lock_error("wget: bad address"));
  }

  #[test]
  fn list_dir_check_requires_regular_files() {
    let dir = tempfile::tempdir().expect("create temp dir");
    assert!(!opkg_list_dir_has_files(dir.path()));

    let nested = dir.path().join("subdir");
    fs::create_dir_all(&nested).expect("create nested dir");
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
