use crate::app::{App, CliError, CliResult, E_CONFIG_INVALID, E_IO, E_MISSING_COMMAND, E_RUNTIME};
use crate::source::{FetchPolicy, MAX_REDIRECTS, resolve_location};
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

/// One request's outcome: either the bytes, or where the server pointed next.
enum Hop {
  Body(Vec<u8>),
  Redirect(String),
}

/// Download `url`, following redirects one at a time so every hop is checked
/// against `policy` before it is followed.
///
/// The hop loop lives here rather than in the fetch tool because `curl -L`
/// (and the equivalents) will happily walk from https to plain http and on to
/// whatever host the server names; the client that is trusting a source has to
/// be the one deciding what it is willing to fetch.
pub fn fetch_url(
  url: &str,
  policy: FetchPolicy,
  timeout_secs: u64,
  retry: u32,
  retry_delay_secs: u64,
) -> CliResult<Vec<u8>> {
  if timeout_secs == 0 {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("invalid --fetch-timeout: {timeout_secs}"),
    ));
  }

  // Detected on first use: a `file://` source needs no downloader at all, and a
  // device without one should still be able to read a local mirror.
  let mut tool: Option<FetchTool> = None;

  let mut current = url.to_string();
  let mut followed = 0_u32;

  loop {
    policy.check(&current)?;

    if let Some(path) = current.strip_prefix("file://") {
      return fs::read(path)
        .map_err(|err| CliError::new(E_IO, format!("read file url failed: {err}")));
    }

    let tool = if let Some(detected) = tool {
      detected
    } else {
      let detected = FetchTool::detect().ok_or_else(|| {
        CliError::new(E_MISSING_COMMAND, "no fetch tool found (need uclient-fetch, wget, or curl)")
      })?;
      tool = Some(detected);
      detected
    };

    match tool.fetch(&current, timeout_secs, retry, retry_delay_secs)? {
      Hop::Body(bytes) => return Ok(bytes),
      Hop::Redirect(location) => {
        followed += 1;
        if followed > MAX_REDIRECTS {
          return Err(CliError::new(
            E_RUNTIME,
            format!("too many redirects (>{MAX_REDIRECTS}) starting at {url}"),
          ));
        }

        let target = resolve_location(&current, &location)?;
        policy.check_redirect(&current, &target)?;
        current = target;
      }
    }
  }
}

/// Which downloader is in use. The one that can show us a redirect is
/// preferred; see `FetchTool::fetch` for what each can and cannot report.
#[derive(Debug, Clone, Copy)]
enum FetchTool {
  Curl,
  Wget,
  UclientFetch,
}

impl FetchTool {
  fn detect() -> Option<Self> {
    if has_command("curl") {
      Some(Self::Curl)
    } else if has_command("wget") {
      Some(Self::Wget)
    } else if has_command("uclient-fetch") {
      Some(Self::UclientFetch)
    } else {
      None
    }
  }

  /// `curl` and `wget` are asked not to follow redirects, so the caller can
  /// inspect each one. `uclient-fetch` has no way to report a redirect before
  /// following it, so it stays a single shot: its own redirect limit applies and
  /// the caller only sees the final body. Packages fetched this way are still
  /// checked against the index's SHA-256.
  fn fetch(
    self,
    url: &str,
    timeout_secs: u64,
    retry: u32,
    retry_delay_secs: u64,
  ) -> CliResult<Hop> {
    let timeout = timeout_secs.to_string();
    match self {
      Self::Curl => {
        with_retry(retry, retry_delay_secs, timeout_secs, url, || curl_hop(url, &timeout))
      }
      Self::Wget => {
        with_retry(retry, retry_delay_secs, timeout_secs, url, || wget_hop(url, &timeout))
      }
      Self::UclientFetch => {
        let args = ["-T", timeout.as_str(), "-O", "-", url];
        with_retry(retry, retry_delay_secs, timeout_secs, url, || {
          run_fetch("uclient-fetch", &args).map(Hop::Body)
        })
      }
    }
  }
}

