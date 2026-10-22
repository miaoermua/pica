//! `pica setup`: the questions it asks, and what reaches the disk.
//!
//! Sources are `file://` directories, so no test here needs a network or any
//! package manager on the host: `PICA_ASSUME_TTY` makes a pipe stand in for a
//! terminal, which also means the numbered menu is what runs - the arrow-key
//! path needs a real tty and is covered from a pty at the bottom of the file.
//!
//! Every test that ends in a cancel or a failure asserts on the files as well as
//! on the message: the promise of this command is that a run which did not
//! finish writes nothing, and only the disk can prove it.

#![cfg(unix)]

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Sandbox {
  dir: tempfile::TempDir,
}

impl Sandbox {
  fn new() -> Self {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::create_dir_all(dir.path().join("state")).expect("create state dir");
    fs::create_dir_all(dir.path().join("etc")).expect("create etc dir");
    Self { dir }
  }

  fn etc(&self) -> PathBuf {
    self.dir.path().join("etc")
  }

  fn conf_path(&self) -> PathBuf {
    self.etc().join("pica.json")
  }

  fn index_path(&self) -> PathBuf {
    self.dir.path().join("state/index.json")
  }

  fn cache_dir(&self) -> PathBuf {
    self.dir.path().join("state/cache/repos")
  }

  fn source_dir(&self) -> PathBuf {
    let root = self.dir.path().join("source");
    fs::create_dir_all(&root).expect("create source dir");
    root
  }

  fn source_url(&self) -> String {
    format!("file://{}", self.source_dir().display())
  }

  /// A source that serves an `index.json` declaring `managers`, with a backend
  /// index for each of them.
  fn publish_source(&self, backends: &[&str]) {
    let managers: Value = backends
      .iter()
      .map(|mgr| ((*mgr).to_string(), json!({ "path": format!("{mgr}/index.json") })))
      .collect::<serde_json::Map<String, Value>>()
      .into();
    let descriptor =
      json!({ "schema": 1, "name": "sandbox", "description": "tests", "managers": managers });
    fs::write(self.source_dir().join("index.json"), descriptor.to_string())
      .expect("write descriptor");

    for mgr in backends {
      let dir = self.source_dir().join(mgr);
      fs::create_dir_all(&dir).expect("create backend dir");
      let index = json!({
        "schema": 1,
        "mgr": mgr,
        "generated_at": "2026-10-20T00:00:00Z",
        "packages": [],
      });
      fs::write(dir.join("index.json"), index.to_string()).expect("write backend index");
    }
  }

  fn write_conf(&self, conf: &Value) {
    fs::write(self.conf_path(), conf.to_string()).expect("write pica.json");
  }

  fn conf(&self) -> Value {
    let raw = fs::read_to_string(self.conf_path()).expect("read pica.json");
    serde_json::from_str(&raw).expect("parse pica.json")
  }

  fn index(&self) -> Value {
    let raw = fs::read_to_string(self.index_path()).expect("read index");
    serde_json::from_str(&raw).expect("parse index")
  }

