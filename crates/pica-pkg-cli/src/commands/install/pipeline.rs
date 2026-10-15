use super::feed::{install_via_feeds_or_pkgs, pkg_list_diff_added, reorder_app_list};
use super::precheck::{build_precheck_report, summarize_missing_precheck, validate_package};
use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_IO, E_MANIFEST_INVALID, E_PACKAGE_INVALID,
  E_RUNTIME, E_VERSION_INCOMPATIBLE, ensure_dirs, resolve_lang,
};
use crate::installed::EntrySource;
use crate::pkgmgr::PackageManagerKind;
use crate::state::{db_has_installed, db_set_installed, report_set_install_result};
use crate::system::{TargetRoot, run_hook, run_tar_extract};
use pica_pkg_core::PICA_VERSION;
use pica_pkg_core::io::make_temp_dir;
use pica_pkg_core::io::{PayloadInstall, install_payload, remove_payload};
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::version::{pkgver_cmp_key, ver_ge};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// Pkgdir layout inside a built artifact. See docs/standard.md.
const MANIFEST: &str = "manifest";
const SCRIPTS: &str = "scripts";
const ROOT: &str = "root";
const PKGS: &str = "pkgs";

/// Where kept lifecycle scripts and env files live on the target system.
const SCRIPTS_DIR: &str = "/usr/lib/pica/scripts";
const ENV_DIR: &str = "/etc/pica/env.d";

pub(super) struct TempDirGuard {
  path: PathBuf,
}

impl TempDirGuard {
  pub(super) fn new(path: PathBuf) -> Self {
    Self { path }
  }
}

impl Drop for TempDirGuard {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.path);
  }
}

pub(super) struct PackageFields {
  pub(super) pkgname: String,
  pub(super) pkgver_display: String,
  pub(super) platform: String,
  pub(super) os: String,
  pub(super) arch: String,
  pub(super) uname: String,
  pub(super) luci: String,
  pub(super) pkgmgr: String,
  pub(super) visibility: String,
}

#[allow(clippy::struct_excessive_bools)]
pub(super) struct ScriptConfig {
  pub(super) install: String,
  pub(super) update: String,
  pub(super) remove: String,
  pub(super) keep_all: bool,
  pub(super) keep_install: bool,
  pub(super) keep_update: bool,
  pub(super) keep_remove: bool,
}

/// The outcome of the dependency precheck (step 3).
///
/// It carries the app list as well, because step 5 needs exactly the list that
/// was just reported, and recomputing it could disagree with the report.
struct DependencyPrecheck {
  report: Value,
  kmod_list: Vec<String>,
  base_list: Vec<String>,
  app_list: Vec<String>,
}

/// Everything one artifact install needs.
///
/// Grouping it keeps a growing set of per-install values out of an argument list
/// that is easy to transpose, and lets the transaction and its rollback share one
/// description of the same install.
struct InstallContext<'a> {
  target: &'a TargetRoot,
  tmpdir: &'a Path,
  pkgfile: &'a Path,
  pkgname: &'a str,
  selector: &'a str,
  pkgmgr: &'a str,
  manifest: &'a Manifest,
  scripts: &'a ScriptConfig,
  is_upgrade: bool,
  source: EntrySource,
}

impl InstallContext<'_> {
  /// The hook that runs for this install: `update` replaces an existing install,
  /// `install` creates a new one.
  fn lifecycle_hook(&self) -> (&'static str, &str) {
    if self.is_upgrade {
      ("update", &self.scripts.update)
    } else {
      ("install", &self.scripts.install)
    }
  }
}

fn kept_scripts_dir(target: &TargetRoot, pkgname: &str) -> PathBuf {
  target.resolve(SCRIPTS_DIR).join(pkgname)
}

pub(super) fn parse_bool_like(value: &str) -> Option<bool> {
  match value.trim().to_ascii_lowercase().as_str() {
    "1" | "true" | "yes" | "on" => Some(true),
    "0" | "false" | "no" | "off" => Some(false),
    _ => None,
  }
}

pub(super) fn read_env_keep_flags(
  env_file: &Path,
) -> (Option<bool>, Option<bool>, Option<bool>, Option<bool>) {
  let Ok(content) = fs::read_to_string(env_file) else {
    return (None, None, None, None);
  };

  let mut keep_all = None;
  let mut keep_install = None;
  let mut keep_update = None;
  let mut keep_remove = None;

  for line in content.lines() {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
      continue;
    }

    let Some((key, value)) = line.split_once('=') else {
      continue;
    };

    let key = key.trim();
    let value = value.trim().trim_matches('"').trim_matches('\'');
    let parsed = parse_bool_like(value);

    match key {
      "PICA_KEEP_SCRIPT_ALL" => keep_all = parsed,
      "PICA_KEEP_SCRIPT_INSTALL" => keep_install = parsed,
      "PICA_KEEP_SCRIPT_UPDATE" => keep_update = parsed,
      "PICA_KEEP_SCRIPT_REMOVE" => keep_remove = parsed,
      _ => {}
    }
  }

  (keep_all, keep_install, keep_update, keep_remove)
}

fn ensure_dir(path: &Path) -> CliResult<()> {
  pica_pkg_core::io::ensure_dir(path).map_err(CliError::from)
}

fn required_manifest_field(manifest: &Manifest, key: &str) -> CliResult<String> {
  let value = manifest.get_first(key);
  if value.is_empty() {
    Err(CliError::new(E_CONFIG_INVALID, format!("manifest missing {key}")))
  } else {
    Ok(value)
  }
}

