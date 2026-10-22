//! How a software source is addressed and read.
//!
//! A source used to be one directory with a `repo.json` in it. It can now be a
//! root that serves an `index.json` descriptor naming one index per backend.
//! These tests drive the real CLI against `file://` sources, so nothing here
//! depends on a network or on which package managers the host has.

#![cfg(unix)]

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const OPKG_STUB: &str = r#"#!/bin/sh
printf 'opkg %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  print-architecture) printf 'all 100\n' ;;
esac
exit 0
"#;

const APK_STUB: &str = r#"#!/bin/sh
printf 'apk %s\n' "$*" >> "$PICA_STUB_LOG"
case "$1" in
  --version) printf 'apk-tools 3.0.0\n' ;;
  --print-arch) printf 'x86_64\n' ;;
  search) printf '%s-1.0.0\n' "$3" ;;
esac
exit 0
"#;

struct Sandbox {
  dir: tempfile::TempDir,
}

impl Sandbox {
  fn new() -> Self {
    let dir = tempfile::tempdir().expect("create temp dir");
    fs::create_dir_all(dir.path().join("state")).expect("create state dir");
    fs::create_dir_all(dir.path().join("etc")).expect("create etc dir");
    fs::write(dir.path().join("state/db.json"), r#"{"installed": {}}"#).expect("write db");

    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).expect("create stub dir");
    stub(&bin.join("opkg"), OPKG_STUB);
    stub(&bin.join("apk"), APK_STUB);

    Self { dir }
  }

  /// The directory a source lives in, served over `file://`.
  fn source_dir(&self) -> PathBuf {
    let root = self.dir.path().join("source");
    fs::create_dir_all(&root).expect("create source dir");
    root
  }

  fn source_url(&self) -> String {
    format!("file://{}", self.source_dir().display())
  }

  fn write(&self, relative: &str, content: &str) {
    let path = self.source_dir().join(relative);
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, content).expect("write source file");
  }

  /// `repos` becomes the configured list verbatim.
  fn write_conf(&self, repos: &Value) {
    let conf = json!({ "repos": repos });
    fs::write(self.dir.path().join("etc/pica.json"), conf.to_string()).expect("write pica.json");
  }

  fn run(&self, args: &[&str]) -> Assert {
    let mut cmd: Command = assert_cmd::cargo_bin_cmd!("pica");
    cmd.env("PICA_ETC_DIR", self.dir.path().join("etc"));
    cmd.env("PICA_STATE_DIR", self.dir.path().join("state"));
    cmd.env("PICA_STUB_LOG", self.dir.path().join("state/calls.log"));
    cmd.env("PATH", self.search_path());
    cmd.args(args);
    cmd.assert()
  }

  fn index(&self) -> Value {
    let raw = fs::read_to_string(self.dir.path().join("state/index.json")).expect("read index");
    serde_json::from_str(&raw).expect("parse index")
  }

  fn search_path(&self) -> std::ffi::OsString {
    let mut dirs = vec![self.dir.path().join("bin")];
    dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    std::env::join_paths(dirs).expect("join PATH")
  }

  /// A pica artifact whose payload is `root/tmp/<pkgname>.conf`.
  fn artifact(&self, pkgname: &str, version: &str) -> PathBuf {
    let src = self.dir.path().join(format!("src-{pkgname}"));
    let root = src.join("root/tmp");
    fs::create_dir_all(&root).expect("create pkgdir");
    fs::write(
      src.join("manifest"),
      format!(
        "pkgname = {pkgname}\nappname = {pkgname}\npkgver = {version}\npkgrel = 1\n\
         pkgmgr = pica\nplatform = all\nos = all\narch = all\n"
      ),
    )
    .expect("write manifest");
    fs::write(root.join(format!("{pkgname}.conf")), b"payload\n").expect("write payload");

    let out = self.dir.path().join(format!("{pkgname}-{version}.pkg.tar.gz"));
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
}

fn stub(path: &Path, body: &str) {
  fs::write(path, body).expect("write stub");
  let mut perms = fs::metadata(path).expect("stat stub").permissions();
  perms.set_mode(0o755);
  fs::set_permissions(path, perms).expect("chmod stub");
}