  /// Every regular file under the state and etc directories, by path, with its
  /// bytes - the snapshot a failure must not change.
  fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
      let Ok(entries) = fs::read_dir(root) else { return };
      for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
          walk(&path, out);
        } else if let Ok(bytes) = fs::read(&path) {
          out.push((path.clone(), bytes));
        }
      }
    }

    let mut out = Vec::new();
    walk(&self.dir.path().join("state"), &mut out);
    walk(&self.etc(), &mut out);
    out.sort();
    out
  }

  /// A command with no `opkg`/`apk` on `PATH`, so the device backend is `pica`.
  fn command(&self) -> Command {
    let mut cmd = assert_cmd::cargo_bin_cmd!("pica");
    cmd.env("PICA_ETC_DIR", self.etc());
    cmd.env("PICA_STATE_DIR", self.dir.path().join("state"));
    cmd.env("PATH", self.empty_bin());
    cmd
  }

  fn empty_bin(&self) -> PathBuf {
    let path = self.dir.path().join("bin");
    fs::create_dir_all(&path).expect("create bin dir");
    path
  }

  /// Runs the CLI as if a person were typing: stdin is a pipe, and the pipe is
  /// what the wizard is told to trust.
  fn interactive(&self, args: &[&str], answers: &str) -> Assert {
    let mut cmd = self.command();
    cmd.env("PICA_ASSUME_TTY", "1");
    cmd.args(args);
    cmd.write_stdin(answers.to_string());
    cmd.assert()
  }

  fn run(&self, args: &[&str]) -> Assert {
    let mut cmd = self.command();
    cmd.args(args);
    cmd.assert()
  }

  /// Nothing at all: no configuration, no index, no cache.
  fn assert_nothing_written(&self) {
    assert!(!self.conf_path().exists(), "a run that did not finish wrote a configuration");
    assert!(!self.index_path().exists(), "a run that did not finish wrote an index");
    assert!(
      !self.cache_dir().join("main.json").exists(),
      "a run that did not finish wrote a source cache"
    );
  }

  fn conf_exists(&self) -> bool {
    Path::new(&self.conf_path()).exists()
  }
}

/// The first stderr line of a `--json-errors` run is the JSON document; the
/// line after it is the same failure written for a person.
fn json_line(assert: &Assert) -> Value {
  let output = assert.get_output();
  let text = String::from_utf8(output.stderr.clone()).expect("utf-8");
  let first = text.lines().next().expect("a JSON line");
  serde_json::from_str(first).expect("parse json")
}

// -- What the wizard asks -----------------------------------------------------

#[test]
fn nothing_is_written_before_the_first_answer() {
  let sandbox = Sandbox::new();

  // A pipe is not a terminal: the wizard refuses instead of blocking forever.
  sandbox.run(&["setup"]).failure().stderr(contains("terminal").and(contains("pica.json")));
  assert!(!sandbox.conf_exists(), "a refused run must not write anything");
}

#[test]
fn end_of_input_at_the_language_menu_cancels_without_writing() {
  let sandbox = Sandbox::new();

  sandbox.interactive(&["setup"], "").success().stderr(contains("Setup cancelled"));

  sandbox.assert_nothing_written();
}

#[test]
fn an_invalid_choice_is_asked_again_instead_of_guessed() {
  let sandbox = Sandbox::new();

  // Two answers that are not numbers, then end of input: the menu says what it
  // accepts, and a closed stdin ends the loop rather than spinning on it.
  sandbox
    .interactive(&["setup"], "9\nabc\n")
    .success()
    .stdout(contains("Invalid choice: pick a number from 1 to 2").count(2));

  sandbox.assert_nothing_written();
}

#[test]
fn the_official_source_is_reported_missing_rather_than_invented() {
  let sandbox = Sandbox::new();

  // This build ships no official address, so the wizard has to say so. A made
  // up default would look like a working source until the first sync.
  sandbox
    .interactive(&["setup"], "\n")
    .success()
    .stdout(contains("OFFICIAL_REPO_ROOT"))
    .stderr(contains("Setup cancelled"));

  sandbox.assert_nothing_written();
}

// -- The happy path -----------------------------------------------------------

#[test]
fn bare_enter_answers_english_and_the_default_source() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  // Enter for the language (English), Enter for the source (the only one there
  // is), the address, then the sync.
  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success();

  let conf = sandbox.conf();
  assert_eq!(conf["language"], json!("en-US"));
  assert_eq!(conf["repos"][0]["name"], json!("main"));
  assert_eq!(conf["repos"][0]["url"], json!(sandbox.source_url()));
  assert_eq!(conf["repos"][0]["backends"], json!(["pica"]));

  // The source is not merely stored: it was fetched, and `-S` ran through the
  // same code path every later command uses.
  assert!(sandbox.index()["repos"]["main"].is_object(), "{:?}", sandbox.index());
}

