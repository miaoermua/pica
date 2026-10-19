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

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use predicates::prelude::PredicateBooleanExt;
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
///
/// `@VERSION@` is replaced by the version the stub reports for `--version`:
/// apk-tools 3.x (`OpenWrt` 25.12 and later) is the one that refuses a local file
/// no repository provides, so a test can put either release on `PATH`.
const APK_STUB: &str = r#"#!/bin/sh
printf 'apk %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  --version) printf 'apk-tools @VERSION@\n' ;;
  --print-arch) printf 'x86_64\n' ;;
  search) printf '%s-1.0.0\n' "$3" ;;
esac
exit 0
"#;

/// The apk-tools release `OpenWrt` 25.12 ships, and the one it shipped before.
const APK_3: &str = "3.0.0";
const APK_2: &str = "2.14.4";

/// `APK_STUB` plus the answer a bound check needs.
///
/// `installed_version` reads `apk list --installed -- <name>`, which apk-tools
/// prints as `<name>-<version>`. `@INSTALLED@` is the version the test says is
/// on the box.
const APK_STUB_WITH_INSTALLED: &str = r#"#!/bin/sh
printf 'apk %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  --version) printf 'apk-tools 3.0.0\n' ;;
  --print-arch) printf 'x86_64\n' ;;
  search) printf '%s-1.0.0\n' "$3" ;;
  list) printf '%s-@INSTALLED@\n' "$4" ;;
esac
exit 0
"#;

/// The opkg counterpart: both `is_installed` and `installed_version` read
/// `opkg status <name>`, so the line has to look like a real status block.
const OPKG_STUB_WITH_INSTALLED: &str = r#"#!/bin/sh
printf 'opkg %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  status) printf 'Package: %s\nStatus: install user installed\nVersion: @INSTALLED@\n' "$2" ;;
  info) printf 'Package: %s\nVersion: @INSTALLED@\n' "$2" ;;
  print-architecture) printf 'all 100\n' ;;
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

