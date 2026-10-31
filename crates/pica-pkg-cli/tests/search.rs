//! `-Ss`, `-Sl`, `-Qs`, `-Qo`, `-Qu`, the JSON form of the queries, and the
//! suggestion after a miss.
//!
//! The sync cache and the database are written directly: a search reads them
//! and nothing else, so no source or package manager is involved.

#![cfg(unix)]

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::{Value, json};
use std::fs;

struct Sandbox {
  dir: tempfile::TempDir,
}

impl Sandbox {
  fn new() -> Self {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::create_dir_all(dir.path().join("state")).expect("create state dir");
    fs::create_dir_all(dir.path().join("etc")).expect("create etc dir");
    fs::write(dir.path().join("state/db.json"), r#"{"installed": {}}"#).expect("write db");
    Self { dir }
  }

  fn write_db(&self, db: &Value) {
    fs::write(self.dir.path().join("state/db.json"), db.to_string()).expect("write db");
  }

  fn write_index(&self, packages: &[Value], updated_at: u64) {
    let index = json!({
      "schema": 1,
      "repos": {
        "main": {
          "name": "main",
          "url": "https://repo.invalid/main",
          "updated_at": updated_at,
          "backends": ["pica"],
          "indexes": { "pica": { "packages": packages } }
        }
      }
    });
    fs::write(self.dir.path().join("state/index.json"), index.to_string()).expect("write index");
  }

  fn run(&self, args: &[&str]) -> Assert {
    let mut cmd: Command = assert_cmd::cargo_bin_cmd!("pica");
    cmd.env("PICA_ETC_DIR", self.dir.path().join("etc"));
    cmd.env("PICA_STATE_DIR", self.dir.path().join("state"));
    // No system package manager: everything here is about the pica backend.
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.args(["--pkgmgr", "pica"]);
    cmd.args(args);
    cmd.assert()
  }
}

fn package(appname: &str, pkgname: &str, version: &str, desc: &str, os: &str) -> Value {
  json!({
    "pkgname": pkgname, "appname": appname, "pkgver": version, "pkgrel": "1",
    "pkgmgr": "pica", "platform": "all", "arch": "all", "os": os,
    "filename": format!("{pkgname}-{version}-1-all-pica.pkg.tar.gz"),
    "sha256": "0".repeat(64), "pkgdesc": desc
  })
}

fn now() -> u64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .expect("clock after the epoch")
    .as_secs()
}

