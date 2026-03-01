use crate::state::write_json_atomic_pretty;
use pica_pkg_core::error::PicaError;
use serde_json::json;
use std::env;
use std::path::PathBuf;

pub const DEFAULT_ETC_DIR: &str = "/etc/pica";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/pica";
pub const E_RUNTIME: &str = "E_RUNTIME";
pub const E_ARG_INVALID: &str = "E_ARG_INVALID";
pub const E_IO: &str = "E_IO";
pub const E_JSON_INVALID: &str = "E_JSON_INVALID";
pub const E_DB_INVALID: &str = "E_DB_INVALID";
pub const E_INDEX_INVALID: &str = "E_INDEX_INVALID";
pub const E_CONFIG_INVALID: &str = "E_CONFIG_INVALID";
pub const E_POLICY_INVALID: &str = "E_POLICY_INVALID";
pub const E_REPO_INVALID: &str = "E_REPO_INVALID";
pub const E_MISSING_COMMAND: &str = "E_MISSING_COMMAND";
pub const E_LOCK_BUSY: &str = "E_LOCK_BUSY";
pub const E_OPKG_INSTALL: &str = "E_OPKG_INSTALL";
pub const E_OPKG_REMOVE: &str = "E_OPKG_REMOVE";
pub const E_NO_SPACE: &str = "E_NO_SPACE";
pub const E_MANIFEST_INVALID: &str = "E_MANIFEST_INVALID";
pub const E_PACKAGE_INVALID: &str = "E_PACKAGE_INVALID";
pub const E_PLATFORM_UNSUPPORTED: &str = "E_PLATFORM_UNSUPPORTED";
pub const E_VERSION_INCOMPATIBLE: &str = "E_VERSION_INCOMPATIBLE";
pub const E_INTEGRITY_INVALID: &str = "E_INTEGRITY_INVALID";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonMode {
  None,
  Errors,
  All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedPolicy {
  Ask,
  FeedFirst,
  PackagedFirst,
  FeedOnly,
  PackagedOnly,
}

impl FeedPolicy {
  pub fn parse(value: &str) -> Option<Self> {
    match value {
      "ask" => Some(Self::Ask),
      "feed-first" => Some(Self::FeedFirst),
      "packaged-first" => Some(Self::PackagedFirst),
      "feed-only" => Some(Self::FeedOnly),
      "packaged-only" => Some(Self::PackagedOnly),
      _ => None,
    }
  }
}

#[derive(Debug, Clone)]
pub struct Paths {
  pub etc_dir: PathBuf,
  pub conf_file: PathBuf,
  pub state_dir: PathBuf,
  pub db_file: PathBuf,
  pub index_file: PathBuf,
  pub cache_dir: PathBuf,
  pub repos_cache_dir: PathBuf,
  pub report_file: PathBuf,
  pub lock_file: PathBuf,
}

impl Paths {
  pub fn from_env() -> Self {
    let etc_dir = env_path_or("PICA_ETC_DIR", DEFAULT_ETC_DIR);
    let conf_file = env_path_or("PICA_CONF_FILE", etc_dir.join("pica.json"));

    let state_dir = env_path_or("PICA_STATE_DIR", DEFAULT_STATE_DIR);
    let db_file = env_path_or("PICA_DB_FILE", state_dir.join("db.json"));
    let index_file = env_path_or("PICA_INDEX_FILE", state_dir.join("index.json"));
    let cache_dir = env_path_or("PICA_CACHE_DIR", state_dir.join("cache"));
    let repos_cache_dir = env_path_or("PICA_REPOS_CACHE_DIR", cache_dir.join("repos"));
    let report_file = env_path_or("PICA_REPORT_FILE", state_dir.join("install-report.json"));
    let lock_file = env_path_or("PICA_LOCK_FILE", state_dir.join("db.lck"));

    Self {
      etc_dir,
      conf_file,
      state_dir,
      db_file,
      index_file,
      cache_dir,
      repos_cache_dir,
      report_file,
      lock_file,
    }
  }
}

#[derive(Debug, Clone)]
pub struct Options {
  pub json_mode: JsonMode,
  pub non_interactive: bool,
  pub feed_policy: FeedPolicy,
  pub fetch_timeout: u64,
  pub fetch_retry: u32,
  pub fetch_retry_delay: u64,
}

#[derive(Debug)]
pub struct CliError {
  pub code: &'static str,
  pub message: String,
}

pub type CliResult<T> = Result<T, CliError>;

impl CliError {
  pub fn new(code: &'static str, message: impl Into<String>) -> Self {
    Self { code, message: message.into() }
  }
}

impl From<PicaError> for CliError {
  fn from(value: PicaError) -> Self {
    match value {
      PicaError::Message(message) => CliError::new(E_RUNTIME, message),
      PicaError::Io(err) => CliError::new(E_IO, err.to_string()),
      PicaError::Json(err) => CliError::new(E_JSON_INVALID, err.to_string()),
    }
  }
}

pub struct App {
  pub paths: Paths,
  pub options: Options,
  pub json_last_phase: String,
  pub json_has_text: bool,
}

impl App {
  pub fn new(paths: Paths, options: Options) -> Self {
    Self { paths, options, json_last_phase: String::new(), json_has_text: false }
  }

  pub fn set_phase(&mut self, phase: &str) {
    self.json_last_phase = phase.to_string();
  }

  pub fn log_info(&mut self, message: impl AsRef<str>) {
    self.json_has_text = true;
    println!(":: {}", message.as_ref());
  }

  pub fn log_warn(&mut self, message: impl AsRef<str>) {
    self.json_has_text = true;
    eprintln!("warning: {}", message.as_ref());
  }

  pub fn emit_success(&self, command: &str, target: &str) {
    if self.options.json_mode != JsonMode::All || self.json_has_text {
      return;
    }

    let payload = json!({
        "ok": true,
        "command": command,
        "target": target,
    });
    println!("{payload}");
  }

  pub fn emit_error(&self, err: &CliError) {
    if self.options.json_mode != JsonMode::None {
      let phase = if self.json_last_phase.is_empty() { "unknown" } else { &self.json_last_phase };

      let payload = json!({
          "ok": false,
          "code": err.code,
          "message": err.message,
          "phase": phase,
      });
      eprintln!("{payload}");
    }

    eprintln!("pica: {}", err.message);
  }
}

pub fn require_arg<'a>(args: &'a [String], index: usize, message: &str) -> CliResult<&'a str> {
  args.get(index).map(String::as_str).ok_or_else(|| CliError::new(E_ARG_INVALID, message))
}