#[test]
fn choosing_chinese_speaks_chinese_from_the_next_question_on() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  let answers = format!("1\n\n{}\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    // The source menu is printed after the language was chosen.
    .stdout(contains("选择软件源"))
    .stdout(contains("初始化完成"));

  assert_eq!(sandbox.conf()["language"], json!("zh-CN"));
}

#[test]
fn arrow_keys_move_the_cursor() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  // The cursor starts on English; one Down moves it to the other language, and
  // the empty line after it is the Enter that takes the highlight. The next
  // empty line then answers the source menu.
  let answers = format!("\x1b[B\n\n\n{}\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success().stdout(contains("简体中文 (zh-CN)"));

  assert_eq!(sandbox.conf()["language"], json!("zh-CN"));
}

#[test]
fn an_explicit_language_skips_the_question() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  let answers = format!("\n{}\n", sandbox.source_url());
  sandbox
    .interactive(&["setup", "--lang", "zh-CN"], &answers)
    .success()
    .stdout(contains("选择软件源"));

  assert_eq!(sandbox.conf()["language"], json!("zh-CN"));
}

// -- Backends -----------------------------------------------------------------

#[test]
fn only_the_backends_this_host_and_the_source_share_are_used() {
  let sandbox = Sandbox::new();
  // The source publishes both; this host has no opkg, so only pica is usable.
  sandbox.publish_source(&["opkg", "pica"]);

  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    .stdout(contains("main: using backend(s) pica"))
    .stdout(contains("sources: main (pica)"));

  assert_eq!(sandbox.conf()["repos"][0]["backends"], json!(["pica"]));
}

#[test]
fn a_source_without_a_common_backend_is_refused_and_nothing_is_written() {
  let sandbox = Sandbox::new();
  // Neither is available here: the intersection is empty, and "empty" must not
  // turn into "every backend the source declares" or into a literal `auto`.
  sandbox.publish_source(&["opkg", "apk"]);

  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    .stderr(contains("no backend in common").and(contains("nothing was written")))
    .stderr(contains("Setup cancelled"));

  sandbox.assert_nothing_written();
}

#[test]
fn a_backend_the_source_does_not_publish_is_never_written() {
  let sandbox = Sandbox::new();
  // `opkg` runs here (a stub is on PATH), but this source does not serve it, so
  // the configuration must not name it.
  let bin = sandbox.empty_bin();
  fs::write(bin.join("opkg"), "#!/bin/sh\nexit 0\n").expect("write opkg stub");
  let mut perms = fs::metadata(bin.join("opkg")).expect("stat stub").permissions();
  perms.set_mode(0o755);
  fs::set_permissions(bin.join("opkg"), perms).expect("chmod stub");
  sandbox.publish_source(&["pica"]);

  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success();

  assert_eq!(sandbox.conf()["repos"][0]["backends"], json!(["pica"]));
}

// -- Sources already in the configuration -------------------------------------

#[test]
fn enter_keeps_the_configured_sources() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  sandbox.write_conf(&json!({
    "language": "zh-CN",
    "repos": [{ "name": "mine", "url": sandbox.source_url(), "backends": ["pica"] }],
  }));

  // English, then Enter: the default is "keep what is there".
  sandbox.interactive(&["setup"], "\n\n").success().stdout(contains("mine"));

  let conf = sandbox.conf();
  assert_eq!(conf["language"], json!("en-US"));
  assert_eq!(conf["repos"][0]["name"], json!("mine"));
  assert_eq!(conf["repos"][0]["backends"], json!(["pica"]));
}

#[test]
fn a_new_source_is_added_beside_the_existing_ones() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  sandbox.write_conf(&json!({ "repos": [{
    "name": "mine",
    "url": sandbox.source_url(),
    "backends": ["pica"],
  }] }));

  // English, "custom source" (the first item), then the address.
  let answers = format!("\n1\n{}\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success();

  let conf = sandbox.conf();
  let repos = conf["repos"].as_array().expect("repos");
  assert_eq!(repos.len(), 2);
  assert_eq!(repos[0]["name"], json!("mine"), "the entry that was there stays first");
  assert_eq!(repos[1]["name"], json!("main"));
}