/// A row carrying every field `-Qi` can report, so a test can tell a field that
/// says nothing from one that is simply old. It is `pica`-native, so showing it
/// never reaches for a backend command.
const FULL_ROW: &str = r#"{
  "installed": {
    "full-app": {
      "manifest": {
        "pkgname": "full-app",
        "appname": "full-app",
        "pkgver": "1.2.3",
        "pkgrel": "4",
        "url": "https://example.test/app",
        "pkgdesc": "A demo app",
        "luci_url": "admin/services/full-app",
        "protocol": "https",
        "branch": "main",
        "luci_desc": "Demo app",
        "os": "linux",
        "platform": "x86_64",
        "arch": "x86_64",
        "uname": "x86_64",
        "type": "app",
        "source": "pica",
        "pkgmgr": "pica",
        "packager": "Someone <someone@example.test>",
        "builddate": "2026-10-01",
        "size": "2048",
        "license": "MIT",
        "visibility": "public"
      },
      "pkgfile": "/tmp/full-app.pkg.tar.gz",
      "files": [],
      "dirs": [],
      "installed_at": 17
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
  /// `db` becomes `state/db.json` verbatim, on an apk host of the current
  /// release: the one whose behaviour the CLI is written for.
  fn new(db: &str) -> Self {
    Self::with_apk_version(db, APK_3)
  }

  /// The same, with the apk on `PATH` reporting `version`.
  fn with_apk_version(db: &str, version: &str) -> Self {
    Self::with_stubs(db, OPKG_STUB, &APK_STUB.replace("@VERSION@", version))
  }

  /// The same, with the two stubs replaced body for body.
  ///
  /// A bound can only be judged against an installed version, and the default
  /// stubs answer nothing about what is installed — the honest answer for a
  /// package manager whose state no test set up. A test that needs a version
  /// supplies a body instead of teaching the shared stub a fact every other
  /// test would then see as well.
  fn with_stubs(db: &str, opkg_body: &str, apk_body: &str) -> Self {
    let dir = tempfile::tempdir().expect("create temp dir");
    let state = dir.path().join("state");
    fs::create_dir_all(&state).expect("create state dir");
    fs::create_dir_all(dir.path().join("etc")).expect("create etc dir");
    fs::write(state.join("db.json"), db).expect("write db.json");

    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).expect("create stub dir");
    stub(&bin.join("opkg"), opkg_body);
    stub(&bin.join("apk"), apk_body);

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

  /// The same, with an empty pipe on stdin.
  ///
  /// `write_stdin` gives the child a pipe that is closed straight away. Whether
  /// the test runs from a terminal or from CI, that is not a terminal — the
  /// situation the optional-dependency selector has to handle without blocking.
  fn run_off_terminal(&self, args: &[&str]) -> Assert {
    let mut cmd = self.command();
    cmd.args(args).write_stdin("");
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

  /// Builds a minimal artifact whose only payload is `pkgs/<declared>/`.
  ///
  /// A system backend hands that directory to its own package manager, which is
  /// a stub in the sandbox — so nothing is written at a real path. `root/` and
  /// `scripts/` are deployed at their absolute paths and would touch the host.
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

  /// A `root/`-only artifact, which is the only shape a `pica`-backend install
  /// takes: a `pica` artifact ships `root/` and `scripts/`, never a container.
  ///
  /// The payload goes under `tmp/` because this sandbox redirects the database
  /// but not the target root, so a `root/etc/...` would try to write to the
  /// real `/etc`. `extra` is appended to the manifest, where a dependency
  /// declaration goes.
  fn root_only_artifact(&self, pkgname: &str, declared: &str, extra: &str) -> PathBuf {
    let src = self.dir.path().join(format!("src-{pkgname}"));
    let root = src.join("root/tmp");
    fs::create_dir_all(&root).expect("create pkgdir");
    fs::write(
      src.join("manifest"),
      format!(
        "pkgname = {pkgname}\nappname = {pkgname}\npkgver = 1.0.0\npkgrel = 1\npkgmgr = {declared}\nplatform = all\nos = all\narch = all\n{extra}"
      ),
    )
    .expect("write manifest");
    fs::write(root.join(format!("{pkgname}.conf")), b"payload\n").expect("write payload");

    let out = self.dir.path().join(format!("{pkgname}.pkg.tar.gz"));
    let status = std::process::Command::new("tar")
      .arg("-czf")
      .arg(&out)
      .args(["-C"])
      .arg(&src)
      .args(["manifest", "root"])
      .status()
      .expect("run tar");
    assert!(status.success(), "tar failed to build {pkgname}");
    out
  }

  /// An artifact whose payload is the `pkgs/` batch `containers` names.
  ///
  /// `stage_packages` flattens `pkgs/<backend>/` into the archive's `pkgs/`, so
  /// the files sit directly there — which is also where `install_pkgs_dir` looks
  /// for them. `extra` is appended to the manifest and is where dependency
  /// declarations (`base = ...`, `app = ...`) go.
  fn container_artifact(
    &self,
    pkgname: &str,
    backend: &str,
    extra: &str,
    containers: &[&str],
  ) -> PathBuf {
    let src = self.dir.path().join(format!("src-{pkgname}"));
    fs::create_dir_all(src.join("pkgs")).expect("create pkgdir");
    fs::write(
      src.join("manifest"),
      format!(
        "pkgname = {pkgname}\nappname = {pkgname}\npkgver = 1.0.0\npkgrel = 1\npkgmgr = {backend}\nplatform = all\nos = all\narch = all\n{extra}"
      ),
    )
    .expect("write manifest");
    for container in containers {
      fs::write(src.join("pkgs").join(container), b"container payload").expect("write container");
    }

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

  /// An `apk` artifact carrying one local container.
  fn apk_artifact(&self, pkgname: &str) -> PathBuf {
    let container = format!("{pkgname}.apk");
    self.container_artifact(pkgname, "apk", "", &[&container])
  }

  /// An `apk` artifact whose `pkgs/` is present and holds nothing: an archive
  /// that would install no package at all.
  fn artifact_with_empty_pkgs(&self, pkgname: &str) -> PathBuf {
    let src = self.dir.path().join(format!("src-{pkgname}"));
    fs::create_dir_all(src.join("pkgs")).expect("create pkgdir");
    fs::write(
      src.join("manifest"),
      format!(
        "pkgname = {pkgname}\nappname = {pkgname}\npkgver = 1.0.0\npkgrel = 1\npkgmgr = apk\nplatform = all\nos = all\narch = all\n"
      ),
    )
    .expect("write manifest");

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

  /// `json` becomes `etc/pica.json` verbatim.
  fn write_conf(&self, json: &str) {
    fs::write(self.dir.path().join("etc/pica.json"), json).expect("write pica.json");
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

/// The labels a `-Qi` listing printed, in order. The CLI's own `::` status
/// lines have an empty label and are not fields.
fn labels_of(stdout: &str) -> Vec<&str> {
  stdout
    .lines()
    .filter_map(|line| line.split_once(':'))
    .map(|(label, _)| label.trim())
    .filter(|label| !label.is_empty())
    .collect()
}

#[test]
fn qi_omits_the_fields_a_row_has_no_value_for() {
  // A label with nothing after it tells a reader no more than an absent label,
  // and reads as a value that was expected and lost. This row predates most of
  // the fields, and the listing says so by leaving them out.
  let sandbox = Sandbox::new(LEGACY_FEED_ROW);

  let assert = sandbox.run(&["-Qi", "legacy-opkg"]).success();
  let stdout = stdout_of(&assert);

  assert_eq!(
    labels_of(&stdout),
    ["Name", "AppName", "Source", "PkgMgr", "Installed At", "Installed From"],
    "{stdout}"
  );
}

#[test]
fn qi_prints_the_fields_a_row_does_carry() {
  // The omission is a default, not a shortened listing: a package that carries
  // all of it is still shown in full, field for field.
  let sandbox = Sandbox::new(FULL_ROW);

  let assert = sandbox.run(&["-Qi", "full-app"]).success();
  let stdout = stdout_of(&assert);

  assert_eq!(
    labels_of(&stdout),
    [
      "Name",
      "Version",
      "Pkgver",
      "Pkgrel",
      "AppName",
      "Program URL",
      "PkgDesc",
      "LuCI URL",
      "Protocol",
      "Branch",
      "LuCI Desc",
      "OS",
      "Platform",
      "Arch",
      "Uname",
      "Type",
      "Source",
      "PkgMgr",
      "Packager",
      "Build Date",
      "Size",
      "Installed At",
      "Installed From",
      "License",
      "Visibility"
    ],
    "{stdout}"
  );
  assert_eq!(field(&stdout, "Version"), "1.2.3-4");
  assert_eq!(field(&stdout, "License"), "MIT");
  assert_eq!(field(&stdout, "Size"), "2048 (2.00 KiB)");
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
fn qi_reports_pica_for_a_backendless_artifact_row() {
  let sandbox = Sandbox::new(BACKENDLESS_ARTIFACT_ROW);

  let assert = sandbox.run(&["-Qi", "old-artifact"]).success();
  let stdout = stdout_of(&assert);

  assert_eq!(field(&stdout, "PkgMgr"), "pica");
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
  // The artifact declares `apk`, but `--pkgmgr pica` installs it without any
  // package manager. Recording the declared value would make `-R` drive apk
  // against files apk never touched.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.root_only_artifact("declared-apk", "apk", "");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["--pkgmgr", "pica", "-U", artifact]).success();

  let manifest = &sandbox.db()["installed"]["declared-apk"]["manifest"];
  assert_eq!(manifest["source"], "local");
  assert_eq!(manifest["pkgmgr"], "pica");

  // Removal follows the record rather than the declaration.
  sandbox.run(&["-R", "declared-apk"]).success();
  assert_eq!(sandbox.calls(), "", "apk must not be asked to remove it");
}

// -- Local apk containers ------------------------------------------------------
//
// `pkgs/*.apk` is a file, not a package a repository provides. apk-tools 3.x,
// which OpenWrt 25.12 ships, refuses such a file unless it is told otherwise, so
// what the CLI hands to `apk add` decides whether a Pica install works at all on
// a current router.

/// The target of the first `apk add` recorded, which is the argument after the
/// `--` separator.
fn installed_apk_target(calls: &str) -> String {
  let add = calls.lines().find(|line| line.starts_with("apk add ")).expect("apk add ran");
  add.rsplit(" -- ").next().expect("a -- separator before the target").to_string()
}

#[test]
fn a_local_apk_is_installed_with_the_non_repository_flag() {
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.apk_artifact("local-apk");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact]).success();

  let calls = sandbox.calls();
  let add = calls.lines().find(|line| line.starts_with("apk add ")).expect("apk add ran");
  assert!(add.contains("--force-non-repository"), "apk 3.x needs it for a local file: {calls}");
  assert!(add.contains("--allow-untrusted"), "{calls}");
  assert!(
    installed_apk_target(&calls).ends_with("local-apk.apk"),
    "apk is given the file, not a name it would look up in a repository: {calls}"
  );
}

#[test]
fn the_setting_can_turn_the_non_repository_flag_off() {
  // The escape hatch for a router whose apk build does not know the option: the
  // flag leaves the command line, and the signature waiver stays, because that
  // is what a file signed by someone other than this router's key needs.
  let sandbox = Sandbox::new(EMPTY_DB);
  sandbox.write_conf(r#"{"apk_force_non_repository": false}"#);
  let artifact = sandbox.apk_artifact("local-apk");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact]).success();

  let calls = sandbox.calls();
  assert!(!calls.contains("--force-non-repository"), "{calls}");
  assert!(calls.contains("--allow-untrusted"), "{calls}");
}

#[test]
fn an_apk_2_host_is_not_given_the_non_repository_flag() {
  // apk-tools 2.x has no such option, and an unknown option stops the command
  // before it installs anything; the setting is on by default, so the version is
  // what has to hold it back.
  let sandbox = Sandbox::with_apk_version(EMPTY_DB, APK_2);
  let artifact = sandbox.apk_artifact("local-apk");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact]).success();

  let calls = sandbox.calls();
  assert!(calls.contains("apk --version"), "the version has to be asked for: {calls}");
  assert!(!calls.contains("--force-non-repository"), "{calls}");
  assert!(calls.contains("--allow-untrusted"), "{calls}");
}

#[test]
fn a_non_boolean_apk_setting_is_refused() {
  // This setting decides whether a signature check is waived, so a typo has to
  // stop the install rather than quietly resolve to one of the two answers.
  let sandbox = Sandbox::new(EMPTY_DB);
  sandbox.write_conf(r#"{"apk_force_non_repository": "yes"}"#);
  let artifact = sandbox.apk_artifact("local-apk");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox
    .run(&["-U", artifact])
    .failure()
    .stderr(contains("apk_force_non_repository").and(contains("expected true or false")));

  assert_eq!(sandbox.calls(), "", "nothing may be installed");
}

// -- One batch per artifact ---------------------------------------------------
//
// `pkgs/` is a set of containers owned by the artifact, not a per-phase
// resource. The containers of one application depend on each other, and a
// backend only resolves such a dependency among the files of one request, so the
// batch has to reach it in a single call — and reach it once.

/// Every `apk add` recorded, as the target list each one ended with.
fn apk_add_targets(calls: &str) -> Vec<Vec<String>> {
  calls
    .lines()
    .filter(|line| line.starts_with("apk add "))
    .map(|line| {
      line
        .split_once(" -- ")
        .map(|(_, targets)| targets.split_whitespace().map(ToString::to_string).collect())
        .unwrap_or_default()
    })
    .collect()
}

/// Every `opkg install` recorded, as the arguments after the sub-command.
fn opkg_install_args(calls: &str) -> Vec<Vec<String>> {
  calls
    .lines()
    .filter_map(|line| line.strip_prefix("opkg install "))
    .map(|args| args.split_whitespace().map(ToString::to_string).collect())
    .collect()
}

/// What a target list installed, as bare file names.
fn file_names(targets: &[String]) -> Vec<String> {
  targets.iter().map(|target| target.rsplit('/').next().unwrap_or(target).to_string()).collect()
}

#[test]
fn the_container_batch_reaches_apk_in_one_call() {
  // `aaa-app` depends on `zzz-core`, and the declared order is not the
  // alphabetical one. Handing them over separately would leave apk looking up the
  // dependency in its repositories, where a container that only this artifact
  // ships is not to be found.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact =
    sandbox.container_artifact("batch-apk", "apk", "", &["aaa-app.apk", "zzz-core.apk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact]).success();

  let calls = sandbox.calls();
  let adds = apk_add_targets(&calls);
  assert_eq!(adds.len(), 1, "one request for the whole batch, at any policy: {calls}");
  assert_eq!(
    file_names(&adds[0]),
    vec!["aaa-app.apk".to_string(), "zzz-core.apk".to_string()],
    "both containers, together: {calls}"
  );

  let add = calls.lines().find(|line| line.starts_with("apk add ")).expect("apk add ran");
  assert_eq!(add.matches(" -- ").count(), 1, "one separator for the whole list: {add}");
  assert!(add.contains("--force-non-repository"), "{add}");
}

#[test]
fn a_second_phase_does_not_reinstall_the_batch() {
  // Both the `base` and the `app` phase used to look at the same `pkgs/` and each
  // decide for itself to hand it over, so the batch was installed twice. The
  // decision is now taken once for the artifact; a phase cannot disagree with it.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.container_artifact(
    "batch-apk",
    "apk",
    "base = base-dep\napp = app-dep\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "packaged-first"]).success();

  let calls = sandbox.calls();
  let adds = apk_add_targets(&calls);
  assert_eq!(adds.len(), 1, "the batch is one request, and it happens once: {calls}");
  assert_eq!(file_names(&adds[0]), vec!["demo-app.apk".to_string()], "{calls}");
  let installed: Vec<String> = adds.iter().flatten().cloned().collect();
  assert!(
    !installed.iter().any(|target| target.contains("base-dep") || target.contains("app-dep")),
    "`packaged-first` takes the containers: the declared names are not installed as well: {calls}"
  );
}

#[test]
fn feeds_answered_in_phases_leave_the_containers_alone() {
  // The other side of the same decision: when the policy takes the feeds, the
  // declared names are installed — base before app — and the containers are never
  // unpacked.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.container_artifact(
    "batch-apk",
    "apk",
    "base = base-dep\napp = app-dep\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "feed-first"]).success();

  let calls = sandbox.calls();
  let adds = apk_add_targets(&calls);
  assert_eq!(adds.len(), 2, "one request per phase: {calls}");
  assert_eq!(adds[0], vec!["base-dep".to_string()], "base goes first: {calls}");
  assert_eq!(adds[1], vec!["app-dep".to_string()], "then app: {calls}");
  assert!(!calls.contains("demo-app.apk"), "the batch stays unused: {calls}");
}

#[test]
fn opkg_is_given_the_whole_batch_too() {
  // The same rule under the other system backend: one `opkg install` for the
  // files, so its own solver sees them as one set.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact =
    sandbox.container_artifact("batch-opkg", "opkg", "", &["aaa-app.ipk", "zzz-core.ipk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--pkgmgr", "opkg"]).success();

  let calls = sandbox.calls();
  let installs = opkg_install_args(&calls);
  assert_eq!(installs.len(), 1, "one request for the whole batch: {calls}");
  assert_eq!(
    file_names(&installs[0]),
    vec!["aaa-app.ipk".to_string(), "zzz-core.ipk".to_string()],
    "{calls}"
  );
}

#[test]
fn a_pkgs_dir_with_no_packages_is_refused_before_anything_is_installed() {
  // An empty `pkgs/` installs nothing, so the artifact is empty rather than
  // merely surprising. Refusing it here keeps the failure at validation instead
  // of in the middle of the install.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.artifact_with_empty_pkgs("no-containers");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact]).failure().stderr(contains("would be empty"));

  assert_eq!(sandbox.calls(), "", "nothing may be installed");
}

#[test]
fn a_policy_that_will_not_install_from_the_feeds_does_not_ask_them() {
  // `-S new-app` may install from the feeds or from the pica repository, and
  // under `packaged-only` the feeds are not a source at all. Asking them anyway
  // would make a repository install depend on an index whose answer nothing
  // uses — and on a host whose feeds are out of reach, on a fetch that must
  // fail before the install starts.
  let sandbox = Sandbox::new(EMPTY_DB);

  sandbox
    .run(&["-S", "new-app", "--feed-policy", "packaged-only"])
    .failure()
    .stderr(contains("package not found in pica repos"));

  assert_eq!(sandbox.calls(), "", "the feeds were not consulted at all");
}

#[test]
fn the_feeds_are_asked_when_they_are_the_source() {
  // The other side of the same decision: under the default policy the feeds
  // decide first, so the detected backend is asked about the package before the
  // repository is. Auto-detection picks the apk on `PATH`, which answers for
  // every name it is given.
  let sandbox = Sandbox::new(EMPTY_DB);

  sandbox.run(&["-S", "feed-app"]).success();

  let calls = sandbox.calls();
  assert!(calls.contains("apk search --exact feed-app"), "the feed answer decides it: {calls}");
  assert!(calls.contains("apk add -- feed-app"), "and the feeds install it: {calls}");
}

// -- Dependency bounds ---------------------------------------------------------
//
// A bound (`base = libfoo>=1.2`) is an assertion pica checks itself. The one
// thing it must never become is an argument: opkg installs by name or by `.ipk`
// path, apk reads a versioned atom, so passing the declaration on makes one
// backend succeed and the other fail. These tests assert both halves — the
// judgement, and that the command line holds only the name.

/// A row whose manifest declares bounds, for `-Qi`.
const BOUNDED_ROW: &str = r#"{
  "installed": {
    "bounded-app": {
      "manifest": {
        "pkgname": "bounded-app",
        "appname": "bounded-app",
        "source": "feed",
        "pkgmgr": "apk",
        "base": ["luci-base>=1.2"],
        "app": ["luci-app-demo>=1.0", "plain-dep"]
      },
      "pkgfile": "apk",
      "files": [],
      "dirs": [],
      "installed_at": 4
    }
  }
}"#;

fn apk_stub_installed(version: &str) -> String {
  APK_STUB_WITH_INSTALLED.replace("@INSTALLED@", version)
}

fn opkg_stub_installed(version: &str) -> String {
  OPKG_STUB_WITH_INSTALLED.replace("@INSTALLED@", version)
}

#[test]
fn a_satisfied_bound_installs_the_name_and_nothing_else() {
  // 1.1 installed, `>=1.0` and `>1.0` both hold. The request handed to apk is
  // the name: this is the regression that keeps the bound on pica's side.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, &apk_stub_installed("1.1"));
  let artifact = sandbox.container_artifact(
    "bound-apk",
    "apk",
    "base = base-dep>=1.0\napp = app-dep>1.0\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "feed-first"]).success();

  let calls = sandbox.calls();
  let adds = apk_add_targets(&calls);
  assert_eq!(adds.len(), 2, "one request per phase: {calls}");
  assert_eq!(adds[0], vec!["base-dep".to_string()], "the name, not the bound: {calls}");
  assert_eq!(adds[1], vec!["app-dep".to_string()], "the name, not the bound: {calls}");
  assert!(!calls.contains('>'), "no bound may reach the backend: {calls}");
  assert!(!calls.contains('<'), "no bound may reach the backend: {calls}");
}

#[test]
fn an_installed_version_that_misses_the_bound_stops_the_install() {
  // 1.1 installed, `>=2.0` asked for. The message has to carry both numbers:
  // "does not satisfy" alone leaves the reader with nothing to compare.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, &apk_stub_installed("1.1"));
  let artifact =
    sandbox.container_artifact("bound-apk", "apk", "base = base-dep>=2.0\n", &["demo-app.apk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox
    .run(&["-U", artifact, "--feed-policy", "feed-first"])
    .failure()
    .stderr(contains("base-dep>=2.0"))
    .stderr(contains("1.1"));

  assert!(!sandbox.calls().contains("apk add"), "nothing may be installed: {}", sandbox.calls());
}

#[test]
fn opkg_is_asked_for_the_name_too() {
  // The same bound under the other backend, read from `opkg status`. `opkg
  // install base-dep>=1.0` would be a package called `base-dep>=1.0`.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, &opkg_stub_installed("1.1"), APK_STUB);
  let artifact =
    sandbox.container_artifact("bound-opkg", "opkg", "base = base-dep>=1.0\n", &["demo.ipk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["--pkgmgr", "opkg", "-U", artifact, "--feed-policy", "feed-first"]).success();

  let calls = sandbox.calls();
  assert!(calls.contains("opkg install base-dep"), "the name, not the bound: {calls}");
  assert!(!calls.contains('>'), "no bound may reach the backend: {calls}");
  assert!(!calls.contains("demo.ipk"), "the feeds were the source: {calls}");
}

#[test]
fn a_bound_on_a_dependency_that_is_not_installed_yet_is_recorded_not_enforced() {
  // The default opkg stub never says "installed", so the precheck has no
  // version to judge. Refusing here would reject every first install; the report
  // is where the assertion goes instead, so an audit can still see it.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact =
    sandbox.container_artifact("bound-opkg", "opkg", "base = base-dep>=1.0\n", &["demo.ipk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["--pkgmgr", "opkg", "-U", artifact, "--feed-policy", "feed-first"]).success();

  let report: serde_json::Value = serde_json::from_str(
    &fs::read_to_string(sandbox.state_dir().join("install-report.json"))
      .expect("the install writes a report"),
  )
  .expect("report is json");
  let base = &report["reports"]["bound-opkg"]["precheck"]["base"][0];
  assert_eq!(base["name"], "base-dep");
  assert_eq!(base["constraint"], ">=1.0", "the bound is kept for auditing: {base}");
}

#[test]
fn qi_shows_a_bound_as_it_was_written() {
  let sandbox = Sandbox::new(BOUNDED_ROW);

  let assert = sandbox.run(&["-Qi", "bounded-app"]).success();
  let stdout = stdout_of(&assert);

  assert_eq!(field(&stdout, "Requires"), "base:luci-base>=1.2 app:luci-app-demo>=1.0");
}

/// An opkg that only starts reporting `base-dep` as installed once an install
/// has run — the shape a first install has, and the reason the precheck cannot
/// judge the bound yet.
const OPKG_STUB_AFTER_INSTALL: &str = r#"#!/bin/sh
printf 'opkg %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  status)
    if grep -q '^opkg install' "$PICA_STUB_LOG"; then
      printf 'Package: %s\nStatus: install user installed\nVersion: 1.1\n' "$2"
    fi
    ;;
  info) printf 'Package: %s\nVersion: 1.0.0\n' "$2" ;;
  print-architecture) printf 'all 100\n' ;;
esac
exit 0
"#;

#[test]
fn a_bound_the_backend_breaks_while_installing_is_reported_not_undone() {
  // The precheck hands `base-dep` over as a name because nothing had installed
  // it yet; opkg then installs 1.1 while the artifact asked for `>=2.0`. The
  // recheck reports that mismatch — and lets the install stand, because undo
  // would be pica overruling the backend's version choice.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB_AFTER_INSTALL, APK_STUB);
  let artifact =
    sandbox.container_artifact("bound-opkg", "opkg", "base = base-dep>=2.0\n", &["demo.ipk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  let assert =
    sandbox.run(&["--pkgmgr", "opkg", "-U", artifact, "--feed-policy", "feed-first"]).success();
  let stderr = String::from_utf8_lossy(&assert.get_output().stderr);

  assert!(
    stderr.contains("bound-opkg requires base:base-dep>=2.0, but 1.1 is installed"),
    "the warning names both versions: {stderr}"
  );
}

// -- Optional dependencies -----------------------------------------------------
//
// A `?` declaration is optional: the precheck still looks it up, so the
// user can be told what is available, but the install must not fail when it is
// absent and must not install it unless it was asked for. The stubs below make
// the two halves of that visible — what reaches `apk add`, and what the install
// report says happened.

/// An `apk` whose feeds hold every name but `absent-dep`, and where nothing is
/// installed. `info` deliberately fails: the shared stubs exit 0 for it, which
/// apk reads as "installed".
const APK_STUB_WITH_ABSENT: &str = r#"#!/bin/sh
printf 'apk %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  --version) printf 'apk-tools 3.0.0\n' ;;
  --print-arch) printf 'x86_64\n' ;;
  info) exit 1 ;;
  search) case "$3" in absent-dep) exit 1 ;; *) printf '%s-1.0.0\n' "$3" ;; esac ;;
esac
exit 0
"#;

/// The same, except that `opt-app` is already on the device.
const APK_STUB_WITH_OPT_INSTALLED: &str = r#"#!/bin/sh
printf 'apk %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  --version) printf 'apk-tools 3.0.0\n' ;;
  --print-arch) printf 'x86_64\n' ;;
  info) case "$3" in opt-app) exit 0 ;; *) exit 1 ;; esac ;;
  search) printf '%s-1.0.0\n' "$3" ;;
esac
exit 0
"#;

/// The same, plus the answers a bound check needs: `apk info --installed` says
/// `opt-app` is on the box, and `apk list --installed` says which version.
const APK_STUB_WITH_OPT_VERSION: &str = r#"#!/bin/sh
printf 'apk %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  --version) printf 'apk-tools 3.0.0\n' ;;
  --print-arch) printf 'x86_64\n' ;;
  info) case "$3" in opt-app) exit 0 ;; *) exit 1 ;; esac ;;
  list) printf '%s-@INSTALLED@\n' "$4" ;;
  search) printf '%s-1.0.0\n' "$3" ;;