fn stdout(assert: &Assert) -> String {
  String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

#[test]
fn a_search_finds_by_name_part_case_and_description() {
  let sandbox = Sandbox::new();
  sandbox.write_index(
    &[
      package("tailscale", "tailscale", "1.0.0", "Zero config VPN", "all"),
      package("tailscale", "tailscale", "1.2.0", "Zero config VPN", "all"),
      package("frpc", "frpc", "0.5.0", "内网穿透客户端", "all"),
    ],
    now(),
  );

  let out = stdout(&sandbox.run(&["-Ss", "TAIL"]).success());
  assert!(out.contains("main/tailscale 1.2.0-1 [pica]"), "{out}");
  assert!(!out.contains("1.0.0-1"), "only the newest version is listed: {out}");
  assert!(!out.contains("frpc"), "{out}");

  let out = stdout(&sandbox.run(&["-Ss", "穿透"]).success());
  assert!(out.contains("main/frpc 0.5.0-1"), "{out}");
}

#[test]
fn a_search_never_installs_and_a_typo_gets_a_suggestion() {
  let sandbox = Sandbox::new();
  sandbox.write_index(&[package("tailscale", "tailscale", "1.0.0", "", "all")], now());

  sandbox
    .run(&["--non-interactive", "--feed-policy", "packaged-only", "-S", "tailscle"])
    .failure()
    .stderr(contains("package not found").and(contains("did you mean: tailscale")));
  let db: Value = serde_json::from_str(
    &fs::read_to_string(sandbox.dir.path().join("state/db.json")).expect("read db"),
  )
  .expect("parse db");
  assert_eq!(db["installed"], json!({}), "a close name is suggested, never installed");
}

#[test]
fn a_package_not_installable_here_is_hidden_unless_asked_for() {
  let sandbox = Sandbox::new();
  sandbox.write_index(&[package("winonly", "winonly", "1.0.0", "", "windows")], now());

  let out = stdout(&sandbox.run(&["-Ss", "winonly"]).success());
  assert!(!out.contains("main/winonly"), "{out}");
  assert!(out.contains("1 result(s) not installable here are hidden"), "{out}");

  let out = stdout(&sandbox.run(&["-Ss", "winonly", "--all"]).success());
  assert!(out.contains("main/winonly 1.0.0-1 [pica] (not installable here: os)"), "{out}");
}

#[test]
fn json_output_is_one_document_with_installed_and_installable() {
  let sandbox = Sandbox::new();
  sandbox.write_index(&[package("demo", "demo@alice", "1.1.0", "a demo", "all")], now());
  sandbox.write_db(&json!({"installed": {"demo@alice": {"manifest": {
    "pkgname": "demo@alice", "appname": "demo", "pkgver": "1.0.0", "pkgrel": "1"
  }}}}));

  let out = stdout(&sandbox.run(&["--json", "-Ss", "demo"]).success());
  let lines: Vec<&str> = out.lines().collect();
  assert_eq!(lines.len(), 1, "exactly one JSON document: {out}");
  let doc: Value = serde_json::from_str(lines[0]).expect("valid JSON");
  let hit = &doc["packages"][0];
  assert_eq!(hit["pkgname"], json!("demo@alice"));
  assert_eq!(hit["installed_version"], json!("1.0.0-1"));
  assert_eq!(hit["installable"], json!(true));
  assert_eq!(hit["match"], json!("exact"));
}

#[test]
fn the_list_and_the_local_search_read_what_they_say() {
  let sandbox = Sandbox::new();
  sandbox.write_index(
    &[package("a", "a", "1.0.0", "", "all"), package("b", "b", "1.0.0", "", "all")],
    now(),
  );
  sandbox.write_db(&json!({"installed": {"local-tool": {"manifest": {
    "pkgname": "local-tool", "pkgver": "2.0", "pkgrel": "1", "pkgdesc": "a local tool"
  }}}}));

  let out = stdout(&sandbox.run(&["-Sl"]).success());
  assert!(out.contains("main/a 1.0.0-1") && out.contains("main/b 1.0.0-1"), "{out}");
  sandbox.run(&["-Sl", "nope"]).failure().stderr(contains("no synced source is named nope"));

  let out = stdout(&sandbox.run(&["-Qs", "tool"]).success());
  assert!(out.contains("local/local-tool 2.0-1"), "{out}");
}

#[test]
fn an_unsynced_host_is_told_to_sync_instead_of_finding_nothing() {
  let sandbox = Sandbox::new();
  sandbox.run(&["-Ss", "anything"]).failure().stderr(contains("run pica -S first"));
}

#[test]
fn an_old_index_is_mentioned() {
  let sandbox = Sandbox::new();
  sandbox.write_index(&[package("a", "a", "1.0.0", "", "all")], now() - 30 * 24 * 60 * 60);
  let out = stdout(&sandbox.run(&["-Ss", "a"]).success());
  assert!(out.contains("the synced index is 30 day(s) old"), "{out}");
}

#[test]
fn a_file_is_traced_to_the_package_that_wrote_it() {
  let sandbox = Sandbox::new();
  sandbox.write_db(&json!({"installed": {"frpc": {
    "manifest": {"pkgname": "frpc", "pkgver": "0.5.0", "pkgrel": "1"},
    "files": ["/etc/frpc.ini"]
  }}}));

  sandbox
    .run(&["-Qo", "/etc/frpc.ini"])
    .success()
    .stdout(contains("/etc/frpc.ini is owned by frpc"));
  sandbox.run(&["-Qo", "/etc/nothing"]).failure().stderr(contains("no installed package owns"));
}

#[test]
fn only_packages_with_a_newer_version_are_upgradable() {
  let sandbox = Sandbox::new();
  sandbox.write_index(
    &[package("old", "old", "2.0.0", "", "all"), package("same", "same", "1.0.0", "", "all")],
    now(),
  );
  let record = |name: &str, version: &str| {
    json!({ "manifest": {"pkgname": name, "appname": name, "pkgver": version, "pkgrel": "1",
      "source": "pica", "pkgmgr": "pica"}, "pkgfile": format!("/x/{name}.pkg.tar.gz") })
  };
  sandbox.write_db(&json!({"installed": {
    "old": record("old", "1.0.0"),
    "same": record("same", "1.0.0"),
    "local": { "manifest": {"pkgname": "local", "pkgver": "1", "source": "local"} }
  }}));

  let out = stdout(&sandbox.run(&["-Qu"]).success());
  assert_eq!(out.trim(), "old 1.0.0-1 -> 2.0.0-1");

  let doc: Value =
    serde_json::from_str(stdout(&sandbox.run(&["--json", "-Qu"]).success()).trim()).expect("json");
  assert_eq!(
    doc["packages"],
    json!([{"pkgname": "old", "installed": "1.0.0-1", "available": "2.0.0-1"}])
  );
}

#[test]
fn every_query_answers_in_one_json_document() {
  let sandbox = Sandbox::new();
  sandbox.write_db(&json!({"installed": {"frpc": {
    "manifest": {"pkgname": "frpc", "pkgver": "0.5.0", "pkgrel": "1", "pkgmgr": "pica"},
    "files": ["/etc/frpc.ini"], "dirs": []
  }}}));

  for args in [&["--json", "-Q"][..], &["--json", "-Qi", "frpc"], &["--json", "-Ql", "frpc"]] {
    let out = stdout(&sandbox.run(args).success());
    assert_eq!(out.lines().count(), 1, "{args:?}: {out}");
    serde_json::from_str::<Value>(out.trim()).expect("valid JSON");
  }
}
