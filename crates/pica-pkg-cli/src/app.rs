use crate::i18n::{Key, Lang};
use crate::state::{read_json_file, write_json_atomic_pretty};
use pica_pkg_core::error::PicaError;
use pica_pkg_core::layout::DEFAULT_LUCI_I18N;
use serde_json::{Value, json};
use std::env;
use std::path::{Path, PathBuf};

pub const DEFAULT_ETC_DIR: &str = "/etc/pica";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/pica";
pub const E_RUNTIME: &str = "E_RUNTIME";
pub const E_ARG_INVALID: &str = "E_ARG_INVALID";
pub const E_IO: &str = "E_IO";
pub const E_JSON_INVALID: &str = "E_JSON_INVALID";
pub const E_DB_INVALID: &str = "E_DB_INVALID";
pub const E_INDEX_INVALID: &str = "E_INDEX_INVALID";
pub const E_INDEX_UNAVAILABLE: &str = "E_INDEX_UNAVAILABLE";
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
/// Something that can only be answered by a person, asked where there is none.
pub const E_NOT_INTERACTIVE: &str = "E_NOT_INTERACTIVE";

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

/// What an install does with the optional dependencies a manifest declares.
///
/// This is deliberately a separate choice from [`FeedPolicy`]: where a
/// dependency comes from and whether the user wants it at all are different
/// questions, and answering one must not silently answer the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionalDeps {
  /// Offer the list on a terminal, install none where nobody can be asked.
  Ask,
  /// Install every optional dependency that is available (`--with-optional`).
  All,
  /// Install none of them (`--no-optional`).
  None,
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
  /// Explicit `--pkgmgr` value. Highest-priority backend selection.
  pub pkgmgr: Option<String>,
  /// What to do with the optional dependencies in a manifest.
  pub optional_deps: OptionalDeps,
  /// Explicit `--lang` value. Overrides the configured language for this run
  /// only; it is never written back to the configuration.
  pub lang: Option<Lang>,
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
  pub package_manager: Option<Box<dyn crate::pkgmgr::PackageManager>>,
  pub backend: crate::pkgmgr::PackageManagerKind,
  /// The `apk_force_non_repository` setting, read by
  /// [`App::apply_backend_config`]. Until then the default stands, which is what
  /// the tests and the probing paths see.
  pub apk_force_non_repository: bool,
  pub json_last_phase: String,
  pub json_has_text: bool,
  /// The language this run speaks: `--lang`, else the configured `language`,
  /// else English. Setup changes it mid-run once the user has picked one.
  pub lang: Lang,
}

impl App {
  pub fn new(paths: Paths, options: Options) -> Self {
    let backend = crate::pkgmgr::detect_kind();
    let apk_force_non_repository = true;
    let lang = options.lang.unwrap_or_else(|| conf_get_language(&paths.conf_file));
    Self {
      paths,
      options,
      package_manager: crate::pkgmgr::package_manager_for(backend),
      backend,
      apk_force_non_repository,
      json_last_phase: String::new(),
      json_has_text: false,
      lang,
    }
  }