#[test]
fn a_source_that_cannot_be_read_is_never_saved() {
  let sandbox = Sandbox::new();

  // The address itself is fine; the `index.json` it should serve is missing.
  // The question is repeated, and end of input cancels.
  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    .stderr(contains("index.json"))
    .stderr(contains("Setup cancelled"));

  sandbox.assert_nothing_written();
}

#[test]
fn an_entry_descriptor_with_an_unknown_backend_key_is_refused() {
  let sandbox = Sandbox::new();
  fs::write(
    sandbox.source_dir().join("index.json"),
    json!({ "schema": 1, "managers": { "rpm": { "path": "rpm/index.json" } } }).to_string(),
  )
  .expect("write descriptor");

  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success().stderr(contains("unsupported manager rpm"));

  sandbox.assert_nothing_written();
}

#[test]
fn a_source_whose_descriptor_points_outside_the_root_is_refused() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  fs::write(
    sandbox.source_dir().join("index.json"),
    json!({ "schema": 1, "managers": { "pica": { "path": "../escape.json" } } }).to_string(),
  )
  .expect("write descriptor");

  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success().stderr(contains("must not contain . or .."));

  sandbox.assert_nothing_written();
}

#[test]
fn plain_http_to_a_public_host_is_asked_about_and_never_assumed() {
  let sandbox = Sandbox::new();

  // Nothing is fetched: the answer is "no", which drops the address and asks
  // again; end of input then cancels.
  sandbox
    .interactive(&["setup"], "\n\nhttp://example.invalid/pica\n2\n")
    .success()
    .stdout(contains("plain HTTP"))
    .stderr(contains("Setup cancelled"));

  sandbox.assert_nothing_written();
}

// -- One transaction ----------------------------------------------------------

#[test]
fn a_source_that_fails_after_another_succeeded_changes_nothing() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  let working = sandbox.source_url();
  sandbox.write_conf(&json!({
    "language": "en-US",
    "repos": [{ "name": "main", "url": working, "backends": ["pica"] }],
  }));
  sandbox.run(&["-S"]).success();

  // Give the working source new content, so a successful second sync would have
  // to replace its cache: that is what makes "unchanged" meaningful.
  let new_index = json!({
    "schema": 1,
    "mgr": "pica",
    "generated_at": "2026-10-21T00:00:00Z",
    "packages": [{
      "pkgname": "late-demo",
      "appname": "late-demo",
      "pkgver": "1.0.0",
      "pkgrel": "1",
      "pkgmgr": "pica",
      "platform": "all",
      "arch": "all",
      "os": "all",
      "filename": "late-demo-1.0.0-1-all-pica.pkg.tar.gz",
      "sha256": "0".repeat(64),
    }],
  });
  fs::write(sandbox.source_dir().join("pica/index.json"), new_index.to_string())
    .expect("rewrite backend index");

  // A second source: its descriptor validates, but the index it names is gone,
  // so the failure only shows up once the first source was already read.
  let second = sandbox.dir.path().join("second");
  fs::create_dir_all(&second).expect("create second source");
  fs::write(
    second.join("index.json"),
    json!({ "schema": 1, "managers": { "pica": { "path": "pica/index.json" } } }).to_string(),
  )
  .expect("write descriptor");

  let before = sandbox.snapshot();
  let answers = format!("\n1\nfile://{}\n", second.display());
  sandbox
    .interactive(&["setup"], &answers)
    .failure()
    .stderr(contains("pica/index.json").and(contains("failed")));

  assert_eq!(
    sandbox.snapshot(),
    before,
    "a source that fails must leave the configuration and every cache as it was"
  );
}

#[test]
fn a_failed_first_run_leaves_no_configuration_index_or_cache() {
  let sandbox = Sandbox::new();
  // The descriptor is valid, so the wizard accepts the address; the sync then
  // fails on the index the descriptor names.
  fs::write(
    sandbox.source_dir().join("index.json"),
    json!({ "schema": 1, "managers": { "pica": { "path": "pica/index.json" } } }).to_string(),
  )
  .expect("write descriptor");

  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).failure().stderr(contains("failed"));

  sandbox.assert_nothing_written();
}

