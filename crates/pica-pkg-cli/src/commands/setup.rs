//! `pica setup`: a numbered menu, not a TUI.
//!
//! The wizard only decides *what to write*. Addresses are resolved by
//! [`crate::source`], the entry descriptor is read by
//! [`pica_pkg_core::repo::parse_entry_descriptor`], downloads go through
//! [`crate::system::fetch_url`] under the same [`FetchPolicy`] the rest of the
//! tool uses, and the last step is simply [`crate::commands::sync::repos`].
//!
//! Commit strategy: save, then prove it. The new configuration is written
//! atomically, the existing sync runs (which is what validates the sources end
//! to end), and if that fails the previous configuration and index are put back
//! byte for byte. Nothing is written before every question is answered, so EOF
//! or a cancel leaves no trace.

use crate::app::{
  App, CliError, CliResult, E_ARG_INVALID, E_CONFIG_INVALID, E_IO, E_NOT_INTERACTIVE,
  E_REPO_INVALID, JsonMode, ensure_dirs,
};
use crate::commands::sync;
use crate::i18n::{Key, Lang};
use crate::source::{self, FetchPolicy};
use crate::state::{read_json_file, write_json_atomic_pretty};
use crate::system::fetch_url;
use pica_pkg_core::repo::parse_entry_descriptor;
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

/// The official source root, when this build has one.
///
/// This is the single place the address lives. It is `None` because this
/// repository does not publish an index yet, and inventing an address would
/// send people to a host nobody maintains. Filling in this constant is the
/// whole change once the official index exists.
pub(crate) const OFFICIAL_REPO_ROOT: Option<&str> = None;

/// The name an official source gets when the user does not pick one.
const OFFICIAL_NAME: &str = "official";
/// The name a custom source gets when the user does not pick one.
const CUSTOM_NAME: &str = "main";
/// Set to `1`/`true`/`yes` to answer the prompts from a pipe.
pub(crate) const ASSUME_TTY_ENV: &str = "PICA_ASSUME_TTY";

pub fn run(app: &mut App) -> CliResult<()> {
  // Menus on stdout next to a JSON document would make the document
  // unparseable, and a wizard cannot run unattended anyway.
  if app.options.json_mode != JsonMode::None {
    return Err(CliError::new(E_ARG_INVALID, app.text(Key::SetupNotJson)));
  }
  if app.options.non_interactive || !stdin_is_interactive() {
    return Err(CliError::new(E_NOT_INTERACTIVE, app.text(Key::SetupNeedsTerminal)));
  }

  // Read before creating: a config that exists but cannot be read must be
  // reported, not replaced by the empty skeleton `ensure_dirs` writes.
  let conf_path = app.paths.conf_file.clone();
  let mut conf = load_config(app, &conf_path)?;

  ensure_dirs(&app.paths)
    .map_err(|err| CliError::new(err.code, app.args(Key::SetupWriteFailed, &[&err.message])))?;

  let Some(language) = ask_language(app)? else {
    cancelled(app);
    return Ok(());
  };
  // Everything after this point is read in the language just picked,
  // including the errors of the steps that follow.
  app.lang = language;

  let mut repos: Vec<Value> =
    conf.get("repos").and_then(Value::as_array).cloned().unwrap_or_default();

  if !configure_sources(app, &mut repos)? {
    cancelled(app);
    return Ok(());
  }

  conf["language"] = json!(language.code());
  conf["repos"] = Value::Array(repos);

  commit(app, &conf_path, &conf)
}

/// Says so and stops: a cancel is a successful "nothing happened".
fn cancelled(app: &App) {
  println!("{}", app.text(Key::SetupCancelled));
}

