//! Installs one artifact: unpacking, checks and the order of the steps.
//! See docs/architecture.md for the step table.

use super::deps::{
  dependency_backend, install_dependency_containers, precheck_dependencies, recheck_constraints,
  touches_package_manager,
};
use super::feed::DependencySource;
use super::hooks::execute_scripts;
use super::optional::Plan as OptionalPlan;
use super::payload::{deploy_payload, payload_extent};
use super::precheck::{DeclaredDeps, validate_package};
use super::transaction::{InstallTransaction, abandon, record_installation, remove_orphans};
use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_MANIFEST_INVALID, E_PACKAGE_INVALID,
  E_VERSION_INCOMPATIBLE, ensure_dirs,
};
use crate::format;
use crate::i18n::tr;
use crate::i18n::{Key, Lang};
use crate::installed::EntrySource;
use crate::pkgmgr::PackageManagerKind;
use crate::state::read_db;
use crate::system::{TargetRoot, run_tar_extract};
use pica_pkg_core::PICA_VERSION;
use pica_pkg_core::io::PayloadInstall;
use pica_pkg_core::io::make_temp_dir;
use pica_pkg_core::layout::{MANIFEST, ROOT, SCRIPTS};
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::naming::{is_valid_appname, is_valid_pkgname};
use pica_pkg_core::version::{pkgver_cmp_key, pkgver_gt, ver_ge};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) struct TempDirGuard {
  pub(super) path: PathBuf,
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

/// The manifest fields the host checks act on. The package name is not among
/// them: it names the transaction, which the header already printed.
pub(super) struct PackageFields {
  pub(super) pkgver_display: String,
  pub(super) os: String,
  pub(super) arch: String,
  pub(super) uname: String,
  pub(super) luci: String,
  pub(super) pkgmgr: String,
  pub(super) source_availability: String,
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
/// It carries the parsed declarations as well, because step 5 needs exactly the
/// list that was just reported, and recomputing it could disagree with the
/// report. The bounds live on in `bound` — but only for the recheck after step
/// 5: a backend is asked for `constraint::names`, never for the declaration.
pub(super) struct DependencyPrecheck {
  pub(super) report: Value,
  pub(super) deps: DeclaredDeps,
  /// Where the artifact's declared dependencies come from, decided here rather
  /// than in step 5 because the selector has to know which optional
  /// dependencies are actually installable before it shows them.
  pub(super) source: Option<DependencySource>,
  /// What the user chose (or what the mode decided) about the optional
  /// dependencies.
  pub(super) optional: OptionalPlan,
}

/// Everything one artifact install needs.
///
/// Grouping it keeps a growing set of per-install values out of an argument list
/// that is easy to transpose, and lets the transaction and its rollback share one
/// description of the same install.
pub(super) struct InstallContext<'a> {
  pub(super) target: &'a TargetRoot,
  pub(super) tmpdir: &'a Path,
  pub(super) pkgfile: &'a Path,
  pub(super) pkgname: &'a str,
  pub(super) selector: &'a str,
  pub(super) pkgmgr: &'a str,
  pub(super) manifest: &'a Manifest,
  pub(super) scripts: &'a ScriptConfig,
  pub(super) is_upgrade: bool,
  pub(super) source: EntrySource,
}

impl InstallContext<'_> {
  /// The hook that runs for this install: `update` replaces an existing install,
  /// `install` creates a new one.
  pub(super) fn lifecycle_hook(&self) -> (&'static str, &str) {
    if self.is_upgrade {
      ("update", &self.scripts.update)
    } else {
      ("install", &self.scripts.install)
    }
  }
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

pub(super) fn ensure_dir(path: &Path) -> CliResult<()> {
  pica_pkg_core::io::ensure_dir(path).map_err(CliError::from)
}

pub(super) fn required_manifest_field(manifest: &Manifest, key: &str) -> CliResult<String> {
  let value = manifest.get_first(key);
  if value.is_empty() {
    Err(CliError::new(E_CONFIG_INVALID, tr(Key::ManifestMissing, &[&key])))
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
pub(super) fn backend_for_package(app: &App, manifest: &Manifest) -> CliResult<PackageManagerKind> {
  let declared = required_manifest_field(manifest, "pkgmgr")?;
  let declared_kind = PackageManagerKind::parse(&declared).ok_or_else(|| {
    CliError::new(E_MANIFEST_INVALID, tr(Key::InvalidManifestPkgmgr, &[&declared]))
  })?;

  let kind = match app.options.pkgmgr.clone() {
    Some(value) => {
      let override_kind = PackageManagerKind::parse(&value)
        .ok_or_else(|| CliError::new(E_CONFIG_INVALID, tr(Key::InvalidPkgmgrFlag, &[&value])))?;
      if override_kind.requires_system_binary()
        && declared_kind.requires_system_binary()
        && override_kind != declared_kind
      {
        return Err(CliError::new(
          E_CONFIG_INVALID,
          tr(Key::PkgmgrConflict, &[&override_kind.as_str(), &declared]),
        ));
      }
      override_kind
    }
    None => declared_kind,
  };

  Ok(kind)
}

pub(super) fn canonicalize_display(path: &Path) -> String {
  fs::canonicalize(path).map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}

/// The two figures the repository index publishes for a package.
///
/// Only a repository has them: a local file or a URL is described by nothing but
/// itself, so those installs pass the default and no size is shown for them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct IndexSizes {
  /// `size`: the installed volume the package declares. What the package says
  /// about itself, not a measurement of the disk after installing it.
  pub installed: Option<u64>,
  /// `csize`: the size of the compressed artifact that was downloaded.
  pub download: Option<u64>,
}

/// The one line that says what this transaction does to this package.
///
/// `installed` is what the database records, so `Upgrading` cannot be claimed
/// for a package that was never installed, and a version that is already on the
/// box is named as the reinstall it is. All four sentences live here so a
/// repository install and a `-U` of the same artifact cannot describe
/// themselves differently.
pub(super) fn transaction_header(
  pkgname: &str,
  new_version: &str,
  installed: Option<&str>,
) -> String {
  let Some(old) = installed else {
    return tr(Key::HeaderInstalling, &[&pkgname, &new_version]);
  };

  if old == new_version {
    return tr(Key::HeaderAlreadyInstalled, &[&pkgname, &new_version]);
  }
  if pkgver_gt(new_version, old) {
    tr(Key::HeaderUpgrading, &[&pkgname, &old, &new_version])
  } else if pkgver_gt(old, new_version) {
    tr(Key::HeaderDowngrading, &[&pkgname, &old, &new_version])
  } else {
    // Two different strings that neither compares above the other: the header
    // does not claim a direction it could not establish.
    tr(Key::HeaderReinstalling, &[&pkgname, &new_version])
  }
}

/// The size lines for the header, indented under it.
///
/// A figure is only shown when it is a usable number: a missing one was never
/// published, and a zero is a feed saying "not measured" rather than a package
/// that occupies nothing. Neither is worth printing, and printing `0 B` would be
/// worse than saying nothing.
pub(super) fn size_lines(sizes: IndexSizes) -> Vec<String> {
  [(Key::SizeInstalledLabel, sizes.installed), (Key::SizeDownloadLabel, sizes.download)]
    .into_iter()
    .filter_map(|(label, value)| {
      let label = tr(label, &[]);
      value.filter(|bytes| *bytes > 0).map(|bytes| format!("  {label}: {}", format::bytes(bytes)))
    })
    .collect()
}

/// `Installed 12 files, 74.2 MiB`, from what the deployment counted.
///
/// `files` is the payload's file list, which holds the symlinks as well as the
/// regular files; `size` sums the bytes of the regular files it wrote. Created
/// directories are not counted as files.
pub(super) fn install_summary(lang: Lang, payload: &PayloadInstall) -> String {
  tr(
    Key::InstalledSummary,
    &[&format::files(lang, payload.files.len()), &format::bytes(payload.size)],
  )
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
  sizes: IndexSizes,
) -> CliResult<()> {
  pkgfile_into(app, pkgfile, selector, source, sizes, &TargetRoot::system())
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
pub(super) fn pkgfile_into(
  app: &mut App,
  pkgfile: &Path,
  selector: Option<String>,
  source: EntrySource,
  sizes: IndexSizes,
  target: &TargetRoot,
) -> CliResult<()> {
  // 1. Unpack the artifact and read its manifest.
  if !pkgfile.is_file() {
    return Err(CliError::new(E_PACKAGE_INVALID, tr(Key::PkgfileNotFound, &[&pkgfile.display()])));
  }

  // Checked before anything is unpacked: a container handed to `-U` is a
  // mistake worth naming, and `tar` would otherwise report it as a corrupt
  // archive — a true statement that says nothing about what went wrong.
  if let Some(container) = foreign_container(pkgfile) {
    return Err(CliError::new(E_PACKAGE_INVALID, container));
  }

  ensure_dirs(&app.paths)?;

  let tmpdir = make_temp_dir("pica-install").map_err(CliError::from)?;
  let _tmpdir_guard = TempDirGuard::new(tmpdir.clone());
  run_tar_extract(pkgfile, &tmpdir)?;

  let manifest_file = tmpdir.join(MANIFEST);
  if !manifest_file.is_file() {
    return Err(CliError::new(E_PACKAGE_INVALID, tr(Key::PackageMissingManifest, &[])));
  }
  let manifest = Manifest::from_file(&manifest_file)
    .map_err(|err| CliError::new(E_MANIFEST_INVALID, tr(Key::InvalidManifest, &[&err])))?;

  let pkgname = required_manifest_field(&manifest, "pkgname")?;
  let appname = required_manifest_field(&manifest, "appname")?;
  check_names(app, &pkgname, &appname)?;
  let pica_required = manifest.get_first("pica");
  let pkgver = required_manifest_field(&manifest, "pkgver")?;
  let pkgrel = manifest.get_first("pkgrel");
  let kind = backend_for_package(app, &manifest)?;
  app.with_backend(kind, |app| {
    install_unpacked(
      app,
      &Unpacked { pkgfile, tmpdir: &tmpdir, manifest: &manifest, pkgname: &pkgname },
      &Versions { pkgver, pkgrel, pica_required },
      selector,
      source,
      sizes,
      target,
    )
  })
}

/// The version fields read from the manifest before the backend is known.
struct Versions {
  pkgver: String,
  pkgrel: String,
  pica_required: String,
}

/// An artifact unpacked into `tmpdir`, with its manifest read.
struct Unpacked<'a> {
  pkgfile: &'a Path,
  tmpdir: &'a Path,
  manifest: &'a Manifest,
  pkgname: &'a str,
}

/// Steps 2 to 8, with the artifact's backend in use. A dry run, or a "no" at
/// the confirmation, ends after step 3, before anything is changed.
fn install_unpacked(
  app: &mut App,
  unpacked: &Unpacked<'_>,
  versions: &Versions,
  selector: Option<String>,
  source: EntrySource,
  sizes: IndexSizes,
  target: &TargetRoot,
) -> CliResult<()> {
  let &Unpacked { pkgfile, tmpdir, manifest, pkgname } = unpacked;
  let Versions { pkgver, pkgrel, pica_required } = versions;
  let pkgmgr = app.backend().as_str().to_string();

  // Part of the artifact contract, checked here so a package cannot leave it
  // out. Nothing compares it against the host yet: `os`, `arch` and `uname` are
  // what `validate_package` acts on.
  let _platform = required_manifest_field(manifest, "platform")?;

  let pkg = PackageFields {
    pkgver_display: pkgver_cmp_key(pkgver, pkgrel),
    os: required_manifest_field(manifest, "os")?,
    arch: required_manifest_field(manifest, "arch")?,
    uname: manifest.get_first("uname"),
    luci: manifest.get_first("luci"),
    pkgmgr,
    source_availability: manifest.get_first("source_availability"),
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

  let canonical_selector = manifest.canonical_selector(pkgname);
  let selector = selector.unwrap_or(canonical_selector);

  // One lookup answers both questions: whether this replaces an installed
  // package, and what the header has to say about the version it replaces.
  let installed_version = read_db(&app.paths)?.installed.get(pkgname).map(|entry| {
    // Recorded without a version: say so rather than inventing one.
    if entry.field("pkgver").is_empty() { String::new() } else { entry.version() }
  });
  let is_upgrade = installed_version.is_some();

  if !pica_required.is_empty() && !ver_ge(PICA_VERSION, pica_required) {
    return Err(CliError::new(
      E_VERSION_INCOMPATIBLE,
      tr(Key::InstallPicaTooOld, &[&pica_required, &PICA_VERSION]),
    ));
  }

  // The transaction says what it is before it touches anything, so every line
  // after this one — including a failure — belongs to a package the user can
  // identify. Sizes come first in the index and say what the package claims;
  // what was actually written is reported at the end.
  app.log_info(transaction_header(pkgname, &pkg.pkgver_display, installed_version.as_deref()));
  for line in size_lines(sizes) {
    app.log_info(line);
  }

  let ctx = InstallContext {
    target,
    tmpdir,
    pkgfile,
    pkgname,
    selector: &selector,
    pkgmgr: &pkg.pkgmgr,
    manifest,
    scripts: &scripts,
    is_upgrade,
    source,
  };

  // 2. Validate the package against this host before anything is written: an
  //    unsupported artifact must not install a single file.
  validate_package(app, manifest, tmpdir, &pkg)?;

  // 3. Dependency precheck. Read-only, so a failure here has nothing to unwind.
  //
  // A `pica` artifact hands its dependencies to the host's manager: pica owns
  // the files, the package database owns the dependencies, and neither solves
  // the other's problem. The switch below covers steps 3 to 5b — every step that
  // asks the backend about packages — and stops before anything writes
  // `db.json`, so the record keeps saying `pica`. Moving a write inside this
  // scope would credit `opkg` with files it never laid down.
  //
  // Asking who would install them reads no database, so it is safe to ask here;
  // a `pica` artifact that declares nothing returns from the precheck without
  // touching one (`touches_package_manager`).
  let deps_from = dependency_backend(app, ctx.pkgmgr);
  let precheck = match deps_from {
    Some(kind) => app.with_backend(kind, |app| precheck_dependencies(app, &ctx, deps_from))?,
    None => precheck_dependencies(app, &ctx, deps_from)?,
  };

  // The plan is complete: the header, the sizes and where each dependency
  // comes from are on screen. Add what the payload writes, then stop for a dry
  // run or ask whether to go on.
  let (files, bytes) = payload_extent(&tmpdir.join(ROOT));
  if files > 0 {
    let (files, bytes) = (format::files(app.lang, files), format::bytes(bytes));
    app.info_args(Key::PlanPayload, &[&files, &bytes]);
  }
  if app.stop_for_dry_run() {
    return Ok(());
  }
  app.confirm()?;

  // 4-7. Everything that touches the system runs inside the transaction.
  let mut tx = InstallTransaction::new();
  if let Err(err) = run_install(app, &ctx, &precheck, &mut tx) {
    return Err(abandon(app, &ctx, &precheck, &mut tx, err));
  }

  // 8. The database record is written last: until it exists the install is not
  //    complete, so a failure here unwinds the files again.
  let orphaned = match record_installation(app, &ctx, &precheck, &tx, false) {
    Ok(orphaned) => orphaned,
    Err(err) => return Err(abandon(app, &ctx, &precheck, &mut tx, err)),
  };

  // Only now is the install complete, so the copies can go, and so can what
  // the previous version wrote that this one does not.
  tx.finish(app);
  remove_orphans(app, pkgname, orphaned);

  Ok(())
}

/// Refuses a name before it is joined onto any path on the device.
///
/// A repository install has already passed this at sync, but `-U` reaches here
/// with nothing in between: the manifest is the only thing that names the
/// package, and `pkgname` is about to pick a directory under the scripts root.
pub(super) fn check_names(app: &App, pkgname: &str, appname: &str) -> CliResult<()> {
  if !is_valid_pkgname(pkgname) {
    return Err(CliError::new(E_MANIFEST_INVALID, app.args(Key::InvalidPkgname, &[pkgname])));
  }
  if !is_valid_appname(appname) {
    return Err(CliError::new(E_MANIFEST_INVALID, app.args(Key::InvalidAppname, &[appname])));
  }
  Ok(())
}

/// The explanation to give when `-U` was handed a package manager's own
/// container rather than a Pica artifact.
///
/// A container is only ever installed from inside an artifact — `pkgs/<backend>/`
/// — so there is no `-U` flag that installs one on its own, and the file is
/// really an instruction for the backend the user already has.
pub(super) fn foreign_container(pkgfile: &Path) -> Option<String> {
  let name = pkgfile.file_name().and_then(|value| value.to_str()).unwrap_or("");
  let extension = pkgfile.extension().and_then(|value| value.to_str())?.to_ascii_lowercase();

  let (backend, hint) = match extension.as_str() {
    "apk" => ("apk", "apk add --allow-untrusted"),
    "ipk" => ("opkg", "opkg install"),
    _ => return None,
  };

  Some(tr(Key::ForeignContainer, &[&name, &backend, &hint, &name, &backend]))
}

/// Steps 4 to 7: the part of an install that changes the system.
///
/// Each step records what it did in `tx` before it can fail, so the caller can
/// undo the whole run with one call.
pub(super) fn run_install(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &mut InstallTransaction,
) -> CliResult<()> {
  // 4. Snapshot before anything is installed. Only a system backend has a
  //    database to snapshot; `pica` keeps its own record of the paths it wrote.
  //    Versions are kept, not just names: they are what tells a package this
  //    install added from one it upgraded.
  // The executor is the artifact's own backend for `opkg`/`apk` artifacts and
  // this host's manager for a `pica` one. `None` means nobody here installs a
  // dependency, or that a `pica` artifact declared none and has no reason to
  // touch a database at all.
  let deps_from = if touches_package_manager(ctx.pkgmgr, &precheck.deps) {
    dependency_backend(app, ctx.pkgmgr)
  } else {
    None
  };
  tx.deps_from = deps_from;

  if let Some(kind) = deps_from.filter(|kind| kind.requires_system_binary()) {
    // Without a baseline there is nothing a rollback could undo, so an
    // unreadable list stops the install here — before anything is changed —
    // instead of letting it proceed with a record that can only be wrong.
    let mut installed = app.manager_for(kind)?.installed()?;
    installed.sort();
    tx.tx_before = Some(installed);
  }

  // 5. Dependency containers. Every phase records what it changed, including a
  //    phase that fails partway (see `install_phase`), so a later failure has
  //    something to roll back.
  match deps_from {
    Some(kind) => app
      .with_backend(kind, |app| install_dependency_containers(app, ctx, precheck, tx, deps_from))?,
    None => install_dependency_containers(app, ctx, precheck, tx, deps_from)?,
  }

  // 5b. The bounds the precheck could not judge yet.
  match deps_from {
    Some(kind) => app.with_backend(kind, |app| {
      recheck_constraints(app, ctx, precheck);
      Ok(())
    })?,
    None => recheck_constraints(app, ctx, precheck),
  }

  if !tx.tx_changed.is_empty() {
    let changes: Vec<String> = tx
      .tx_changed
      .iter()
      .map(|(name, before, after)| format!("{name} {before} -> {after}"))
      .collect();
    app.log_warn(tr(Key::DepsChangedVersion, &[&changes.join(", ")]));
  }

  // 6. Lifecycle hook.
  execute_scripts(app, ctx, tx)?;

  // 7. The artifact's own payload.
  deploy_payload(app, ctx, tx)?;

  Ok(())
}