#[test]
fn a_successful_run_leaves_no_temporary_files() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success().stdout(contains("Setup complete"));

  let stray: Vec<PathBuf> = sandbox
    .snapshot()
    .into_iter()
    .map(|(path, _)| path)
    .filter(|path| {
      path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.contains(".pica-"))
    })
    .collect();
  assert!(stray.is_empty(), "temp files were left behind: {stray:?}");

  // Everything the run promised is there.
  assert!(sandbox.conf_exists());
  assert!(sandbox.index()["repos"]["main"].is_object());
  assert!(sandbox.cache_dir().join("main.json").is_file());
}

/// A failure on the *third* file still undoes the first two: the commit writes
/// the configuration, then the caches, then the index, and a cache directory
/// that cannot be written to makes the run fail after the configuration had
/// already been replaced.
#[test]
fn a_write_that_fails_midway_puts_the_previous_files_back() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  let url = sandbox.source_url();
  sandbox.write_conf(&json!({
    "language": "en-US",
    "repos": [{ "name": "main", "url": url, "backends": ["pica"] }],
  }));
  sandbox.run(&["-S"]).success();

  // New content in the source, so a successful run would have to overwrite it.
  let newer = json!({
    "schema": 1,
    "mgr": "pica",
    "generated_at": "2026-10-21T00:00:00Z",
    "packages": [{
      "pkgname": "late-demo",
      "appname": "late-demo",
      "pkgver": "1.0.0",
      "pkgrel": "1",
      "pkgmgr": "pica",
      "platform": "all",
      "arch": "all",
      "os": "all",
      "filename": "late-demo-1.0.0-1-all-pica.pkg.tar.gz",
      "sha256": "0".repeat(64),
    }],
  });
  fs::write(sandbox.source_dir().join("pica/index.json"), newer.to_string())
    .expect("rewrite backend index");

  let cache = sandbox.cache_dir();
  let mut perms = fs::metadata(&cache).expect("stat cache").permissions();
  perms.set_mode(0o555);
  fs::set_permissions(&cache, perms).expect("chmod cache");
  if fs::write(cache.join(".probe"), b"x").is_ok() {
    let _ = fs::remove_file(cache.join(".probe"));
    let _ = fs::set_permissions(&cache, fs::Permissions::from_mode(0o755));
    return;
  }

  let before = sandbox.snapshot();
  // Chinese this time: the configuration would change, so restoring it is
  // visible, not just "identical bytes written twice".
  let answers = format!("1\n\n{}\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .failure()
    // The run speaks Chinese by now, so the message it prints does too.
    .stderr(contains("已恢复"));

  let _ = fs::set_permissions(&cache, fs::Permissions::from_mode(0o755));
  assert_eq!(
    sandbox.snapshot(),
    before,
    "a failed commit must restore every file it had already written"
  );
}

#[test]
fn a_configuration_that_cannot_be_saved_is_reported_as_such() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  // A directory that cannot be written to is the simplest honest way to make
  // the save fail. Root ignores the mode, so there is nothing to test then.
  let etc = sandbox.etc();
  let mut perms = fs::metadata(&etc).expect("stat etc").permissions();
  perms.set_mode(0o555);
  fs::set_permissions(&etc, perms).expect("chmod etc");
  if fs::write(etc.join(".probe"), b"x").is_ok() {
    let _ = fs::remove_file(etc.join(".probe"));
    return;
  }

  let answers = format!("\n\n{}\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .failure()
    .stderr(contains("could not write").and(contains("pica.json")));

  // The configuration was the first file to be written, so the index and the
  // cache that follow it were never committed.
  assert!(!sandbox.conf_exists());
  assert!(!sandbox.index_path().exists());
  assert!(!sandbox.cache_dir().join("main.json").exists());
}