/// The existing configuration, or an empty one on a fresh machine.
///
/// A file that exists but cannot be read is an error rather than something to
/// overwrite: it may be the only record of sources the user still wants, and
/// "setup replaced my config" is not a recoverable surprise.
fn load_config(app: &App, conf_path: &Path) -> CliResult<Value> {
  match read_json_file(conf_path) {
    Ok(value) => Ok(value),
    Err(_) if !conf_path.exists() => Ok(json!({})),
    Err(err) => Err(CliError::new(
      err.code,
      app.args(Key::SetupConfUnreadable, &[&conf_path.display().to_string(), &err.message]),
    )),
  }
}

fn ask_language(app: &App) -> CliResult<Option<Lang>> {
  println!();
  println!("{}", app.text(Key::SetupTitle));
  println!();
  for (index, lang) in Lang::ALL.iter().enumerate() {
    println!("{}. {} ({})", index + 1, lang.native_name(), lang.code());
  }
  println!();

  let Some(choice) = ask_number(app, Key::SetupLanguagePrompt, Lang::ALL.len())? else {
    return Ok(None);
  };
  Ok(Some(Lang::ALL[choice - 1]))
}

/// The source half of the wizard. `Ok(false)` means the user cancelled.
fn configure_sources(app: &mut App, repos: &mut Vec<Value>) -> CliResult<bool> {
  loop {
    println!();
    println!("{}", app.text(Key::SetupSources));

    if repos.is_empty() {
      if add_source(app, repos)?.is_none() {
        return Ok(false);
      }
    } else {
      print_existing(app, repos);
      println!();
      println!("1. {}", app.text(Key::SetupSourceMenuAdd));
      println!("2. {}", app.text(Key::SetupSourceMenuEdit));
      println!("3. {}", app.text(Key::SetupSourceMenuKeep));

      let Some(choice) = ask_number(app, Key::SetupSourceMenuPrompt, 3)? else {
        return Ok(false);
      };
      match choice {
        1 => {
          if add_source(app, repos)?.is_none() {
            return Ok(false);
          }
        }
        2 => {
          println!();
          let Some(pick) = ask_number(app, Key::SetupPickSource, repos.len())? else {
            return Ok(false);
          };
          if edit_source(app, repos, pick - 1)?.is_none() {
            return Ok(false);
          }
        }
        _ => return Ok(true),
      }
    }

    // One source is a complete answer; a second one is one keystroke away.
    println!();
    match ask_yes(app, Key::SetupAddAnother)? {
      Some(true) => {}
      Some(false) => return Ok(true),
      None => return Ok(false),
    }
  }
}

fn print_existing(app: &App, repos: &[Value]) {
  for (index, repo) in repos.iter().enumerate() {
    let name = repo.get("name").and_then(Value::as_str).unwrap_or("");
    let url = repo.get("url").and_then(Value::as_str).unwrap_or("");
    let backends = backends_of(repo).join(", ");
    println!(
      "{}",
      app.args(Key::SetupSourceLine, &[&(index + 1).to_string(), name, url, &backends])
    );
  }
}

fn backends_of(repo: &Value) -> Vec<String> {
  repo
    .get("backends")
    .and_then(Value::as_array)
    .map(|list| list.iter().filter_map(Value::as_str).map(ToString::to_string).collect())
    .unwrap_or_default()
}