esac
exit 0
"#;

/// An `apk` whose feed offers `opt-app` in two versions, with the sizes each
/// one would cost. `1.10` is newer than `1.9`, so its figures are the ones the
/// install should carry — a string comparison would pick `1.9` and understate
/// both numbers.
const APK_STUB_WITH_SIZES: &str = r#"#!/bin/sh
printf 'apk %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  --version) printf 'apk-tools 3.0.0\n' ;;
  --print-arch) printf 'x86_64\n' ;;
  info) exit 1 ;;
  query) printf '[{"name":"opt-app","version":"1.9.0-r1","installed-size":1024,"file-size":512},
                  {"name":"opt-app","version":"1.10.0-r1","installed-size":40960,"file-size":8192}]\n' ;;
  search) printf '%s-1.0.0\n' "$3" ;;
esac
exit 0
"#;

/// The `precheck.optional` entries of an install report, as `(name, outcome)`.
fn optional_outcomes(sandbox: &Sandbox, pkgname: &str) -> Vec<(String, String)> {
  optional_entries(sandbox, pkgname)
    .iter()
    .map(|entry| {
      (
        entry["name"].as_str().unwrap_or_default().to_string(),
        entry["outcome"].as_str().unwrap_or_default().to_string(),
      )
    })
    .collect()
}

