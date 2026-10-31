//! `pica setup`: language, source, backends - then one commit.
//!
//! The wizard asks the language and the source, reads the source's `index.json`
//! to learn which backends it publishes, keeps only the ones this host can
//! really use, and only then writes anything. The configuration, the verified
//! index and the per-source caches all go in through one [`Transaction`], so a
//! source that fails half way through leaves the previous state as it was.

use crate::app::{
  App, CliError, CliResult, E_ARG_INVALID, E_CONFIG_INVALID, E_NOT_INTERACTIVE, E_REPO_INVALID,
  JsonMode,
};
use crate::commands::sync;
use crate::i18n::{Key, Lang};
use crate::menu;
use crate::pkgmgr;
use crate::source::{self, FetchPolicy};
use crate::state::{Transaction, read_json_file};
use serde_json::{Map, Value, json};
use std::env;
use std::io::{self, IsTerminal};
use std::path::Path;

/// Set when a test or a wrapper wants the wizard's prompts with a pipe on
/// stdin. It never turns on raw mode: that needs a real terminal.
pub(crate) const ASSUME_TTY_ENV: &str = "PICA_ASSUME_TTY";

/// The official source this build ships with, as a source root (the address
/// `index.json` is joined onto, not the file itself).
///
/// There is no confirmed official `index.json` address in this repository, and
/// inventing one would make `pica setup` write a source that does not exist, so
/// this is `None`: the wizard says the address is missing instead of offering a
/// fake default. Fill it in once a real source is published.
pub(crate) const OFFICIAL_REPO_ROOT: Option<&str> = None;

/// The name an entry pointing at the official source gets.
const OFFICIAL_NAME: &str = "official";

/// The name an entry pointing at a typed address gets.
///
/// The wizard asks about one source, so a second custom source is something to
/// edit into the configuration rather than a dialogue to go through.
const CUSTOM_NAME: &str = "main";

/// What the source menu can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
  Official,
  Custom,
  Keep,
}

/// A source that was read successfully and is ready to be written down.
struct Source {
  root: String,
  backends: Vec<String>,
  allow_insecure_http: bool,
}

/// The outcome of reading one source root.
enum Resolved {
  Accepted(Source),
  /// The user refused something the source needs, so this root is dropped.
  Declined,
}

/// # Errors
/// Returns an error when the terminal cannot run the wizard, when the
/// configuration cannot be read, or when the chosen source cannot be fetched,
/// parsed or matched against this host's backends. Nothing is written before
/// every source has been read and validated.
pub fn run(app: &mut App) -> CliResult<()> {
  if app.options.json_mode != JsonMode::None {
    return Err(CliError::new(E_ARG_INVALID, app.text(Key::SetupNotJson)));
  }
  if !stdin_is_interactive() {
    return Err(CliError::new(E_NOT_INTERACTIVE, app.text(Key::SetupNeedsTerminal)));
  }

  let conf_path = app.paths.conf_file.clone();
  let mut conf = read_conf(app, &conf_path)?;

  println!();
  println!("{}", app.text(Key::SetupTitle));

  let Some(lang) = choose_language(app) else {
    cancelled(app);
    return Ok(());
  };
  app.lang = lang;
  lang.make_current();
  conf["language"] = json!(lang.code());

  let Some(repos) = choose_source(app, &conf)? else {
    cancelled(app);
    return Ok(());
  };
  conf["repos"] = Value::Array(repos);

  save(app, &conf_path, &conf)
}

pub(crate) fn stdin_is_interactive() -> bool {
  io::stdin().is_terminal() || env::var_os(ASSUME_TTY_ENV).is_some()
}

fn cancelled(app: &mut App) {
  app.warn(Key::SetupCancelled);
}

/// The configuration as it is, or an empty one when there is no file yet.
///
/// A file that exists but does not parse stops the wizard: overwriting it would
/// throw away whatever the user meant to keep, and guessing its contents would
/// write a configuration nobody asked for.
fn read_conf(app: &App, path: &Path) -> CliResult<Value> {
  if !path.exists() {
    return Ok(json!({}));
  }

  let conf = read_json_file(path).map_err(|err| {
    let shown = path.display().to_string();
    CliError::new(E_CONFIG_INVALID, app.args(Key::SetupConfUnreadable, &[&shown, &err.message]))
  })?;
  if !conf.is_object() {
    let shown = path.display().to_string();
    let cause = app.text(Key::SetupNotAnObject);
    return Err(CliError::new(
      E_CONFIG_INVALID,
      app.args(Key::SetupConfUnreadable, &[&shown, cause]),
    ));
  }
  Ok(conf)
}

/// `--lang` answers this question; otherwise Enter keeps English.
fn choose_language(app: &App) -> Option<Lang> {
  if let Some(lang) = app.options.lang {
    return Some(lang);
  }

  let options: Vec<String> = Lang::ALL.iter().map(|lang| language_label(*lang)).collect();
  let default = Lang::ALL.iter().position(|lang| *lang == Lang::EnUs).unwrap_or(0);
  let prompt = app.text(Key::SetupLanguagePrompt);
  menu::select(app, prompt, &options, default).map(|index| Lang::ALL[index])
}