#[test]
fn an_unreadable_configuration_is_not_silently_replaced() {
  let sandbox = Sandbox::new();
  // A directory where the config file should be: reading it fails, and setup
  // must refuse rather than treat the file as absent and overwrite it later.
  fs::create_dir(sandbox.conf_path()).expect("create conf dir");

  sandbox.interactive(&["setup"], "\n").failure().stderr(contains("Cannot read"));
}

#[test]
fn setup_is_not_a_json_command() {
  let sandbox = Sandbox::new();

  sandbox.run(&["--json", "setup"]).failure().stderr(contains("--json"));
  assert!(!sandbox.conf_exists());
}

// -- Language plumbing that shares this binary --------------------------------

#[test]
fn sync_says_so_in_the_language_the_config_speaks() {
  let sandbox = Sandbox::new();

  sandbox.write_conf(&json!({ "language": "zh-CN", "repos": [] }));
  let before = fs::read_to_string(sandbox.conf_path()).expect("read conf");
  sandbox.run(&["-S"]).failure().stderr(contains("未在").and(contains("配置软件源")));

  // The run-time override changes this run only.
  sandbox.run(&["--lang", "en-US", "-S"]).failure().stderr(contains("no repos configured"));
  assert_eq!(
    fs::read_to_string(sandbox.conf_path()).expect("read conf"),
    before,
    "--lang must not rewrite the stored language"
  );
}

#[test]
fn an_unknown_stored_language_falls_back_to_english() {
  let sandbox = Sandbox::new();
  sandbox.write_conf(&json!({ "language": "de-DE", "repos": [] }));

  sandbox.run(&["-S"]).failure().stderr(contains("no repos configured"));
}

#[test]
fn the_machine_readable_field_list_does_not_depend_on_the_language() {
  let sandbox = Sandbox::new();
  sandbox.write_conf(&json!({
    "language": "en-US",
    "repos": [],
  }));
  let db = json!({
    "schema": 1,
    "installed": {
      "demo": {
        "pkgname": "demo",
        "appname": "demo",
        "version": "1.0.0-1",
        "pkgmgr": "pica",
        "files": [],
        "source": "pica",
      }
    }
  });
  fs::write(sandbox.dir.path().join("state/db.json"), db.to_string()).expect("write db");

  let english = sandbox.run(&["-Qi", "demo"]).success().get_output().stdout.clone();
  sandbox.write_conf(&json!({ "language": "zh-CN", "repos": [] }));
  let chinese = sandbox.run(&["-Qi", "demo"]).success().get_output().stdout.clone();

  assert_eq!(english, chinese, "`-Qi` labels are an interface, not prose");
  let text = String::from_utf8(english).expect("utf-8");
  assert!(text.contains("Name            : demo"), "{text}");
}

#[test]
fn the_error_code_is_the_same_in_every_language() {
  let sandbox = Sandbox::new();

  sandbox.write_conf(&json!({ "language": "en-US", "repos": [] }));
  let english = json_line(&sandbox.run(&["--json-errors", "-S"]).failure());
  sandbox.write_conf(&json!({ "language": "zh-CN", "repos": [] }));
  let chinese = json_line(&sandbox.run(&["--json-errors", "-S"]).failure());

  assert_eq!(english["code"], chinese["code"]);
  assert_eq!(english["code"], json!("E_CONFIG_INVALID"));
  // The message is for a person, so it does differ.
  assert_ne!(english["message"], chinese["message"]);
}

#[test]
fn an_unsupported_language_argument_is_refused_by_code() {
  let sandbox = Sandbox::new();

  sandbox
    .run(&["--lang", "de-DE", "-Q"])
    .failure()
    .stderr(contains("invalid --lang").and(contains("de-DE")));
}

// -- A real terminal ----------------------------------------------------------