/// The whole `precheck.optional` array of an install report.
fn optional_entries(sandbox: &Sandbox, pkgname: &str) -> Vec<serde_json::Value> {
  let raw = fs::read_to_string(sandbox.state_dir().join("install-report.json"))
    .expect("the install writes a report");
  let report: serde_json::Value = serde_json::from_str(&raw).expect("report is json");
  report["reports"][pkgname]["precheck"]["optional"]
    .as_array()
    .unwrap_or_else(|| panic!("no optional section for {pkgname}"))
    .clone()
}

/// One `precheck.optional` entry by package name.
fn optional_entry(sandbox: &Sandbox, pkgname: &str, name: &str) -> serde_json::Value {
  optional_entries(sandbox, pkgname)
    .into_iter()
    .find(|entry| entry["name"].as_str() == Some(name))
    .unwrap_or_else(|| panic!("no optional entry for {name}"))
}

#[test]
fn off_a_terminal_the_optional_dependencies_are_skipped_and_said_so() {
  // Nobody can be asked, and reading stdin would block a script or a cron job.
  // The default is therefore "install none", and it must not be silent: the
  // user has to learn that `--with-optional` exists.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_ABSENT);
  let artifact = sandbox.container_artifact(
    "opt-apk",
    "apk",
    "base = base-dep\napp = app-dep\napp = opt-app?\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  let assert = sandbox.run_off_terminal(&["-U", artifact, "--feed-policy", "feed-first"]).success();

  let calls = sandbox.calls();
  assert_eq!(
    apk_add_targets(&calls),
    vec![vec!["base-dep".to_string()], vec!["app-dep".to_string()]],
    "only the required dependencies are installed: {calls}"
  );

  let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
  assert!(stderr.contains("stdin is not a terminal"), "the skip is explained: {stderr}");
  assert!(stderr.contains("--with-optional"), "and the way out is named: {stderr}");

  let stdout = stdout_of(&assert);
  assert!(
    !stdout.contains("Optional dependencies"),
    "the selector never reaches stdout, where it would corrupt `--json`: {stdout}"
  );

  assert_eq!(
    optional_outcomes(&sandbox, "opt-apk"),
    vec![("opt-app".to_string(), "declined".to_string())],
    "a skipped optional dependency is recorded as declined, not as installed"
  );
}

#[test]
fn with_optional_installs_the_optional_dependency_in_its_phase() {
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_ABSENT);
  let artifact = sandbox.container_artifact(
    "opt-apk",
    "apk",
    "base = base-dep\napp = app-dep\napp = opt-app?\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "feed-first", "--with-optional"]).success();

  let calls = sandbox.calls();
  assert_eq!(
    apk_add_targets(&calls),
    vec![vec!["base-dep".to_string()], vec!["app-dep".to_string(), "opt-app".to_string()],],
    "the chosen optional dependency joins its own phase, not a third request: {calls}"
  );
  assert_eq!(
    optional_outcomes(&sandbox, "opt-apk"),
    vec![("opt-app".to_string(), "installed".to_string())]
  );
}

#[test]
fn the_newest_offer_supplies_the_sizes_the_report_keeps() {
  // The two figures answer different questions — bytes over the air and bytes
  // on the device — and come from the version `apk add` would actually pick.
  // Reporting `1.9`'s numbers for a `1.10` install would understate both.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_SIZES);
  let artifact = sandbox.container_artifact(
    "opt-apk",
    "apk",
    "base = base-dep\napp = opt-app?\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "feed-first", "--with-optional"]).success();

  let entry = optional_entry(&sandbox, "opt-apk", "opt-app");
  assert_eq!(entry["outcome"], "installed");
  assert_eq!(entry["download_size"], 8192, "the package file: {entry}");
  assert_eq!(entry["installed_size"], 40960, "what it unpacks to: {entry}");
}

#[test]
fn no_optional_is_the_same_choice_without_the_explanation() {
  // The flag exists for scripts that would otherwise see the "stdin is not a
  // terminal" note on every run.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_ABSENT);
  let artifact = sandbox.container_artifact(
    "opt-apk",
    "apk",
    "app = app-dep\napp = opt-app?\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  let assert = sandbox
    .run_off_terminal(&["-U", artifact, "--feed-policy", "feed-first", "--no-optional"])
    .success();

  let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
  assert!(!stderr.contains("stdin is not a terminal"), "the choice was explicit: {stderr}");

  let calls = sandbox.calls();
  assert_eq!(
    apk_add_targets(&calls),
    vec![vec!["app-dep".to_string()]],
    "nothing optional is installed: {calls}"
  );
}

#[test]
fn the_two_optional_flags_cannot_both_be_given() {
  // Guessing which one wins would silently install packages the user said not
  // to install, or skip ones they asked for.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.apk_artifact("opt-apk");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox
    .run(&["-U", artifact, "--with-optional", "--no-optional"])
    .failure()
    .stderr(contains("mutually exclusive"));

  assert_eq!(sandbox.calls(), "", "the argument error stops the install first");
}

#[test]
fn an_unavailable_optional_dependency_is_skipped_with_a_warning() {
  // The feeds are the source here, and they do not have `absent-dep`. The
  // install proceeds — the manifest said it may — but the report must not claim
  // the package was declined: nobody was asked.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_ABSENT);
  let artifact = sandbox.container_artifact(
    "opt-apk",
    "apk",
    "app = app-dep\napp = absent-dep?\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  let assert =
    sandbox.run(&["-U", artifact, "--feed-policy", "feed-first", "--with-optional"]).success();

  let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
  assert!(
    stderr.contains("optional dependencies skipped (not available from the feeds): absent-dep"),
    "the warning names the package and the reason: {stderr}"
  );

  let calls = sandbox.calls();
  assert_eq!(
    apk_add_targets(&calls),
    vec![vec!["app-dep".to_string()]],
    "the absent optional dependency is not asked for: {calls}"
  );
  assert_eq!(
    optional_outcomes(&sandbox, "opt-apk"),
    vec![("absent-dep".to_string(), "unavailable".to_string())]
  );
}

#[test]
fn an_already_installed_optional_dependency_is_not_installed_again() {
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_OPT_INSTALLED);
  let artifact = sandbox.container_artifact(
    "opt-apk",
    "apk",
    "app = app-dep\napp = opt-app?\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "feed-first", "--with-optional"]).success();

  let calls = sandbox.calls();
  assert_eq!(
    apk_add_targets(&calls),
    vec![vec!["app-dep".to_string()]],
    "`--with-optional` means `all`, not `all again`: {calls}"
  );
  assert_eq!(
    optional_outcomes(&sandbox, "opt-apk"),
    vec![("opt-app".to_string(), "installed_before".to_string())]
  );
}

#[test]
fn an_installed_optional_dependency_short_of_its_bound_says_so() {
  // The version on the box is 1.0 and the artifact asks for >=2.0. The package
  // stays installed and untouched, but `installed_before` alone would be a
  // claim that the declaration held.
  let sandbox = Sandbox::with_stubs(
    EMPTY_DB,
    OPKG_STUB,
    &APK_STUB_WITH_OPT_VERSION.replace("@INSTALLED@", "1.0.0"),
  );
  let artifact =
    sandbox.container_artifact("opt-apk", "apk", "app = opt-app>=2.0?\n", &["demo-app.apk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "feed-first"]).success();

  let entry = optional_entry(&sandbox, "opt-apk", "opt-app");
  assert_eq!(entry["outcome"], "installed_before", "{entry}");
  assert_eq!(entry["constraint"], ">=2.0", "{entry}");
  assert_eq!(
    entry["reason"], "the installed version does not satisfy the declared constraint",
    "{entry}"
  );
  assert!(
    !apk_add_targets(&sandbox.calls()).iter().flatten().any(|target| target == "opt-app"),
    "nothing is installed over the one on the box: {}",
    sandbox.calls()
  );
}

#[test]
fn an_installed_optional_dependency_that_meets_its_bound_gains_no_reason() {
  let sandbox = Sandbox::with_stubs(
    EMPTY_DB,
    OPKG_STUB,
    &APK_STUB_WITH_OPT_VERSION.replace("@INSTALLED@", "2.1.0"),
  );
  let artifact =
    sandbox.container_artifact("opt-apk", "apk", "app = opt-app>=2.0?\n", &["demo-app.apk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "feed-first"]).success();

  let entry = optional_entry(&sandbox, "opt-apk", "opt-app");
  assert_eq!(entry["outcome"], "installed_before", "{entry}");
  assert_eq!(entry["constraint"], ">=2.0", "{entry}");
  assert!(entry.get("reason").is_none(), "{entry}");
}

#[test]
fn an_optional_dependency_the_containers_will_supply_is_not_a_feed_install() {
  // `packaged-first` takes the artifact's own containers, so a feed package
  // cannot be installed in the same run: apk would resolve it outside `pkgs/`
  // while the batch installs a second copy. The optional dependency is shown
  // as unavailable rather than silently turned into a feed install.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_ABSENT);
  let artifact =
    sandbox.container_artifact("opt-apk", "apk", "app = opt-app?\n", &["demo-app.apk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  let assert =
    sandbox.run(&["-U", artifact, "--feed-policy", "packaged-first", "--with-optional"]).success();

  let calls = sandbox.calls();
  let adds = apk_add_targets(&calls);
  assert_eq!(adds.len(), 1, "the batch is the only request: {calls}");
  assert_eq!(file_names(&adds[0]), vec!["demo-app.apk".to_string()], "{calls}");

  let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
  assert!(
    stderr.contains(
      "optional dependencies skipped (this artifact installs its dependencies from pkgs/)"
    ),
    "the reason is the source decision, not the feeds: {stderr}"
  );
}

#[test]
fn an_optional_kmod_goes_through_the_feeds_even_when_the_containers_supply_the_rest() {
  // An application artifact cannot ship a kernel module, so the containers
  // decision does not apply: a kmod is offered and installed from the feeds.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_ABSENT);
  let artifact =
    sandbox.container_artifact("kmod-holder", "apk", "kmod = opt-kmod?\n", &["demo.apk"]);
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox.run(&["-U", artifact, "--feed-policy", "packaged-first", "--with-optional"]).success();

  let calls = sandbox.calls();
  let adds = apk_add_targets(&calls);
  assert_eq!(adds[0], vec!["opt-kmod".to_string()], "the kmod is one request of its own: {calls}");
  assert!(
    adds.iter().any(|targets| file_names(targets) == vec!["demo.apk".to_string()]),
    "the container batch still installs: {calls}"
  );
}

#[test]
fn a_required_dependency_that_is_missing_still_stops_the_install() {
  // The whole point of `?` is that the same absence is fatal without it. This is
  // the guard against the optional path accidentally loosening the required one.
  let sandbox = Sandbox::with_stubs(EMPTY_DB, OPKG_STUB, APK_STUB_WITH_ABSENT);
  let artifact = sandbox.container_artifact(
    "opt-apk",
    "apk",
    "app = absent-dep\napp = opt-app?\n",
    &["demo-app.apk"],
  );
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  sandbox
    .run(&["-U", artifact, "--feed-policy", "feed-first", "--with-optional"])
    .failure()
    .stderr(contains("dependency precheck failed, missing: app:absent-dep"));

  assert!(
    !sandbox.calls().contains("apk add"),
    "the precheck stopped before anything was installed: {}",
    sandbox.calls()
  );
}

#[test]
fn a_pica_artifact_may_declare_optional_dependencies_it_cannot_install() {
  // `pkgmgr=pica` has no resolver, so a required declaration is refused (the
  // package would silently lose something it promised). An optional dependency
  // is the one case where the honest answer is a warning: the install is
  // complete without it.
  let sandbox = Sandbox::new(EMPTY_DB);
  let artifact = sandbox.root_only_artifact("opt-pica", "pica", "app = opt-app?\n");
  let artifact = artifact.to_str().expect("artifact path is utf-8");

  let assert = sandbox.run(&["-U", artifact, "--with-optional"]).success();

  let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
  assert!(
    stderr.contains("pkgmgr=pica cannot install optional dependencies; skipping opt-app"),
    "{stderr}"
  );
  assert_eq!(sandbox.calls(), "", "no package manager may be driven");
  assert_eq!(
    optional_outcomes(&sandbox, "opt-pica"),
    vec![("opt-app".to_string(), "unavailable".to_string())]
  );
}
