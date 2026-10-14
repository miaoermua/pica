//! Behavioural coverage for the database's `source` / `pkgmgr` fields.
//!
//! `src/installed.rs` unit-tests the resolution ladder in isolation. These tests
//! cover the wiring around it: which package manager a command actually drives,
//! and what `-Qi` reports, for rows written by older releases.
//!
//! Pica reaches `opkg` / `apk` through `Command::new`, so every test runs it
//! against recording stub scripts placed first on `PATH` and asserts on the calls
//! they log. No real package manager is ever executed.

#![cfg(unix)]

use assert_cmd::assert::Assert;
use assert_cmd::Command;
use predicates::str::contains;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Records every invocation and answers the sub-commands the CLI branches on.
/// `info` reports an installed package so the feed path treats the target as
/// already present, which is what a re-install of a `-So` row expects.
const OPKG_STUB: &str = r#"#!/bin/sh
printf 'opkg %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  info) printf 'Package: %s\nVersion: 1.0.0\n' "$2" ;;
  status) printf 'Version: 1.0.0\n' ;;
  print-architecture) printf 'all 100\n' ;;
esac
exit 0
"#;

/// The `apk` counterpart. Its presence also means auto-detection would pick
/// `apk` over `opkg`, which is what makes the "recorded backend wins" cases
/// meaningful.
const APK_STUB: &str = r#"#!/bin/sh
printf 'apk %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  --print-arch) printf 'x86_64\n' ;;
  search) printf '%s-1.0.0\n' "$2" ;;
esac
exit 0
"#;

/// A row from the `opkg` era: the backend lived in `source`, `pkgmgr` did not
/// exist yet, and `pkgfile` held a backend name instead of an artifact path.
const LEGACY_FEED_ROW: &str = r#"{
  "installed": {
    "legacy-opkg": {
      "manifest": {
        "pkgname": "legacy-opkg",
        "appname": "legacy-opkg",
        "source": "opkg",
        "app": ["legacy-opkg"]
      },
      "pkgfile": "opkg",
      "files": [],
      "dirs": [],
      "installed_at": 1
    }
  }
}"#;

/// An artifact row whose backend was lost. `opkg` used to be injected as a
/// default even for lifecycle packages, so reading it back as `opkg` would make
/// `-R` drive a package manager that never touched these files.
const BACKENDLESS_ARTIFACT_ROW: &str = r#"{
  "installed": {
    "old-artifact": {
      "manifest": {
        "pkgname": "old-artifact",
        "appname": "old-artifact",
        "source": "pica"
      },
      "pkgfile": "/tmp/old-artifact.pkg.tar.gz",
      "files": [],
      "dirs": [],
      "installed_at": 2
    }
  }
}"#;

/// A feed install recorded after the split: channel and backend are separate
/// fields, and the backend is `apk` while `pkgfile` is only a backend name.
const APK_FEED_ROW: &str = r#"{
  "installed": {
    "feed-apk": {
      "manifest": {
        "pkgname": "feed-apk",
        "appname": "feed-apk",
        "source": "feed",
        "pkgmgr": "apk",
        "app": ["feed-apk"]
      },
      "pkgfile": "apk",
      "files": [],
      "dirs": [],
      "installed_at": 3
    }
  }
}"#;

/// Rows no repository index can resolve: a local file, a URL download, and a
/// feed install.
const UNRESOLVABLE_ROWS: &str = r#"{
  "installed": {
    "from-file": {
      "manifest": {"pkgname": "from-file", "appname": "from-file", "source": "local", "pkgmgr": "none", "pkgver": "1.0.0"},
      "pkgfile": "/tmp/from-file.pkg.tar.gz",
      "files": [],
      "dirs": [],
      "installed_at": 1
    },
    "from-url": {
      "manifest": {"pkgname": "from-url", "appname": "from-url", "source": "url", "pkgmgr": "none", "pkgver": "1.0.0"},
      "pkgfile": "/tmp/from-url.pkg.tar.gz",
      "files": [],
      "dirs": [],
      "installed_at": 2
    },
    "from-feed": {
      "manifest": {"pkgname": "from-feed", "appname": "from-feed", "source": "feed", "pkgmgr": "apk", "pkgver": "1.0.0"},
      "pkgfile": "apk",
      "files": [],
      "dirs": [],
      "installed_at": 3
    }
  }
}"#;