/// Picks the backend for an artifact.
///
/// Priority is `--pkgmgr`, then the artifact's own `pkgmgr` (written by the
/// packer), and finally whatever the config file or auto-detection already
/// selected in [`App::new`]. Two system backends cannot be substituted for each
/// other, so that contradiction is rejected instead of silently installing into
/// the wrong database; choosing `pica` for a system artifact is allowed and
/// means "unpack this ourselves".
fn select_backend_for_package(app: &mut App, manifest: &Manifest) -> CliResult<String> {
  let declared = required_manifest_field(manifest, "pkgmgr")?;
  let declared_kind = PackageManagerKind::parse(&declared).ok_or_else(|| {
    CliError::new(E_MANIFEST_INVALID, format!("invalid manifest pkgmgr: {declared}"))
  })?;

  let kind = match app.options.pkgmgr.clone() {
    Some(value) => {
      let override_kind = PackageManagerKind::parse(&value)
        .ok_or_else(|| CliError::new(E_CONFIG_INVALID, format!("invalid --pkgmgr: {value}")))?;
      if override_kind.requires_system_binary()
        && declared_kind.requires_system_binary()
        && override_kind != declared_kind
      {
        return Err(CliError::new(
          E_CONFIG_INVALID,
          format!("--pkgmgr {} conflicts with package pkgmgr {declared}", override_kind.as_str()),
        ));
      }
      override_kind
    }
    None => declared_kind,
  };

  app.select_package_manager(kind.as_str())?;
  Ok(kind.as_str().to_string())
}

fn canonicalize_display(path: &Path) -> String {
  fs::canonicalize(path).map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}

/// Installs an artifact file into the running system's root.
///
/// `source` is the channel the artifact came from, recorded in the database so
/// later queries and upgrades can tell a repository install from a one-off file
/// or URL.
pub(super) fn pkgfile(
  app: &mut App,
  pkgfile: &Path,
  selector: Option<String>,
  source: EntrySource,
) -> CliResult<()> {
  pkgfile_into(app, pkgfile, selector, source, &TargetRoot::system())
}

/// [`pkgfile`] against an explicit root.
///
/// Real installs target `/`; tests pass a temporary directory so the whole
/// sequence — including what a rollback has to undo — can run without touching
/// the host.
///
/// The run is transactional: nothing is written to the database until the last
/// step, and every earlier failure unwinds what was already done. Otherwise a
/// failure halfway through would leave installed packages and deployed files
/// behind with no record, so `-R` could not clean them up either.
fn pkgfile_into(
  app: &mut App,
  pkgfile: &Path,
  selector: Option<String>,
  source: EntrySource,
  target: &TargetRoot,
) -> CliResult<()> {
  // 1. Unpack the artifact and read its manifest.
  if !pkgfile.is_file() {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      format!("pkgfile not found: {}", pkgfile.display()),
    ));
  }

  // Checked before anything is unpacked: a container handed to `-U` is a
  // mistake worth naming, and `tar` would otherwise report it as a corrupt
  // archive — a true statement that says nothing about what went wrong.
  if let Some(container) = foreign_container(pkgfile) {
    return Err(CliError::new(E_PACKAGE_INVALID, container));
  }

  ensure_dirs(&app.paths)?;

  app.log_info("Loading package files...");

  let tmpdir = make_temp_dir("pica-install").map_err(CliError::from)?;
  let _tmpdir_guard = TempDirGuard::new(tmpdir.clone());
  run_tar_extract(pkgfile, &tmpdir)?;

  let manifest_file = tmpdir.join(MANIFEST);
  if !manifest_file.is_file() {
    return Err(CliError::new(E_PACKAGE_INVALID, "package missing manifest"));
  }
  let manifest = Manifest::from_file(&manifest_file)
    .map_err(|err| CliError::new(E_MANIFEST_INVALID, format!("invalid manifest: {err}")))?;

  let pkgname = required_manifest_field(&manifest, "pkgname")?;
  let _appname = required_manifest_field(&manifest, "appname")?;
  let pica_required = manifest.get_first("pica");
  let pkgver = required_manifest_field(&manifest, "pkgver")?;
  let pkgrel = manifest.get_first("pkgrel");
  let pkgmgr = select_backend_for_package(app, &manifest)?;

  let pkg = PackageFields {
    pkgname: pkgname.clone(),
    pkgver_display: pkgver_cmp_key(&pkgver, &pkgrel),
    platform: required_manifest_field(&manifest, "platform")?,
    os: required_manifest_field(&manifest, "os")?,
    arch: required_manifest_field(&manifest, "arch")?,
    uname: manifest.get_first("uname"),
    luci: manifest.get_first("luci"),
    pkgmgr,
    visibility: manifest.get_first("visibility"),
  };

  let env_file = tmpdir.join(SCRIPTS).join(".env");
  let (env_keep_all, env_keep_install, env_keep_update, env_keep_remove) =
    if env_file.is_file() { read_env_keep_flags(&env_file) } else { (None, None, None, None) };

  let scripts = ScriptConfig {
    install: manifest.get_scalar("script_install"),
    update: manifest.get_scalar("script_update"),
    remove: manifest.get_scalar("script_remove"),
    keep_all: env_keep_all.unwrap_or(false),
    keep_install: env_keep_install.unwrap_or(false),
    keep_update: env_keep_update.unwrap_or(false),
    keep_remove: env_keep_remove.unwrap_or(true),
  };

  let canonical_selector = manifest.canonical_selector(&pkgname);
  let selector = selector.unwrap_or(canonical_selector);

  let is_upgrade = db_has_installed(&app.paths.db_file, &pkgname)?;

  if !pica_required.is_empty() && !ver_ge(PICA_VERSION, &pica_required) {
    return Err(CliError::new(
      E_VERSION_INCOMPATIBLE,
      format!("pica too old: pkg requires >= {pica_required}, cli is {PICA_VERSION}"),
    ));
  }

  let ctx = InstallContext {
    target,
    tmpdir: &tmpdir,
    pkgfile,
    pkgname: &pkgname,
    selector: &selector,
    pkgmgr: &pkg.pkgmgr,
    manifest: &manifest,
    scripts: &scripts,
    is_upgrade,
    source,
  };

  // 2. Validate the package against this host before anything is written: an
  //    unsupported artifact must not install a single file.
  validate_package(app, &manifest, &tmpdir, &pkg)?;

  // 3. Dependency precheck. Read-only, so a failure here has nothing to unwind.
  let precheck = precheck_dependencies(app, &ctx)?;

  // 4-8. Everything that touches the system runs inside the transaction.
  let mut tx = InstallTransaction::new();
  if let Err(err) = run_install(app, &ctx, &precheck, &mut tx) {
    return Err(abandon(app, &ctx, &precheck, &mut tx, err));
  }

  // 9. The database record is written last: until it exists the install is not
  //    complete, so a failure here unwinds the files again.
  if let Err(err) = record_installation(app, &ctx, &precheck, &tx) {
    return Err(abandon(app, &ctx, &precheck, &mut tx, err));
  }

  Ok(())
}