fn sha256_of(path: &Path) -> String {
  pica_pkg_core::io::sha256_hex(&fs::read(path).expect("read artifact"))
}

/// One package entry. An empty `download_url` means the field is not published
/// at all, which is what a source that relies on the `/packages/` fallback does.
fn package(pkgname: &str, version: &str, pkgmgr: &str, download_url: &str) -> Value {
  let mut pkg = json!({
    "pkgname": pkgname,
    "appname": pkgname,
    "pkgver": version,
    "pkgrel": "1",
    "pkgmgr": pkgmgr,
    "platform": "all",
    "arch": "all",
    "os": "all",
    "filename": format!("{pkgname}-{version}-1-all-{pkgmgr}.pkg.tar.gz"),
    "sha256": "0".repeat(64),
  });
  if !download_url.is_empty() {
    pkg["download_url"] = json!(download_url);
  }
  pkg
}

fn descriptor(managers: &Value) -> String {
  json!({ "schema": 1, "name": "sandbox", "description": "tests", "managers": managers })
    .to_string()
}

fn backend_index(mgr: &str, packages: &[Value]) -> String {
  json!({ "schema": 1, "mgr": mgr, "generated_at": "2026-10-20T00:00:00Z", "packages": packages })
    .to_string()
}

#[test]
fn a_schema_2_source_is_still_read_as_one_repo_json() {
  let sandbox = Sandbox::new();
  sandbox.write(
    "repo.json",
    &json!({
      "schema": 2,
      "packages": [package("legacy-demo", "1.0.0", "pica", "file:///tmp/legacy-demo.pkg.tar.gz")],
    })
    .to_string(),
  );
  sandbox.write_conf(&json!([{ "name": "main", "url": sandbox.source_url() }]));

  sandbox.run(&["-S"]).success();

  let repo = &sandbox.index()["repos"]["main"];
  assert_eq!(repo["kind"], "legacy", "{repo}");
  assert_eq!(repo["data"]["packages"][0]["pkgname"], "legacy-demo");

  sandbox.run(&["-Si", "legacy-demo"]).success().stdout(contains("legacy-demo"));
}

#[test]
fn an_entry_descriptor_source_reads_only_the_backend_asked_for() {
  let sandbox = Sandbox::new();
  sandbox.write(
    "index.json",
    &descriptor(&json!({
      "opkg": { "path": "opkg/index.json" },
      "pica": { "path": "pica/index.json" },
    })),
  );
  sandbox.write(
    "pica/index.json",
    &backend_index("pica", &[package("entry-demo", "1.0.0", "pica", "")]),
  );
  sandbox.write(
    "opkg/index.json",
    &backend_index("opkg", &[package("entry-demo", "9.0.0", "opkg", "")]),
  );
  sandbox.write_conf(&json!([{
    "name": "main",
    "url": sandbox.source_url(),
    "backends": ["pica"],
  }]));

  sandbox.run(&["-S"]).success();

  let repo = &sandbox.index()["repos"]["main"];
  assert_eq!(repo["kind"], "entry", "{repo}");
  assert!(repo["indexes"]["pica"].is_object(), "the selected backend was fetched: {repo}");
  assert!(repo["indexes"]["opkg"].is_null(), "an unselected backend is not fetched: {repo}");

  // The opkg copy of the same name is not visible to a pica-only combination.
  sandbox.run(&["-Si", "entry-demo"]).success().stdout(contains("1.0.0"));
}

#[test]
fn a_backend_the_source_does_not_provide_is_reported_not_substituted() {
  let sandbox = Sandbox::new();
  sandbox.write("index.json", &descriptor(&json!({ "pica": { "path": "pica/index.json" } })));
  sandbox.write(
    "pica/index.json",
    &backend_index("pica", &[package("entry-demo", "1.0.0", "pica", "")]),
  );
  sandbox.write_conf(&json!([{
    "name": "main",
    "url": sandbox.source_url(),
    "backends": ["apk", "pica"],
  }]));

  sandbox
    .run(&["-S"])
    .failure()
    .stderr(contains("does not provide backend(s) apk").and(contains("declares: pica")));
}

