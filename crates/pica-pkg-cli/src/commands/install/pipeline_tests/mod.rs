//! Tests of the install pipeline, grouped by step. Fixtures live here.

mod backup;
mod guard;
mod hooks;
mod precheck;
mod rollback;
mod transaction;

use super::backup::{BackupEntry, BackupKind, PayloadBackup};
use super::deps::{dependency_source_lines, ensure_index, install_phase};
use super::optional::{self};
use super::payload::{copy_recording_new_target, deploy_payload};
use super::pipeline::{
  IndexSizes, InstallContext, ScriptConfig, install_summary, pkgfile_into, size_lines,
  transaction_header,
};
use super::precheck::{DeclaredDeps, build_precheck_report};
use super::transaction::InstallTransaction;
use crate::app::{App, CliError, CliResult, E_CONFIG_INVALID, E_MANIFEST_INVALID, E_RUNTIME};
use crate::app::{
  E_INDEX_UNAVAILABLE, E_VERSION_INCOMPATIBLE, FeedPolicy, JsonMode, OptionalDeps, Options, Paths,
};
use crate::i18n::Lang;
use crate::installed::EntrySource;
use crate::pkgmgr::FeedCandidate;
use crate::pkgmgr::PackageManagerKind;
use crate::system::TargetRoot;
use pica_pkg_core::constraint::Constraint;
use pica_pkg_core::io::{PayloadInstall, remove_payload};
use pica_pkg_core::layout::{MANIFEST, PKGS, ROOT};
use pica_pkg_core::manifest::Manifest;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

const MANIFEST_DEMO: &str = "\
pkgname = demo
appname = demo
pkgver = 1.0.0
pkgrel = 1
platform = all
os = all
arch = all
";

/// An app whose state lives under `root`, with the `pica` backend forced so a
/// test does not depend on which package managers the host happens to have.
fn test_app(root: &Path) -> App {
  let etc = root.join("etc");
  let state = root.join("state");
  let paths = Paths {
    etc_dir: etc.clone(),
    conf_file: etc.join("pica.json"),
    state_dir: state.clone(),
    db_file: state.join("db.json"),
    index_file: state.join("index.json"),
    cache_dir: state.join("cache"),
    repos_cache_dir: state.join("cache").join("repos"),
    report_file: state.join("install-report.json"),
    lock_file: state.join("db.lck"),
  };
  let options = Options {
    json_mode: JsonMode::None,
    non_interactive: true,
    feed_policy: FeedPolicy::PackagedOnly,
    fetch_timeout: 1,
    fetch_retry: 0,
    fetch_retry_delay: 0,
    pkgmgr: None,
    optional_deps: OptionalDeps::None,
    lang: None,
    dry_run: false,
    noconfirm: false,
  };

  let mut app = App::new(paths, options);
  app.with_backend_for_tests(PackageManagerKind::Pica);
  app
}

/// A backend that answers the questions the precheck asks, without a filesystem:
/// whether the index can be made readable, which names and versions the feeds
/// list, and which are installed. `readable: false` is what unreachable feeds
/// look like from the precheck's side; anything not put in `offers`/`installed`
/// simply is not there.
struct IndexStub {
  readable: bool,
  /// `(name, version)` the feeds list, newest last — `feed_candidates` hands
  /// back every entry for the name, as the real backends do.
  offers: Vec<(&'static str, &'static str)>,
  /// `(name, version)` already on the device.
  installed: Vec<(&'static str, &'static str)>,
}

impl IndexStub {
  fn new(readable: bool) -> Self {
    Self { readable, offers: Vec::new(), installed: Vec::new() }
  }

  /// The feeds list `name` at `version`.
  fn offering(mut self, name: &'static str, version: &'static str) -> Self {
    self.offers.push((name, version));
    self
  }

  /// The device already has `name` at `version`.
  fn with_installed(mut self, name: &'static str, version: &'static str) -> Self {
    self.installed.push((name, version));
    self
  }
}

impl crate::pkgmgr::PackageManager for IndexStub {
  fn kind(&self) -> PackageManagerKind {
    PackageManagerKind::Apk
  }