/// The explanation to give when `-U` was handed a package manager's own
/// container rather than a Pica artifact.
///
/// A container is only ever installed from inside an artifact — `pkgs/<backend>/`
/// — so there is no `-U` flag that installs one on its own, and the file is
/// really an instruction for the backend the user already has.
fn foreign_container(pkgfile: &Path) -> Option<String> {
  let name = pkgfile.file_name().and_then(|value| value.to_str()).unwrap_or("");
  let extension = pkgfile.extension().and_then(|value| value.to_str())?.to_ascii_lowercase();

  let (backend, hint) = match extension.as_str() {
    "apk" => ("apk", "apk add --allow-untrusted"),
    "ipk" => ("opkg", "opkg install"),
    _ => return None,
  };

  Some(format!(
    "{name} is an {backend} package, not a Pica artifact: install it with `{hint} {name}`, \
     or ship it inside a Pica package under pkgs/{backend}/"
  ))
}

/// Whether the selected backend hands its dependency lists to something that
/// installs them.
///
/// The question is deliberately "will someone else install these?" and not "is
/// this a real backend?", because owning files and solving dependencies are two
/// separate capabilities: `pica` has the first without the second, while
/// `opkg`/`apk` pass the second on to the system rather than doing it in Pica.
/// Both call sites that ask it — the step 3 gate and the D3 check in
/// `validate_package` — must agree, so the predicate lives in one place.
pub(super) fn installs_declared_dependencies(pkgmgr: &str) -> bool {
  PackageManagerKind::parse(pkgmgr)
    .is_some_and(|kind| kind.requires_system_binary() || kind.can_resolve_dependencies())
}

fn non_empty(items: Vec<String>) -> Vec<String> {
  items.into_iter().filter(|item| !item.is_empty()).collect()
}

/// Step 3: works out which dependencies this install would need and whether they
/// can be satisfied.
///
/// Read-only: it drives the package manager's index and queries only, so a
/// failure here leaves nothing behind to roll back.
fn precheck_dependencies(app: &mut App, ctx: &InstallContext<'_>) -> CliResult<DependencyPrecheck> {
  let mut app_list = non_empty(ctx.manifest.get_array_for("app", ctx.pkgmgr));
  let lang = resolve_lang(&app.paths.conf_file);
  let i18n_template = ctx.manifest.get_first("app_i18n");
  if !i18n_template.is_empty() && lang == "zh-cn" {
    app_list.push(i18n_template.replace("{lang}", &lang));
  }
  app_list = reorder_app_list(app_list);

  // Under `pica` there is no feed to consult and no resolver to run. The
  // dependency lists are guaranteed empty by `validate_package`, so the checked
  // report is empty rather than skipped-with-declarations-present.
  if !installs_declared_dependencies(ctx.pkgmgr) {
    return Ok(DependencyPrecheck {
      report: json!({"kmod": [], "base": [], "app": []}),
      kmod_list: Vec::new(),
      base_list: Vec::new(),
      app_list,
    });
  }

  app.log_info(format!("Resolving {} dependencies and app list...", ctx.pkgmgr));

  let kmod_list = non_empty(ctx.manifest.get_array_for("kmod", ctx.pkgmgr));
  let base_list = non_empty(ctx.manifest.get_array_for("base", ctx.pkgmgr));

  let package_manager = app.package_manager()?;
  package_manager.update_index();

  let report = build_precheck_report(app, ctx.manifest, &ctx.tmpdir.join(PKGS), &app_list);
  let missing = summarize_missing_precheck(&report);
  if !missing.is_empty() {
    app.log_warn(format!("dependency precheck: missing {}", missing.join(" ")));
    return Err(CliError::new(
      E_RUNTIME,
      format!("dependency precheck failed, missing: {}", missing.join(" ")),
    ));
  }

  Ok(DependencyPrecheck { report, kmod_list, base_list, app_list })
}