/// A database with no installs, for tests that build their rows by installing.
const EMPTY_DB: &str = r#"{"installed": {}}"#;

/// One throwaway installation root with recording stub package managers.
struct Sandbox {
  dir: tempfile::TempDir,
}

impl Sandbox {
  /// `db` becomes `state/db.json` verbatim.
  fn new(db: &str) -> Self {
    let dir = tempfile::tempdir().expect("create temp dir");
    let state = dir.path().join("state");
    fs::create_dir_all(&state).expect("create state dir");
    fs::create_dir_all(dir.path().join("etc")).expect("create etc dir");
    fs::write(state.join("db.json"), db).expect("write db.json");

    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).expect("create stub dir");
    stub(&bin.join("opkg"), OPKG_STUB);
    stub(&bin.join("apk"), APK_STUB);

    Self { dir }
  }

  /// The CLI with this sandbox's environment: redirected directories and the
  /// stubs ahead of the real `PATH`.
  fn command(&self) -> Command {
    let mut cmd = assert_cmd::cargo_bin_cmd!("pica");
    cmd.env("PICA_ETC_DIR", self.dir.path().join("etc"));
    cmd.env("PICA_STATE_DIR", self.state_dir());
    cmd.env("PICA_STUB_LOG", self.state_dir().join("calls.log"));
    cmd.env("PATH", self.search_path());
    cmd
  }

  fn run(&self, args: &[&str]) -> Assert {
    let mut cmd = self.command();
    cmd.args(args);
    cmd.assert()
  }

  /// Package manager invocations recorded so far, one per line.
  fn calls(&self) -> String {
    fs::read_to_string(self.state_dir().join("calls.log")).unwrap_or_default()
  }

  /// The installed database as written by the CLI.
  fn db(&self) -> serde_json::Value {
    let raw = fs::read_to_string(self.state_dir().join("db.json")).expect("read db.json");
    serde_json::from_str(&raw).expect("parse db.json")
  }

  /// Builds a minimal artifact whose only payload is `pkgs/<declared>/`, which
  /// is the one payload kind a `none` install ignores. That keeps install tests
  /// from writing to the host file system, since `root/` and `scripts/` are
  /// deployed at their real absolute paths.
  fn artifact(&self, pkgname: &str, declared: &str) -> PathBuf {
    let src = self.dir.path().join(format!("src-{pkgname}"));
    fs::create_dir_all(src.join("pkgs").join(declared)).expect("create pkgdir");
    fs::write(
      src.join("manifest"),
      format!(
        "pkgname = {pkgname}\nappname = {pkgname}\npkgver = 1.0.0\npkgrel = 1\npkgmgr = {declared}\nplatform = all\nos = all\narch = all\n"
      ),
    )
    .expect("write manifest");
    fs::write(src.join("pkgs").join(declared).join("marker"), b"payload").expect("write payload");

    let out = self.dir.path().join(format!("{pkgname}.pkg.tar.gz"));
    let status = std::process::Command::new("tar")
      .arg("-czf")
      .arg(&out)
      .args(["-C"])
      .arg(&src)
      .args(["manifest", "pkgs"])
      .status()
      .expect("run tar");
    assert!(status.success(), "tar failed to build {pkgname}");
    out
  }

  fn state_dir(&self) -> PathBuf {
    self.dir.path().join("state")
  }

  fn search_path(&self) -> std::ffi::OsString {
    let mut dirs = vec![self.dir.path().join("bin")];
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    std::env::join_paths(dirs).expect("join PATH")
  }
}

fn stub(path: &Path, body: &str) {
  fs::write(path, body).expect("write stub");
  let mut perms = fs::metadata(path).expect("stat stub").permissions();
  perms.set_mode(0o755);
  fs::set_permissions(path, perms).expect("chmod stub");
}