  fn update_index(&self) -> CliResult<()> {
    if self.readable {
      return Ok(());
    }
    Err(CliError::new(E_INDEX_UNAVAILABLE, "apk update failed (exit 1): Network unreachable"))
  }

  fn package_exists(&self, name: &str) -> CliResult<bool> {
    Ok(self.offers.iter().any(|(offered, _)| *offered == name))
  }

  fn feed_candidates(&self, name: &str) -> CliResult<Vec<FeedCandidate>> {
    Ok(
      self
        .offers
        .iter()
        .filter(|(offered, _)| *offered == name)
        .map(|(_, version)| FeedCandidate {
          version: (*version).to_string(),
          download: None,
          installed: None,
        })
        .collect(),
    )
  }

  fn is_installed(&self, name: &str) -> bool {
    self.installed.iter().any(|(installed, _)| *installed == name)
  }

  fn installed_version(&self, name: &str) -> Option<String> {
    self
      .installed
      .iter()
      .find(|(installed, _)| *installed == name)
      .map(|(_, version)| (*version).to_string())
  }

  fn install(&self, _label: &str, _targets: &[String]) -> CliResult<()> {
    Ok(())
  }

  fn remove(&self, _target: &str) -> CliResult<()> {
    Ok(())
  }

  fn architectures(&self) -> Vec<String> {
    Vec::new()
  }

  fn installed(&self) -> CliResult<Vec<(String, String)>> {
    Ok(Vec::new())
  }

  fn depends(&self, _name: &str) -> Vec<String> {
    Vec::new()
  }

  fn owns_file(&self, _path: &str) -> Option<String> {
    None
  }
}

fn declarations(kmod: &[&str], base: &[&str], app: &[&str]) -> DeclaredDeps {
  let parse = |items: &[&str]| {
    items.iter().map(|item| Constraint::parse(item).expect("valid declaration")).collect()
  };
  DeclaredDeps { kmod: parse(kmod), base: parse(base), app: parse(app) }
}

fn app_with_index(work: &Path, readable: bool) -> App {
  let mut app = test_app(work);
  app.use_manager(Box::new(IndexStub::new(readable)));
  app
}

fn write_file(path: &Path, contents: &str) {
  fs::create_dir_all(path.parent().expect("path has a parent")).expect("mkdir");
  fs::write(path, contents).expect("write file");
}

fn run_tool(program: &str, args: &[&str]) {
  let status = Command::new(program).args(args).status().expect("run tool");
  assert!(status.success(), "{program} {args:?} failed");
}

fn pack(work: &Path, name: &str, staging: &Path) {
  let artifact = work.join(name);
  run_tool(
    "tar",
    &["-czf", &artifact.display().to_string(), "-C", &staging.display().to_string(), "."],
  );
  assert!(artifact.is_file(), "{}", artifact.display());
}

/// A minimal but well-formed `.ipk` whose payload installs `files`.
fn build_ipk(dir: &Path, pkgname: &str, files: &[(&str, &str)]) -> PathBuf {
  let data_root = dir.join(format!("{pkgname}-data"));
  for (relative, contents) in files {
    write_file(&data_root.join(relative), contents);
  }
  let data_tar = dir.join(format!("{pkgname}-data.tar.gz"));
  run_tool(
    "tar",
    &["-czf", &data_tar.display().to_string(), "-C", &data_root.display().to_string(), "."],
  );

  let outer = dir.join(format!("{pkgname}-outer"));
  write_file(&outer.join("debian-binary"), "2.0\n");
  write_file(
    &outer.join("control"),
    &format!("Package: {pkgname}\nVersion: 1.0.0\nArchitecture: all\n"),
  );
  fs::copy(&data_tar, outer.join("data.tar.gz")).expect("copy data member");

  pack(dir, &format!("{pkgname}.ipk"), &outer);
  dir.join(format!("{pkgname}.ipk"))
}