/// Step 5: installs whatever the artifact's dependencies need, and nothing when
/// it has none.
///
/// A system backend is asked to install the app/base/kmod lists — from its feeds
/// when the policy says so, from the containers in the artifact otherwise.
///
/// The `pica` backend has no feeds and no solver, so it is given no dependency
/// work at all: `pica-pack` expanded any container into `root/` at build time,
/// and `deploy_payload` lays those files down like any other payload. A
/// `pica`-backend artifact that still carries `pkgs/` was built by a packer that
/// predates that rule, and is refused rather than half-installed.
fn install_dependency_containers(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &mut InstallTransaction,
) -> CliResult<()> {
  let pkgs_dir = ctx.tmpdir.join(PKGS);

  if !installs_declared_dependencies(ctx.pkgmgr) {
    if pkgs_dir.is_dir() {
      return Err(CliError::new(
        E_PACKAGE_INVALID,
        format!(
          "a pica-backend artifact must not carry {PKGS}/: pica-pack expands containers \
           into root/ at build time. Rebuild it, or build with `targets = apk` to let \
           the device's own package manager install them"
        ),
      ));
    }
    return Ok(());
  }

  let package_manager = app.package_manager()?;
  for dep in &precheck.kmod_list {
    if !package_manager.is_installed(dep) {
      package_manager.install("kmod", dep)?;
    }
  }

  let has_pkgs_dir = pkgs_dir.is_dir();
  install_via_feeds_or_pkgs(app, "base", &precheck.base_list, &pkgs_dir, has_pkgs_dir)?;

  let snap_before_app = app.package_manager()?.snapshot_installed();
  install_via_feeds_or_pkgs(app, "app", &precheck.app_list, &pkgs_dir, true)?;
  let snap_after_app = app.package_manager()?.snapshot_installed();
  tx.app_added = pkg_list_diff_added(&snap_before_app, &snap_after_app);

  Ok(())
}

/// Steps 4 to 8: the part of an install that changes the system.
///
/// Each step records what it did in `tx` before it can fail, so the caller can
/// undo the whole run with one call.
fn run_install(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &mut InstallTransaction,
) -> CliResult<()> {
  // 4. Snapshot before anything is installed. Only a system backend has a
  //    database to snapshot; `pica` records the containers it unpacks instead.
  if app.backend().requires_system_binary() {
    tx.tx_before = Some(app.package_manager()?.snapshot_installed());
  }

  // 5. Dependency containers.
  install_dependency_containers(app, ctx, precheck, tx)?;

  // 8. What this install added, diffed against the snapshot from step 4.
  if let Some(before) = &tx.tx_before {
    let after = app.package_manager()?.snapshot_installed();
    tx.tx_added = pkg_list_diff_added(before, &after);
  }

  // 6. Lifecycle hook.
  execute_scripts(app, ctx, tx)?;

  // 7. The artifact's own payload.
  deploy_payload(ctx, tx)?;

  Ok(())
}

fn execute_scripts(
  app: &mut App,
  ctx: &InstallContext<'_>,
  tx: &mut InstallTransaction,
) -> CliResult<()> {
  let scripts = ctx.scripts;
  let (label, hook) = ctx.lifecycle_hook();
  if !hook.is_empty() {
    // Recorded before it runs: a hook that dies halfway has still run, and the
    // package's own `remove` hook is the only thing that can undo what it did.
    tx.record_hook(label);
    run_hook(app, ctx.tmpdir, hook, label)?;
  }

  let target_dir = kept_scripts_dir(ctx.target, ctx.pkgname);
  if target_dir.is_dir() {
    // An upgrade replaces the kept scripts instead of merging into them.
    let _ = fs::remove_dir_all(&target_dir);
  }

  let mut items: Vec<(&str, bool)> = Vec::new();
  if !scripts.install.is_empty() && !scripts.install.starts_with('/') {
    items.push((&scripts.install, scripts.keep_all || scripts.keep_install));
  }
  if !scripts.update.is_empty() && !scripts.update.starts_with('/') {
    items.push((&scripts.update, scripts.keep_all || scripts.keep_update));
  }
  if !scripts.remove.is_empty() && !scripts.remove.starts_with('/') {
    items.push((&scripts.remove, scripts.keep_all || scripts.keep_remove));
  }

  let mut kept_any = false;
  for (relative, keep) in &items {
    let source = ctx.tmpdir.join(relative);
    if !source.is_file() {
      return Err(CliError::new(
        E_PACKAGE_INVALID,
        format!("script not found in package: {relative}"),
      ));
    }

    if !*keep {
      continue;
    }

    let target = target_dir.join(relative);
    if let Some(parent) = target.parent() {
      ensure_dir(parent)?;
    }
    fs::copy(&source, &target)
      .map_err(|err| CliError::new(E_IO, format!("copy script failed: {err}")))?;
    kept_any = true;
  }

  if !kept_any && target_dir.is_dir() {
    let _ = fs::remove_dir_all(&target_dir);
  }

  Ok(())
}