/// The arrow keys only exist on a terminal: the menu switches the line off
/// canonical mode and reads single bytes, which a pipe cannot exercise. `script`
/// hands the child a pty, which is the only way to reach that code without a
/// new dependency.
///
/// The shell asks for its line settings before and after the wizard, so this
/// also checks the promise `Terminal` makes: whatever it changes, it puts back -
/// on the way out of a successful run as much as out of a cancelled one.
#[test]
fn arrow_keys_work_on_a_terminal_and_the_line_settings_come_back() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  // The menu needs `stty` on PATH; a symlink keeps opkg and apk out of it.
  let bin = sandbox.empty_bin();
  let stty = which("stty").expect("stty on PATH");
  std::os::unix::fs::symlink(stty, bin.join("stty")).expect("link stty");

  let inner = format!(
    "stty -g; env PICA_ETC_DIR='{etc}' PICA_STATE_DIR='{state}' PATH='{bin}' '{pica}' setup; stty -g",
    etc = sandbox.etc().display(),
    state = sandbox.dir.path().join("state").display(),
    bin = bin.display(),
    pica = env!("CARGO_BIN_EXE_pica"),
  );

  let Some(mut child) = Script::spawn(&inner) else {
    eprintln!("skipping: no `script` on this host");
    return;
  };

  // Typed at a person's pace: the terminal is in cooked mode until the wizard
  // switches it, so the bytes have to arrive while it is ready for them.
  for chunk in [&b"\x1b[B"[..], b"\r", b"\r", format!("{}\r", sandbox.source_url()).as_bytes()] {
    child.type_bytes(chunk);
  }

  let output = child.finish();
  let text = String::from_utf8_lossy(&output.stdout).into_owned();
  assert!(output.status.success(), "{text}");

  // The escape-coded redraw, and no numbered list: the raw path really ran.
  assert!(text.contains("\x1b[K"), "the menu was not drawn in place: {text}");
  assert!(!text.contains("  1. "), "the numbered fallback ran instead: {text}");

  let settings = line_settings(&text);
  assert_eq!(settings.len(), 2, "two `stty -g` readings: {text}");
  assert_eq!(settings[0], settings[1], "the line settings must come back: {text}");

  assert_eq!(sandbox.conf()["language"], json!("zh-CN"), "{text}");
}

/// A pty for the child, fed a chunk at a time.
struct Script {
  child: std::process::Child,
}

impl Script {
  /// `None` when `script` is not installed, so a host without it skips instead
  /// of failing.
  fn spawn(inner: &str) -> Option<Self> {
    let mut command = std::process::Command::new("script");
    // BSD `script` takes the file first, util-linux takes `-c`.
    #[cfg(target_os = "macos")]
    command.args(["-q", "/dev/null", "sh", "-c", inner]);
    #[cfg(not(target_os = "macos"))]
    // util-linux needs `-e` to hand the child's exit status back.
    command.args(["-q", "-e", "-c", inner, "/dev/null"]);

    let child = command
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::piped())
      .spawn()
      .ok()?;
    Some(Self { child })
  }

  fn type_bytes(&mut self, bytes: &[u8]) {
    use std::io::Write;
    std::thread::sleep(std::time::Duration::from_millis(400));
    let stdin = self.child.stdin.as_mut().expect("stdin");
    stdin.write_all(bytes).expect("write to the pty");
    stdin.flush().expect("flush");
  }

  fn finish(mut self) -> std::process::Output {
    // Give the last answer time to be read before the pipe closes: closing it
    // early turns the end of input into the wizard's next key press.
    std::thread::sleep(std::time::Duration::from_millis(600));
    drop(self.child.stdin.take());
    self.child.wait_with_output().expect("wait for script")
  }
}

/// The `stty -g` readings in `text`, in order, whichever format the platform
/// prints.
fn line_settings(text: &str) -> Vec<String> {
  text
    .lines()
    .map(str::trim)
    .filter(|line| {
      let bsd = line.starts_with("gfmt");
      let util_linux =
        line.contains(':') && line.chars().all(|c| c.is_ascii_hexdigit() || c == ':');
      bsd || util_linux
    })
    .map(str::to_string)
    .collect()
}

/// The absolute path of `name` on this process's `PATH`.
fn which(name: &str) -> Option<PathBuf> {
  let path = std::env::var_os("PATH")?;
  std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}