fn db_record(app: &App, pkgname: &str) -> Option<Value> {
  let text = fs::read_to_string(&app.paths.db_file).ok()?;
  let db: Value = serde_json::from_str(&text).ok()?;
  db.get("installed")?.get(pkgname).cloned()
}

/// Builds an artifact and installs it into `target`.
///
/// The pkgdir is assembled here rather than through `pica-pack` so a test can
/// describe exactly the pkgdir that triggers the behaviour it is checking.
#[allow(clippy::too_many_arguments)]
fn install_artifact(
  work: &Path,
  app: &mut App,
  target: &TargetRoot,
  manifest: &str,
  root_files: &[(&str, &str)],
  root_symlinks: &[(&str, &str)],
  containers: &[(&str, PathBuf)],
  extra_files: &[(&str, &str)],
) -> CliResult<()> {
  let staging = work.join("staging");
  // Each call builds the artifact it describes. Leftovers from an earlier
  // call would ship files this one does not have — which is exactly what an
  // upgrade test drops on purpose.
  let _ = fs::remove_dir_all(&staging);
  write_file(&staging.join(MANIFEST), manifest);
  for (relative, contents) in root_files {
    write_file(&staging.join(ROOT).join(relative), contents);
  }
  for (relative, link_target) in root_symlinks {
    let path = staging.join(ROOT).join(relative);
    fs::create_dir_all(path.parent().expect("path has a parent")).expect("mkdir");
    symlink(link_target, &path).expect("create symlink");
  }
  for (name, source) in containers {
    let destination = staging.join(PKGS).join(name);
    fs::create_dir_all(destination.parent().expect("path has a parent")).expect("mkdir");
    fs::copy(source, &destination).expect("copy container");
  }
  for (relative, contents) in extra_files {
    write_file(&staging.join(relative), contents);
  }

  let artifact = work.join("demo.pkg.tar.gz");
  pack(work, "demo.pkg.tar.gz", &staging);
  pkgfile_into(app, &artifact, None, EntrySource::Repo, IndexSizes::default(), target)
}

/// A fixture that installs a `demo-lib` container plus a `root/` overlay.
///
/// The scratch directory is kept as a path rather than fetched through a
/// method, so a test can borrow it and the app at the same time.
struct PicaFixture {
  _work: tempfile::TempDir,
  work_path: PathBuf,
  target: TargetRoot,
  app: App,
  container: PathBuf,
}

impl PicaFixture {
  /// A fixture whose artifact carries a `demo-lib` container and a `root/`
  /// overlay, for the backends that still hand containers to a system package
  /// manager.
  fn new() -> Self {
    let work = tempfile::tempdir().expect("temp dir");
    let work_path = work.path().to_path_buf();
    let target = TargetRoot::new(work_path.join("target-root"));
    let app = test_app(&work_path);

    let containers = work_path.join("containers");
    fs::create_dir_all(&containers).expect("mkdir containers");
    let container = build_ipk(
      &containers,
      "demo-lib",
      &[("usr/lib/demo/helper", "payload\n"), ("etc/demo.conf", "dist\n")],
    );

    Self { _work: work, work_path, target, app, container }
  }

  fn install(
    &mut self,
    manifest: &str,
    root_files: &[(&str, &str)],
    root_symlinks: &[(&str, &str)],
    extra_files: &[(&str, &str)],
  ) -> CliResult<()> {
    install_artifact(
      &self.work_path,
      &mut self.app,
      &self.target,
      manifest,
      root_files,
      root_symlinks,
      &[("demo-lib.ipk", self.container.clone())],
      extra_files,
    )
  }

  /// The same install without a `pkgs/` entry.
  ///
  /// A `pica`-backend artifact ships only `root/` and `scripts/` — the packer
  /// refuses containers — so a pica install never has one to lay down.
  /// Rollback tests want the payload path, not the container path.
  fn install_without_containers(
    &mut self,
    manifest: &str,
    root_files: &[(&str, &str)],
    root_symlinks: &[(&str, &str)],
    extra_files: &[(&str, &str)],
  ) -> CliResult<()> {
    install_artifact(
      &self.work_path,
      &mut self.app,
      &self.target,
      manifest,
      root_files,
      root_symlinks,
      &[],
      extra_files,
    )
  }