fn stdout_of(assert: &Assert) -> String {
  String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

/// Reads one `-Qi` field by label, independent of the column padding.
fn field(stdout: &str, label: &str) -> String {
  stdout
    .lines()
    .find_map(|line| line.strip_prefix(label))
    .and_then(|rest| rest.split_once(':'))
    .map_or_else(String::new, |(_, value)| value.trim().to_string())
}

#[test]
fn qi_normalises_a_legacy_feed_row() {
  let sandbox = Sandbox::new(LEGACY_FEED_ROW);

  let assert = sandbox.run(&["-Qi", "legacy-opkg"]).success();
  let stdout = stdout_of(&assert);

  assert_eq!(field(&stdout, "Source"), "feed");
  assert_eq!(field(&stdout, "PkgMgr"), "opkg");
}

#[test]
fn qi_reports_none_for_a_backendless_artifact_row() {
  let sandbox = Sandbox::new(BACKENDLESS_ARTIFACT_ROW);

  let assert = sandbox.run(&["-Qi", "old-artifact"]).success();
  let stdout = stdout_of(&assert);

  assert_eq!(field(&stdout, "PkgMgr"), "none");
  assert_eq!(
    sandbox.calls(),
    "",
    "showing a Pica-native package must not consult a package manager"
  );
}

#[test]
fn qi_reads_dependencies_through_the_recorded_backend() {
  let sandbox = Sandbox::new(APK_FEED_ROW);

  let assert = sandbox.run(&["-Qi", "feed-apk"]).success();
  let stdout = stdout_of(&assert);

  assert_eq!(field(&stdout, "Source"), "feed");
  assert_eq!(field(&stdout, "PkgMgr"), "apk");

  let calls = sandbox.calls();
  assert!(calls.contains("apk"), "expected apk to be queried: {calls}");
  assert!(!calls.contains("opkg"), "opkg must not be involved: {calls}");
}

#[test]
fn remove_uses_the_recorded_backend_and_warns_when_it_must_infer() {
  let sandbox = Sandbox::new(LEGACY_FEED_ROW);

  sandbox
    .run(&["-R", "legacy-opkg"])
    .success()
    .stderr(contains("inferred from the legacy source field"));

  let calls = sandbox.calls();
  assert!(calls.contains("opkg remove legacy-opkg"), "{calls}");
}

#[test]
fn install_reuses_the_recorded_backend_instead_of_detecting_one() {
  // Both stubs are on `PATH` and detection prefers apk, so opkg only gets the
  // call because the row's recorded backend was honoured.
  let sandbox = Sandbox::new(LEGACY_FEED_ROW);

  sandbox.run(&["-S", "legacy-opkg"]).success();

  let calls = sandbox.calls();
  assert!(calls.contains("opkg install legacy-opkg"), "{calls}");
  assert!(!calls.contains("apk"), "{calls}");

  // Re-installing also rewrites the row: the channel is now explicit and the
  // backend is recorded separately from it.
  let manifest = &sandbox.db()["installed"]["legacy-opkg"]["manifest"];
  assert_eq!(manifest["source"], "feed");
  assert_eq!(manifest["pkgmgr"], "opkg");
}

#[test]
fn install_refuses_a_backend_that_contradicts_the_artifact() {
  // `apk` and `opkg` disagree about who owns the files, so the mismatch is
  // refused rather than installed under the wrong owner.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.artifact("declared-opkg", "opkg");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox
    .run(&["--pkgmgr", "apk", "-U", artifact])
    .failure()
    .stderr(contains("conflicts with package pkgmgr opkg"));

  assert_eq!(sandbox.calls(), "", "nothing may be installed");
  assert!(
    sandbox.db()["installed"].as_object().expect("installed object").is_empty(),
    "a refused install must not be recorded"
  );
}

#[test]
fn upgrade_skips_rows_that_no_index_can_resolve() {
  // A `local` or `url` row has no index entry to compare versions against, so
  // the sweep must leave it alone instead of reinstalling it.
  let sandbox = Sandbox::new(UNRESOLVABLE_ROWS);

  sandbox.run(&["-Su"]).success();

  assert_eq!(sandbox.calls(), "", "nothing may be upgraded");
}

#[test]
fn install_records_the_backend_in_use_not_the_declared_one() {
  // The artifact declares `apk`, but `--pkgmgr none` installs it without any
  // package manager. Recording the declared value would make `-R` drive apk
  // against files apk never touched.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.artifact("declared-apk", "apk");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["--pkgmgr", "none", "-U", artifact]).success();

  let manifest = &sandbox.db()["installed"]["declared-apk"]["manifest"];
  assert_eq!(manifest["source"], "local");
  assert_eq!(manifest["pkgmgr"], "none");

  // Removal follows the record rather than the declaration.
  sandbox.run(&["-R", "declared-apk"]).success();
  assert_eq!(sandbox.calls(), "", "apk must not be asked to remove it");
}