/// A language is named in itself and never translated: someone looking for
/// English must find it whatever the current language is.
fn language_label(lang: Lang) -> String {
  format!("{} ({})", lang.native_name(), lang.code())
}

/// The `repos[]` the run ends up with, or `None` when the wizard was cancelled.
fn choose_source(app: &mut App, conf: &Value) -> CliResult<Option<Vec<Value>>> {
  let existing: Vec<Value> =
    conf.get("repos").and_then(Value::as_array).cloned().unwrap_or_default();

  if !existing.is_empty() {
    println!();
    println!("{}", app.text(Key::SetupSources));
    for (index, repo) in existing.iter().enumerate() {
      let number = (index + 1).to_string();
      let name = field(repo, "name");
      let url = field(repo, "url");
      let backends = repo
        .get("backends")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
      println!("{}", app.args(Key::SetupSourceLine, &[&number, &name, &url, &backends]));
    }
  }

  if OFFICIAL_REPO_ROOT.is_none() {
    println!();
    println!("{}", app.text(Key::SetupOfficialMissing));
  }

  let mut items: Vec<(Choice, String)> = Vec::new();
  if let Some(root) = OFFICIAL_REPO_ROOT {
    items.push((Choice::Official, app.args(Key::SetupSourceOfficial, &[root])));
  }
  items.push((Choice::Custom, app.text(Key::SetupSourceCustom).to_string()));
  if !existing.is_empty() {
    items.push((Choice::Keep, app.text(Key::SetupSourceKeep).to_string()));
  }

  // The default is the source a bare Enter should pick: the official one when
  // this build has it, otherwise the sources already configured, otherwise
  // typing an address.
  let default =
    if OFFICIAL_REPO_ROOT.is_some() || existing.is_empty() { 0 } else { items.len() - 1 };

  let labels: Vec<String> = items.iter().map(|(_, label)| label.clone()).collect();
  let prompt = app.args(Key::SetupSourcePrompt, &[&labels[default]]);
  let Some(index) = menu::select(app, &prompt, &labels, default) else {
    return Ok(None);
  };
  println!();

  match items[index].0 {
    Choice::Keep => Ok(Some(existing)),
    Choice::Official => {
      let root = OFFICIAL_REPO_ROOT.unwrap_or_default();
      match resolve(app, OFFICIAL_NAME, root)? {
        Resolved::Accepted(source) => Ok(Some(with_source(&existing, OFFICIAL_NAME, &source))),
        Resolved::Declined => Ok(None),
      }
    }
    Choice::Custom => ask_custom(app, &existing),
  }
}

/// Asks for an address until one works, or input runs out.
///
/// The failure is shown and the question repeated: a source that cannot be read
/// is never written down, and a loop that only ends at EOF cannot spin forever
/// on a closed stdin.
fn ask_custom(app: &mut App, existing: &[Value]) -> CliResult<Option<Vec<Value>>> {
  loop {
    let Some(text) = menu::line(app.text(Key::SetupSourceUrl)) else {
      return Ok(None);
    };
    let root = text.trim();
    if root.is_empty() {
      app.warn(Key::SetupUrlEmpty);
      continue;
    }

    match resolve(app, CUSTOM_NAME, root) {
      Ok(Resolved::Accepted(source)) => {
        return Ok(Some(with_source(existing, CUSTOM_NAME, &source)));
      }
      Ok(Resolved::Declined) => {}
      Err(err) => app.log_warn(err.message),
    }
  }
}

/// Reads one root: policy, entry descriptor, and the backends to use.
fn resolve(app: &mut App, name: &str, root: &str) -> CliResult<Resolved> {
  let root = tidy(root);
  let Some(allow_insecure_http) = ask_plain_http(app, &root) else {
    return Ok(Resolved::Declined);
  };

  let policy = FetchPolicy::source(allow_insecure_http);
  policy.check(&root).map_err(|err| {
    CliError::new(E_CONFIG_INVALID, app.args(Key::SyncRootRejected, &[name, &root, &err.message]))
  })?;

  let declared = sync::declared_backends(app, name, &root, policy)?;
  let backends = shared_backends(app, name, &declared)?;
  app.info_args(Key::SetupBackends, &[name, &backends.join(", ")]);

  Ok(Resolved::Accepted(Source { root, backends, allow_insecure_http }))
}

/// Asks before touching a source over plain HTTP outside the local network.
///
/// `None` means the user refused, and the root is dropped rather than fetched
/// against a policy they just said no to.
fn ask_plain_http(app: &App, root: &str) -> Option<bool> {
  if !source::needs_insecure_http_opt_in(root) {
    return Some(false);
  }

  let prompt = app.args(Key::SetupAllowHttp, &[root]);
  let options = vec![app.text(Key::SetupYes).to_string(), app.text(Key::SetupNo).to_string()];
  // Enter answers "no": a security decision is never the default.
  match menu::select(app, &prompt, &options, 1) {
    Some(0) => Some(true),
    _ => None,
  }
}