  fn path(&self, relative: &str) -> PathBuf {
    self.target.as_path().join(relative)
  }
}

/// Points `App`'s state paths at `<target>/var/lib/pica` and leaves a database
/// there, the way an installed device has one.
fn state_with_a_database(fixture: &mut PicaFixture) -> (PathBuf, PathBuf) {
  let state = fixture.target.as_path().join("var/lib/pica");
  fs::create_dir_all(&state).expect("mkdir state");
  let database = state.join("db.json");
  fs::write(&database, "{\"schema\":1,\"installed\":{}}\n").expect("existing database");
  fixture.app.paths.db_file = database.clone();
  fixture.app.paths.state_dir = state.clone();
  fixture.app.paths.index_file = state.join("index.json");
  fixture.app.paths.report_file = state.join("install-report.json");
  fixture.app.paths.lock_file = state.join("db.lck");
  (state, database)
}

/// Installs a `pica` artifact with one `root/` file and, when asked, an env
/// file — the two steps `deploy_payload` runs in that order.
fn deploy_with_env(fixture: &mut PicaFixture, with_env: bool) -> CliResult<()> {
  let mut extra: Vec<(&str, &str)> = Vec::new();
  if with_env {
    extra.push(("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n"));
  }
  fixture.install_without_containers(
    &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
    &[("etc/demo.conf", "payload\n")],
    &[],
    &extra,
  )
}

/// A step-7 deploy and its undo, without the steps around them.
///
/// Everything above reaches `deploy_payload` through a real artifact, which is
/// what an install actually does. These two functions are where the guarantee
/// about a file an install *replaced* lives, and driving them directly is what
/// lets a test change the filesystem between the deploy and its undo — the
/// moment the copies are the only version of a file that is left.
struct Deployed {
  fixture: PicaFixture,
  target: TargetRoot,
  staging: PathBuf,
  manifest: Manifest,
  scripts: ScriptConfig,
  tx: InstallTransaction,
}

impl Deployed {
  /// Stages the unpacked tree step 7 reads: `root_files` under `root/`,
  /// `extra_files` at their own paths (so a test can ship `scripts/.env`), and
  /// deploys them.
  fn new(
    fixture: PicaFixture,
    root_files: &[(&str, &str)],
    extra_files: &[(&str, &str)],
  ) -> CliResult<Self> {
    let target = fixture.target.clone();
    let staging = fixture.work_path.join("deployed-staging");
    let _ = fs::remove_dir_all(&staging);
    let text = format!("{MANIFEST_DEMO}pkgmgr = pica\n");
    write_file(&staging.join(MANIFEST), &text);
    for (relative, contents) in root_files {
      write_file(&staging.join(ROOT).join(relative), contents);
    }
    for (relative, contents) in extra_files {
      write_file(&staging.join(relative), contents);
    }

    let mut out = Self {
      fixture,
      target,
      staging,
      manifest: Manifest::from_text(&text).expect("a well-formed manifest"),
      scripts: ScriptConfig {
        install: String::new(),
        update: String::new(),
        remove: String::new(),
        keep_all: false,
        keep_install: false,
        keep_update: false,
        keep_remove: false,
      },
      tx: InstallTransaction::new(),
    };
    out.deploy()?;
    Ok(out)
  }

  /// Runs step 7: the copies are taken, then the payload replaces what was
  /// there. A deploy that is *expected* to fail returns the error instead of
  /// the transaction, because then the rollback has nothing of this install's
  /// to undo.
  fn deploy(&mut self) -> CliResult<()> {
    let ctx = InstallContext {
      target: &self.target,
      tmpdir: &self.staging,
      pkgfile: &self.staging,
      pkgname: "demo",
      selector: "demo",
      pkgmgr: "pica",
      manifest: &self.manifest,
      scripts: &self.scripts,
      is_upgrade: false,
      source: EntrySource::Repo,
    };
    deploy_payload(&self.fixture.app, &ctx, &mut self.tx)
  }