pub fn parse_options(args: &[String]) -> CliResult<(Options, Vec<String>)> {
  let mut json_mode = JsonMode::None;
  let mut non_interactive = false;
  let mut feed_policy = FeedPolicy::Ask;
  let mut fetch_timeout = 30u64;
  let mut fetch_retry = 2u32;
  let mut fetch_retry_delay = 1u64;
  let mut positional = Vec::new();

  let mut index = 0;
  while index < args.len() {
    match args[index].as_str() {
      "--json" => {
        json_mode = JsonMode::All;
        index += 1;
      }
      "--json-errors" => {
        json_mode = JsonMode::Errors;
        index += 1;
      }
      "--non-interactive" => {
        non_interactive = true;
        index += 1;
      }
      "--feed-policy" => {
        let Some(value) = args.get(index + 1) else {
          return Err(CliError::new(E_POLICY_INVALID, "--feed-policy requires <mode>"));
        };
        feed_policy = FeedPolicy::parse(value).ok_or_else(|| {
          CliError::new(E_POLICY_INVALID, format!("invalid --feed-policy: {value}"))
        })?;
        index += 2;
      }
      "--fetch-timeout" => {
        let Some(value) = args.get(index + 1) else {
          return Err(CliError::new(E_CONFIG_INVALID, "--fetch-timeout requires <seconds>"));
        };
        fetch_timeout = parse_positive_u64_option("--fetch-timeout", value)?;
        index += 2;
      }
      "--fetch-retry" => {
        let Some(value) = args.get(index + 1) else {
          return Err(CliError::new(E_CONFIG_INVALID, "--fetch-retry requires <count>"));
        };
        fetch_retry = parse_u32_option("--fetch-retry", value)?;
        index += 2;
      }
      "--fetch-retry-delay" => {
        let Some(value) = args.get(index + 1) else {
          return Err(CliError::new(E_CONFIG_INVALID, "--fetch-retry-delay requires <seconds>"));
        };
        fetch_retry_delay = parse_u64_option("--fetch-retry-delay", value)?;
        index += 2;
      }
      other => {
        positional.push(other.to_string());
        index += 1;
      }
    }
  }

  Ok((
    Options {
      json_mode,
      non_interactive,
      feed_policy,
      fetch_timeout,
      fetch_retry,
      fetch_retry_delay,
    },
    positional,
  ))
}