/// Adds one source: the official one, or a root the user types.
///
/// `None` means the user cancelled, `Some(())` that one was added.
fn add_source(app: &mut App, repos: &mut Vec<Value>) -> CliResult<Option<()>> {
  loop {
    println!();
    match OFFICIAL_REPO_ROOT {
      Some(root) => println!("1. {} ({root})", app.text(Key::SetupSourceMenuOfficial)),
      // The entry stays and says why it cannot be picked: a menu that hides the
      // official source would leave a user looking for it.
      None => println!(
        "1. {} — {}",
        app.text(Key::SetupSourceMenuOfficial),
        app.text(Key::SetupOfficialUnavailable)
      ),
    }
    println!("2. {}", app.text(Key::SetupSourceMenuCustom));

    let Some(choice) = ask_number(app, Key::SetupSourceMenuPrompt, 2)? else {
      return Ok(None);
    };

    if choice == 1 {
      let Some(root) = OFFICIAL_REPO_ROOT else {
        continue;
      };
      return match prepare_source(app, root, "")? {
        Some(prepared) => {
          let name = ask_name(app, OFFICIAL_NAME, repos, None)?;
          match name {
            Some(name) => {
              repos.push(source_json(&name, &prepared));
              Ok(Some(()))
            }
            None => Ok(None),
          }
        }
        None => Ok(None),
      };
    }

    let Some(root) = ask_line(app, Key::SetupSourceUrlNew, &[])? else {
      return Ok(None);
    };
    let root = root.trim();
    if root.is_empty() {
      app.warn(Key::SetupUrlEmpty);
      continue;
    }

    let Some(prepared) = prepare_source(app, root, "")? else {
      // The reason was already printed; ask again rather than saving a source
      // that has not been shown to work.
      continue;
    };
    let Some(name) = ask_name(app, CUSTOM_NAME, repos, None)? else {
      return Ok(None);
    };
    repos.push(source_json(&name, &prepared));
    return Ok(Some(()));
  }
}

fn edit_source(app: &mut App, repos: &mut [Value], index: usize) -> CliResult<Option<()>> {
  let current = repos[index].clone();
  let current_name = current.get("name").and_then(Value::as_str).unwrap_or("").to_string();
  let current_url = current.get("url").and_then(Value::as_str).unwrap_or("").to_string();

  let Some(root) = ask_line(app, Key::SetupSourceUrl, &[&current_url])? else {
    return Ok(None);
  };
  let root = if root.trim().is_empty() { current_url } else { root.trim().to_string() };

  let Some(prepared) = prepare_source(app, &root, &current_name)? else {
    return Ok(None);
  };
  let Some(name) = ask_name(app, &current_name, repos, Some(index))? else {
    return Ok(None);
  };

  repos[index] = source_json(&name, &prepared);
  Ok(Some(()))
}

/// A root whose `index.json` has been read and whose backends were chosen.
struct Prepared {
  root: String,
  allow_insecure_http: bool,
  backends: Vec<String>,
}

struct Checked {
  root: String,
  allow_insecure_http: bool,
  declared: Vec<String>,
}

/// Reads and validates the entry descriptor, then asks which declared backends
/// to enable.
fn prepare_source(app: &mut App, root: &str, _name: &str) -> CliResult<Option<Prepared>> {
  let Some(checked) = check_source(app, root)? else {
    return Ok(None);
  };
  let Some(backends) = choose_backends(app, &checked.declared)? else {
    return Ok(None);
  };
  Ok(Some(Prepared {
    root: checked.root,
    allow_insecure_http: checked.allow_insecure_http,
    backends,
  }))
}