/// Step 7: installs the artifact's declarative payload, recording every path it
/// creates in `tx` so removal can undo exactly this install.
///
/// `root/` is mirrored onto the target rootfs (so `root/etc/init.d/demo` becomes
/// `/etc/init.d/demo`), while kept lifecycle scripts and the env file are copied
/// into Pica's own directories.
fn deploy_payload(ctx: &InstallContext<'_>, tx: &mut InstallTransaction) -> CliResult<()> {
  let target = ctx.target;
  let mut payload = PayloadInstall::default();

  let root_dir = ctx.tmpdir.join(ROOT);
  if root_dir.is_dir() {
    // Enumerated *before* the walk: only a path that does not exist yet can be
    // one the walk creates, and only such a path may be removed again.
    let intended = intended_payload(target, &root_dir);
    match install_payload(&root_dir, target.as_path()) {
      Ok(installed) => {
        payload.files.extend(installed.files);
        payload.dirs.extend(installed.dirs);
        payload.size += installed.size;
      }
      Err(err) => {
        // `install_payload` validates as it copies, so a payload it rejects can
        // still have created files before it got there — and it reports nothing
        // at all when it fails. The intended paths are recorded instead, so the
        // rollback knows what to look for and can delete whatever exists.
        tx.record_payload(intended);
        return Err(CliError::from(err));
      }
    }
  }

  let env_file = ctx.tmpdir.join(SCRIPTS).join(".env");
  if env_file.is_file() {
    let env_dir = target.resolve(ENV_DIR);
    ensure_dir(&env_dir)?;
    let file = env_dir.join(format!("{}.env", ctx.pkgname));
    fs::copy(&env_file, &file)
      .map_err(|err| CliError::new(E_IO, format!("copy env file failed: {err}")))?;
    payload.files.push(file.display().to_string());
  }

  let scripts_dir = kept_scripts_dir(target, ctx.pkgname);
  if scripts_dir.is_dir() {
    payload.dirs.push(scripts_dir.display().to_string());
    if let Ok(entries) = fs::read_dir(&scripts_dir) {
      for entry in entries.flatten() {
        if entry.path().is_file() {
          payload.files.push(entry.path().display().to_string());
        }
      }
    }
  }

  tx.record_payload(payload);

  Ok(())
}

/// Every path a `root/` payload could still create, mirroring the walker's
/// mapping without copying anything.
///
/// Only used to make a *failed* deploy reversible, and only for paths that do
/// not exist yet: those are the ones the walk can create, so they are the ones
/// its failure can have half-created. A path that is already there is left out
/// on purpose — the walk may never have reached it, and removing a file or a
/// directory this install did not create is destruction the rollback has no
/// right to perform.
///
/// That leaves the reverse gap, a pre-existing file the walk overwrote before
/// it failed and the rollback therefore keeps, which is the cheaper of the two
/// mistakes: an untracked file is visible and removable by hand.
fn intended_payload(target: &TargetRoot, root_dir: &Path) -> PayloadInstall {
  let mut payload = PayloadInstall::default();
  collect_intended(target, root_dir, root_dir, &mut payload);
  payload
}

fn collect_intended(target: &TargetRoot, base: &Path, dir: &Path, payload: &mut PayloadInstall) {
  let Ok(entries) = fs::read_dir(dir) else {
    return;
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(relative) = path.strip_prefix(base) else {
      continue;
    };
    let to = target.as_path().join(relative);

    let Ok(metadata) = fs::symlink_metadata(&path) else {
      continue;
    };

    // The walker calls `create_dir_all` for a file's parent, so a missing
    // ancestor is created even when the payload has no entry for it. Walking up
    // from the root records those in the order removal expects (parents before
    // children) instead of leaving the rollback to trip over them.
    push_missing_ancestors(target, &to, payload);

    let missing = fs::symlink_metadata(&to).is_err();
    if metadata.is_dir() {
      if missing {
        payload.dirs.push(to.display().to_string());
      }
      collect_intended(target, base, &path, payload);
    } else if missing {
      payload.files.push(to.display().to_string());
    }
  }
}

/// Adds every ancestor of `path` that does not exist yet, outermost first.
fn push_missing_ancestors(target: &TargetRoot, path: &Path, payload: &mut PayloadInstall) {
  let root = target.as_path();
  let Ok(relative) = path.strip_prefix(root) else {
    return;
  };

  let mut current = root.to_path_buf();
  for component in relative.components().take(relative.components().count().saturating_sub(1)) {
    current.push(component);
    let missing = fs::symlink_metadata(&current).is_err();
    let text = current.display().to_string();
    if missing && !payload.dirs.contains(&text) {
      payload.dirs.push(text);
    }
  }
}

/// What one install has done so far, so that it can be undone.
///
/// Every field is filled in *before* the step that could fail, so a step that
/// dies halfway still leaves the transaction holding whatever it managed to do.
/// Nothing here is written to the database: the record is step 9, and until it
/// exists this struct is the only description of the install.
struct InstallTransaction {
  /// Packages installed before step 5, when the backend keeps such a list.
  tx_before: Option<Vec<String>>,
  /// Packages step 5 added, diffed against `tx_before` once it returned.
  tx_added: Vec<String>,
  /// The part of `tx_added` that came from the `app` list.
  app_added: Vec<String>,
  /// Containers Pica unpacked itself, by package name.
  pkgs: Vec<String>,
  /// Paths deployed, in deployment order so removal can replay them backwards.
  payload_records: Vec<PayloadInstall>,
  /// Labels of the hooks that ran, in the order they ran.
  executed_hooks: Vec<String>,
}

impl InstallTransaction {
  fn new() -> Self {
    Self {
      tx_before: None,
      tx_added: Vec::new(),
      app_added: Vec::new(),
      pkgs: Vec::new(),
      payload_records: Vec::new(),
      executed_hooks: Vec::new(),
    }
  }

