use crate::error::{PicaError, PicaResult};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[must_use]
pub fn now_unix_secs() -> u64 {
  SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_secs()).unwrap_or(0)
}

#[must_use]
pub fn now_unix_nanos() -> u128 {
  SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_nanos()).unwrap_or(0)
}

/// # Errors
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn read_json_file(path: &Path) -> PicaResult<serde_json::Value> {
  let content = fs::read_to_string(path)?;
  Ok(serde_json::from_str(&content)?)
}

/// # Errors
/// Returns an error if serialization fails or the file cannot be written.
pub fn write_json_file_pretty(path: &Path, value: &serde_json::Value) -> PicaResult<()> {
  let content = serde_json::to_string_pretty(value)?;
  write_atomic(path, content.as_bytes())
}

/// # Errors
/// Returns an error if the temporary file cannot be written or renamed.
pub fn write_atomic(path: &Path, content: &[u8]) -> PicaResult<()> {
  let tmp = temp_path_for(path);
  if let Some(parent) = tmp.parent() {
    fs::create_dir_all(parent)?;
  }
  fs::write(&tmp, content)?;
  fs::rename(&tmp, path)?;
  Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
  let mut candidate = path.as_os_str().to_os_string();
  candidate.push(".tmp");
  PathBuf::from(candidate)
}

/// # Errors
/// Returns an error if the directory cannot be created.
pub fn ensure_dir(path: &Path) -> PicaResult<()> {
  fs::create_dir_all(path)?;
  Ok(())
}

#[must_use]
pub fn sha256_hex(content: &[u8]) -> String {
  let hash = Sha256::digest(content);
  format!("{hash:x}")
}

/// # Errors
/// Returns an error if the file cannot be read.
pub fn sha256_file(path: &Path) -> PicaResult<String> {
  let content = fs::read(path)?;
  Ok(sha256_hex(&content))
}

/// Creates a temporary directory with a unique name.
///
/// Note: This returns an owned `PathBuf` rather than using `tempfile::TempDir`
/// because callers (e.g., install's `TempDirGuard`) need ownership of the path
/// and manage the directory lifetime themselves.
///
/// # Errors
/// Returns an error if the temporary directory cannot be created.
pub fn make_temp_dir(prefix: &str) -> PicaResult<PathBuf> {
  let path = env::temp_dir().join(format!("{prefix}-{}-{}", process::id(), now_unix_nanos()));
  fs::create_dir_all(&path)?;
  Ok(path)
}

/// # Errors
/// Returns an error if the source is not a directory or any file operation fails.
pub fn copy_dir_recursive(source: &Path, target: &Path) -> PicaResult<()> {
  if !source.is_dir() {
    return Err(PicaError::msg(format!("not a directory: {}", source.display())));
  }

  ensure_dir(target)?;
  for entry in fs::read_dir(source)? {
    let entry = entry?;
    let from = entry.path();
    let to = target.join(entry.file_name());
    let metadata = fs::symlink_metadata(&from)?;

    if metadata.is_dir() {
      copy_dir_recursive(&from, &to)?;
    } else if metadata.is_file() {
      fs::copy(&from, &to)?;
    } else if metadata.file_type().is_symlink() {
      copy_symlink(&from, &to)?;
    }
  }

  Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path) -> PicaResult<()> {
  use std::os::unix::fs::symlink;

  let link_target = fs::read_link(source)?;
  symlink(link_target, target)?;
  Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(source: &Path, target: &Path) -> PicaResult<()> {
  let _ = target;
  fs::copy(source, target)?;
  Ok(())
}

/// # Errors
/// Returns an error if the executable path cannot be resolved.
pub fn resolve_script_dir_from_exe() -> PicaResult<PathBuf> {
  let exe = env::current_exe()?;
  let Some(parent) = exe.parent() else {
    return Err(PicaError::msg("cannot resolve executable directory"));
  };
  Ok(parent.to_path_buf())
}

/// # Errors
/// Returns an error if the command cannot be spawned.
pub fn run_command_capture(mut command: Command) -> PicaResult<Output> {
  let output = command.output()?;
  Ok(output)
}

/// # Errors
/// Returns an error if the command fails or exits with a non-zero status.
pub fn run_command_success(mut command: Command, context: &str) -> PicaResult<()> {
  let output = command.output()?;
  if output.status.success() {
    return Ok(());
  }
  let stderr = String::from_utf8_lossy(&output.stderr);
  let stdout = String::from_utf8_lossy(&output.stdout);
  let detail =
    if stderr.trim().is_empty() { stdout.trim().to_string() } else { stderr.trim().to_string() };
  Err(PicaError::msg(format!("{context}: {detail}")))
}