#[test]
fn a_manager_path_may_not_climb_out_of_the_source() {
  let sandbox = Sandbox::new();
  sandbox
    .write("index.json", &descriptor(&json!({ "pica": { "path": "../elsewhere/index.json" } })));
  sandbox.write_conf(&json!([{
    "name": "main",
    "url": sandbox.source_url(),
    "backends": ["pica"],
  }]));

  sandbox.run(&["-S"]).failure().stderr(contains("path").and(contains("..")));
}

#[test]
fn an_unknown_backend_in_the_configuration_is_refused() {
  let sandbox = Sandbox::new();
  sandbox.write("index.json", &descriptor(&json!({ "pica": { "path": "pica/index.json" } })));
  sandbox.write_conf(&json!([{
    "name": "main",
    "url": sandbox.source_url(),
    "backends": ["pacman"],
  }]));

  sandbox.run(&["-S"]).failure().stderr(contains("unsupported backend"));
}

#[test]
fn a_relative_download_url_is_resolved_against_the_source_root() {
  let sandbox = Sandbox::new();
  let artifact = sandbox.artifact("rel-demo", "1.0.0");
  let name = artifact.file_name().and_then(|n| n.to_str()).expect("filename").to_string();
  let target = sandbox.source_dir().join("packages").join(&name);
  fs::create_dir_all(sandbox.source_dir().join("packages")).expect("mkdir packages");
  fs::rename(&artifact, &target).expect("move artifact");

  // The index publishes a path relative to the source root; the client is what
  // turns it into an address, so a source can move between hosts unchanged.
  let mut pkg = package("rel-demo", "1.0.0", "pica", &format!("packages/{name}"));
  pkg["sha256"] = json!(sha256_of(&target));

  sandbox.write("index.json", &descriptor(&json!({ "pica": { "path": "pica/index.json" } })));
  sandbox.write("pica/index.json", &backend_index("pica", &[pkg]));
  sandbox.write_conf(&json!([{
    "name": "main",
    "url": sandbox.source_url(),
    "backends": ["pica"],
  }]));

  sandbox.run(&["-S"]).success();
  sandbox.run(&["-Sp", "rel-demo"]).success().stdout(contains("Installing rel-demo (1.0.0-1)"));
}

#[test]
fn a_package_nobody_publishes_says_which_backends_were_searched() {
  let sandbox = Sandbox::new();
  sandbox.write("index.json", &descriptor(&json!({ "pica": { "path": "pica/index.json" } })));
  sandbox.write(
    "pica/index.json",
    &backend_index("pica", &[package("entry-demo", "1.0.0", "pica", "")]),
  );
  sandbox.write_conf(&json!([{
    "name": "main",
    "url": sandbox.source_url(),
    "backends": ["pica"],
  }]));

  sandbox.run(&["-S"]).success();
  sandbox
    .run(&["-Si", "no-such-package"])
    .failure()
    .stderr(contains("package not found").and(contains("main provides pica")));
}

#[test]
fn a_hash_mismatch_in_the_new_layout_stops_the_install() {
  let sandbox = Sandbox::new();
  let artifact = sandbox.artifact("bad-demo", "1.0.0");
  let name = artifact.file_name().and_then(|n| n.to_str()).expect("filename").to_string();
  let target = sandbox.source_dir().join("packages").join(&name);
  fs::create_dir_all(sandbox.source_dir().join("packages")).expect("mkdir packages");
  fs::rename(&artifact, &target).expect("move artifact");

  // Right file, wrong digest: the only honest outcome is to refuse it.
  let mut pkg = package("bad-demo", "1.0.0", "pica", &format!("packages/{name}"));
  pkg["sha256"] = json!("0".repeat(64));

  sandbox.write("index.json", &descriptor(&json!({ "pica": { "path": "pica/index.json" } })));
  sandbox.write("pica/index.json", &backend_index("pica", &[pkg]));
  sandbox.write_conf(&json!([{
    "name": "main",
    "url": sandbox.source_url(),
    "backends": ["pica"],
  }]));

  sandbox.run(&["-S"]).success();
  sandbox.run(&["-Sp", "bad-demo"]).failure().stderr(contains("sha256").or(contains("integrity")));
}
