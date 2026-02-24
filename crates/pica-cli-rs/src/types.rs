use serde_json::{json, Value};
use std::env;
use std::path::{Path, PathBuf};

pub const DEFAULT_ETC_DIR: &str = "/etc/pica";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/pica";
pub const DEFAULT_ERROR_CODE: &str = "E_RUNTIME";

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
}

#[derive(Debug)]
pub struct CliError {
    pub code: &'static str,
    pub message: String,
}

pub type CliResult<T> = Result<T, CliError>;

impl CliError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
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
        Self {
            paths,
            options,
            json_last_phase: String::new(),
            json_has_text: false,
        }
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
            let phase = if self.json_last_phase.is_empty() {
                "unknown"
            } else {
                &self.json_last_phase
            };

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
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| CliError::new(DEFAULT_ERROR_CODE, message))
}

pub fn parse_options(args: Vec<String>) -> CliResult<(Options, Vec<String>)> {
    let mut json_mode = JsonMode::None;
    let mut non_interactive = false;
    let mut feed_policy = FeedPolicy::Ask;
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
                    return Err(CliError::new(
                        "E_POLICY_INVALID",
                        "--feed-policy requires <mode>",
                    ));
                };
                feed_policy = FeedPolicy::parse(value).ok_or_else(|| {
                    CliError::new(
                        "E_POLICY_INVALID",
                        format!("invalid --feed-policy: {value}"),
                    )
                })?;
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
        },
        positional,
    ))
}

pub fn ensure_dirs(paths: &Paths) -> CliResult<()> {
    std::fs::create_dir_all(&paths.etc_dir).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("mkdir {} failed: {err}", paths.etc_dir.display()),
        )
    })?;
    std::fs::create_dir_all(&paths.state_dir).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("mkdir {} failed: {err}", paths.state_dir.display()),
        )
    })?;
    std::fs::create_dir_all(&paths.cache_dir).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("mkdir {} failed: {err}", paths.cache_dir.display()),
        )
    })?;
    std::fs::create_dir_all(&paths.repos_cache_dir).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("mkdir {} failed: {err}", paths.repos_cache_dir.display()),
        )
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

fn write_json_atomic_pretty(path: &Path, value: &Value) -> CliResult<()> {
    let mut tmp_name = std::ffi::OsString::from(path.as_os_str());
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            CliError::new(
                DEFAULT_ERROR_CODE,
                format!("mkdir {} failed: {err}", parent.display()),
            )
        })?;
    }

    let content = serde_json::to_string_pretty(value)
        .map_err(|err| CliError::new(DEFAULT_ERROR_CODE, err.to_string()))?;
    std::fs::write(&tmp_path, content).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("write {} failed: {err}", tmp_path.display()),
        )
    })?;
    std::fs::rename(&tmp_path, path).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("rename {} failed: {err}", path.display()),
        )
    })?;

    Ok(())
}

fn env_path_or<V>(key: &str, default: V) -> PathBuf
where
    V: Into<PathBuf>,
{
    env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| default.into())
}