  fn path(&self, relative: &str) -> PathBuf {
    self.fixture.path(relative)
  }

  /// Removes the directory `relative` lives in and plants a regular file at
  /// its name. That makes a restore fail for a reason the filesystem
  /// guarantees rather than one a test fakes: the copy is still there, the
  /// destination's directory is not.
  fn make_the_restore_fail(&mut self, relative: &str) {
    let parent = self.path(relative).parent().expect("a path with a parent").to_path_buf();
    fs::remove_dir_all(&parent).expect("remove the directory");
    fs::write(&parent, "not a directory\n").expect("plant a file in its place");
  }

  /// Runs the rollback and reports what it could not undo.
  fn rollback(&mut self) -> Vec<String> {
    let ctx = InstallContext {
      target: &self.target,
      tmpdir: &self.staging,
      pkgfile: &self.staging,
      pkgname: "demo",
      selector: "demo",
      pkgmgr: "pica",
      manifest: &self.manifest,
      scripts: &self.scripts,
      is_upgrade: false,
      source: EntrySource::Repo,
    };
    self.tx.rollback(&mut self.fixture.app, &ctx)
  }
}

/// The store directories under `<cache>/backup` that a transaction kept.
fn backup_stores(fixture: &PicaFixture) -> Vec<PathBuf> {
  let Ok(entries) = fs::read_dir(fixture.app.paths.cache_dir.join("backup")) else {
    return Vec::new();
  };
  let mut stores: Vec<PathBuf> =
    entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect();
  stores.sort();
  stores
}

/// A system backend whose installed list stops answering on demand.
///
/// The transaction builds its rollback record by diffing this list. A backend
/// that cannot answer must not be read as an empty system: that is exactly
/// how a confirmed addition would be forgotten.
struct ListingStub {
  installed: std::rc::Rc<std::cell::RefCell<Vec<(String, String)>>>,
  reads: std::rc::Rc<std::cell::RefCell<usize>>,
  fail_from_call: usize,
  fail_install_label: &'static str,
}

impl crate::pkgmgr::PackageManager for ListingStub {
  fn kind(&self) -> PackageManagerKind {
    PackageManagerKind::Opkg
  }

  fn update_index(&self) -> CliResult<()> {
    Ok(())
  }

  fn package_exists(&self, _name: &str) -> CliResult<bool> {
    Ok(true)
  }

  fn feed_candidates(&self, _name: &str) -> CliResult<Vec<FeedCandidate>> {
    Ok(Vec::new())
  }

  fn is_installed(&self, name: &str) -> bool {
    self.installed.borrow().iter().any(|(known, _)| known == name)
  }

  fn installed_version(&self, name: &str) -> Option<String> {
    self
      .installed
      .borrow()
      .iter()
      .find(|(known, _)| known == name)
      .map(|(_, version)| version.clone())
  }

  fn install(&self, label: &str, targets: &[String]) -> CliResult<()> {
    // The request is applied first: a backend that then fails has still
    // installed what it was asked for.
    for name in targets {
      self.installed.borrow_mut().push((name.clone(), "1.0.0".to_string()));
    }
    if label == self.fail_install_label {
      return Err(CliError::new(E_RUNTIME, "simulated install failure"));
    }
    Ok(())
  }

  fn remove(&self, target: &str) -> CliResult<()> {
    self.installed.borrow_mut().retain(|(name, _)| name != target);
    Ok(())
  }

  fn architectures(&self) -> Vec<String> {
    Vec::new()
  }

  fn installed(&self) -> CliResult<Vec<(String, String)>> {
    let mut reads = self.reads.borrow_mut();
    *reads += 1;
    if *reads > self.fail_from_call {
      return Err(CliError::new(E_RUNTIME, "simulated listing failure"));
    }
    Ok(self.installed.borrow().clone())
  }

  fn depends(&self, _name: &str) -> Vec<String> {
    Vec::new()
  }

  fn owns_file(&self, _path: &str) -> Option<String> {
    None
  }
}