  /// The text for `key` in this run's language.
  pub fn text(&self, key: Key) -> &'static str {
    self.lang.text(key)
  }

  /// The text for `key` with its placeholders filled in.
  pub fn args(&self, key: Key, args: &[&str]) -> String {
    self.lang.args(key, args)
  }

  /// A localized progress line, with the same `::` prefix as [`App::log_info`].
  pub fn info(&mut self, key: Key) {
    let message = self.text(key);
    self.log_info(message);
  }

  /// A localized progress line with placeholders.
  pub fn info_args(&mut self, key: Key, args: &[&str]) {
    let message = self.args(key, args);
    self.log_info(message);
  }

  /// A localized warning, with the same `warning:` prefix as [`App::log_warn`].
  pub fn warn(&mut self, key: Key) {
    let message = self.text(key);
    self.log_warn(message);
  }

  /// A localized warning with placeholders.
  pub fn warn_args(&mut self, key: Key, args: &[&str]) {
    let message = self.args(key, args);
    self.log_warn(message);
  }

  pub fn package_manager(&self) -> CliResult<&dyn crate::pkgmgr::PackageManager> {
    self
      .package_manager
      .as_deref()
      .ok_or_else(|| CliError::new(E_MISSING_COMMAND, "no supported package manager found"))
  }

  /// Backend identity of this invocation, including the Pica-native `pica`.
  #[must_use]
  pub fn backend(&self) -> crate::pkgmgr::PackageManagerKind {
    self.backend
  }

  /// Switches backend explicitly. A backend that needs a system binary which is
  /// absent is an error rather than a silent fallback, so packages never land in
  /// the wrong package database; `pica` needs no command and always succeeds.
  pub fn select_package_manager(&mut self, value: &str) -> CliResult<()> {
    let kind = crate::pkgmgr::PackageManagerKind::parse(value)
      .ok_or_else(|| CliError::new(E_CONFIG_INVALID, format!("invalid pkgmgr: {value}")))?;
    if !kind.requires_system_binary() {
      self.backend = kind;
      self.package_manager = None;
      return Ok(());
    }
    if self.backend == kind && self.package_manager.is_some() {
      return Ok(());
    }
    let manager = crate::pkgmgr::package_manager_with(kind, self.apk_force_non_repository)
      .ok_or_else(|| {
        CliError::new(E_MISSING_COMMAND, format!("missing required command: {}", kind.as_str()))
      })?;
    self.backend = kind;
    self.package_manager = Some(manager);
    Ok(())
  }

  /// Applies backend selection in priority order: `--pkgmgr`, then the config
  /// file, then the auto-detected backend chosen in [`App::new`].
  ///
  /// The apk setting is read first and the auto-detected backend rebuilt with
  /// it, so the policy applies whether or not the config names a backend:
  /// `apk` is what most routers end up with, and it is the backend that has a
  /// setting to carry.
  pub fn apply_backend_config(&mut self) -> CliResult<()> {
    self.apk_force_non_repository = conf_get_apk_force_non_repository(&self.paths.conf_file)?;
    if self.backend.requires_system_binary() {
      self.package_manager =
        crate::pkgmgr::package_manager_with(self.backend, self.apk_force_non_repository);
    }
    if let Some(value) = self.options.pkgmgr.clone() {
      return self.select_package_manager(&value);
    }
    if let Some(value) = conf_get_pkgmgr(&self.paths.conf_file) {
      return self.select_package_manager(&value);
    }
    Ok(())
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
  let mut pkgmgr = None;
  let mut optional_deps = OptionalDeps::Ask;
  let mut optional_given = false;
  let mut lang = None;
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
      // Both flags answer the same question, so guessing which one wins would
      // be a silent change of what the user asked for. Say so instead.
      "--with-optional" | "--no-optional" => {
        if optional_given {
          return Err(CliError::new(
            E_ARG_INVALID,
            "--with-optional and --no-optional are mutually exclusive",
          ));
        }
        optional_given = true;
        optional_deps =
          if args[index] == "--with-optional" { OptionalDeps::All } else { OptionalDeps::None };
        index += 1;
      }
      "--feed-policy" => {
        let value = flag_value(args, index, "--feed-policy", "<mode>", E_POLICY_INVALID)?;
        feed_policy = FeedPolicy::parse(value).ok_or_else(|| {
          CliError::new(E_POLICY_INVALID, format!("invalid --feed-policy: {value}"))
        })?;
        index += 2;
      }
      "--fetch-timeout" => {
        let value = flag_value(args, index, "--fetch-timeout", "<seconds>", E_CONFIG_INVALID)?;
        fetch_timeout = parse_positive_u64_option("--fetch-timeout", value)?;
        index += 2;
      }
      "--fetch-retry" => {
        let value = flag_value(args, index, "--fetch-retry", "<count>", E_CONFIG_INVALID)?;
        fetch_retry = parse_u32_option("--fetch-retry", value)?;
        index += 2;
      }
      "--fetch-retry-delay" => {
        let value = flag_value(args, index, "--fetch-retry-delay", "<seconds>", E_CONFIG_INVALID)?;
        fetch_retry_delay = parse_u64_option("--fetch-retry-delay", value)?;
        index += 2;
      }
      "--pkgmgr" => {
        let value = flag_value(args, index, "--pkgmgr", "<opkg|apk|pica>", E_CONFIG_INVALID)?;
        if crate::pkgmgr::PackageManagerKind::parse(value).is_none() {
          return Err(CliError::new(E_CONFIG_INVALID, format!("invalid --pkgmgr: {value}")));
        }
        pkgmgr = Some(value.clone());
        index += 2;
      }
      // The interface language for this run only. What is stored stays stored.
      "--lang" | "--language" => {
        let value = flag_value(args, index, "--lang", "<zh-CN|en-US>", E_CONFIG_INVALID)?;
        lang = Some(
          Lang::parse(value)
            .ok_or_else(|| CliError::new(E_CONFIG_INVALID, format!("invalid --lang: {value}")))?,
        );
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
      pkgmgr,
      optional_deps,
      lang,
    },
    positional,
  ))
}

