//! `pica setup` and the language it writes.
//!
//! The wizard reads answers from stdin, so every test here feeds a script and
//! checks what reached the disk. Sources are `file://` directories, which means
//! no network, no package manager, and no dependency on the host.

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

  fn source_dir(&self) -> PathBuf {
    let root = self.dir.path().join("source");
    fs::create_dir_all(&root).expect("create source dir");
    root
  }

  fn source_url(&self) -> String {
    format!("file://{}", self.source_dir().display())
  }

  /// A source that serves an `index.json` declaring `backends`.
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
    let raw = fs::read_to_string(self.dir.path().join("state/index.json")).expect("read index");
    serde_json::from_str(&raw).expect("parse index")
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

  /// The files a run that wrote nothing of its own still leaves behind: every
  /// command creates the empty `pica.json` skeleton before it does anything.
  fn assert_skeleton_only(&self) {
    let conf = self.conf();
    assert_eq!(conf["repos"], json!([]), "{conf}");
    assert!(conf.get("language").is_none(), "{conf}");
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

  sandbox.interactive(&["setup"], "").success().stdout(contains("Setup cancelled"));

  // The only file that appears is the empty skeleton every command creates;
  // the wizard itself contributed neither a language nor a source.
  let conf = sandbox.conf();
  assert_eq!(conf["repos"], json!([]));
  assert!(conf.get("language").is_none(), "{conf}");
}

#[test]
fn an_invalid_choice_is_asked_again_instead_of_guessed() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  // "9" and "abc" are not languages; the third answer is English, then the
  // custom source, then its URL, then "no, that is all".
  let answers = format!("9\nabc\n2\n2\n{}\n\nn\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    .stdout(contains("Invalid choice: pick a number from 1 to 2"));
  assert_eq!(sandbox.conf()["language"], json!("en-US"));
}

#[test]
fn the_answers_become_a_configuration_and_a_synced_index() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  let answers = format!("2\n2\n{}\n\nn\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success();

  let conf = sandbox.conf();
  assert_eq!(conf["language"], json!("en-US"));
  assert_eq!(conf["repos"][0]["name"], json!("main"));
  assert_eq!(conf["repos"][0]["url"], json!(sandbox.source_url()));
  assert_eq!(conf["repos"][0]["backends"], json!(["pica"]));

  // The source is not merely stored: it was fetched, and `-S` ran through the
  // same code path every later command uses.
  let index = sandbox.index();
  assert!(index["repos"]["main"].is_object(), "{index}");
}

#[test]
fn choosing_chinese_speaks_chinese_from_the_next_question_on() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  let answers = format!("1\n2\n{}\n\nn\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    // The source menu is printed after the language was chosen.
    .stdout(contains("请选择"))
    .stdout(contains("初始化完成"));

  assert_eq!(sandbox.conf()["language"], json!("zh-CN"));
}

#[test]
fn a_device_backend_the_source_publishes_is_the_default_choice() {
  let sandbox = Sandbox::new();
  // Both are published; this host has no opkg/apk, so its backend is pica and
  // that is what the default answer selects.
  sandbox.publish_source(&["opkg", "pica"]);

  let answers = format!("2\n2\n{}\n\n\nn\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success();

  assert_eq!(sandbox.conf()["repos"][0]["backends"], json!(["pica"]));
}

#[test]
fn every_published_backend_can_be_enabled_by_name() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["opkg", "pica"]);

  let answers = format!("2\n2\n{}\nopkg,pica\n\nn\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success();

  assert_eq!(sandbox.conf()["repos"][0]["backends"], json!(["opkg", "pica"]));
}

#[test]
fn a_backend_the_source_does_not_publish_is_refused_by_name() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["opkg", "pica"]);

  // `apk` is a backend Pica knows but this source does not serve; it is
  // refused by name, then the default is accepted.
  let answers = format!("2\n2\n{}\napk\n\n\nn\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    .stderr(contains("apk is not declared by this source"));
  assert_eq!(sandbox.conf()["repos"][0]["backends"], json!(["pica"]));
}

#[test]
fn a_source_that_cannot_be_read_is_never_saved() {
  let sandbox = Sandbox::new();

  // The second round of the source menu gets EOF, which cancels the setup.
  let answers = format!("2\n2\nfile://{}/missing\n", sandbox.dir.path().display());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    .stderr(contains("Cannot read"))
    .stdout(contains("Setup cancelled"));

  sandbox.assert_skeleton_only();
  assert_eq!(sandbox.index()["repos"], json!({}), "an unverified source must not be synced");
}