  fn record_payload(&mut self, payload: PayloadInstall) {
    self.payload_records.push(payload);
  }

  /// Records a container `pica` unpacked.
  ///
  /// Its files join the payload history like any other deployed path, and its
  /// name is the container's contribution to "packages this install added" —
  fn record_hook(&mut self, label: &str) {
    self.executed_hooks.push(label.to_string());
  }

  /// Every path this install created, as one payload.
  ///
  /// Container payloads come first and `root/` last, which keeps parents before
  /// children: `remove_payload` walks directories in reverse and only removes
  /// the ones that ended up empty.
  fn merged_payload(&self) -> PayloadInstall {
    let mut merged = PayloadInstall::default();
    for record in &self.payload_records {
      merged.files.extend(record.files.iter().cloned());
      merged.dirs.extend(record.dirs.iter().cloned());
      merged.size += record.size;
    }
    merged
  }

  /// Undoes everything recorded so far, and returns what could not be undone.
  ///
  /// The order mirrors the install: deployed files come off first (deepest
  /// payload last), then the hooks are compensated, and finally the packages
  /// step 5 added are handed back to the backend that installed them.
  fn rollback(&mut self, app: &mut App, ctx: &InstallContext<'_>) -> Vec<String> {
    let mut leftovers = Vec::new();

    for record in self.payload_records.iter().rev() {
      remove_payload(record);
    }

    // `remove_payload` is best effort — it ignores the errors it hits — so what
    // survived is found by looking rather than by trusting a return value. The
    // survivors are both reported and kept as this install's payload record,
    // because a `-R` driven by that record is the only way left to clean them.
    //
    // A recorded directory is one this install created, so a directory that is
    // still there is a survivor too: it is either not empty (something else —
    // a hook's file, another package's file — moved in) or could not be removed.
    // Ignoring it would report a clean rollback while leaving a record nothing
    // points `-R` at.
    let mut remnant = PayloadInstall::default();
    for record in &self.payload_records {
      for file in &record.files {
        if Path::new(file).exists() {
          leftovers.push(file.clone());
          remnant.files.push(file.clone());
        }
      }
      for dir in &record.dirs {
        if Path::new(dir).exists() {
          leftovers.push(dir.clone());
          remnant.dirs.push(dir.clone());
        }
      }
    }
    self.payload_records = vec![remnant];

    if !self.executed_hooks.is_empty() {
      // A hook may have changed things only the package knows how to undo, so
      // the package's own `remove` hook is the compensation. It runs in reverse
      // order, once per hook that ran.
      for label in self.executed_hooks.iter().rev() {
        if let Err(err) = run_hook(app, ctx.tmpdir, &ctx.scripts.remove, "rollback-remove") {
          leftovers.push(format!("hook:{label} ({})", err.message));
        }
      }
      self.executed_hooks.clear();
    }

    if app.backend().requires_system_binary() {
      for name in std::mem::take(&mut self.tx_added).into_iter().rev() {
        if let Err(err) = app.package_manager().and_then(|manager| manager.remove(&name)) {
          leftovers.push(format!("pkg:{name} ({})", err.message));
        }
      }
    }
    self.tx_added.clear();
    self.app_added.clear();
    self.pkgs.clear();

    leftovers
  }
}

/// Rolls a failed install back and reports what could not be undone.
///
/// A rollback that finishes cleanly leaves no trace, so the original error is
/// returned unchanged. One that cannot finish keeps a database record of what is
/// left: without it `-R` would have nothing to act on and the files would be
/// unreachable.
fn abandon(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &mut InstallTransaction,
  err: CliError,
) -> CliError {
  app.log_warn(format!("{}: install failed, rolling back", ctx.pkgname));

  let leftovers = tx.rollback(app, ctx);
  if leftovers.is_empty() {
    app.log_warn(format!("{}: rolled back to the previous state", ctx.pkgname));
    return err;
  }

  app.log_warn(format!(
    "{}: rollback left {} path(s) behind: {}",
    ctx.pkgname,
    leftovers.len(),
    leftovers.join(" ")
  ));

  if let Err(record_err) = record_installation(app, ctx, precheck, tx) {
    app.log_warn(format!(
      "{}: could not record the partial install: {}",
      ctx.pkgname, record_err.message
    ));
  }

  CliError::new(err.code, format!("{}; rollback incomplete, see the warnings above", err.message))
}