/// The root, plus what the source itself says it provides.
///
/// Failures keep the library's own words and wrap them in a translated
/// sentence: "invalid index.json" without the parser's complaint only tells
/// half the story.
fn check_source(app: &mut App, root: &str) -> CliResult<Option<Checked>> {
  let root = root.trim().trim_end_matches('/').to_string();

  if !root.contains("://") {
    return Err(CliError::new(E_CONFIG_INVALID, app.args(Key::SetupRootIsRelative, &[&root])));
  }

  let mut allow_insecure_http = false;
  let mut policy = FetchPolicy::source(false);
  if let Err(err) = policy.check(&root) {
    if !err.message.contains("allow_insecure_http") {
      return Err(CliError::new(
        E_CONFIG_INVALID,
        app.args(Key::SetupUrlRejected, &[&err.message]),
      ));
    }
    match ask_yes_args(app, Key::SetupAllowHttp, &[&root])? {
      Some(true) => {}
      Some(false) | None => return Ok(None),
    }
    allow_insecure_http = true;
    policy = FetchPolicy::source(true);
    if let Err(err) = policy.check(&root) {
      return Err(CliError::new(
        E_CONFIG_INVALID,
        app.args(Key::SetupUrlRejected, &[&err.message]),
      ));
    }
  }

  let entry_url = source::join_child(&root, "index.json").map_err(|err| {
    CliError::new(E_CONFIG_INVALID, app.args(Key::SetupUrlRejected, &[&err.message]))
  })?;

  let raw = match fetch_url(
    &entry_url,
    policy,
    app.options.fetch_timeout,
    app.options.fetch_retry,
    app.options.fetch_retry_delay,
  ) {
    Ok(raw) => raw,
    Err(err) => {
      app.warn_args(Key::SetupFetchFailed, &[&entry_url, &err.message]);
      return Ok(None);
    }
  };

  let text = String::from_utf8(raw).map_err(|_| {
    CliError::new(E_REPO_INVALID, app.args(Key::SetupFetchFailed, &[&entry_url, "not UTF-8"]))
  })?;

  let declared = match parse_entry_descriptor(&text) {
    Ok(descriptor) => descriptor.managers.keys().cloned().collect(),
    Err(error) => {
      app.warn_args(Key::SetupDescriptorInvalid, &[&entry_url, &error.to_string()]);
      return Ok(None);
    }
  };

  Ok(Some(Checked { root, allow_insecure_http, declared }))
}

/// What this device uses when the source serves it, otherwise every declared
/// backend. `--pkgmgr` is not consulted: it picks a backend for one operation,
/// while this list is what the device syncs every time.
fn default_backends(app: &App, declared: &[String]) -> Vec<String> {
  let device = app.backend().as_str();
  if declared.iter().any(|name| name == device) {
    vec![device.to_string()]
  } else {
    declared.to_vec()
  }
}

fn choose_backends(app: &mut App, declared: &[String]) -> CliResult<Option<Vec<String>>> {
  if declared.is_empty() {
    app.warn(Key::SetupNoBackends);
    return Ok(None);
  }

  let default = default_backends(app, declared);
  println!();
  println!("{}", app.args(Key::SetupBackendDevice, &[app.backend().as_str()]));
  println!("{}", app.args(Key::SetupBackendsDeclared, &[&declared.join(", ")]));

  if declared.len() == 1 {
    return Ok(Some(default));
  }

  loop {
    let default_text = default.join(",");
    let Some(line) = ask_line(app, Key::SetupBackendsPrompt, &[&default_text])? else {
      return Ok(None);
    };
    match parse_backends(line.trim(), declared, &default) {
      Ok(backends) => return Ok(Some(backends)),
      Err(unknown) => {
        app.warn_args(Key::SetupUnknownBackend, &[&unknown, &declared.join(", ")]);
      }
    }
  }
}

/// Turns `"opkg, pica"` into the subset of `declared` the user named.
///
/// Empty input is the default. Names are matched exactly and a name the source
/// does not publish is an error rather than something to drop silently: a
/// dropped backend would sync fewer indexes than the user asked for.
fn parse_backends(
  input: &str,
  declared: &[String],
  default: &[String],
) -> Result<Vec<String>, String> {
  if input.trim().is_empty() {
    return Ok(default.to_vec());
  }

  let mut backends: Vec<String> = Vec::new();
  for token in input.split([',', ' ', '\t']).map(str::trim).filter(|token| !token.is_empty()) {
    if !declared.iter().any(|name| name == token) {
      return Err(token.to_string());
    }
    if !backends.iter().any(|name| name == token) {
      backends.push(token.to_string());
    }
  }

  if backends.is_empty() {
    return Err(input.to_string());
  }
  Ok(backends)
}