#[test]
fn an_entry_descriptor_with_an_unknown_backend_key_is_refused() {
  let sandbox = Sandbox::new();
  fs::write(
    sandbox.source_dir().join("index.json"),
    json!({ "schema": 1, "managers": { "rpm": { "path": "rpm/index.json" } } }).to_string(),
  )
  .expect("write descriptor");

  let answers = format!("2\n2\n{}\nn\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    .stderr(contains("not a valid source descriptor"));
  sandbox.assert_skeleton_only();
}

#[test]
fn existing_sources_can_be_kept_untouched() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  let repo = json!({
    "name": "mine",
    "url": sandbox.source_url(),
    "backends": ["pica"],
  });
  sandbox.write_conf(&json!({ "language": "zh-CN", "repos": [repo] }));

  // English, then "keep what is there", then "no more sources".
  sandbox.interactive(&["setup"], "2\n3\nn\n").success().stdout(contains("mine"));

  let conf = sandbox.conf();
  assert_eq!(conf["language"], json!("en-US"));
  assert_eq!(conf["repos"][0]["name"], json!("mine"));
  assert_eq!(conf["repos"][0]["backends"], json!(["pica"]));
}

#[test]
fn a_second_source_is_added_beside_the_first() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  sandbox.write_conf(&json!({ "repos": [{
    "name": "mine",
    "url": sandbox.source_url(),
    "backends": ["pica"],
  }] }));

  // English, "add", custom, URL, default name, default backends, then stop.
  let answers = format!("2\n1\n2\n{}\n\n\nn\n", sandbox.source_url());
  sandbox.interactive(&["setup"], &answers).success();

  let conf = sandbox.conf();
  let repos = conf["repos"].as_array().expect("repos");
  assert_eq!(repos.len(), 2);
  assert_eq!(repos[0]["name"], json!("mine"));
  // The default name is taken, so the second source gets a free one.
  assert_eq!(repos[1]["name"], json!("main"));
}

#[test]
fn a_source_whose_descriptor_cannot_find_its_index_is_not_synced() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);
  // The descriptor names a file that is not there: setup must say so and let
  // the user try again rather than store a source sync would then fail on.
  fs::write(
    sandbox.source_dir().join("index.json"),
    json!({ "schema": 1, "managers": { "pica": { "path": "../escape.json" } } }).to_string(),
  )
  .expect("write descriptor");

  let answers = format!("2\n2\n{}\nn\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .success()
    .stderr(contains("not a valid source descriptor"));
  sandbox.assert_skeleton_only();
}

#[test]
fn a_sync_that_fails_after_saving_puts_the_previous_state_back() {
  let sandbox = Sandbox::new();
  sandbox.publish_source(&["pica"]);

  // A working configuration with its index already on disk, so "restored"
  // means "exactly what was there before" rather than "empty".
  let before = json!({
    "language": "en-US",
    "repos": [{ "name": "main", "url": sandbox.source_url(), "backends": ["pica"] }],
  });
  sandbox.write_conf(&before);
  sandbox.run(&["-S"]).success();
  let conf_before = fs::read_to_string(sandbox.conf_path()).expect("read conf");
  let index_before =
    fs::read_to_string(sandbox.dir.path().join("state/index.json")).expect("read index");

  // Descriptor still validates; the index it names is gone, so the failure only
  // shows up during the sync that follows the write.
  fs::remove_file(sandbox.source_dir().join("pica/index.json")).expect("remove backend index");

  let second = sandbox.dir.path().join("second");
  fs::create_dir_all(&second).expect("create second source");
  fs::write(
    second.join("index.json"),
    json!({ "schema": 1, "managers": { "pica": { "path": "pica/index.json" } } }).to_string(),
  )
  .expect("write descriptor");

  let answers = format!("2\n1\n2\nfile://{}\n\n\nn\n", second.display());
  sandbox
    .interactive(&["setup"], &answers)
    .failure()
    .stderr(contains("Sync failed, so the previous configuration was restored"));

  assert_eq!(
    fs::read_to_string(sandbox.conf_path()).expect("read conf"),
    conf_before,
    "the configuration on disk must be byte for byte what it was"
  );
  assert_eq!(
    fs::read_to_string(sandbox.dir.path().join("state/index.json")).expect("read index"),
    index_before,
    "a failed sync must not touch the verified index cache"
  );
}

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
fn setup_is_not_a_json_command() {
  let sandbox = Sandbox::new();

  sandbox.run(&["--json", "setup"]).failure().stderr(contains("--json"));
  assert!(!sandbox.conf_exists());
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

  let answers = format!("2\n2\n{}\n\nn\n", sandbox.source_url());
  sandbox
    .interactive(&["setup"], &answers)
    .failure()
    .stderr(contains("Cannot save the configuration"));
  assert!(!sandbox.conf_exists());
}

#[test]
fn an_unreadable_configuration_is_not_silently_replaced() {
  let sandbox = Sandbox::new();
  // A directory where the config file should be: reading it fails, and setup
  // must refuse rather than treat the file as absent and overwrite it later.
  fs::create_dir(sandbox.conf_path()).expect("create conf dir");

  sandbox.interactive(&["setup"], "2\n").failure().stderr(contains("Cannot read"));
}

#[test]
fn an_unsupported_language_argument_is_refused_by_code() {
  let sandbox = Sandbox::new();

  sandbox
    .run(&["--lang", "de-DE", "-Q"])
    .failure()
    .stderr(contains("invalid --lang").and(contains("de-DE")));
}