fn with_retry(
  retry: u32,
  retry_delay_secs: u64,
  timeout_secs: u64,
  url: &str,
  mut attempt: impl FnMut() -> CliResult<Hop>,
) -> CliResult<Hop> {
  let max_attempts = retry.saturating_add(1);
  let mut last_error = String::new();

  for try_index in 1..=max_attempts {
    match attempt() {
      Ok(hop) => return Ok(hop),
      Err(err) => {
        last_error = err.message;
        if try_index < max_attempts {
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

/// One `curl` request that never follows a redirect, reporting either the body
/// or the `Location` it would have followed.
fn curl_hop(url: &str, timeout: &str) -> CliResult<Hop> {
  let headers = tempfile::NamedTempFile::new()
    .map_err(|err| CliError::new(E_IO, format!("temp file for headers failed: {err}")))?;
  let body = tempfile::NamedTempFile::new()
    .map_err(|err| CliError::new(E_IO, format!("temp file for body failed: {err}")))?;

  let header_path = headers.path().to_string_lossy().to_string();
  let body_path = body.path().to_string_lossy().to_string();

  let output = Command::new("curl")
    .args([
      "--connect-timeout",
      timeout,
      "--max-time",
      timeout,
      "--max-redirs",
      "0",
      "-sS",
      "-D",
      header_path.as_str(),
      "-o",
      body_path.as_str(),
      "-w",
      "%{http_code}",
      url,
    ])
    .output()
    .map_err(|err| CliError::new(E_IO, format!("curl failed: {err}")))?;

  if !output.status.success() {
    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    return Err(CliError::new(E_RUNTIME, format!("curl download failed: {detail}")));
  }

  let status: u16 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap_or(0);

  if (300..400).contains(&status) {
    let raw_headers = fs::read_to_string(headers.path())
      .map_err(|err| CliError::new(E_IO, format!("read redirect headers failed: {err}")))?;
    return match last_location(&raw_headers) {
      Some(location) => Ok(Hop::Redirect(location)),
      None => {
        Err(CliError::new(E_RUNTIME, format!("HTTP {status} without a Location header: {url}")))
      }
    };
  }

  if status >= 400 {
    return Err(CliError::new(E_RUNTIME, format!("HTTP {status} for {url}")));
  }

  let bytes = fs::read(body.path())
    .map_err(|err| CliError::new(E_IO, format!("read download body failed: {err}")))?;
  Ok(Hop::Body(bytes))
}

/// One `wget` request that asks it not to follow redirects. Busybox builds
/// without these options fall back to the tool's own redirect handling.
fn wget_hop(url: &str, timeout: &str) -> CliResult<Hop> {
  let body = tempfile::NamedTempFile::new()
    .map_err(|err| CliError::new(E_IO, format!("temp file for body failed: {err}")))?;
  let body_path = body.path().to_string_lossy().to_string();

  let output = Command::new("wget")
    .args([
      "-T",
      timeout,
      "-q",
      "--max-redirect=0",
      "--server-response",
      "-O",
      body_path.as_str(),
      url,
    ])
    .output()
    .map_err(|err| CliError::new(E_IO, format!("wget failed: {err}")))?;

  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

  if output.status.success() {
    let bytes = fs::read(body.path())
      .map_err(|err| CliError::new(E_IO, format!("read download body failed: {err}")))?;
    return Ok(Hop::Body(bytes));
  }

  if let Some(location) = last_location(&stderr) {
    return Ok(Hop::Redirect(location));
  }

  // A wget that does not know these options would otherwise stop working on
  // every redirecting source; fall back to its own behaviour for this request.
  let unsupported = ["unrecognized", "invalid option", "unknown option", "not supported"]
    .iter()
    .any(|needle| stderr.to_ascii_lowercase().contains(needle));
  if unsupported {
    let args = ["-T", timeout, "-qO-", url];
    return run_fetch("wget", &args).map(Hop::Body);
  }

  Err(CliError::new(E_RUNTIME, format!("wget download failed: {stderr}")))
}

/// The last `Location:` header in a header block. Following the last one is
/// what a browser does when a response carries more than one.
fn last_location(headers: &str) -> Option<String> {
  headers
    .lines()
    .filter_map(|line| {
      let (name, value) = line.split_once(':')?;
      if name.trim().eq_ignore_ascii_case("location") {
        Some(value.trim().to_string())
      } else {
        None
      }
    })
    .next_back()
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