/// A name that is safe as a cache file name and unique among `repos`.
///
/// The name becomes `cache/repos/<name>.json`, so it must not be able to walk
/// out of that directory; and two sources sharing a name would overwrite each
/// other's cached index, so the clash is resolved here, not at sync time.
fn unique_name(repos: &[Value], requested: &str, skip: Option<usize>) -> String {
  let mut base: String = requested
    .trim()
    .chars()
    .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') { ch } else { '-' })
    .collect();

  while base.starts_with('.') {
    base.remove(0);
  }
  if base.is_empty() {
    base = CUSTOM_NAME.to_string();
  }

  let taken = |candidate: &str| {
    repos.iter().enumerate().any(|(index, repo)| {
      Some(index) != skip && repo.get("name").and_then(Value::as_str) == Some(candidate)
    })
  };

  if !taken(&base) {
    return base;
  }

  for suffix in 2.. {
    let candidate = format!("{base}-{suffix}");
    if !taken(&candidate) {
      return candidate;
    }
  }
  unreachable!("the suffix counter always finds a free name")
}

fn ask_name(
  app: &App,
  default: &str,
  repos: &[Value],
  skip: Option<usize>,
) -> CliResult<Option<String>> {
  let Some(line) = ask_line(app, Key::SetupSourceName, &[default])? else {
    return Ok(None);
  };
  let requested = if line.trim().is_empty() { default } else { line.trim() };
  Ok(Some(unique_name(repos, requested, skip)))
}

fn source_json(name: &str, prepared: &Prepared) -> Value {
  let mut entry = json!({
    "name": name,
    "url": prepared.root,
    "backends": prepared.backends,
  });
  if prepared.allow_insecure_http {
    entry["allow_insecure_http"] = json!(true);
  }
  entry
}