/// The backends this host supports **and** the source publishes, in host order.
///
/// Neither side is guessed: the host side comes from the binaries that are
/// actually on `PATH`, the source side from its `index.json`. An empty result is
/// an error, never "use everything the source declares" - a configuration
/// naming a backend this machine cannot run would break every later command.
fn shared_backends(app: &App, name: &str, declared: &[String]) -> CliResult<Vec<String>> {
  let host = pkgmgr::available_kinds();
  let shared: Vec<String> = host
    .iter()
    .filter(|kind| declared.iter().any(|other| other == kind.as_str()))
    .map(|kind| kind.as_str().to_string())
    .collect();

  if shared.is_empty() {
    let host = host.iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join(", ");
    return Err(CliError::new(
      E_REPO_INVALID,
      app.args(Key::SetupNoCommonBackend, &[name, &host, &declared.join(", ")]),
    ));
  }
  Ok(shared)
}

/// `repos[]` with `name` replaced or added, keeping every other entry and the
/// fields the old entry carried.
fn with_source(existing: &[Value], name: &str, source: &Source) -> Vec<Value> {
  let mut fields: Map<String, Value> = existing
    .iter()
    .find(|repo| repo.get("name").and_then(Value::as_str) == Some(name))
    .and_then(Value::as_object)
    .cloned()
    .unwrap_or_default();

  fields.insert("name".to_string(), json!(name));
  fields.insert("url".to_string(), json!(source.root));
  fields.insert("backends".to_string(), json!(source.backends));
  if source.allow_insecure_http {
    fields.insert("allow_insecure_http".to_string(), json!(true));
  } else {
    fields.remove("allow_insecure_http");
  }
  let entry = Value::Object(fields);

  let mut repos = existing.to_vec();
  match repos.iter().position(|repo| repo.get("name").and_then(Value::as_str) == Some(name)) {
    Some(index) => repos[index] = entry,
    None => repos.push(entry),
  }
  repos
}

/// Fetches, validates and then commits everything in one transaction.
///
/// The sync is built before anything is staged, so a source that fails to fetch
/// or fails its checks stops the run while the configuration, the verified index
/// and the per-source caches are still exactly what they were. The commit then
/// writes them together, and a write that fails puts the previous bytes back.
fn save(app: &mut App, conf_path: &Path, conf: &Value) -> CliResult<()> {
  let repos: Vec<Value> = conf.get("repos").and_then(Value::as_array).cloned().unwrap_or_default();

  app.info(Key::SyncStarted);
  let plan = sync::build(app, &repos)?;

  let mut transaction = Transaction::new();
  transaction.stage_json(app, conf_path, conf)?;
  plan.stage(app, &mut transaction)?;
  transaction.commit(app)?;

  let shown = conf_path.display().to_string();
  let code = app.lang.code();
  let summary = repos.iter().map(describe).collect::<Vec<_>>().join("; ");
  app.info_args(Key::SetupSaved, &[&shown]);
  app.info_args(Key::SetupComplete, &[code, &summary]);
  app.info(Key::SetupNextSteps);
  Ok(())
}

/// `name (backend, backend)` - what `SetupComplete` counts as a source.
fn describe(repo: &Value) -> String {
  let name = field(repo, "name");
  let backends = repo
    .get("backends")
    .and_then(Value::as_array)
    .map(|list| list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
    .unwrap_or_default();
  format!("{name} ({backends})")
}

fn field(repo: &Value, name: &str) -> String {
  repo.get(name).and_then(Value::as_str).unwrap_or_default().trim().to_string()
}

/// The root as the sync will store it: no trailing slash, unless trimming would
/// leave nothing that could still be an address (`/`, `file:///`).
fn tidy(root: &str) -> String {
  let trimmed = root.trim();
  let cut = trimmed.trim_end_matches('/');
  if cut.is_empty() || cut.ends_with(':') { trimmed.to_string() } else { cut.to_string() }
}

#[cfg(test)]
mod tests {
  use super::{describe, tidy};
  use serde_json::json;

  #[test]
  fn a_trailing_slash_is_dropped_but_a_bare_scheme_is_not() {
    assert_eq!(tidy("https://example.com/pica/"), "https://example.com/pica");
    assert_eq!(tidy("  file:///srv/pica/  "), "file:///srv/pica");
    assert_eq!(tidy("file:///"), "file:///", "the filesystem root keeps its slashes");
    assert_eq!(tidy("/srv/pica/"), "/srv/pica", "a bare path is not a URL to trim around");
  }

  #[test]
  fn a_source_is_described_by_its_name_and_backends() {
    let repo =
      json!({ "name": "main", "url": "https://example.com", "backends": ["opkg", "pica"] });
    assert_eq!(describe(&repo), "main (opkg, pica)");
  }
}