fn parse_positive_u64_option(flag: &str, value: &str) -> CliResult<u64> {
  let parsed = parse_u64_option(flag, value)?;
  if parsed == 0 {
    return Err(CliError::new(E_CONFIG_INVALID, format!("invalid {flag}: {value}")));
  }
  Ok(parsed)
}

fn parse_u64_option(flag: &str, value: &str) -> CliResult<u64> {
  value
    .parse::<u64>()
    .map_err(|_| CliError::new(E_CONFIG_INVALID, format!("invalid {flag}: {value}")))
}

fn parse_u32_option(flag: &str, value: &str) -> CliResult<u32> {
  value
    .parse::<u32>()
    .map_err(|_| CliError::new(E_CONFIG_INVALID, format!("invalid {flag}: {value}")))
}

pub fn ensure_dirs(paths: &Paths) -> CliResult<()> {
  std::fs::create_dir_all(&paths.etc_dir).map_err(|err| {
    CliError::new(E_IO, format!("mkdir {} failed: {err}", paths.etc_dir.display()))
  })?;
  std::fs::create_dir_all(&paths.state_dir).map_err(|err| {
    CliError::new(E_IO, format!("mkdir {} failed: {err}", paths.state_dir.display()))
  })?;
  std::fs::create_dir_all(&paths.cache_dir).map_err(|err| {
    CliError::new(E_IO, format!("mkdir {} failed: {err}", paths.cache_dir.display()))
  })?;
  std::fs::create_dir_all(&paths.repos_cache_dir).map_err(|err| {
    CliError::new(E_IO, format!("mkdir {} failed: {err}", paths.repos_cache_dir.display()))
  })?;

  if !paths.conf_file.is_file() {
    let content = json!({
        "repos": [],
        "i18n": "zh-cn",
    });
    write_json_atomic_pretty(&paths.conf_file, &content)?;
  }

  if !paths.db_file.is_file() {
    let content = json!({
        "schema": 1,
        "installed": {},
    });
    write_json_atomic_pretty(&paths.db_file, &content)?;
  }

  if !paths.index_file.is_file() {
    let content = json!({
        "schema": 1,
        "repos": {},
    });
    write_json_atomic_pretty(&paths.index_file, &content)?;
  }

  if !paths.report_file.is_file() {
    let content = json!({
        "schema": 1,
        "reports": {},
    });
    write_json_atomic_pretty(&paths.report_file, &content)?;
  }

  Ok(())
}

fn env_path_or<V>(key: &str, default: V) -> PathBuf
where
  V: Into<PathBuf>,
{
  env::var_os(key).map_or_else(|| default.into(), PathBuf::from)
}

#[cfg(test)]
mod tests {
  use super::{parse_options, FeedPolicy};

  #[test]
  fn parse_options_fetch_settings() {
    let input = vec![
      "--fetch-timeout".to_string(),
      "15".to_string(),
      "--fetch-retry".to_string(),
      "4".to_string(),
      "--fetch-retry-delay".to_string(),
      "2".to_string(),
      "-S".to_string(),
    ];

    let (options, positional) = parse_options(&input).expect("parse options");
    assert_eq!(options.fetch_timeout, 15);
    assert_eq!(options.fetch_retry, 4);
    assert_eq!(options.fetch_retry_delay, 2);
    assert_eq!(options.feed_policy, FeedPolicy::Ask);
    assert_eq!(positional, vec!["-S".to_string()]);
  }

  #[test]
  fn parse_options_rejects_invalid_fetch_timeout() {
    let input = vec!["--fetch-timeout".to_string(), "0".to_string()];
    let err = parse_options(&input).expect_err("must reject zero timeout");
    assert!(err.message.contains("--fetch-timeout"));
  }

  #[test]
  fn parse_options_rejects_invalid_fetch_retry() {
    let input = vec!["--fetch-retry".to_string(), "x".to_string()];
    let err = parse_options(&input).expect_err("must reject invalid retry");
    assert!(err.message.contains("--fetch-retry"));
  }

  #[test]
  fn parse_options_rejects_invalid_fetch_retry_delay() {
    let input = vec!["--fetch-retry-delay".to_string(), "-1".to_string()];
    let err = parse_options(&input).expect_err("must reject invalid retry delay");
    assert!(err.message.contains("--fetch-retry-delay"));
  }
}