/// The value a flag takes, or an error naming the flag and what it accepts.
fn flag_value<'a>(
  args: &'a [String],
  index: usize,
  flag: &str,
  takes: &str,
  code: &'static str,
) -> CliResult<&'a String> {
  args.get(index + 1).ok_or_else(|| CliError::new(code, format!("{flag} requires {takes}")))
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
        "i18n": DEFAULT_LUCI_I18N,
        "pkgmgr": Value::Null,
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

pub(crate) fn prompt_yn(question: &str, default_yes: bool) -> bool {
  use std::io::{self, BufRead, Write};

  let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
  eprint!("{question} {hint} ");
  let _ = io::stderr().flush();

  let mut input = String::new();
  let stdin = io::stdin();
  let mut handle = stdin.lock();
  if handle.read_line(&mut input).is_err() {
    return default_yes;
  }

  let answer = input.trim().to_ascii_lowercase();
  match answer.as_str() {
    "y" | "yes" => true,
    "n" | "no" => false,
    _ => default_yes,
  }
}

pub(crate) fn conf_get_i18n(conf_file: &Path) -> Option<String> {
  let conf = read_json_file(conf_file).ok()?;
  let value = conf.get("i18n").and_then(Value::as_str).unwrap_or(DEFAULT_LUCI_I18N);
  Some(value.to_string())
}

/// Reads the configured backend. `null`, a missing key and an empty string all
/// mean "auto-detect"; an unrecognized value is an error so a typo cannot
/// silently pick the wrong package database.
pub(crate) fn conf_get_pkgmgr(conf_file: &Path) -> Option<String> {
  let conf = read_json_file(conf_file).ok()?;
  let value = conf.get("pkgmgr")?.as_str()?.trim();
  (!value.is_empty()).then(|| value.to_string())
}

/// Whether a local `.apk` is installed with `--force-non-repository`.
///
/// apk-tools 3.x, which `OpenWrt` 25.12 and later ship, refuses to install a
/// package that no configured repository provides unless that flag is given.
/// Pica hands the `pkgs/apk/*.apk` files themselves to `apk add`, so the
/// default is `true`; `false` keeps the call to what apk 2.x accepted, for a
/// router whose apk build does not know the option.
///
/// A missing key or `null` means the default. Anything other than `true`/`false`
/// is an error: this decides whether a check is waived, so a typo must not fall
/// back to a value the operator did not choose.
pub(crate) fn conf_get_apk_force_non_repository(conf_file: &Path) -> CliResult<bool> {
  let Ok(conf) = read_json_file(conf_file) else { return Ok(true) };
  match conf.get("apk_force_non_repository") {
    None | Some(Value::Null) => Ok(true),
    Some(Value::Bool(value)) => Ok(*value),
    Some(other) => Err(CliError::new(
      E_CONFIG_INVALID,
      format!(
        "invalid \"apk_force_non_repository\" in {}: expected true or false, found {other}",
        conf_file.display()
      ),
    )),
  }
}

pub(crate) fn resolve_lang(conf_file: &Path) -> String {
  let conf = read_json_file(conf_file).ok();
  conf
    .and_then(|value| value.get("i18n").and_then(Value::as_str).map(ToString::to_string))
    .unwrap_or_else(|| DEFAULT_LUCI_I18N.to_string())
}

