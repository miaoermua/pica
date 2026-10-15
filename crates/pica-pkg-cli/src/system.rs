use crate::app::{App, CliError, CliResult, E_CONFIG_INVALID, E_IO, E_MISSING_COMMAND, E_RUNTIME};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Filesystem root an install or removal writes into.
///
/// Real invocations use `/`. Tests pass a temporary directory, which is the only
/// way to exercise the whole path — including what lands in `/usr/lib/pica` and
/// what a rollback has to undo — without touching the host.
#[derive(Debug, Clone)]
pub struct TargetRoot {
  root: PathBuf,
}

impl TargetRoot {
  #[must_use]
  pub fn system() -> Self {
    Self::new("/")
  }

  #[must_use]
  pub fn new(root: impl Into<PathBuf>) -> Self {
    Self { root: root.into() }
  }

  #[must_use]
  pub fn as_path(&self) -> &Path {
    &self.root
  }

  /// Maps an absolute target path (such as `/usr/lib/pica`) onto this root.
  #[must_use]
  pub fn resolve(&self, absolute: &str) -> PathBuf {
    self.root.join(absolute.trim_start_matches('/'))
  }
}

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
  run_tar(&["-xzf"], &[], pkgfile, target_dir)
}

/// Runs `tar` with the archive in the place an option expects it.
///
/// `flags` come before the archive and `after` — anything else the archive needs
/// to be read, such as `--ignore-zeros` — comes after it, because the argument
/// of `-f` is whatever directly follows it.
fn run_tar(flags: &[&str], after: &[&str], archive: &Path, target_dir: &Path) -> CliResult<()> {
  let output = Command::new("tar")
    .args(flags)
    .arg(archive)
    .args(after)
    .arg("-C")
    .arg(target_dir)
    .output()
    .map_err(|err| CliError::new(E_IO, format!("tar extract failed: {err}")))?;

  if output.status.success() {
    Ok(())
  } else {
    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    Err(CliError::new(E_RUNTIME, format!("tar extract failed: {}: {detail}", archive.display())))
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

pub(crate) fn stderr_or_stdout(stdout: &[u8], stderr: &[u8]) -> String {
  let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
  if !stderr_text.is_empty() {
    return stderr_text;
  }

  let stdout_text = String::from_utf8_lossy(stdout).trim().to_string();
  if stdout_text.is_empty() { "unknown error".to_string() } else { stdout_text }
}