/// Writes the configuration, syncs it, and puts the old files back if the sync
/// says no.
fn commit(app: &mut App, conf_path: &Path, conf: &Value) -> CliResult<()> {
  let conf_before = fs::read_to_string(conf_path).ok();
  let index_before = fs::read_to_string(&app.paths.index_file).ok();

  write_json_atomic_pretty(conf_path, conf)
    .map_err(|err| CliError::new(err.code, app.args(Key::SetupWriteFailed, &[&err.message])))?;

  if let Err(err) = sync::repos(app) {
    let mut detail = err.message.clone();
    if let Err(restore) = restore(conf_path, conf_before.as_deref())
      .and_then(|()| restore(&app.paths.index_file, index_before.as_deref()))
    {
      detail = app.args(Key::SetupRestoreFailed, &[&detail, &restore]);
    }
    return Err(CliError::new(err.code, app.args(Key::SetupSyncFailed, &[&detail])));
  }

  app.info_args(Key::SetupSaved, &[&conf_path.display().to_string()]);
  let summary = conf
    .get("repos")
    .and_then(Value::as_array)
    .map(|repos| {
      repos
        .iter()
        .map(|repo| {
          let name = repo.get("name").and_then(Value::as_str).unwrap_or("");
          format!("{name} ({})", backends_of(repo).join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
    })
    .unwrap_or_default();
  app.info_args(Key::SetupComplete, &[app.lang.code(), &summary]);
  app.info(Key::SetupNextSteps);
  Ok(())
}

/// Puts a file back exactly as it was, including "it did not exist".
fn restore(path: &Path, previous: Option<&str>) -> Result<(), String> {
  match previous {
    Some(text) => fs::write(path, text).map_err(|err| format!("{}: {err}", path.display())),
    None => match fs::remove_file(path) {
      Ok(()) => Ok(()),
      Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(err) => Err(format!("{}: {err}", path.display())),
    },
  }
}

// ---- the menu --------------------------------------------------------------

/// Asks for a 1-based number in `1..=count`, again on anything else.
///
/// The count is also offered to the prompt as `{0}`, so the upper bound a menu
/// shows cannot drift from the one it enforces.
///
/// `None` is EOF: the caller cancels, because a prompt that is no longer being
/// answered must not be filled in with a guess.
fn ask_number(app: &App, prompt: Key, count: usize) -> CliResult<Option<usize>> {
  let count_text = count.to_string();
  loop {
    let Some(line) = ask_line(app, prompt, &[&count_text])? else {
      return Ok(None);
    };
    if let Ok(choice) = line.trim().parse::<usize>() {
      if (1..=count).contains(&choice) {
        return Ok(Some(choice));
      }
    }
    println!("{}", app.args(Key::SetupInvalidChoice, &["1", &count_text]));
  }
}

/// Asks a yes/no question. `None` is EOF, never a silent "no".
fn ask_yes(app: &App, prompt: Key) -> CliResult<Option<bool>> {
  ask_yes_args(app, prompt, &[])
}

fn ask_yes_args(app: &App, prompt: Key, args: &[&str]) -> CliResult<Option<bool>> {
  loop {
    let Some(line) = ask_line(app, prompt, args)? else {
      return Ok(None);
    };
    match line.trim().to_ascii_lowercase().as_str() {
      "" | "n" | "no" => return Ok(Some(false)),
      "y" | "yes" => return Ok(Some(true)),
      _ => println!("{}", app.args(Key::SetupInvalidChoice, &["1", "2"])),
    }
  }
}

/// Prints `prompt` and reads one line. `None` on EOF.
fn ask_line(app: &App, prompt: Key, args: &[&str]) -> CliResult<Option<String>> {
  let text = app.args(prompt, args);
  print!("{text}");
  io::stdout().flush().map_err(|err| CliError::new(E_IO, err.to_string()))?;

  let mut line = String::new();
  let read = io::stdin().lock().read_line(&mut line);
  match read {
    Ok(0) => Ok(None),
    Ok(_) => Ok(Some(line)),
    Err(err) => Err(CliError::new(E_IO, err.to_string())),
  }
}

fn stdin_is_interactive() -> bool {
  if env::var(ASSUME_TTY_ENV)
    .is_ok_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
  {
    return true;
  }
  io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
  use super::{OFFICIAL_REPO_ROOT, parse_backends, unique_name};
  use serde_json::json;

  fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
  }

  #[test]
  fn backend_names_are_matched_against_what_the_source_declares() {
    let declared = names(&["opkg", "pica"]);
    let default = names(&["pica"]);

    assert_eq!(parse_backends("", &declared, &default).unwrap(), default);
    assert_eq!(
      parse_backends(" opkg , pica ", &declared, &default).unwrap(),
      names(&["opkg", "pica"])
    );
    // Repeated names collapse instead of syncing the same index twice.
    assert_eq!(parse_backends("pica,pica", &declared, &default).unwrap(), names(&["pica"]));
    // A backend the source does not publish is an error, not a silent drop.
    assert_eq!(parse_backends("apk", &declared, &default), Err("apk".to_string()));
    assert_eq!(parse_backends("pkg", &declared, &default), Err("pkg".to_string()));
  }

  #[test]
  fn a_source_name_cannot_leave_the_cache_directory_or_collide() {
    let repos = vec![json!({ "name": "main" }), json!({ "name": "main-2" })];

    assert_eq!(unique_name(&repos, "custom", None), "custom");
    // Separators and leading dots cannot turn a name into a path.
    let escaped = unique_name(&repos, "../../etc/passwd", None);
    assert!(!escaped.contains('/'), "{escaped}");
    assert!(!escaped.starts_with('.'), "{escaped}");
    assert_eq!(unique_name(&repos, "main", None), "main-3");
    // Editing a source keeps its own name.
    assert_eq!(unique_name(&repos, "main", Some(0)), "main");
    assert_eq!(unique_name(&repos, "  ", None), "main-3");
  }

  #[test]
  fn this_build_does_not_claim_an_official_address_it_does_not_have() {
    // Filling this in is a deliberate act with a real index behind it.
    match OFFICIAL_REPO_ROOT {
      None => {}
      Some(root) => assert!(root.starts_with("https://"), "{root}"),
    }
  }
}