/// The interface language stored in the configuration, or the default.
///
/// A config file that cannot be read or parsed is not an error here: every
/// command that needs the configuration reports that on its own, and refusing
/// to print a message because the language is unknown would hide the real
/// problem. An unknown value is the default for the same reason.
pub(crate) fn conf_get_language(conf_file: &Path) -> Lang {
  read_json_file(conf_file).map_or(Lang::default(), |conf| Lang::from_config(&conf))
}

#[cfg(test)]
mod tests {
  use super::{FeedPolicy, conf_get_apk_force_non_repository, conf_get_pkgmgr, parse_options};
  use serde_json::json;
  use std::fs;

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

  #[test]
  fn parse_options_reads_explicit_pkgmgr() {
    let input =
      vec!["--pkgmgr".to_string(), "apk".to_string(), "-S".to_string(), "demo".to_string()];
    let (options, positional) = parse_options(&input).expect("parse options");
    assert_eq!(options.pkgmgr.as_deref(), Some("apk"));
    assert_eq!(positional, vec!["-S".to_string(), "demo".to_string()]);
  }

  #[test]
  fn parse_options_rejects_unknown_pkgmgr() {
    let err = parse_options(&["--pkgmgr".to_string(), "pacman".to_string()])
      .expect_err("must reject unknown backend");
    assert!(err.message.contains("--pkgmgr"));
  }

  #[test]
  fn config_backend_ignores_null_and_empty_values() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let conf = dir.path().join("pica.json");
    fs::write(&conf, json!({ "pkgmgr": null }).to_string()).expect("write config");
    assert_eq!(conf_get_pkgmgr(&conf), None);
    fs::write(&conf, json!({ "pkgmgr": "  " }).to_string()).expect("write config");
    assert_eq!(conf_get_pkgmgr(&conf), None);
    fs::write(&conf, json!({ "repos": [] }).to_string()).expect("write config");
    assert_eq!(conf_get_pkgmgr(&conf), None);
    assert_eq!(conf_get_pkgmgr(&dir.path().join("missing.json")), None);
  }

  #[test]
  fn config_backend_is_returned_verbatim_for_validation() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let conf = dir.path().join("pica.json");
    fs::write(&conf, json!({ "pkgmgr": " opkg " }).to_string()).expect("write config");
    assert_eq!(conf_get_pkgmgr(&conf).as_deref(), Some("opkg"));
    fs::write(&conf, json!({ "pkgmgr": "bogus" }).to_string()).expect("write config");
    assert_eq!(conf_get_pkgmgr(&conf).as_deref(), Some("bogus"));
  }

  #[test]
  fn the_apk_local_install_setting_defaults_to_on() {
    // apk-tools 3.x refuses a local file that no repository provides, which is
    // every file Pica installs from its own `pkgs/`, so an unset key means the
    // flag is passed rather than the install failing on a current router.
    let dir = tempfile::tempdir().expect("create temp dir");
    let conf = dir.path().join("pica.json");

    assert!(conf_get_apk_force_non_repository(&dir.path().join("missing.json")).expect("default"));
    fs::write(&conf, json!({ "repos": [] }).to_string()).expect("write config");
    assert!(conf_get_apk_force_non_repository(&conf).expect("default"));
    fs::write(&conf, json!({ "apk_force_non_repository": null }).to_string())
      .expect("write config");
    assert!(conf_get_apk_force_non_repository(&conf).expect("null means the default"));
    fs::write(&conf, json!({ "apk_force_non_repository": false }).to_string())
      .expect("write config");
    assert!(!conf_get_apk_force_non_repository(&conf).expect("off"));
  }

  #[test]
  fn a_non_boolean_apk_local_install_setting_is_rejected() {
    // The setting decides whether a signature check is waived, so a value that
    // is neither true nor false fails the command instead of picking one.
    let dir = tempfile::tempdir().expect("create temp dir");
    let conf = dir.path().join("pica.json");
    fs::write(&conf, json!({ "apk_force_non_repository": "yes" }).to_string()).expect("config");

    let err = conf_get_apk_force_non_repository(&conf).expect_err("must reject the value");
    assert!(err.message.contains("apk_force_non_repository"), "{}", err.message);
    assert!(err.message.contains("expected true or false"), "{}", err.message);
  }
}