/// Step 9: writes the completed install to the database.
fn record_installation(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &InstallTransaction,
) -> CliResult<()> {
  report_set_install_result(
    &app.paths,
    ctx.pkgname,
    ctx.selector,
    &ctx.manifest.value,
    &precheck.report,
    &tx.tx_added,
    &tx.app_added,
  )?;

  let mut manifest_stored = ctx.manifest.value.clone();
  // `source` describes how *this* install obtained the package, not where the
  // artifact was built, so it is overwritten instead of defaulted: a URL
  // download stays `url` even when the packer stamped `pica` into the manifest.
  manifest_stored["source"] = json!(ctx.source.as_str());
  // `pkgmgr` is the backend that actually owns the files. Recording the one in
  // use (rather than the artifact's declared value) keeps `-R` from driving a
  // package manager that never touched this install, as happens with
  // `--pkgmgr pica` over an `opkg` artifact.
  manifest_stored["pkgmgr"] = json!(app.backend().as_str());
  if !tx.pkgs.is_empty() {
    // Containers `pica` unpacked itself, by name. `-Qi` can show them and a
    // reinstall can tell they were already on this system.
    manifest_stored["installed_pkgs"] = json!(tx.pkgs);
  }
  if manifest_stored.get("url").is_none() {
    let origin = manifest_stored.get("origin").and_then(Value::as_str).unwrap_or("").to_string();
    manifest_stored["url"] = json!(origin);
  }
  for key in ["branch", "protocol", "url", "luci_url", "luci_desc", "visibility"] {
    if manifest_stored.get(key).is_none() {
      manifest_stored[key] = json!("");
    }
  }

  let payload = tx.merged_payload();
  db_set_installed(
    &app.paths.db_file,
    ctx.pkgname,
    &manifest_stored,
    &canonicalize_display(ctx.pkgfile),
    &payload.files,
    &payload.dirs,
  )?;

  app.log_info("Transaction completed");

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::app::{FeedPolicy, JsonMode, Options, Paths};
  use pretty_assertions::assert_eq;
  use std::os::unix::fs::symlink;
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
    };

    let mut app = App::new(paths, options);
    app.select_package_manager(PackageManagerKind::Pica.as_str()).expect("force the pica backend");
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

  /// The DB record of `pkgname`, or `None` when nothing was recorded.
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
    pkgfile_into(app, &artifact, None, EntrySource::Repo, target)
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
    /// A `pica`-backend artifact is built by `pica-pack`, which expands any
    /// container into `root/`, so a pica install never has containers to lay
    /// down. Rollback tests want the payload path, not the container path.
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

  #[test]
  fn a_pica_artifact_must_not_carry_containers() {
    // pica-pack expands pkgs/ into root/ at build time, so an artifact that
    // still has containers came from a packer that predates that rule. Laying
    // some of it down and then failing would leave the device inconsistent, so
    // the install is refused before anything is written.
    let mut fixture = PicaFixture::new();

    let err = fixture
      .install(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "overlay\n")],
        &[],
        &[],
      )
      .expect_err("a pica artifact must not ship containers");

    assert!(err.message.contains("must not carry pkgs/"), "{}", err.message);
    assert!(
      err.message.contains("targets = apk"),
      "the error must offer the way forward: {}",
      err.message
    );
    assert!(
      !fixture.path("etc/demo.conf").exists(),
      "nothing may be laid down when the artifact is refused"
    );
    assert!(
      db_record(&fixture.app, "demo").is_none(),
      "a refused install must leave no database record"
    );
  }

  #[test]
  fn pica_rejects_a_package_that_declares_dependencies() {
    let mut fixture = PicaFixture::new();

    let err = fixture
      .install(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\nbase = luci-base\n"),
        &[("etc/demo.conf", "overlay\n")],
        &[],
        &[],
      )
      .expect_err("D3: pica cannot resolve dependencies");

    assert!(err.message.contains("cannot resolve dependencies"), "{}", err.message);
    assert!(
      err.message.contains("pkgmgr=opkg"),
      "the message must suggest a backend that can install them: {}",
      err.message
    );
    assert_eq!(err.code, E_PACKAGE_INVALID);
    assert!(!fixture.path("etc/demo.conf").exists(), "validation runs before anything is written");
    assert!(db_record(&fixture.app, "demo").is_none());
  }

  #[test]
  fn a_broken_container_leaves_the_system_untouched() {
    let mut fixture = PicaFixture::new();
    let broken = fixture.work_path.join("broken.ipk");
    write_file(&broken, "this is not a container\n");

    let err = install_artifact(
      &fixture.work_path,
      &mut fixture.app,
      &fixture.target,
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "overlay\n")],
      &[],
      &[("broken.ipk", broken)],
      &[],
    )
    .expect_err("step 5 fails");

    assert!(err.message.contains("tar"), "{}", err.message);
    assert!(
      !fixture.path("etc/demo.conf").exists(),
      "the payload must not run after the container step failed"
    );
    assert!(db_record(&fixture.app, "demo").is_none(), "a failed install leaves no record");
  }

  #[test]
  fn a_failed_hook_rolls_the_container_back_out() {
    let mut fixture = PicaFixture::new();

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/fail\n"),
        &[("etc/demo.conf", "overlay\n")],
        &[],
        &[("scripts/fail", "#!/bin/sh\nexit 1\n")],
      )
      .expect_err("step 6 fails");

    // A rollback that finishes cleanly adds nothing to the error: it is the
    // hook's own failure, unchanged. Only an incomplete one rewrites it.
    assert!(err.message.contains("sh failed"), "{}", err.message);
    assert!(!err.message.contains("rollback incomplete"), "{}", err.message);
    assert!(
      !fixture.path("usr/lib/demo/helper").exists(),
      "step 5 already unpacked this container, so the rollback has to take it back out"
    );
    assert!(
      !fixture.path("usr/lib/demo").exists(),
      "the directories the container created are empty again and must go too"
    );
    assert!(!fixture.path("etc/demo.conf").exists());
    assert!(db_record(&fixture.app, "demo").is_none());
  }

  #[test]
  fn a_rejected_payload_rolls_back_what_it_already_copied() {
    let mut fixture = PicaFixture::new();

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("aaa.conf", "copied before the walk failed\n")],
        &[("zzz-escape", "/etc/passwd")],
        &[],
      )
      .expect_err("step 7 fails");

    assert!(err.message.contains("escapes the target root"), "{}", err.message);
    assert!(
      !fixture.path("usr/lib/demo/helper").exists(),
      "the container from step 5 must be taken back out"
    );
    assert!(
      !fixture.path("aaa.conf").exists(),
      "the walker reports nothing when it fails, so the file it had already copied is only \
       reachable through the paths this step intended to create"
    );
    assert!(db_record(&fixture.app, "demo").is_none());
  }

  #[test]
  fn a_concrete_arch_is_measured_against_the_kernel_on_the_pica_backend() {
    let mut fixture = PicaFixture::new();

    // The `pica` backend has no package manager to ask, so the kernel's name is
    // the only thing left to compare against. Before the fallback existed the
    // list came back empty and every package naming a concrete arch was refused
    // as unsupported, so the message is what this test is really watching.
    let manifest =
      format!("{}pkgmgr = pica\n", MANIFEST_DEMO.replace("arch = all", "arch = s390x"));
    let err = fixture.install(&manifest, &[], &[], &[]).expect_err("the arch is foreign");

    assert!(err.message.contains("unsupported arch: pkg=s390x"), "{}", err.message);
    assert!(
      err.message.contains("uname -m reports"),
      "the host has to say where its answer came from, not just that it has none: {}",
      err.message
    );
    assert!(
      !fixture.path("usr/lib/demo/helper").exists(),
      "a rejected arch must not deploy the payload first"
    );
    assert!(
      db_record(&fixture.app, "demo").is_none(),
      "nothing was installed, nothing is recorded"
    );
  }

  #[test]
  fn a_container_is_named_as_one_instead_of_as_a_broken_archive() {
    let work = tempfile::tempdir().expect("temp dir");
    let apk = work.path().join("demo.apk");
    fs::write(&apk, "an apk is not a tar every tar can read\n").expect("write apk");

    let mut app = test_app(work.path());
    let target = TargetRoot::new(work.path().join("target-root"));
    let err = pkgfile_into(&mut app, &apk, None, EntrySource::Local, &target)
      .expect_err("-U does not install containers");

    assert!(err.message.contains("is an apk package"), "{}", err.message);
    assert!(err.message.contains("apk add"), "{}", err.message);
    assert!(
      !err.message.contains("invalid magic"),
      "the reader should not have to decode a tar error: {}",
      err.message
    );
  }

  #[test]
  fn a_failed_walk_never_claims_a_path_that_was_already_there() {
    let mut fixture = PicaFixture::new();

    // A directory that existed before this install and that the payload also
    // writes into. The failed walk reports nothing, so the rollback works from
    // the paths this step could have created — and a path that was already
    // there is not one of them: removing it would delete what this install
    // never made.
    let pre_existing = fixture.path("keep");
    fs::create_dir_all(&pre_existing).expect("mkdir keep");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("keep/file", "copied before the walk failed\n")],
        &[("zzz-escape", "/etc/passwd")],
        &[],
      )
      .expect_err("step 7 fails");

    assert!(err.message.contains("escapes the target root"), "{}", err.message);
    assert!(pre_existing.is_dir(), "a directory the install did not create must survive");
    assert!(!fixture.path("keep/file").exists(), "what the walk did copy is still taken back out");
    assert!(!fixture.path("usr/lib/demo/helper").exists(), "the container is rolled back too");
    assert!(
      db_record(&fixture.app, "demo").is_none(),
      "nothing was left behind, so there is nothing to record"
    );
  }

  #[test]
  fn a_rollback_that_cannot_finish_keeps_a_record_to_remove_by_hand() {
    let mut fixture = PicaFixture::new();

    // The install hook runs, so the rollback knows a hook changed something only
    // this package can undo and tries to compensate with the package's own
    // `remove` hook. That hook is declared but fails, so the compensation cannot
    // finish — the case a surviving record exists for, because `-R` driven by
    // that record is the only thing left that can clean up.
    let install_hook = "#!/bin/sh\nmkdir -p /tmp/pica-install-ran\n";
    let failing_remove = "#!/bin/sh\nexit 1\n";

    let err = fixture
      .install_without_containers(
        &format!(
          "{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\nscript_remove = scripts/remove\n"
        ),
        // `aaa.conf` sorts ahead of the escaping symlink, so the walk copies it
        // and then trips, which is what starts the rollback in the first place.
        &[("aaa.conf", "copied before the walk failed\n")],
        &[("zzz-escape", "/etc/passwd")],
        &[("scripts/install", install_hook), ("scripts/remove", failing_remove)],
      )
      .expect_err("the payload walk trips on the escaping symlink");

    assert!(err.message.contains("rollback incomplete"), "{}", err.message);
    assert!(!fixture.path("aaa.conf").exists(), "what the walk did copy is taken back out");

    let record = db_record(&fixture.app, "demo")
      .expect("an incomplete rollback keeps a record, because -R is the only way left");
    assert!(
      record["files"].as_array().is_some_and(Vec::is_empty),
      "the recorded files were all removed; only the hook residue is left: {record}"
    );
  }

  #[test]
  fn a_pica_artifact_without_containers_only_lays_the_overlay() {
    let mut fixture = PicaFixture::new();

    install_artifact(
      &fixture.work_path,
      &mut fixture.app,
      &fixture.target,
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "overlay\n")],
      &[],
      &[],
      &[],
    )
    .expect("install");

    assert_eq!(fs::read_to_string(fixture.path("etc/demo.conf")).expect("overlay"), "overlay\n");
    let record = db_record(&fixture.app, "demo").expect("recorded");
    assert!(record["manifest"].get("installed_pkgs").is_none(), "{record}");
  }
}
