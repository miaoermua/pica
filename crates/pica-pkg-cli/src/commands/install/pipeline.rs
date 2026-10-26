use super::feed::{
  DependencySource, dependency_source, install_pkgs_dir, pkg_list_diff_added, reorder_app_list,
};
use super::optional::{self, Plan as OptionalPlan};
use super::precheck::{
  DeclaredDeps, build_precheck_report, summarize_missing_precheck, validate_package,
};
use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_IO, E_MANIFEST_INVALID, E_PACKAGE_INVALID,
  E_RUNTIME, E_VERSION_INCOMPATIBLE, FeedPolicy, ensure_dirs, resolve_lang,
};
use crate::format;
use crate::i18n::{Key, Lang};
use crate::installed::EntrySource;
use crate::pkgmgr::PackageManagerKind;
use crate::state::{
  db_installed_version, db_record_remnant, db_set_installed, report_set_install_result,
};
use crate::system::{TargetRoot, run_hook, run_tar_extract};
use pica_pkg_core::PICA_VERSION;
use pica_pkg_core::constraint::{self, Constraint};
use pica_pkg_core::io::make_temp_dir;
use pica_pkg_core::io::{
  PayloadInstall, copy_symlink, deploy_file, install_payload_with, remove_payload,
};
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::version::{pkgver_cmp_key, pkgver_gt, ver_ge};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// The manifest fields the host checks act on. The package name is not among
/// them: it names the transaction, which the header already printed.
pub(super) struct PackageFields {
  pub(super) pkgver_display: String,
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
/// It carries the parsed declarations as well, because step 5 needs exactly the
/// list that was just reported, and recomputing it could disagree with the
/// report. The bounds live on in `bound` — but only for the recheck after step
/// 5: a backend is asked for `constraint::names`, never for the declaration.
struct DependencyPrecheck {
  report: Value,
  deps: DeclaredDeps,
  /// Where the artifact's declared dependencies come from, decided here rather
  /// than in step 5 because the selector has to know which optional
  /// dependencies are actually installable before it shows them.
  source: Option<DependencySource>,
  /// What the user chose (or what the mode decided) about the optional
  /// dependencies.
  optional: OptionalPlan,
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
fn transaction_header(pkgname: &str, new_version: &str, installed: Option<&str>) -> String {
  let Some(old) = installed else {
    return format!("Installing {pkgname} ({new_version})");
  };

  if old == new_version {
    return format!("{pkgname} {new_version} is already installed; reinstalling");
  }
  if pkgver_gt(new_version, old) {
    format!("Upgrading {pkgname} ({old} -> {new_version})")
  } else if pkgver_gt(old, new_version) {
    format!("Downgrading {pkgname} ({old} -> {new_version})")
  } else {
    // Two different strings that neither compares above the other: the header
    // does not claim a direction it could not establish.
    format!("Reinstalling {pkgname} ({new_version})")
  }
}

/// The size lines for the header, indented under it.
///
/// A figure is only shown when it is a usable number: a missing one was never
/// published, and a zero is a feed saying "not measured" rather than a package
/// that occupies nothing. Neither is worth printing, and printing `0 B` would be
/// worse than saying nothing.
fn size_lines(sizes: IndexSizes) -> Vec<String> {
  [("Package installed size", sizes.installed), ("Download size", sizes.download)]
    .into_iter()
    .filter_map(|(label, value)| {
      value.filter(|bytes| *bytes > 0).map(|bytes| format!("  {label}: {}", format::bytes(bytes)))
    })
    .collect()
}

/// `Installed 12 files, 74.2 MiB`, from what the deployment counted.
///
/// `files` is the payload's file list, which holds the symlinks as well as the
/// regular files; `size` sums the bytes of the regular files it wrote. Created
/// directories are not counted as files.
fn install_summary(lang: Lang, payload: &PayloadInstall) -> String {
  format!("Installed {}, {}", format::files(lang, payload.files.len()), format::bytes(payload.size))
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
fn pkgfile_into(
  app: &mut App,
  pkgfile: &Path,
  selector: Option<String>,
  source: EntrySource,
  sizes: IndexSizes,
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

  // Part of the artifact contract, checked here so a package cannot leave it
  // out. Nothing compares it against the host yet: `os`, `arch` and `uname` are
  // what `validate_package` acts on.
  let _platform = required_manifest_field(&manifest, "platform")?;

  let pkg = PackageFields {
    pkgver_display: pkgver_cmp_key(&pkgver, &pkgrel),
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

  // One lookup answers both questions: whether this replaces an installed
  // package, and what the header has to say about the version it replaces.
  let installed_version = db_installed_version(&app.paths.db_file, &pkgname)?;
  let is_upgrade = installed_version.is_some();

  if !pica_required.is_empty() && !ver_ge(PICA_VERSION, &pica_required) {
    return Err(CliError::new(
      E_VERSION_INCOMPATIBLE,
      format!("pica too old: pkg requires >= {pica_required}, cli is {PICA_VERSION}"),
    ));
  }

  // The transaction says what it is before it touches anything, so every line
  // after this one — including a failure — belongs to a package the user can
  // identify. Sizes come first in the index and say what the package claims;
  // what was actually written is reported at the end.
  app.log_info(transaction_header(&pkgname, &pkg.pkgver_display, installed_version.as_deref()));
  for line in size_lines(sizes) {
    app.log_info(line);
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
  if let Err(err) = record_installation(app, &ctx, &precheck, &tx, false) {
    return Err(abandon(app, &ctx, &precheck, &mut tx, err));
  }

  // Only now is the install complete: the backup can be marked, then removed.
  tx.mark_committed(app);
  tx.finish(app);

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
/// The question is concretely "does this backend have a system package manager
/// to ask?". `pica` assumes none exists on the host — it unpacks its own
/// containers and has nobody to hand a dependency list to — so the answer is
/// `false` there by construction, not because a solver is missing. Owning files
/// and having dependencies installed elsewhere stay separate questions, but only
/// the first is a property of the artifact.
/// Both call sites that ask it — the step 3 gate and the D3 check in
/// `validate_package` — must agree, so the predicate lives in one place.
pub(super) fn installs_declared_dependencies(pkgmgr: &str) -> bool {
  PackageManagerKind::parse(pkgmgr).is_some_and(PackageManagerKind::requires_system_binary)
}

fn non_empty(items: Vec<String>) -> Vec<String> {
  items.into_iter().filter(|item| !item.is_empty()).collect()
}

/// Brings the host's dependency index up to date, or establishes that this
/// artifact does not need it.
///
/// The precheck below calls a dependency missing when the backend's index does
/// not list it, so an index that cannot be read has to stop the run: a router
/// with unreachable feeds would otherwise be told its dependencies were missing
/// and the install would be refused for them.
///
/// An artifact that requires nothing has nothing the index could judge. Its
/// optional entries are skipped by the missing summary and already degrade to
/// "unknown" on their own, so an unreadable index there costs those options
/// their offer and nothing else — and blocking the install would turn an
/// optional dependency into a mandatory one.
fn ensure_index(app: &mut App, deps: &DeclaredDeps) -> CliResult<()> {
  let Err(err) = app.package_manager()?.update_index() else {
    return Ok(());
  };
  if declares_required(deps) {
    return Err(err);
  }
  app.log_warn(format!(
    "dependency index unavailable, optional dependencies cannot be offered: {}",
    err.message
  ));
  Ok(())
}

/// Whether any of the three groups declares something required, `kmod`
/// included: the index answers for kernel modules exactly as it does for
/// packages, so a group left out here would unlock the downgrade on an artifact
/// that still needs the answer.
fn declares_required(deps: &DeclaredDeps) -> bool {
  [&deps.kmod, &deps.base, &deps.app]
    .into_iter()
    .any(|constraints| constraint::required(constraints).next().is_some())
}

/// Step 3: works out which dependencies this install would need and whether they
/// can be satisfied.
///
/// Read-only: it drives the package manager's index and queries only, so a
/// failure here leaves nothing behind to roll back.
fn precheck_dependencies(app: &mut App, ctx: &InstallContext<'_>) -> CliResult<DependencyPrecheck> {
  let pkgs_dir = ctx.tmpdir.join(PKGS);

  // Under `pica` there is no feed to consult and no resolver to run. A package
  // with required dependencies was already refused by `validate_package`; only
  // optional ones can still be there, and they are recorded as unavailable
  // instead of being dropped, so the report says the manifest asked for them.
  if !installs_declared_dependencies(ctx.pkgmgr) {
    let declared_app = non_empty(ctx.manifest.get_array_for("app", ctx.pkgmgr));
    let deps = DeclaredDeps::parse(ctx.manifest, ctx.pkgmgr, &declared_app)?;
    let optional = OptionalPlan::unavailable(&deps, "pkgmgr=pica cannot install dependencies");
    return Ok(DependencyPrecheck {
      report: json!({"kmod": [], "base": [], "app": []}),
      deps,
      source: None,
      optional,
    });
  }

  let mut app_list = non_empty(ctx.manifest.get_array_for("app", ctx.pkgmgr));
  let lang = resolve_lang(&app.paths.conf_file);
  let i18n_template = ctx.manifest.get_first("app_i18n");
  if !i18n_template.is_empty() && lang == "zh-cn" {
    app_list.push(i18n_template.replace("{lang}", &lang));
  }
  app_list = reorder_app_list(app_list);

  // Names what the precheck below actually does: it reads the artifact's
  // declarations, asks the backend's index about each one and reports where it
  // would come from. It is not a solver — no version is chosen, no conflict
  // between packages is detected — so it must not be called one.
  app.log_info("Checking declared dependencies...");

  let deps = DeclaredDeps::parse(ctx.manifest, ctx.pkgmgr, &app_list)?;

  ensure_index(app, &deps)?;

  let report = build_precheck_report(app, ctx.manifest, &pkgs_dir, &deps)?;
  let missing = summarize_missing_precheck(&report);
  if !missing.is_empty() {
    app.log_warn(format!("dependency precheck: missing {}", missing.join(" ")));
    let hint = pica_package_hint(app, &missing);
    return Err(CliError::new(
      E_RUNTIME,
      format!("dependency precheck failed, missing: {}{hint}", missing.join(" ")),
    ));
  }

  // The source is decided before the selector, not in step 5: an option that
  // cannot be installed from the policy's source must not be offered.
  let source = dependency_source_for(app, &deps, pkgs_dir.is_dir())?;
  let optional = optional::plan(app, ctx.pkgname, &deps, &report, source);
  optional.warn_skipped(app);
  if optional.selected_count() > 0 {
    app.log_info(format!(
      "Installing {} optional dependencies on request",
      optional.selected_count()
    ));
  }
  for line in dependency_source_lines(&report, &optional) {
    app.log_info(line);
  }

  Ok(DependencyPrecheck { report, deps, source, optional })
}

/// Says, for the missing names that are published as Pica packages, why that
/// does not make them installable.
///
/// A dependency name is resolved against the backend's installed set, the
/// backend's feeds and the artifact's own `pkgs/`. A Pica package is in none of
/// those: pica keeps its own ledger and never reads another artifact's manifest,
/// so "it is in the source" and "it can be a dependency" are different things.
/// Naming that difference is the whole point — otherwise the user reads "missing"
/// and goes looking for the package they just saw listed.
///
/// The synced index is read directly, without the host filters the selector
/// applies: the question here is whether the name is published at all, not
/// whether it would install on this machine. No network request is made. An
/// index that cannot be read is not an answer about the sources, so the hint is
/// omitted rather than claiming the package is unavailable.
fn pica_package_hint(app: &App, missing: &[String]) -> String {
  let Ok(index) = crate::state::read_json_file(&app.paths.index_file) else {
    return String::new();
  };
  let Some(repos) = index.get("repos").and_then(Value::as_object) else {
    return String::new();
  };

  let mut found = Vec::new();
  for entry in missing {
    let name = entry.split_once(':').map_or(entry.as_str(), |(_, name)| name);
    let published = repos.values().flat_map(crate::state::repo_package_sets).any(|packages| {
      packages.iter().any(|pkg| {
        pkg.get("appname").and_then(Value::as_str) == Some(name)
          || pkg.get("pkgname").and_then(Value::as_str) == Some(name)
      })
    });
    if published {
      found.push(name.to_string());
    }
  }
  if found.is_empty() {
    return String::new();
  }
  format!(
    "; {} exist in the Pica sources, but pica does not resolve Pica packages as dependencies — \
     declare a system package name instead, or ship the container in this artifact's pkgs/",
    found.join(" ")
  )
}

/// The container-or-feeds decision, taken once in the precheck.
///
/// [`dependency_source`] answers for the required dependencies and keeps its
/// existing errors and prompts. The one case it does not answer is an artifact
/// whose required lists are empty: it returns `None` there, and an optional
/// dependency still needs a source to be installable. With no `pkgs/` the feeds
/// are the only candidate, and `packaged-only` is the one policy that forbids
/// them — in which case there is nothing to offer and the install proceeds.
fn dependency_source_for(
  app: &mut App,
  deps: &DeclaredDeps,
  have_pkgs_dir: bool,
) -> CliResult<Option<DependencySource>> {
  let declared: Vec<String> =
    required_names(&deps.base).into_iter().chain(required_names(&deps.app)).collect();

  if declared.is_empty() && !have_pkgs_dir {
    return Ok(match app.options.feed_policy {
      FeedPolicy::PackagedOnly => None,
      _ => Some(DependencySource::Feeds),
    });
  }

  dependency_source(app, &declared, have_pkgs_dir)
}

/// The declarations a backend must be given: the required ones.
fn required_names(constraints: &[Constraint]) -> Vec<String> {
  constraint::required(constraints).map(|constraint| constraint.name.clone()).collect()
}

/// The dependencies this install has to do something about, each with the source
/// the precheck found for it: `kmod-tun (feed)`, `helper (packaged)`.
///
/// Installed ones are left out — nothing is done about them — and the status
/// word is the one the precheck recorded, so the line cannot claim a source the
/// check did not find. An optional dependency is listed only when this install
/// covers it: a declined one is not being brought in, and saying `(feed)` next to
/// it would read as though it were.
fn dependency_source_lines(report: &Value, optional: &OptionalPlan) -> Vec<String> {
  let mut lines = Vec::new();

  for group in crate::constraints::GROUPS {
    let Some(entries) = report.get(group).and_then(Value::as_array) else {
      continue;
    };
    for entry in entries {
      let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
      if status != "feed" && status != "packaged" {
        continue;
      }
      let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
      if name.is_empty() {
        continue;
      }
      let is_optional = entry.get("optional").and_then(Value::as_bool).unwrap_or(false);
      if is_optional && !optional.covers(group, name) {
        continue;
      }
      lines.push(format!("  {name} ({status})"));
    }
  }

  lines
}

/// One feed phase: what must be installed plus what the user chose.
///
/// `resolve` already removed any name declared both ways, so the two halves
/// cannot hold the same package and the request holds each name once.
fn phase_names(
  constraints: &[Constraint],
  precheck: &DependencyPrecheck,
  group: &str,
) -> Vec<String> {
  let mut names = required_names(constraints);
  names.extend(precheck.optional.selected_names(group));
  names
}

/// Installs one dependency phase, skipping what is already installed.
///
/// The filter is what keeps an install from touching packages it did not come
/// for: a backend asked for a name it already has may upgrade it, and pica has
/// no upgrade policy on this path — that decision belongs to `-Su`. A dependency
/// whose installed version misses a declared bound never reaches here: the
/// precheck refuses it when it is required, and the selector never offers it
/// when it is optional.
///
/// The bookkeeping runs whatever the command returned, because a backend that
/// fails partway has still installed part of the request. See
/// [`InstallTransaction::record_backend_changes`].
fn install_phase(
  app: &mut App,
  tx: &mut InstallTransaction,
  label: &str,
  names: &[String],
) -> CliResult<()> {
  if names.is_empty() {
    return Ok(());
  }

  let missing: Vec<String> = {
    let manager = app.package_manager()?;
    names.iter().filter(|name| !manager.is_installed(name)).cloned().collect()
  };
  if missing.is_empty() {
    return Ok(());
  }

  let result = app.package_manager()?.install(label, &missing);
  tx.record_backend_changes(app);
  result
}

/// Step 5: installs whatever the artifact's dependencies need, and nothing when
/// it has none.
///
/// A system backend is asked to install the app/base/kmod lists — from its feeds
/// when the policy says so, from the containers in the artifact otherwise. Which
/// of the two is decided once, before either phase runs: see
/// [`dependency_source`]. The two phases are then either two feed requests, in
/// `base`-then-`app` order, or one request holding the whole container batch.
///
/// The `pica` backend has no feeds and no solver, so it is given no dependency
/// work at all: its artifact ships `root/` and `scripts/`, and `deploy_payload`
/// lays those files down. A `pica` artifact carrying `pkgs/` is refused instead
/// of half-installed: pica does not read container formats, so it could neither
/// install the container nor account for the files inside it.
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
          "a pica-backend artifact must not carry {PKGS}/: a pica artifact ships {ROOT}/ \
           and scripts/, and pica does not hand containers to opkg or apk. Rebuild it with \
           `targets = opkg` or `targets = apk` so the device's package manager installs \
           them, or extract them into {ROOT}/ before packing"
        ),
      ));
    }
    return Ok(());
  }

  // kmods are always resolved through the feeds: an OpenWrt kernel module is not
  // something an application artifact can carry (see `build_precheck_report`).
  // One request for the missing ones, so the backend sees the set at once.
  // A chosen optional kmod joins that one request; an installed one never does,
  // which is the rule every other phase follows too (`install_phase`).
  let mut kmods = required_names(&precheck.deps.kmod);
  kmods.extend(precheck.optional.selected_names("kmod"));
  install_phase(app, tx, "kmod", &kmods)?;

  // The source was decided in the precheck (see `dependency_source_for`), so the
  // selector could only offer what this install can deliver.
  let Some(source) = precheck.source else {
    return Ok(());
  };

  match source {
    // Feeds are installed in phases, base first: a base dependency has to be on
    // the device before the app asks its own resolver for anything. Each phase is
    // one request, so a name in a phase can depend on a sibling of the same phase.
    // Only the names go into the requests: the bounds are pica's to check, and
    // opkg would read `libfoo>=1.2` as a package called that.
    DependencySource::Feeds => {
      install_phase(app, tx, "base", &phase_names(&precheck.deps.base, precheck, "base"))?;
      let before_app = app.package_manager()?.snapshot_installed();
      install_phase(app, tx, "app", &phase_names(&precheck.deps.app, precheck, "app"))?;
      let after_app = app.package_manager()?.snapshot_installed();
      match (before_app, after_app) {
        (Ok(before), Ok(after)) => tx.app_added = pkg_list_diff_added(&before, &after),
        // Which packages the `app` phase added is a report field; no rollback
        // depends on it. An unreadable list costs the report, not the install —
        // and leaving it empty is better than claiming they were all added.
        (before, after) => {
          let err = before.err().or(after.err()).expect("one of the two failed");
          app.log_warn(format!(
            "could not tell which packages the app phase added: {}",
            err.message
          ));
        }
      }
    }
    // The artifact's containers are one batch, installed once. Nothing in the
    // artifact says which container serves `base` and which serves `app`, and the
    // containers of one application depend on each other, so the batch is left
    // whole: a split would be a guess, and a guess that separates two dependent
    // containers fails the install outright.
    DependencySource::Containers => {
      let result = install_pkgs_dir(app, "packaged", &pkgs_dir);
      // A batch that fails partway has still put some of its containers on the
      // device, and the backend may have pulled dependencies in with them.
      tx.record_backend_changes(app);
      result?;
    }
  }

  Ok(())
}

/// Re-checks the artifact's bounds once the backend has installed its part.
///
/// The precheck can only refuse a bound the system already breaks: a dependency
/// that was not installed then is expected to arrive at the right version, and
/// the backend may still install an older one — a pinned feed, or a container
/// that carries its own copy. The mismatch is reported rather than undone: the
/// containers are already on disk, and pica does not overrule the backend's
/// choice of version by downgrading it.
fn recheck_constraints(app: &mut App, ctx: &InstallContext<'_>, precheck: &DependencyPrecheck) {
  for (group, constraints) in
    [("kmod", &precheck.deps.kmod), ("base", &precheck.deps.base), ("app", &precheck.deps.app)]
  {
    // Only required dependencies and the optional ones this install actually
    // put on the device: a declined one that happens to be installed was already
    // judged by the precheck, and warning about it again would blame this
    // install for a version it did not choose.
    let checked: Vec<Constraint> = constraints
      .iter()
      .filter(|dep| !dep.optional || precheck.optional.covers(group, &dep.name))
      .cloned()
      .collect();

    for message in crate::constraints::unmet(app, ctx.pkgname, group, &checked) {
      app.log_warn(message);
    }
  }
}

/// Steps 4 to 7: the part of an install that changes the system.
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
  //    database to snapshot; `pica` keeps its own record of the paths it wrote.
  //    Versions are kept, not just names: they are what tells a package this
  //    install added from one it upgraded.
  if app.backend().requires_system_binary() {
    // Without a baseline there is nothing a rollback could undo, so an
    // unreadable list stops the install here — before anything is changed —
    // instead of letting it proceed with a record that can only be wrong.
    let mut installed = app.package_manager()?.installed()?;
    installed.sort();
    tx.tx_before = Some(installed);
  }

  // 5. Dependency containers. Every phase records what it changed, including a
  //    phase that fails partway (see `install_phase`), so a later failure has
  //    something to roll back.
  install_dependency_containers(app, ctx, precheck, tx)?;

  // 5b. The bounds the precheck could not judge yet.
  recheck_constraints(app, ctx, precheck);

  if !tx.tx_changed.is_empty() {
    let changes: Vec<String> = tx
      .tx_changed
      .iter()
      .map(|(name, before, after)| format!("{name} {before} -> {after}"))
      .collect();
    app.log_warn(format!(
      "dependencies changed version during this install and a rollback would not restore them: {}",
      changes.join(", ")
    ));
  }

  // 6. Lifecycle hook.
  execute_scripts(app, ctx, tx)?;

  // 7. The artifact's own payload.
  deploy_payload(app, ctx, tx)?;

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
  let backup_dir = kept_scripts_backup(&target_dir);
  let dir_existed = target_dir.is_dir();

  // A backup from an earlier run is dealt with before this one takes its own:
  // a finished one is removed, anything else is moved to a name of its own so
  // it cannot be mistaken for this transaction's.
  let canonical_free = reconcile_scripts_backup(app, &backup_dir);

  // An upgrade replaces the kept scripts instead of merging into them, and the
  // old ones are the only copy: they are moved aside rather than deleted. The
  // transaction owns the backup — rollback puts it back, a finished install
  // marks it committed and removes it — so a failure anywhere later still
  // restores them. A rename inside the same directory is atomic and copies
  // nothing.
  let mut backed_up = false;
  if dir_existed && canonical_free {
    let marker = ScriptsMarker::pending(ctx);
    match begin_scripts_backup(app, &target_dir, &backup_dir, marker, |path, text| {
      fs::write(path, text)
    })? {
      BackupStart::Active(marker) => {
        backed_up = true;
        tx.record_scripts_backup(target_dir.clone(), backup_dir, marker);
      }
      BackupStart::None => {}
    }
  }
  // Everything under the directory is this install's own — unless the old files
  // are still sitting there, in which case a copy would be a replacement and
  // the path must stay out of the rollback record.
  let record_copies = !dir_existed || backed_up;

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
    let mut created = PayloadInstall::default();
    push_missing_ancestors(ctx.target, &target, &mut created);
    // Directories are recorded only when the kept-scripts directory did not
    // exist before this install: an upgrade must not turn a pre-existing
    // directory into something `-R` may delete later. A rollback of an upgrade
    // removes the whole directory and renames the backup over it instead.
    if !dir_existed {
      tx.record_created(created);
    }

    if let Some(parent) = target.parent() {
      ensure_dir(parent)?;
    }

    let path = target.display().to_string();
    match fs::copy(&source, &target) {
      Ok(_) => {
        if record_copies {
          tx.record_created(PayloadInstall { files: vec![path], ..PayloadInstall::default() });
        }
        kept_any = true;
      }
      Err(err) => {
        // A copy opens the destination first, so a failed one can leave a file
        // behind — one of this install's own, because the old files are either
        // gone or were never recorded.
        if record_copies && target.symlink_metadata().is_ok() {
          tx.record_created(PayloadInstall { files: vec![path], ..PayloadInstall::default() });
        }
        return Err(CliError::new(E_IO, format!("copy script failed: {err}")));
      }
    }
  }

  if !kept_any && target_dir.is_dir() {
    let _ = fs::remove_dir_all(&target_dir);
  }

  Ok(())
}

/// Where the previous kept scripts wait while an upgrade writes its own.
fn kept_scripts_backup(dir: &Path) -> PathBuf {
  let mut name = dir.as_os_str().to_os_string();
  name.push(".rollback");
  PathBuf::from(name)
}

/// The marker that describes the backup it sits beside: `<backup>.json`.
fn kept_scripts_marker(backup: &Path) -> PathBuf {
  let mut name = backup.as_os_str().to_os_string();
  name.push(".json");
  PathBuf::from(name)
}

/// What the marker beside a backup says about the transaction that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptsState {
  /// The backup was taken and the install has not finished yet.
  Pending,
  /// `record_installation` returned Ok: the new scripts are in the database.
  Committed,
}

impl ScriptsState {
  fn as_str(self) -> &'static str {
    match self {
      Self::Pending => "pending",
      Self::Committed => "committed",
    }
  }

  fn parse(text: &str) -> Option<Self> {
    match text {
      "pending" => Some(Self::Pending),
      "committed" => Some(Self::Committed),
      _ => None,
    }
  }
}

/// The marker schema this build writes and understands.
const SCRIPTS_MARKER_SCHEMA: u64 = 1;

/// One upgrade's backup of the kept scripts.
struct ScriptsBackup {
  target: PathBuf,
  backup: PathBuf,
  marker: ScriptsMarker,
}

/// What a backup directory is, written beside it.
///
/// It exists so a backup left behind by a crash can be told apart from one that
/// belongs to a finished install. `state` is the only input to the cleanup
/// decision: a version that happens to match the database says nothing about
/// which transaction made the backup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScriptsMarker {
  txid: String,
  pkgname: String,
  pkgver: String,
  pkgrel: String,
  state: ScriptsState,
}

impl ScriptsMarker {
  fn pending(ctx: &InstallContext<'_>) -> Self {
    Self {
      txid: new_txid(),
      pkgname: ctx.pkgname.to_string(),
      pkgver: ctx.manifest.get_first("pkgver"),
      pkgrel: ctx.manifest.get_first("pkgrel"),
      state: ScriptsState::Pending,
    }
  }

  fn to_json(&self) -> String {
    json!({
      "schema": SCRIPTS_MARKER_SCHEMA,
      "txid": self.txid,
      "pkgname": self.pkgname,
      "pkgver": self.pkgver,
      "pkgrel": self.pkgrel,
      "state": self.state.as_str(),
    })
    .to_string()
  }

  /// `None` for anything this build cannot vouch for: unreadable JSON, another
  /// schema, a state it does not know.
  fn parse(text: &str) -> Option<Self> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("schema").and_then(Value::as_u64)? != SCRIPTS_MARKER_SCHEMA {
      return None;
    }
    Some(Self {
      txid: value.get("txid")?.as_str()?.to_string(),
      pkgname: value.get("pkgname")?.as_str()?.to_string(),
      pkgver: value.get("pkgver")?.as_str()?.to_string(),
      pkgrel: value.get("pkgrel")?.as_str()?.to_string(),
      state: ScriptsState::parse(value.get("state")?.as_str()?)?,
    })
  }
}

fn new_txid() -> String {
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_or(0, |elapsed| elapsed.as_nanos());
  format!("{nanos:x}-{:x}", std::process::id())
}

/// What to do with a backup found at the start of an install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupDisposition {
  /// The transaction that made it finished: the scripts it holds are
  /// superseded, and the database already names their replacements.
  DeleteCommitted,
  /// Anything else — pending, missing marker, unreadable marker, unknown
  /// schema. The backup is kept under a name of its own; it is never guessed to
  /// be finished, because it may be the only copy of those scripts.
  MoveAside,
}

fn backup_disposition(marker: Option<&ScriptsMarker>) -> BackupDisposition {
  match marker {
    Some(marker) if marker.state == ScriptsState::Committed => BackupDisposition::DeleteCommitted,
    _ => BackupDisposition::MoveAside,
  }
}

/// The outcome of trying to take a backup.
enum BackupStart {
  /// The old scripts are in the backup and the caller must tell the transaction.
  Active(ScriptsMarker),
  /// No backup: there was nothing to move, or it could not be tracked.
  None,
}

/// Moves the old kept scripts aside and writes the marker that describes them.
///
/// The marker is written *after* the rename and before anything else: a backup
/// nobody can classify is worse than no backup, because the next run could not
/// tell whether it may clean it up. If the marker cannot be written the old
/// directory is put back and the caller continues without a backup; if even
/// that fails the only copy is in the backup, so the error names it and the
/// install stops.
fn begin_scripts_backup(
  app: &mut App,
  target: &Path,
  backup: &Path,
  marker: ScriptsMarker,
  write: impl FnOnce(&Path, &str) -> std::io::Result<()>,
) -> CliResult<BackupStart> {
  if let Err(err) = fs::rename(target, backup) {
    app.log_warn(format!(
      "could not move the kept scripts aside ({err}); this upgrade leaves them in place, and the \
       scripts it writes will not be rolled back"
    ));
    return Ok(BackupStart::None);
  }

  let marker_path = kept_scripts_marker(backup);
  if let Err(err) = write(&marker_path, &marker.to_json()) {
    return match fs::rename(backup, target) {
      Ok(()) => {
        app.log_warn(format!(
          "could not write the rollback marker for {} ({err}); the upgrade continues without \
           script rollback",
          backup.display()
        ));
        Ok(BackupStart::None)
      }
      Err(restore) => Err(CliError::new(
        E_IO,
        format!(
          "could not write the rollback marker for {} ({err}), and the previous scripts could not \
           be moved back from it: {restore}",
          backup.display()
        ),
      )),
    };
  }

  Ok(BackupStart::Active(marker))
}

/// Puts the backup back where the kept scripts belong.
fn restore_scripts_backup(
  target: &Path,
  backup: &Path,
  mv: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
  // The rename needs its destination gone; everything under it belongs to the
  // transaction that moved the old scripts aside.
  let _ = fs::remove_dir_all(target);
  mv(backup, target)
}

/// Moves an old backup out of the canonical name and brings its marker along.
///
/// Returns whether the canonical name is free for this install. Every
/// destination is *claimed* before it is used: `create_dir` and an exclusive
/// create both fail when something is already there, so an existing file or
/// directory — empty or not — is never replaced. `std` has no
/// `renameat2(RENAME_NOREPLACE)` and this workspace forbids `unsafe`, so the
/// claim and the move are two steps: a third party writing into the claimed
/// path between them can only make the move fail, never lose the backup.
fn move_aside_scripts_backup(
  app: &mut App,
  backup: &Path,
  marker_path: &Path,
  marker: Option<&ScriptsMarker>,
) -> bool {
  let suffix = marker.map_or_else(new_txid, |marker| marker.txid.clone());
  let stale = PathBuf::from(format!("{}.{suffix}", backup.display()));

  // Claiming the name atomically: this fails for a file, an empty directory and
  // a full one alike, and never touches what is already there.
  if let Err(err) = fs::create_dir(&stale) {
    app.log_warn(format!(
      "an earlier upgrade left its previous kept scripts at {}, and {} is already in the way \
       ({err}); this upgrade will not back up the current ones",
      backup.display(),
      stale.display()
    ));
    return false;
  }

  // Renaming onto the empty directory just created replaces only that.
  if let Err(err) = fs::rename(backup, &stale) {
    let _ = fs::remove_dir(&stale);
    app.log_warn(format!(
      "an earlier upgrade left its previous kept scripts at {}, which could not be moved aside \
       ({err}); this upgrade will not back up the current ones",
      backup.display()
    ));
    return false;
  }

  // The file's existence decides this, not whether it parses: a marker this
  // build cannot read still belongs to the backup being moved, and leaving it
  // at the canonical name would let the next backup overwrite it.
  if marker_path.exists() {
    let marker_target = kept_scripts_marker(&stale);
    match fs::OpenOptions::new().write(true).create_new(true).open(&marker_target) {
      Ok(_) => {
        if let Err(err) = fs::rename(marker_path, &marker_target) {
          let _ = fs::remove_file(&marker_target);
          let restored = fs::rename(&stale, backup).is_ok();
          app.log_warn(format!(
            "the previous kept scripts were moved to {}, but their marker could not follow \
             ({err}){}",
            stale.display(),
            if restored {
              "; the backup was put back where it was"
            } else {
              "; the backup stays there and its marker at its old name"
            }
          ));
          return false;
        }
      }
      Err(err) => {
        // Whatever occupies the marker's destination is left untouched, and the
        // directory move is undone so this install cannot overwrite the marker
        // that still sits at its canonical name.
        let restored = fs::rename(&stale, backup).is_ok();
        app.log_warn(format!(
          "a previous backup marker is in the way at {} ({err}){}",
          marker_target.display(),
          if restored {
            "; the previous backup was put back where it was"
          } else {
            "; the backup stays at the name it was moved to"
          }
        ));
        return false;
      }
    }
  }

  app.log_warn(format!(
    "an earlier upgrade left its previous kept scripts at {}, which may still be the only copy; \
     they were moved to {} and left there",
    backup.display(),
    stale.display()
  ));
  true
}

/// Deals with a backup left by an earlier run, and reports whether the
/// canonical backup name is free for this one.
///
/// A backup whose marker says `committed` is removed: that state is only ever
/// written after the database record landed, so the scripts it holds are
/// superseded. Anything else is moved to a name of its own and left alone —
/// pending (the install never finished) or a marker this build cannot read (it
/// must not guess). Neither is ever deleted.
fn reconcile_scripts_backup(app: &mut App, backup: &Path) -> bool {
  let marker_path = kept_scripts_marker(backup);

  if !backup.exists() {
    // A marker with no directory beside it: the two renames of an earlier move
    // are not one operation, so this is what a crash between them leaves. It
    // describes a backup that is no longer at this name, so it is moved aside —
    // never overwritten by the marker this install is about to write.
    return move_orphan_marker_aside(app, &marker_path, &new_txid());
  }

  let marker = fs::read_to_string(&marker_path).ok().and_then(|text| ScriptsMarker::parse(&text));

  match backup_disposition(marker.as_ref()) {
    BackupDisposition::DeleteCommitted => {
      delete_committed_backup(app, backup, &marker_path, |file| fs::remove_file(file))
    }
    BackupDisposition::MoveAside => {
      move_aside_scripts_backup(app, backup, &marker_path, marker.as_ref())
    }
  }
}

/// Removes a backup whose transaction finished, and its marker.
///
/// Returns whether both canonical names are free. A marker that cannot be
/// removed blocks the new backup: writing over it would erase the only
/// description of a backup the next run may still need to classify.
fn delete_committed_backup(
  app: &mut App,
  backup: &Path,
  marker_path: &Path,
  remove_marker: impl FnOnce(&Path) -> std::io::Result<()>,
) -> bool {
  if let Err(err) = fs::remove_dir_all(backup) {
    app.log_warn(format!(
      "a finished upgrade left its previous kept scripts at {}, which could not be removed \
       ({err}); this upgrade will not back up the current ones",
      backup.display()
    ));
    return false;
  }

  match remove_marker(marker_path) {
    Ok(()) => true,
    Err(err) => {
      app.log_warn(format!(
        "the previous kept scripts at {} were removed, but their marker could not be ({err}); \
         this upgrade will not back up the current ones",
        backup.display()
      ));
      false
    }
  }
}

/// Moves a marker that has no backup beside it out of the canonical name.
///
/// Returns whether the canonical name is free. The destination is claimed with
/// an exclusive create first, so an existing file or directory there is never
/// replaced; when the claim fails the marker stays where it is and this install
/// takes no backup, because writing its own marker would overwrite this one.
fn move_orphan_marker_aside(app: &mut App, marker_path: &Path, suffix: &str) -> bool {
  if !marker_path.exists() {
    return true;
  }

  let stale = PathBuf::from(format!("{}.orphan.{suffix}", marker_path.display()));
  match fs::OpenOptions::new().write(true).create_new(true).open(&stale) {
    Ok(_) => match fs::rename(marker_path, &stale) {
      Ok(()) => {
        app.log_warn(format!(
          "a marker without its backup was found at {} and moved to {}; its backup is no longer \
           at that path",
          marker_path.display(),
          stale.display()
        ));
        true
      }
      Err(err) => {
        let _ = fs::remove_file(&stale);
        app.log_warn(format!(
          "a marker without its backup is at {} and could not be moved aside ({err}); this \
           upgrade will not back up the current ones",
          marker_path.display()
        ));
        false
      }
    },
    Err(err) => {
      app.log_warn(format!(
        "a marker without its backup is at {}, and {} is already in the way ({err}); this upgrade \
         will not back up the current ones",
        marker_path.display(),
        stale.display()
      ));
      false
    }
  }
}

/// Step 7: installs the artifact's declarative payload, recording every path it
/// creates in `tx` so removal can undo exactly this install.
///
/// `root/` is mirrored onto the target rootfs (so `root/etc/init.d/demo` becomes
/// `/etc/init.d/demo`), while kept lifecycle scripts and the env file are copied
/// into Pica's own directories.
fn deploy_payload(
  app: &App,
  ctx: &InstallContext<'_>,
  tx: &mut InstallTransaction,
) -> CliResult<()> {
  let target = ctx.target;
  let mut payload = PayloadInstall::default();

  let root_dir = ctx.tmpdir.join(ROOT);
  // Before the copies, not merely before the walk: a refusal that happened after
  // the snapshot would leave every replaced file copied out and rewritten back on
  // the way out, so "nothing on the device has changed" would be false for a file
  // whose bytes and inode quietly did.
  refuse_pica_state(&app.paths, target, &root_dir)?;
  // The copies go first, and the store exists even for a payload that turns out
  // to replace nothing: the env file below is a destination too.
  tx.prepare_payload_backup(app, ctx.pkgname, target, &root_dir)?;
  if root_dir.is_dir() {
    // Enumerated *before* the walk: only a path that does not exist yet can be
    // one the walk creates, and only such a path may be removed again.
    let intended = intended_payload(target, &root_dir);
    // The walk records into `walked` as it goes, so a walk that stops on an error
    // still says which destinations it got to — which is exactly what the rollback
    // needs, and what a plain `Result` throws away.
    let mut walked = PayloadInstall::default();
    let walked_error = install_payload_with(&root_dir, target.as_path(), &mut walked).err();
    tx.note_deployed(&walked);
    match walked_error {
      None => {
        let installed = walked;
        // Recorded here, not at the end of this function: everything below can
        // fail, and a failure there must not lose the files that are already on
        // disk. Only the paths this walk created may be removed again — the
        // result also lists the files it replaced, and deleting one of those
        // would take away a path that existed before the install. `intended`
        // holds the paths that were missing before the walk, so the intersection
        // is exactly what this install created; the ones it replaced go back
        // from the backup instead.
        tx.record_created(PayloadInstall {
          files: installed
            .files
            .iter()
            .filter(|file| intended.files.contains(file))
            .cloned()
            .collect(),
          // The walker only pushes the directories it had to create.
          dirs: installed.dirs.clone(),
          size: installed.size,
        });
        payload.files.extend(installed.files);
        payload.dirs.extend(installed.dirs);
        payload.size += installed.size;
      }
      Some(err) => {
        // `install_payload` validates as it copies, so a payload it rejects can
        // still have created files before it got there. The paths that are on disk now are recorded:
        // every one of them was missing before the walk, so every one of them is
        // this install's own to remove, and a path that turns out not to have
        // been reached is left out rather than claimed. What the walk *did*
        // replace is not in this list at all — the backup puts that back.
        let landed = existing_only(intended);
        tx.record_payload(landed.clone());
        tx.record_created(landed);
        return Err(CliError::from(err));
      }
    }
  }

  let env_file = ctx.tmpdir.join(SCRIPTS).join(".env");
  if env_file.is_file() {
    let env_dir = target.resolve(ENV_DIR);
    let file = env_dir.join(format!("{}.env", ctx.pkgname));
    // A directory this install had to create is its own; recording it before
    // the copy means a copy that fails does not leave it behind. Directories
    // that already existed are not recorded, and `remove_payload` only removes
    // a recorded one when it ends up empty.
    let mut created_dirs = PayloadInstall::default();
    push_missing_ancestors(target, &file, &mut created_dirs);
    ensure_dir(&env_dir)?;
    tx.record_created(created_dirs);

    let existed = file.symlink_metadata().is_ok();
    if existed {
      // The env file is Pica's own metadata, but a package replacing it is the
      // same destruction as replacing a payload file: the operator's copy goes
      // into the same store and comes back from the same rollback.
      tx.back_up_destination(&file)?;
    }
    let written = copy_recording_new_target(tx, &file, existed, || fs::copy(&env_file, &file))
      .map_err(|err| CliError::new(E_IO, format!("copy env file failed: {err}")))?;
    payload.files.push(file.display().to_string());
    payload.size += written;
  }

  let scripts_dir = kept_scripts_dir(target, ctx.pkgname);
  if scripts_dir.is_dir() {
    payload.dirs.push(scripts_dir.display().to_string());
    if let Ok(entries) = fs::read_dir(&scripts_dir) {
      for entry in entries.flatten() {
        if entry.path().is_file() {
          // Kept scripts are Pica's own files, so their bytes belong in what
          // this install wrote — the payload walk did not count them.
          payload.size += entry.metadata().map_or(0, |meta| meta.len());
          payload.files.push(entry.path().display().to_string());
        }
      }
    }
  }

  tx.record_payload(payload);

  Ok(())
}

/// Refuses a payload that would write Pica's own state.
///
/// `etc/pica/pica.json`, `var/lib/pica/db.json` and the rest are the files this
/// install is about to read and rewrite. A package that ships one of them would
/// otherwise be deployed like any other file: the database it replaced is the
/// one the commit then loads, and a rollback would put the package's copy back
/// instead of the record that was there.
///
/// The directories matter as much as the files. The lock lives in
/// `<state>/db.lck.d/pid`, and a payload that writes a pid there makes the lock
/// answer for a process that is not running, so the next `pica` reclaims it while
/// this install is still going. `<scripts>/<pkgname>/` holds the hooks `-R` and
/// `-U` run for a package, so a payload landing there replaces a hook nobody
/// asked it to replace. Nothing legitimate installs any of these paths — a
/// package's own hooks and env file reach the device through `scripts/`, never
/// through `root/`.
///
/// Every path is checked where it *lands*, not where it is spelled. A payload
/// naming `usr/lib/demo/db.json` writes the database when `usr/lib/demo` is a
/// symlink to the state directory, and it lands on `install-report.json` when it
/// plants `opt/alias -> ../var/lib/pica` first; comparing the spelled path would
/// miss both. So the components above the entry are resolved through the symlinks
/// that are already there, the entry's own link target is resolved too, and a path
/// whose location cannot be established — an unreadable link, a loop, one that
/// leads out of the target root — refuses the payload before anything is written
/// rather than being guessed at. None of this depends on the order the walk takes
/// the entries in: the checks are about the destination as a whole.
///
/// This is a collision guard, not a privilege boundary. A lifecycle hook is
/// already arbitrary code running as root and can write wherever root can; what
/// this stops is a package's *payload* quietly overwriting the files this
/// transaction is reading, the lock it holds, or another package's hooks.
fn refuse_pica_state(
  paths: &crate::app::Paths,
  target: &TargetRoot,
  root_dir: &Path,
) -> CliResult<()> {
  let protected = [
    ("config file", paths.conf_file.as_path()),
    ("database", paths.db_file.as_path()),
    ("index", paths.index_file.as_path()),
    ("install report", paths.report_file.as_path()),
    ("lock file", paths.lock_file.as_path()),
  ];

  let mut lock_dir = paths.lock_file.as_os_str().to_os_string();
  lock_dir.push(".d");
  let lock_dir = PathBuf::from(lock_dir);
  let scripts_dir = target.resolve(SCRIPTS_DIR);
  let env_dir = target.resolve(ENV_DIR);
  // Compared by component, so `/var/lib/pica` does not claim `/var/lib/pica-old`.
  // The narrowest directory comes first so the message names the thing that is
  // actually at risk: the lock directory is inside the state directory, and the
  // kept scripts are not described by "a directory Pica owns".
  let protected_dirs = [
    ("lock directory", lock_dir.as_path()),
    ("kept scripts", scripts_dir.as_path()),
    ("env fragments", env_dir.as_path()),
    ("state directory", paths.state_dir.as_path()),
    ("configuration directory", paths.etc_dir.as_path()),
  ];

  let mut nodes = Vec::new();
  payload_nodes(root_dir, root_dir, &mut nodes);

  for (relative, node) in &nodes {
    // A directory entry is a no-op when a directory is already there, and makes an
    // empty one otherwise: it overwrites nothing, and everything written *below* it
    // comes through as a file or a link and is checked on its own. Checking it here
    // would report the directory holding Pica's database instead of the database.
    if matches!(node, PayloadNode::Dir) {
      continue;
    }

    // Where the entry itself is written. `deploy_file` renames over the name and
    // `copy_symlink` clears it first, so neither follows a symlink standing at the
    // last component; the components above it are another matter, and they are the
    // ones this resolves.
    let parent = relative.parent().unwrap_or(Path::new(""));
    // `None` here means the parent chain leaves the target root. Pica's files are
    // inside it, so there is nothing this entry could overwrite that the walk is
    // about to depend on — and the walk refuses a payload that leads out of the
    // root before it writes the link in any case.
    let Some(landing_parent) = resolve_in_root(target, parent, relative)? else {
      continue;
    };
    let mut landing = landing_parent.clone();
    if let Some(name) = relative.file_name() {
      landing.push(name);
    }
    refuse_protected(&protected, &protected_dirs, &landing)?;

    // A symlink is a name the next install would write through, so it is refused
    // when it *points* into Pica's state, not only when it sat there. Directories
    // need no check of their own: one that lands on a directory is a no-op, and one
    // that lands nowhere only makes an empty directory, while everything written
    // *below* it is a file or a link and is checked as such — which is also what
    // keeps the message naming the database rather than the directory holding it.
    if let PayloadNode::Link(points_at) = node {
      let resolved = pica_pkg_core::io::resolve_link(target.as_path(), &landing_parent, points_at)
        .map_err(|err| {
          CliError::new(
            E_PACKAGE_INVALID,
            format!("cannot tell where the symlink {} would lead: {}", relative.display(), err),
          )
        })?;
      if let Some(resolved) = resolved {
        refuse_protected(&protected, &protected_dirs, &resolved)?;
      }
    }
  }

  Ok(())
}

/// Where `relative` would land under `root_dir`, following the symlinks already
/// standing in its way.
///
/// Refusing is the answer to every question this cannot answer: a link that cannot
/// be read, one that leads out of the target root, and one that never settles. A
/// guard that guessed here would be guarding the wrong path.
fn resolve_in_root(
  target: &TargetRoot,
  relative: &Path,
  spelled: &Path,
) -> CliResult<Option<PathBuf>> {
  pica_pkg_core::io::resolve_in_root(target.as_path(), relative).map_err(|err| {
    CliError::new(
      E_PACKAGE_INVALID,
      format!("cannot tell where payload path {} would land: {}", spelled.display(), err),
    )
  })
}

fn refuse_protected(
  files: &[(&str, &Path)],
  dirs: &[(&str, &Path)],
  landing: &Path,
) -> CliResult<()> {
  if let Some((label, _)) = files.iter().find(|(_, guard)| *guard == landing) {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      format!("payload writes Pica's own {label}: {}", landing.display()),
    ));
  }
  if let Some((label, _)) = dirs.iter().find(|(_, dir)| landing.starts_with(dir)) {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      format!("payload writes Pica's own {label}: {}", landing.display()),
    ));
  }
  Ok(())
}

/// One entry of a `root/` payload, as the walk sees it.
enum PayloadNode {
  /// A directory, merged into whatever is at the destination.
  Dir,
  /// A regular file, put in place by a rename over the destination's name.
  File,
  /// A symlink, created at the destination's name after clearing it. What it
  /// points at is kept: that is the path a later write would take.
  Link(PathBuf),
}

/// Every path the payload would write, whether or not something is there now.
///
/// [`intended_payload`] answers a different question — which paths the walk can
/// create — so a payload that overwrites Pica's database is invisible to it for
/// the very reason that matters: the database already exists.
///
/// The paths are relative to `base` on purpose: where they land is not a matter
/// of string concatenation, because the target may have symlinks a component
/// passes through. Resolving them is the caller's job, once, against a real
/// filesystem.
fn payload_nodes(base: &Path, dir: &Path, nodes: &mut Vec<(PathBuf, PayloadNode)>) {
  let Ok(entries) = fs::read_dir(dir) else {
    return;
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(relative) = path.strip_prefix(base) else {
      continue;
    };
    let Ok(metadata) = fs::symlink_metadata(&path) else {
      continue;
    };

    if metadata.is_dir() {
      nodes.push((relative.to_path_buf(), PayloadNode::Dir));
      payload_nodes(base, &path, nodes);
    } else if metadata.is_symlink() {
      let Ok(link) = fs::read_link(&path) else {
        continue;
      };
      nodes.push((relative.to_path_buf(), PayloadNode::Link(link)));
    } else if metadata.is_file() {
      nodes.push((relative.to_path_buf(), PayloadNode::File));
    }
  }
}

/// Every path a `root/` payload could still create, mirroring the walker's
/// mapping without copying anything.
///
/// Only used to make a *failed* deploy reversible, and only for paths that do
/// not exist yet: those are the ones the walk can create, so they are the ones
/// its failure can have half-created. A path that is already there is left out
/// on purpose — the walk may never have reached it, and removing a file or a
/// directory this install did not create is destruction the rollback has no
/// right to perform. Only the paths of this list that are actually on disk when
/// the walk fails are recorded; see [`existing_only`].
///
/// The other half of a failed deploy — a pre-existing file the walk overwrote
/// before it failed — is not this list's business at all. [`PayloadBackup`] took
/// a copy of it before the walk started, and `rollback` puts that back.
fn intended_payload(target: &TargetRoot, root_dir: &Path) -> PayloadInstall {
  let mut payload = PayloadInstall::default();
  collect_intended(target, root_dir, root_dir, &mut payload);
  payload
}

/// Copies a file, and keeps the transaction honest when the copy dies after
/// creating its destination.
///
/// A copy truncates and writes the target, so a failure partway leaves a file
/// behind that no `Ok` value will ever describe. Only a destination that did
/// not exist before the copy is this install's to remove; one that did is left
/// where it is, and its previous content comes back from the backup the
/// transaction took before the copy started.
///
/// The copy itself is a parameter so a test can fail it at the one point a real
/// filesystem cannot be made to fail on demand.
fn copy_recording_new_target(
  tx: &mut InstallTransaction,
  to: &Path,
  existed: bool,
  copy: impl FnOnce() -> std::io::Result<u64>,
) -> std::io::Result<u64> {
  match copy() {
    Ok(written) => Ok(written),
    Err(err) => {
      if !existed && to.symlink_metadata().is_ok() {
        tx.record_created(PayloadInstall {
          files: vec![to.display().to_string()],
          ..PayloadInstall::default()
        });
      }
      Err(err)
    }
  }
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

/// Copies out everything this `root/` walk is about to replace, before it
/// replaces any of it.
///
/// The traversal mirrors [`collect_intended`] — the same mapping the deploy
/// makes — on purpose: a backup taken of the wrong path restores the wrong file.
/// A destination that is not there yet is skipped, because this install owns that
/// one and removing it is the whole undo.
fn snapshot_payload(
  target: &TargetRoot,
  base: &Path,
  dir: &Path,
  backup: &mut PayloadBackup,
) -> CliResult<()> {
  let Ok(entries) = fs::read_dir(dir) else {
    return Ok(());
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(relative) = path.strip_prefix(base) else {
      continue;
    };
    let Ok(metadata) = fs::symlink_metadata(&path) else {
      continue;
    };

    if metadata.is_dir() {
      snapshot_payload(target, base, &path, backup)?;
      continue;
    }
    // Only a file or a symlink puts something at its own destination; anything
    // else in the payload the walker ignores, so nothing of the device's is at
    // risk for it.
    if !metadata.is_file() && !metadata.is_symlink() {
      continue;
    }

    let to = target.as_path().join(relative);
    let Ok(existing) = fs::symlink_metadata(&to) else {
      continue;
    };
    backup.back_up(&to, &existing)?;
    if let Some(entry) = backup.entries.last_mut() {
      entry.walked = true;
    }
  }

  Ok(())
}

/// The paths of a payload that are actually on disk.
///
/// `install_payload` reports nothing when it fails, so a deploy that died
/// partway is described only by looking: of the paths this install could still
/// create, the ones that are there are the ones it did create — and the only
/// ones its rollback has a right to remove.
fn existing_only(payload: PayloadInstall) -> PayloadInstall {
  let present = |path: &String| Path::new(path).symlink_metadata().is_ok();
  PayloadInstall {
    files: payload.files.into_iter().filter(present).collect(),
    dirs: payload.dirs.into_iter().filter(present).collect(),
    size: 0,
  }
}

/// Names the copies inside one transaction's backup directory. Process-wide so
/// that two transactions — two packages in one run, or two `pica` processes on
/// one device — cannot pick the same name for different files.
static BACKUP_SEQ: AtomicU64 = AtomicU64::new(0);

/// What a transaction keeps of the files it is about to replace.
///
/// A payload file goes in by renaming a draft over the destination: atomic, and
/// final — the previous content is unreachable the moment the rename happens.
/// This store is the copy taken *before* the walk starts, so a deploy that fails
/// partway puts the device back the way it found it instead of leaving this
/// artifact's bytes in a file the transaction never finished.
///
/// Three things it deliberately is not:
///
/// - **Not a journal.** It lives in this process and names no state a later run
///   can resume. A process killed mid-deploy leaves both the device's files and
///   the copies exactly where they are, and nothing across runs reads them: the
///   guarantee is *in-process* rollback, not crash recovery.
/// - **Not the payload.** Nothing here is recorded as an installed file, so the
///   database list, the size accounting and what `-R` may delete are unchanged.
///   The copies live under Pica's own cache directory, not inside the target
///   rootfs they came from.
/// - **Not a general-purpose archiver.** Only a path the walk is about to
///   destroy gets a copy, and only in the three forms the walk can destroy —
///   see [`PayloadBackup::back_up`] for what is refused instead.
struct PayloadBackup {
  /// `<cache>/backup`, where this store's own directory is made.
  parent: PathBuf,
  /// Names which package the copies belong to.
  prefix: String,
  /// Made with the first file that needs storing, so an install that replaces
  /// nothing — the common first install — creates no directory at all.
  /// `tempfile` gives it a name no other transaction can pick, and Pica asks it
  /// for mode `0700` wherever the platform has a mode to ask for: the copies are
  /// the device's own files and one of them may be a configuration file only
  /// root may read. The copies themselves are `0600` by the way they are made.
  dir: Option<tempfile::TempDir>,
  entries: Vec<BackupEntry>,
}

/// One destination kept as it was, and how to put it back.
struct BackupEntry {
  destination: PathBuf,
  kind: BackupKind,
  /// Whether the payload walk is what writes this destination. Only such a file
  /// can be skipped on the way back: the walk replaces it in one rename, so a walk
  /// that never reached it left it exactly as the copy found it.
  walked: bool,
}

enum BackupKind {
  /// A regular file. Its bytes are at `stored` and `permissions` is the mode it
  /// had; a restore puts both back with one rename, the same way a deploy does.
  File { stored: PathBuf, permissions: fs::Permissions },
  /// A symlink, kept as a symlink rather than as what it points at.
  Link { stored: PathBuf },
  /// A directory that held nothing. The walk only replaces one after removing
  /// it, so putting it back is removing what took its place and making the
  /// directory again, with the mode it had — a directory Pica did not create is
  /// not one Pica gets to re-permission, and `mkdir` would apply the umask.
  EmptyDir { permissions: fs::Permissions },
}

impl PayloadBackup {
  fn new(app: &App, pkgname: &str) -> Self {
    Self {
      parent: app.paths.cache_dir.join("backup"),
      prefix: format!("{pkgname}-"),
      dir: None,
      entries: Vec::new(),
    }
  }

  /// Keeps `destination` as it stands right now, before anything can replace it.
  ///
  /// A path that does not exist yet needs no copy: this install created it, so
  /// removing it is the whole undo, and a copy of it would be a file the record
  /// does not name. A directory the walk cannot empty needs no copy either — it
  /// survives, which is what a non-empty one deserves.
  ///
  /// Anything that is neither a file, a symlink nor a directory is refused here
  /// rather than replaced: a socket, a fifo or a device node cannot be recreated
  /// from a copy of its bytes, and this is the last moment at which refusing
  /// costs nothing — after the walk starts, the destination is already gone.
  fn back_up(&mut self, destination: &Path, metadata: &fs::Metadata) -> CliResult<()> {
    let kind = if metadata.is_symlink() {
      BackupKind::Link { stored: self.copy_link(destination)? }
    } else if metadata.is_file() {
      BackupKind::File { stored: self.copy_file(destination)?, permissions: metadata.permissions() }
    } else if metadata.is_dir() {
      if dir_has_entries(destination)? {
        return Ok(());
      }
      BackupKind::EmptyDir { permissions: metadata.permissions() }
    } else {
      return Err(CliError::new(
        E_IO,
        format!(
          "{} is neither a file, a symlink nor a directory; this install stops before writing \
           anything, because pica could not put that back",
          destination.display()
        ),
      ));
    };

    self.entries.push(BackupEntry {
      destination: destination.to_path_buf(),
      kind,
      // Only `snapshot_payload` sets this: everything else here is a file this
      // install writes itself, and a copy of it may be the only version left even
      // if the write failed.
      walked: false,
    });
    Ok(())
  }

  /// Takes the copies out, deepest path first, and puts each one back.
  ///
  /// Reverse order is the same rule `remove_payload` follows for directories: a
  /// path that has to be cleared before it can become a directory again is
  /// handled before its parent is made. Restoring is done by the same primitives
  /// as deploying — one file by rename, one link by recreating the link — so the
  /// undo of a deploy is not a second, different deploy, and a restore that
  /// already happened is a no-op rather than a hazard: every path left alone is
  /// still the path the entry names.
  ///
  /// The copies are kept when one of them cannot go back: they are the only copy
  /// of what the device had, so the paths that failed are returned for the caller
  /// to name, and the directory stays where the message says it is.
  #[cfg(test)]
  fn restore(&mut self) -> Vec<String> {
    self.restore_reaching(|_| true)
  }

  /// The same restore, but only for destinations the walk got as far as writing.
  ///
  /// A copy taken of a file the walk never reached is a copy of a file that is
  /// still exactly as it was: putting it back is a rewrite with the same bytes —
  /// a new inode, broken hard links, a fresh mtime, and a full read and write of a
  /// file that may be large — with nothing to show for it. `deployed` is what the
  /// walk itself reported writing, so a file it did not reach is simply dropped
  /// from the record.
  ///
  /// Two kinds are restored whether or not the walk reported them. `deploy_file`
  /// and `copy_symlink` clear the destination before they make the new thing, so a
  /// deploy that fails after clearing leaves the name *missing*, and the entry is
  /// the only way back. Neither is expensive to redo: a link is one syscall, and an
  /// empty directory that is still a directory is the no-op case above.
  fn restore_replacing(&mut self, deployed: &HashSet<PathBuf>) -> Vec<String> {
    let reached = |entry: &BackupEntry| match &entry.kind {
      // Only a destination the walk replaced atomically can be left alone. A file
      // Pica wrote itself — an env file or a kept script — is copied into place,
      // so a copy that failed may have left it truncated: it always goes back.
      BackupKind::File { .. } => !entry.walked || deployed.contains(&entry.destination),
      BackupKind::Link { .. } | BackupKind::EmptyDir { .. } => true,
    };
    self.restore_reaching(reached)
  }

  fn restore_reaching(&mut self, reached: impl Fn(&BackupEntry) -> bool) -> Vec<String> {
    let mut failed = Vec::new();

    for entry in self.entries.iter().rev() {
      if !reached(entry) {
        continue;
      }
      let result = match &entry.kind {
        BackupKind::File { stored, permissions } => {
          deploy_file(stored, &entry.destination, permissions).map_err(CliError::from)
        }
        BackupKind::Link { stored } => {
          let _ = remove_any(&entry.destination);
          copy_symlink(stored, &entry.destination).map_err(CliError::from)
        }
        BackupKind::EmptyDir { permissions } => {
          // A directory that is still a directory is what this entry kept. The
          // walk may have put files inside it, and those are the record's to
          // remove; recreating it here would only fail on a directory that is
          // not empty and report a path that never went missing.
          let still_a_directory =
            fs::symlink_metadata(&entry.destination).is_ok_and(|existing| existing.is_dir());
          if still_a_directory {
            Ok(())
          } else {
            remove_any(&entry.destination)
              .and_then(|()| fs::create_dir_all(&entry.destination))
              .and_then(|()| fs::set_permissions(&entry.destination, permissions.clone()))
              .map_err(|err| {
                CliError::new(
                  E_IO,
                  format!(
                    "could not make {} a directory again: {err}",
                    entry.destination.display()
                  ),
                )
              })
          }
        }
      };

      if let Err(err) = result {
        failed.push(format!("{} ({})", entry.destination.display(), err.message));
      }
    }

    if failed.is_empty() {
      self.entries.clear();
      self.dir = None;
    }
    failed
  }

  /// Hands the directory over and stops `tempfile` from cleaning it up
  /// (`TempDir::keep`), returning where it is.
  ///
  /// Whoever calls this decides what the copies are worth: a committed install
  /// removes the directory it gets back — and says so when it cannot — while a
  /// rollback that could not put everything back keeps it, because it is the only
  /// copy of what the device had. A restore that finished cleared the directory
  /// already, so this returns `None` there.
  fn release(mut self) -> Option<PathBuf> {
    self.dir.take().map(tempfile::TempDir::keep)
  }

  fn copy_file(&mut self, destination: &Path) -> CliResult<PathBuf> {
    let dir = self.dir()?;
    let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    let stored = dir.join(format!("file-{seq}"));

    let mut draft = tempfile::NamedTempFile::new_in(&dir)
      .map_err(|err| write_back_up_failure(destination, &stored, err))?;
    let copied =
      fs::File::open(destination).and_then(|mut from| std::io::copy(&mut from, &mut draft));
    if let Err(err) = copied {
      return Err(read_back_up_failure(destination, &err));
    }
    // The draft is `0600` and owner-only, which is the point: the mode here is a
    // secret, not a permission bit, so the copy stays unreadable even when the
    // file on the device was world-readable. The mode that goes *back* onto the
    // device is the one kept in the entry.
    draft.persist(&stored).map_err(|err| write_back_up_failure(destination, &stored, err.error))?;
    Ok(stored)
  }

  fn copy_link(&mut self, destination: &Path) -> CliResult<PathBuf> {
    let stored = self.next_path("link")?;
    // The destination's *own* link is kept, not what it points at, and
    // `fs::symlink` refuses a name that is already taken: a file planted at the
    // name can only make this fail, never be written through.
    copy_symlink(destination, &stored)
      .map_err(|err| write_back_up_failure(destination, &stored, err))?;
    Ok(stored)
  }

  /// A name in the backup directory for the next copy.
  fn next_path(&mut self, label: &str) -> CliResult<PathBuf> {
    let dir = self.dir()?;
    let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("{label}-{seq}")))
  }

  fn dir(&mut self) -> CliResult<PathBuf> {
    if let Some(dir) = &self.dir {
      return Ok(dir.path().to_path_buf());
    }

    fs::create_dir_all(&self.parent).map_err(|err| {
      CliError::new(E_IO, format!("mkdir {} failed: {err}", self.parent.display()))
    })?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(&self.prefix);
    // The copies are the device's own files, and one of them may be a
    // configuration file holding a secret, so the store is asked for owner-only
    // — a mode set *at creation*, so there is no moment in which it is not. A
    // platform with no mode to set keeps its default, and what is inside is
    // still `0600`.
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let made = builder.tempdir_in(&self.parent).map_err(|err| {
      CliError::new(
        E_IO,
        format!(
          "could not make a directory for the files this install replaces ({}): {err}",
          self.parent.display()
        ),
      )
    })?;
    let path = made.path().to_path_buf();
    self.dir = Some(made);
    Ok(path)
  }
}

/// Whether `dir` holds anything, including a broken symlink.
fn dir_has_entries(dir: &Path) -> CliResult<bool> {
  let mut entries = fs::read_dir(dir)
    .map_err(|err| CliError::new(E_IO, format!("read {} failed: {err}", dir.display())))?;
  Ok(entries.next().is_some())
}

/// Takes `path` away whether it is a file, a link or a directory, and reports
/// only what is still standing afterwards: a path that is already gone was not
/// this install's to keep.
fn remove_any(path: &Path) -> std::io::Result<()> {
  if fs::symlink_metadata(path).is_err() {
    return Ok(());
  }

  match fs::remove_file(path).or_else(|_| fs::remove_dir(path)) {
    Ok(()) => Ok(()),
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(err) => Err(err),
  }
}

fn read_back_up_failure(destination: &Path, err: &std::io::Error) -> CliError {
  CliError::new(
    E_IO,
    format!(
      "could not read {} before replacing it ({err}); nothing has been written",
      destination.display()
    ),
  )
}

fn write_back_up_failure(
  destination: &Path,
  stored: &Path,
  err: impl std::fmt::Display,
) -> CliError {
  CliError::new(
    E_IO,
    format!(
      "could not keep a copy of {} at {} ({err}); nothing has been written",
      destination.display(),
      stored.display()
    ),
  )
}

/// What one install has done so far, so that it can be undone.
///
/// Every field is filled in *before* the step that could fail, so a step that
/// dies halfway still leaves the transaction holding whatever it managed to do.
/// Nothing here is written to the database: the record is step 9, and until it
/// exists this struct is the only description of the install.
struct InstallTransaction {
  /// Every installed package and the version the backend reported for it,
  /// taken before step 5. Names alone cannot tell a dependency this install
  /// added from one it upgraded.
  tx_before: Option<Vec<(String, String)>>,
  /// Packages step 5 added, diffed against `tx_before` after every phase.
  /// Rollback removes exactly these.
  tx_added: Vec<String>,
  /// Packages that were already installed and changed version during step 5, as
  /// `(name, before, after)`. Rollback cannot undo these — pica has no previous
  /// version to put back — so they are reported instead of silently accepted.
  tx_changed: Vec<(String, String, String)>,
  /// The part of `tx_added` that came from the `app` stage, which is the `app`
  /// list when the dependencies came from feeds. A container batch is installed
  /// as one request covering `base` and `app` at once, so it appears in
  /// `tx_added` only.
  app_added: Vec<String>,
  /// Paths deployed, in deployment order so removal can replay them backwards.
  /// This is what the database records, so it lists every file the install
  /// wrote — including the ones it replaced.
  payload_records: Vec<PayloadInstall>,
  /// The subset of `payload_records` this transaction **created**. Rollback
  /// removes only these: a file the install replaced was already on the device,
  /// and deleting it would take the path itself away on top of the content.
  created_records: Vec<PayloadInstall>,
  /// The backup an upgrade took of the kept scripts, with the marker that says
  /// which transaction made it. Rollback puts it back; a finished install
  /// removes it.
  scripts_backup: Option<ScriptsBackup>,
  /// What this transaction has copied out of the way, ready to go back. Made in
  /// step 7 and emptied when the install commits.
  payload_backup: Option<PayloadBackup>,
  /// The destinations the payload walk actually wrote, in the order it wrote
  /// them. A walk that fails halfway leaves what it reached here, which is how a
  /// rollback tells a file it replaced from one it never got to: putting a copy
  /// back over the latter would rewrite a file nothing touched.
  deployed: HashSet<PathBuf>,
  /// Labels of the hooks that ran, in the order they ran.
  executed_hooks: Vec<String>,
}

impl InstallTransaction {
  fn new() -> Self {
    Self {
      tx_before: None,
      tx_added: Vec::new(),
      tx_changed: Vec::new(),
      app_added: Vec::new(),
      payload_records: Vec::new(),
      created_records: Vec::new(),
      scripts_backup: None,
      payload_backup: None,
      deployed: HashSet::new(),
      executed_hooks: Vec::new(),
    }
  }

  fn record_payload(&mut self, payload: PayloadInstall) {
    self.payload_records.push(payload);
  }

  /// Notes every destination a walk reached, finished or not.
  fn note_deployed(&mut self, walked: &PayloadInstall) {
    self.deployed.extend(walked.files.iter().map(PathBuf::from));
  }

  /// Takes the copies of everything step 7 is about to replace, before it
  /// replaces any of it.
  ///
  /// The order is the whole point: a cache directory that cannot be written, a
  /// file that cannot be read and a destination that cannot be put back (a
  /// socket, a fifo, a device node) all fail *here*, while the device still has
  /// every byte it had.
  fn prepare_payload_backup(
    &mut self,
    app: &App,
    pkgname: &str,
    target: &TargetRoot,
    root_dir: &Path,
  ) -> CliResult<()> {
    let mut backup = PayloadBackup::new(app, pkgname);
    if root_dir.is_dir() {
      snapshot_payload(target, root_dir, root_dir, &mut backup)?;
    }
    self.payload_backup = Some(backup);
    Ok(())
  }

  /// Keeps one more path that step 7 is about to overwrite.
  ///
  /// The env file is Pica's own metadata, but it is a destination this install
  /// replaces exactly like a payload file does, so it is backed up by the same
  /// store and restored by the same rollback.
  fn back_up_destination(&mut self, path: &Path) -> CliResult<()> {
    let Some(backup) = self.payload_backup.as_mut() else {
      return Ok(());
    };
    let Ok(metadata) = fs::symlink_metadata(path) else {
      return Ok(());
    };
    backup.back_up(path, &metadata)
  }

  /// Remembers the directory an upgrade moved aside, so rollback can put it
  /// back and a finished install can drop it.
  fn record_scripts_backup(&mut self, target: PathBuf, backup: PathBuf, marker: ScriptsMarker) {
    self.scripts_backup = Some(ScriptsBackup { target, backup, marker });
  }

  /// Marks the backup as belonging to a finished install.
  ///
  /// Written only after `record_installation` returned Ok, which is the whole
  /// correspondence between the marker and the database: `committed` means the
  /// record landed, and nothing else does. A write that fails leaves the marker
  /// saying `pending`, so the next run moves the backup aside instead of
  /// deleting it.
  fn mark_committed(&mut self, app: &mut App) {
    let Some(entry) = self.scripts_backup.as_mut() else {
      return;
    };
    entry.marker.state = ScriptsState::Committed;
    let marker_path = kept_scripts_marker(&entry.backup);
    if let Err(err) = fs::write(&marker_path, entry.marker.to_json()) {
      app.log_warn(format!(
        "the install completed, but {} could not be updated ({err}); the previous kept scripts \
         stay at {} and the next upgrade will move them aside",
        marker_path.display(),
        entry.backup.display()
      ));
    }
  }

  /// Removes what this transaction kept, once the install is complete. A failure
  /// is reported, not hidden: the directory stays at a path the message names,
  /// and the scripts' marker stays with its backup so a later run can classify
  /// it.
  fn finish(&mut self, app: &mut App) {
    self.finish_with(
      app,
      |dir| fs::remove_dir_all(dir),
      |file| fs::remove_file(file),
      |dir| fs::remove_dir_all(dir),
    );
  }

  fn finish_with(
    &mut self,
    app: &mut App,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
    remove_marker: impl FnOnce(&Path) -> std::io::Result<()>,
    remove_payload_backup: impl FnOnce(&Path) -> std::io::Result<()>,
  ) {
    // First, because the scripts branch below returns early. The install is
    // complete, so the copies of the files this transaction replaced are
    // superseded — and Pica says so out loud when it cannot collect them,
    // because they are the last copy of what was on the device before.
    if let Some(backup) = self.payload_backup.take().and_then(PayloadBackup::release) {
      if let Err(err) = remove_payload_backup(&backup) {
        app.log_warn(format!(
          "the install completed, but the copies taken of the files it replaced could not be \
           removed from {} ({err})",
          backup.display()
        ));
      }
    }

    let Some(entry) = self.scripts_backup.take() else {
      return;
    };
    let marker_path = kept_scripts_marker(&entry.backup);
    match remove(&entry.backup) {
      Ok(()) => {
        // A marker left without its backup is handled by the next install
        // (`reconcile_scripts_backup` moves it aside), but the failure is said
        // out loud instead of being dropped.
        if let Err(err) = remove_marker(&marker_path) {
          app.log_warn(format!(
            "the previous kept scripts were removed, but their marker at {} could not be ({err}); \
             the next upgrade will move it aside",
            marker_path.display()
          ));
        }
      }
      Err(err) => app.log_warn(format!(
        "the previous kept scripts could not be removed from {} ({err}); the next upgrade will \
         find them and move them aside",
        entry.backup.display()
      )),
    }
  }

  /// Records the paths this transaction created, which rollback may remove.
  ///
  /// Kept apart from [`Self::record_payload`] on purpose: the database record
  /// has to name every file the install wrote, while a rollback may only take
  /// away paths that were not there before it started.
  fn record_created(&mut self, payload: PayloadInstall) {
    self.created_records.push(payload);
  }

  fn record_hook(&mut self, label: &str) {
    self.executed_hooks.push(label.to_string());
  }

  /// Diffs the backend's installed set against the snapshot from step 4, keeping
  /// an addition and a version change apart: only the first can be undone.
  ///
  /// Called after every phase *and* on its error path. A backend that fails
  /// halfway through a request has still installed part of it, so the exit
  /// status is not evidence that nothing landed.
  ///
  /// A list that cannot be read keeps what earlier phases confirmed. Treating
  /// the failure as "nothing installed" is what makes a transaction forget the
  /// packages it just added — and those are exactly the ones rollback is for.
  fn record_backend_changes(&mut self, app: &mut App) {
    let Some(before) = &self.tx_before else {
      return;
    };
    let mut after = match app.package_manager().and_then(crate::pkgmgr::PackageManager::installed) {
      Ok(after) => after,
      Err(err) => {
        app.log_warn(format!(
          "could not read the installed package list; the rollback record may be incomplete: {}",
          err.message
        ));
        return;
      }
    };
    after.sort();

    let mut added = Vec::new();
    let mut changed = Vec::new();
    for (name, version) in &after {
      match before.iter().find(|(known, _)| known == name) {
        None => added.push(name.clone()),
        Some((_, previous)) if previous != version => {
          changed.push((name.clone(), previous.clone(), version.clone()));
        }
        Some(_) => {}
      }
    }
    self.tx_added = added;
    self.tx_changed = changed;
  }

  /// Every path this install created, as one payload.
  ///
  /// `root/` is deployed first and the kept scripts after it, so parents come
  /// before children: `remove_payload` walks directories in reverse and only
  /// removes the ones that ended up empty.
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
  /// The order mirrors the install backwards: the content this transaction
  /// replaced goes back first, then the paths it created come off (deepest
  /// payload last), then the hooks are compensated, and finally the packages
  /// step 5 added are handed back to the backend that installed them.
  fn rollback(&mut self, app: &mut App, ctx: &InstallContext<'_>) -> Vec<String> {
    let mut leftovers = Vec::new();

    // Content first. A path this install replaced is not its own to delete, so
    // the only way back is the copy taken before the walk — and putting it back
    // has to happen before anything is removed, because a removal can empty a
    // directory this transaction never created.
    if let Some(mut backup) = self.payload_backup.take() {
      let unrestored = backup.restore_replacing(&self.deployed);
      for path in &unrestored {
        leftovers.push(format!("content:{path}"));
      }
      if let Some(dir) = backup.release() {
        // A copy that could not go back is the only version of that file anyone
        // has left, so the store stays where it is and the message says where.
        let paths = unrestored.join(" ");
        app.log_warn(format!(
          "pica could not put {} back; the copies it kept are in {}, which is the only place \
           those files still exist",
          if paths.is_empty() { "the files it replaced".to_string() } else { paths },
          dir.display()
        ));
      }
    }

    for record in self.created_records.iter().rev() {
      // Its count is dropped because it does not answer the question below: a
      // directory may survive too, and that is not a file.
      let _ = remove_payload(record);
    }

    // `remove_payload` is best effort, so what survived is found by looking
    // rather than by trusting a return value. The survivors are both reported
    // and kept as this install's payload record, because a `-R` driven by that
    // record is the only way left to clean them.
    //
    // A recorded directory is one `remove_payload` was told this install
    // created, so a directory that is still there is a survivor too: it is
    // either not empty (something else — a hook's file, another package's file
    // — moved in) or could not be removed. Ignoring it would report a clean
    // rollback while leaving a record nothing points `-R` at.
    //
    // Only `created_records` is walked: a file the install *replaced* is not a
    // survivor of this rollback, and listing it here would make `-R` delete a
    // path the transaction did not create.
    // Asked with `symlink_metadata` rather than `exists`: a created symlink whose
    // target is gone is a survivor too, and `exists` follows the link when it is
    // exactly what may not be followed.
    let present = |path: &String| Path::new(path).symlink_metadata().is_ok();
    let mut remnant = PayloadInstall::default();
    for record in &self.created_records {
      for file in &record.files {
        if present(file) {
          leftovers.push(file.clone());
          remnant.files.push(file.clone());
        }
      }
      for dir in &record.dirs {
        if present(dir) {
          leftovers.push(dir.clone());
          remnant.dirs.push(dir.clone());
        }
      }
    }
    self.payload_records = vec![remnant.clone()];
    self.created_records = vec![remnant];

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

    // The kept scripts: everything under the directory was written by this
    // transaction, and the old ones are waiting in the backup. Removing the
    // directory is what makes the rename below possible; anything still in it
    // that this transaction did not write is reported if the rename fails.
    if let Some(entry) = self.scripts_backup.take() {
      let marker_path = kept_scripts_marker(&entry.backup);
      match restore_scripts_backup(&entry.target, &entry.backup, |from, to| fs::rename(from, to)) {
        Ok(()) => {
          let _ = fs::remove_file(&marker_path);
        }
        Err(err) => {
          // The backup holds the only copy of the old scripts, so it is
          // reported rather than recorded for removal: `-R` must not delete
          // it. Its marker stays beside it so a later upgrade can classify it.
          leftovers.push(format!("scripts:{} ({err})", entry.backup.display()));
        }
      }
    }

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
  let name = ctx.pkgname.to_string();
  app.warn_args(Key::InstallRollingBack, &[&name]);

  let leftovers = tx.rollback(app, ctx);
  if leftovers.is_empty() {
    app.warn_args(Key::InstallRolledBack, &[&name]);
    return err;
  }

  let count = leftovers.len().to_string();
  let paths = leftovers.join(" ");
  app.warn_args(Key::InstallRollbackLeftovers, &[&name, &count, &paths]);

  if let Err(record_err) = record_installation(app, ctx, precheck, tx, true) {
    let message = record_err.message.clone();
    app.warn_args(Key::InstallRollbackRecordFailed, &[&name, &message]);
  }

  CliError::new(err.code, app.args(Key::InstallRollbackIncomplete, &[&err.message]))
}

/// Step 9: writes the completed install to the database.
fn record_installation(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &InstallTransaction,
  failed: bool,
) -> CliResult<()> {
  // The precheck object is stored verbatim, so the optional dependencies are an
  // extra key inside it: a reader that predates `?` sees the same kmod/base/app
  // arrays it always did, and ignores the new key.
  let mut report = precheck.report.clone();
  if !precheck.optional.is_empty() {
    report["optional"] = precheck.optional.report(if failed { "failed" } else { "installed" });
  }

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
  if failed {
    // The rollback has already put the previous version's files back when there
    // was one, so the entry that describes it must survive: only the paths this
    // install could not remove are added to it.
    db_record_remnant(
      &app.paths.db_file,
      ctx.pkgname,
      &manifest_stored,
      &canonicalize_display(ctx.pkgfile),
      &payload.files,
      &payload.dirs,
    )?;
  } else {
    db_set_installed(
      &app.paths.db_file,
      ctx.pkgname,
      &manifest_stored,
      &canonicalize_display(ctx.pkgfile),
      &payload.files,
      &payload.dirs,
    )?;
  }

  // The database is what makes an install an install: `-Qi`, `-R` and `-U` read
  // it, and none of them reads the report. Writing the report second means a
  // report that cannot be written leaves a recorded install rather than a report
  // describing one the database never took; and since the install is already
  // recorded and its files are in place, that failure is a warning instead of a
  // reason to undo them.
  if let Err(err) = report_set_install_result(
    &app.paths,
    ctx.pkgname,
    ctx.selector,
    &ctx.manifest.value,
    &report,
    &tx.tx_added,
    &tx.app_added,
  ) {
    let message = err.message.clone();
    app.warn_args(Key::InstallReportFailed, &[ctx.pkgname, &message]);
  }

  // What Pica itself wrote, and only when Pica is the one that wrote it: a
  // native backend prints its own summary, and counting Pica's containers for it
  // would be a second, different answer to the same question.
  if !failed && !app.backend().requires_system_binary() && !payload.files.is_empty() {
    let summary = install_summary(app.lang, &payload);
    app.log_info(summary);
  }

  // This function is also the writer of a failed attempt's remnant record, and
  // that attempt has just been reported as a rollback. Saying "transaction
  // completed" on the way out would contradict the error printed right after it.
  if !failed {
    app.info(Key::TransactionCompleted);
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::app::{E_INDEX_UNAVAILABLE, FeedPolicy, JsonMode, OptionalDeps, Options, Paths};
  use crate::pkgmgr::FeedCandidate;
  use pretty_assertions::assert_eq;
  use std::os::unix::fs::{MetadataExt, symlink};
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
    };

    let mut app = App::new(paths, options);
    app.select_package_manager(PackageManagerKind::Pica.as_str()).expect("force the pica backend");
    app
  }

  /// A backend that answers the one question `ensure_index` asks: whether the
  /// index can be made readable. `readable: false` is what unreachable feeds
  /// look like from the precheck's side; nothing else is answered.
  struct IndexStub {
    readable: bool,
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

    fn package_exists(&self, _name: &str) -> CliResult<bool> {
      Ok(false)
    }

    fn feed_candidates(&self, _name: &str) -> CliResult<Vec<FeedCandidate>> {
      Ok(Vec::new())
    }

    fn is_installed(&self, _name: &str) -> bool {
      false
    }

    fn installed_version(&self, _name: &str) -> Option<String> {
      None
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
    app.package_manager = Some(Box::new(IndexStub { readable }));
    app
  }

  #[test]
  fn an_unreadable_index_does_not_stop_an_artifact_that_requires_nothing() {
    let work = tempfile::tempdir().expect("temp dir");
    let mut app = app_with_index(work.path(), false);

    let deps = declarations(&[], &[], &["opt-app?"]);
    ensure_index(&mut app, &deps).expect("optional declarations do not need the index");
  }

  #[test]
  fn an_unreadable_index_still_stops_anything_required() {
    let work = tempfile::tempdir().expect("temp dir");

    for deps in
      [declarations(&[], &["base-dep"], &["opt-app?"]), declarations(&[], &[], &["app-dep"])]
    {
      let mut app = app_with_index(work.path(), false);
      let err = ensure_index(&mut app, &deps).expect_err("a required dependency needs the index");
      assert_eq!(err.code, E_INDEX_UNAVAILABLE, "{err:?}");
    }
  }

  #[test]
  fn a_required_kernel_module_counts_as_requiring_the_index() {
    // The precheck has three groups, and the source decision only looks at two.
    // A guard that forgot `kmod` would let a module-only artifact through on an
    // index that never answered.
    let work = tempfile::tempdir().expect("temp dir");
    let mut app = app_with_index(work.path(), false);

    let deps = declarations(&["kmod-tun"], &[], &["opt-app?"]);
    let err = ensure_index(&mut app, &deps).expect_err("a required module needs the index");
    assert_eq!(err.code, E_INDEX_UNAVAILABLE, "{err:?}");
  }

  #[test]
  fn a_readable_index_is_asked_for_optional_and_required_alike() {
    // The downgrade is about a failure, not about skipping the refresh: an index
    // that answers must still be asked, so the optional names get a real status
    // rather than "unknown".
    for deps in [declarations(&[], &[], &["opt-app?"]), declarations(&[], &["base-dep"], &[])] {
      let work = tempfile::tempdir().expect("temp dir");
      let mut app = app_with_index(work.path(), true);
      ensure_index(&mut app, &deps).expect("a readable index never fails the precheck");
    }
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
  /// Writes the marker beside `name` under the fixture's scripts directory.
  fn write_marker(fixture: &PicaFixture, name: &str, txid: &str, state: ScriptsState) {
    let marker = ScriptsMarker {
      txid: txid.to_string(),
      pkgname: "demo".to_string(),
      pkgver: "1.0.0".to_string(),
      pkgrel: "1".to_string(),
      state,
    };
    fs::write(fixture.path(&format!("usr/lib/pica/scripts/{name}")), marker.to_json())
      .expect("write the marker");
  }

  /// Backup directories whose name starts with `prefix.` — the ones moved
  /// aside to keep the canonical name free.
  fn moved_aside(fixture: &PicaFixture, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(fixture.path("usr/lib/pica/scripts")) else {
      return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
      .flatten()
      .map(|entry| entry.path())
      .filter(|path| {
        path.is_dir()
          && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&format!("{prefix}.")))
      })
      .collect();
    out.sort();
    out
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

  #[test]
  fn a_pica_artifact_must_not_carry_containers() {
    // pica does not read container formats: it could neither install one nor
    // account for the files inside it, so an artifact that carries `pkgs/` is
    // refused before anything is written rather than half-installed.
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
  fn a_container_in_a_pica_artifact_is_refused_before_anything_is_written() {
    // pica never opens a container, so it cannot tell a broken one from a sound
    // one — the artifact is refused on its shape alone.
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

    assert!(err.message.contains("must not carry pkgs/"), "{}", err.message);
    assert!(
      err.message.contains("targets = opkg") && err.message.contains("targets = apk"),
      "the error must name the backends that do install it: {}",
      err.message
    );
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
       reachable through the paths this step meant to create that are actually on disk"
    );
    assert!(db_record(&fixture.app, "demo").is_none());
  }

  #[test]
  fn a_failed_upgrade_leaves_the_previous_record_and_the_previous_bytes() {
    let mut fixture = PicaFixture::new();
    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "one\n"), ("etc/keep.conf", "keeper\n")],
        &[],
        &[],
      )
      .expect("the first version installs");
    let before = db_record(&fixture.app, "demo").expect("the first version is recorded");

    // The upgrade gets as far as replacing `etc/demo.conf`, then the env copy
    // dies on a directory sitting at its destination.
    fs::create_dir_all(fixture.path("etc/pica/env.d/demo.env")).expect("block the env copy");
    let err = fixture
      .install_without_containers(
        &format!("{}pkgmgr = pica\n", MANIFEST_DEMO.replace("pkgver = 1.0.0", "pkgver = 2.0.0")),
        &[("etc/demo.conf", "two\n")],
        &[],
        &[("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n")],
      )
      .expect_err("the upgrade fails");
    assert!(err.message.contains("copy env file failed"), "{}", err.message);

    assert_eq!(
      fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
      "one\n",
      "the file the upgrade replaced is back to the bytes the installed version had"
    );
    // The record is not rewritten at all: a rollback that undid everything leaves
    // nothing for `-R` to act on, so the device is what the old record describes.
    assert_eq!(db_record(&fixture.app, "demo").expect("still recorded"), before);
  }

  #[cfg(unix)]
  #[test]
  fn an_empty_directory_the_payload_replaced_comes_back_with_its_mode() {
    use std::os::unix::fs::PermissionsExt;

    let mut fixture = PicaFixture::new();
    fs::create_dir_all(fixture.path("etc/demo.conf"))
      .expect("a directory where the payload wants a file");
    fs::set_permissions(fixture.path("etc/demo.conf"), fs::Permissions::from_mode(0o700))
      .expect("give the directory a mode of its own");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "a file where the directory was\n")],
        &[("zzz-escape", "/etc/passwd")],
        &[],
      )
      .expect_err("the walk fails after the file replaced the directory");

    assert!(err.message.contains("escapes the target root"), "{}", err.message);
    let path = fixture.path("etc/demo.conf");
    assert!(path.is_dir(), "the directory is what was there before the install");
    assert_eq!(
      fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
      0o700,
      "a directory Pica did not create keeps the mode it had, not the umask's"
    );
  }

  #[cfg(unix)]
  #[test]
  fn a_restore_leaves_a_directory_alone_when_the_walk_only_filled_it() {
    // The entry kept an empty directory; the walk then put a file inside it. The
    // record removes that file, so the directory is already what it was — and a
    // restore that tried to recreate it would fail on a directory it cannot
    // empty and report a path that never went missing.
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().expect("temp dir");
    let destination = work.path().join("etc/demo");
    fs::create_dir_all(&destination).expect("mkdir");
    fs::write(destination.join("added"), "written by the walk\n").expect("write");

    let mut backup = PayloadBackup {
      parent: work.path().join("backup"),
      prefix: "demo-".to_string(),
      dir: None,
      entries: vec![BackupEntry {
        destination: destination.clone(),
        kind: BackupKind::EmptyDir { permissions: fs::Permissions::from_mode(0o700) },
        walked: true,
      }],
    };

    let failed = backup.restore();

    assert!(failed.is_empty(), "nothing went missing: {failed:?}");
    assert!(destination.is_dir());
    assert!(
      destination.join("added").is_file(),
      "removing what the walk wrote is the record's job, not the restore's"
    );
  }

  #[test]
  fn a_payload_that_ships_pica_own_database_is_refused_before_anything_is_written() {
    // A package that writes `/var/lib/pica/db.json` is writing the record this
    // very install reads and rewrites. Nothing legitimate does it, so the
    // payload is refused while the database is still the operator's.
    let mut fixture = PicaFixture::new();
    let state = fixture.target.as_path().join("var/lib/pica");
    fs::create_dir_all(&state).expect("mkdir state");
    let database = state.join("db.json");
    fs::write(&database, "{\"schema\":1,\"installed\":{}}\n").expect("existing database");
    let before = fs::read_to_string(&database).expect("read");
    // A hard link tells "untouched" apart from "rewritten with the same bytes":
    // putting a copy back is a rename of a fresh inode, which leaves `db.json`
    // with one link while the sibling keeps the old one.
    let sibling = state.join("db.json.link");
    fs::hard_link(&database, &sibling).expect("hard link the database");
    fixture.app.paths.db_file = database.clone();
    fixture.app.paths.state_dir = state.clone();
    fixture.app.paths.index_file = state.join("index.json");
    fixture.app.paths.report_file = state.join("install-report.json");
    fixture.app.paths.lock_file = state.join("db.lck");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("var/lib/pica/db.json", "{\"schema\":1,\"installed\":{}}\n")],
        &[],
        &[],
      )
      .expect_err("a payload may not write the database");

    assert!(err.message.contains("database"), "{}", err.message);
    assert_eq!(fs::read_to_string(&database).expect("read"), before, "the database is untouched");
    assert_eq!(
      fs::metadata(&database).expect("db").ino(),
      fs::metadata(&sibling).expect("sibling").ino(),
      "the refusal must come before the copy, or a rollback rewrites a file nothing touched"
    );
    assert!(
      !fixture.app.paths.cache_dir.join("backup").exists(),
      "a payload refused before the walk has nothing to back up"
    );
    assert!(
      !fixture.target.as_path().join("etc").exists(),
      "the payload must be refused before the walk starts"
    );
  }

  #[test]
  fn a_payload_that_ships_the_lock_directory_is_refused() {
    // The lock is `<state>/db.lck.d/pid`. A payload that writes a pid there makes
    // the lock name a process that is not running, so the next `pica` reclaims it
    // while this install is still halfway through.
    let mut fixture = PicaFixture::new();
    let state = fixture.target.as_path().join("var/lib/pica");
    fs::create_dir_all(&state).expect("mkdir state");
    fixture.app.paths.db_file = state.join("db.json");
    fixture.app.paths.state_dir = state.clone();
    fixture.app.paths.index_file = state.join("index.json");
    fixture.app.paths.report_file = state.join("install-report.json");
    fixture.app.paths.lock_file = state.join("db.lck");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("var/lib/pica/db.lck.d/pid", "1\n"), ("etc/demo.conf", "payload\n")],
        &[],
        &[],
      )
      .expect_err("a payload may not write the lock directory");

    assert!(err.message.contains("lock directory"), "{}", err.message);
    assert!(!state.join("db.lck.d").exists(), "no lock directory was created");
    assert!(
      !fixture.path("etc/demo.conf").exists(),
      "the refusal comes before any of the payload is deployed"
    );
  }

  #[test]
  fn a_payload_that_ships_another_packages_lifecycle_script_is_refused() {
    // Kept scripts are executed as root by `-R` and `-U`, and they live in
    // `/usr/lib/pica/scripts/<pkgname>/`. A payload that can write one can plant
    // or replace a hook for a package it is not.
    let mut fixture = PicaFixture::new();
    let victim = fixture.target.as_path().join("usr/lib/pica/scripts/victim/install.sh");
    write_file(&victim, "#!/bin/sh\necho the installed one\n");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("usr/lib/pica/scripts/victim/install.sh", "#!/bin/sh\nrm -rf /\n")],
        &[],
        &[],
      )
      .expect_err("a payload may not write another package's hook");

    assert!(err.message.contains("kept scripts"), "{}", err.message);
    assert_eq!(
      fs::read_to_string(&victim).expect("read"),
      "#!/bin/sh\necho the installed one\n",
      "the hook `-U` will run is still the one that was there"
    );
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

  #[test]
  fn a_payload_that_writes_through_a_symlink_into_pica_state_is_refused() {
    // `usr/lib/demo` is a symlink to the state directory, so `usr/lib/demo/db.json`
    // *is* the database. The name says `usr/lib`; the kernel says `/var/lib/pica`.
    // Which one the guard believes is the whole question, and it believes the
    // filesystem.
    let mut fixture = PicaFixture::new();
    let (state, database) = state_with_a_database(&mut fixture);
    let sibling = state.join("db.json.link");
    fs::hard_link(&database, &sibling).expect("hard link the database");
    let alias = fixture.path("usr/lib/demo");
    fs::create_dir_all(alias.parent().expect("parent")).expect("mkdir usr/lib");
    symlink("../../var/lib/pica", &alias).expect("link the state directory");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[
          ("usr/lib/demo/db.json", "{\"schema\":1,\"installed\":{}}\n"),
          ("etc/demo.conf", "payload\n"),
        ],
        &[],
        &[],
      )
      .expect_err("a payload may not write the database through a symlink");

    assert!(err.message.contains("database"), "{}", err.message);
    assert_eq!(
      fs::read_to_string(&database).expect("read"),
      "{\"schema\":1,\"installed\":{}}\n",
      "the database still holds what the operator left"
    );
    assert_eq!(
      fs::metadata(&database).expect("db").ino(),
      fs::metadata(&sibling).expect("sibling").ino(),
      "the refusal comes before the copy, so the database is not a fresh inode"
    );
    assert!(!fixture.path("etc/demo.conf").exists(), "nothing of the payload was deployed");
  }

  #[test]
  fn a_payload_that_writes_through_a_dangling_symlink_into_pica_state_is_refused() {
    // The link is there and the state directory is not. `create_dir_all` would
    // follow the link and build the directory wherever it names, which is the
    // order that matters: the symlink comes first, the write second, and the guard
    // refuses on where the write would land rather than on what exists yet.
    let mut fixture = PicaFixture::new();
    let (state, _database) = state_with_a_database(&mut fixture);
    fs::remove_dir_all(&state).expect("take the state directory away");
    let alias = fixture.path("usr/lib/demo");
    fs::create_dir_all(alias.parent().expect("parent")).expect("mkdir usr/lib");
    symlink("../../var/lib/pica", &alias).expect("link the state directory");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[
          ("usr/lib/demo/db.json", "{\"schema\":1,\"installed\":{}}\n"),
          ("etc/demo.conf", "payload\n"),
        ],
        &[],
        &[],
      )
      .expect_err("a payload may not create Pica's state through a symlink");

    assert!(err.message.contains("database"), "{}", err.message);
    // Pica makes its own state directory when it opens the store; what matters is
    // that the *payload* stopped at the guard, before the walk wrote anything.
    assert!(!fixture.path("etc/demo.conf").exists(), "nothing of the payload was deployed");
  }

  #[test]
  fn a_payload_that_plants_a_symlink_into_pica_state_is_refused() {
    // A link that only *points* at the state directory writes nothing today. It is
    // what the next install of this package would write through, so it is refused
    // now, while it is still a name and not a way in.
    let mut fixture = PicaFixture::new();
    let _ = state_with_a_database(&mut fixture);

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "payload\n")],
        &[("opt/alias", "../var/lib/pica")],
        &[],
      )
      .expect_err("a payload may not plant an alias for Pica's state");

    assert!(err.message.contains("state directory"), "{}", err.message);
    assert!(!fixture.path("opt/alias").exists(), "the alias was never created");
    assert!(!fixture.path("etc/demo.conf").exists(), "nothing of the payload was deployed");
  }

  #[test]
  fn a_payload_is_refused_whichever_order_the_walk_takes_its_entries_in() {
    // `root/` is walked in name order, and a guard that decided entry by entry on
    // the way past would answer differently depending on where the names sort. The
    // check is about the destination, not about what was seen before it: `var`
    // sorts after `etc` and before `zzz`, and both orders stop before the walk.
    for harmless in ["etc/aaa.conf", "zzz/zzz.conf"] {
      let mut fixture = PicaFixture::new();
      let (_, database) = state_with_a_database(&mut fixture);

      let err = fixture
        .install_without_containers(
          &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
          &[(harmless, "fine\n"), ("var/lib/pica/db.json", "{\"schema\":1,\"installed\":{}}\n")],
          &[],
          &[],
        )
        .expect_err("a payload may not write the database");

      assert!(err.message.contains("database"), "{harmless}: {}", err.message);
      assert!(!fixture.path(harmless).exists(), "{harmless}: refused before the walk starts");
      assert_eq!(
        fs::read_to_string(&database).expect("read"),
        "{\"schema\":1,\"installed\":{}}\n",
        "{harmless}: the database is untouched"
      );
    }
  }

  #[test]
  fn a_payload_that_writes_through_a_symlink_elsewhere_is_allowed() {
    // Not every symlink in the way is Pica's: `usr/lib/demo` pointing at a version
    // directory is how a package with two versions installed stays addressable.
    // The guard has to tell that apart from the state directory, or it breaks a
    // layout it was never meant to police.
    let mut fixture = PicaFixture::new();
    let _ = state_with_a_database(&mut fixture);
    let version = fixture.path("opt/demo-1.0");
    fs::create_dir_all(&version).expect("mkdir version");
    let alias = fixture.path("usr/lib/demo");
    fs::create_dir_all(alias.parent().expect("parent")).expect("mkdir usr/lib");
    symlink("../../opt/demo-1.0", &alias).expect("link the version directory");

    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("usr/lib/demo/app.conf", "ok\n")],
        &[],
        &[],
      )
      .expect("a symlink that leads somewhere else is the package's business");

    assert_eq!(
      fs::read_to_string(version.join("app.conf")).expect("read"),
      "ok\n",
      "the file lands where the link points, as the walk meant it to"
    );
    assert!(
      fs::symlink_metadata(&alias).expect("metadata").is_symlink(),
      "the link itself stays a link"
    );
  }

  #[test]
  fn a_restore_leaves_a_file_the_walk_never_reached_alone() {
    // Two files are copied out before the walk starts. It replaces the first and
    // stops; the second is exactly what it was. Putting the copy back over the
    // second would rewrite the same bytes into a fresh inode — a hard link to it
    // would come apart, and a large file would be read and written for nothing —
    // so the rollback restores only what the walk reported reaching.
    let work = tempfile::tempdir().expect("temp dir");
    let app = test_app(work.path());
    let first = work.path().join("one.conf");
    let second = work.path().join("two.conf");
    write_file(&first, "old-one\n");
    write_file(&second, "old-two\n");
    let sibling = work.path().join("two.conf.link");
    fs::hard_link(&second, &sibling).expect("hard link the second file");

    let mut backup = PayloadBackup::new(&app, "demo");
    for destination in [&first, &second] {
      let metadata = fs::symlink_metadata(destination).expect("metadata");
      backup.back_up(destination, &metadata).expect("back up");
      // What `snapshot_payload` marks on the entries it takes: these are files the
      // walk may replace, unlike the ones Pica writes itself.
      if let Some(entry) = backup.entries.last_mut() {
        entry.walked = true;
      }
    }

    // What a deploy that reached only the first file leaves behind: the rename is
    // the same one `deploy_file` makes, so the first path is a new inode.
    let staged = work.path().join("one.conf.tmp");
    write_file(&staged, "new-one\n");
    fs::rename(&staged, &first).expect("replace the first file");

    let deployed: HashSet<PathBuf> = [first.clone()].into_iter().collect();
    let failed = backup.restore_replacing(&deployed);
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(
      fs::read_to_string(&first).expect("read"),
      "old-one\n",
      "the reached file went back"
    );
    assert_eq!(
      fs::metadata(&second).expect("second").ino(),
      fs::metadata(&sibling).expect("sibling").ino(),
      "the file the walk never reached was not rewritten"
    );
    assert_eq!(fs::read_to_string(&second).expect("read"), "old-two\n");
  }

  #[test]
  fn a_restore_that_failed_partway_puts_the_rest_back_when_it_is_run_again() {
    // A rollback can fail on one path and succeed on the rest. Running it again
    // after the fault is repaired has to finish the job, and has to leave the
    // paths it already restored with the bytes, the mode and the shape they had —
    // not with what the failed install put there.
    let work = tempfile::tempdir().expect("temp dir");
    let app = test_app(work.path());
    let first_dir = work.path().join("first");
    let second_dir = work.path().join("second");
    fs::create_dir_all(&first_dir).expect("mkdir first");
    fs::create_dir_all(&second_dir).expect("mkdir second");
    let first = first_dir.join("one.conf");
    let second = second_dir.join("two.conf");
    write_file(&first, "old-one\n");
    write_file(&second, "old-two\n");
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(&second, fs::Permissions::from_mode(0o640)).expect("mode");
    }

    let mut backup = PayloadBackup::new(&app, "demo");
    for destination in [&first, &second] {
      let metadata = fs::symlink_metadata(destination).expect("metadata");
      backup.back_up(destination, &metadata).expect("back up");
    }

    // What the deploy would have done to both files.
    write_file(&first, "new-one\n");
    write_file(&second, "new-two\n");

    // The fault: the directory `second` lives in is gone, so that copy cannot go
    // back. The other one still can, and does.
    fs::remove_dir_all(&second_dir).expect("take the parent away");
    let failed = backup.restore();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].contains("two.conf"), "{failed:?}");
    assert_eq!(fs::read_to_string(&first).expect("read"), "old-one\n", "the first one went back");
    assert!(!second.exists(), "the second one could not");

    // The operator repairs the fault and the same rollback runs again. Nothing is
    // deleted, and the file the first run already restored is not replaced with
    // the failed install's bytes.
    fs::create_dir_all(&second_dir).expect("put the parent back");
    let failed = backup.restore();
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(fs::read_to_string(&first).expect("read"), "old-one\n");
    assert_eq!(fs::read_to_string(&second).expect("read"), "old-two\n");
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(
        fs::metadata(&second).expect("metadata").permissions().mode() & 0o777,
        0o640,
        "the mode the file had comes back with its bytes"
      );
    }
  }

  #[test]
  fn a_database_that_cannot_be_updated_undoes_the_files_it_just_deployed() {
    // The database is the commit point. An install whose record cannot be
    // written is not an install: the files go back to what the old record
    // describes, instead of staying on the device with nothing naming them.
    let mut fixture = PicaFixture::new();
    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "one\n")],
        &[],
        &[],
      )
      .expect("the first version installs");
    let before = db_record(&fixture.app, "demo").expect("recorded");

    // The atomic write needs `db.json.tmp-<pid>`: a directory standing on that
    // name is a write that cannot happen.
    let blocked = fixture.app.paths.db_file.with_file_name(format!(
      "{}.tmp-{}",
      fixture.app.paths.db_file.file_name().expect("name").to_string_lossy(),
      std::process::id()
    ));
    fs::create_dir_all(&blocked).expect("block the database's temporary name");
    assert!(
      pica_pkg_core::io::write_atomic(&fixture.app.paths.db_file, b"{}").is_err(),
      "the block has to be what stops the write"
    );

    fixture
      .install_without_containers(
        &format!("{}pkgmgr = pica\n", MANIFEST_DEMO.replace("pkgver = 1.0.0", "pkgver = 2.0.0")),
        &[("etc/demo.conf", "two\n")],
        &[],
        &[],
      )
      .expect_err("an install that cannot be recorded fails");

    assert_eq!(
      fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
      "one\n",
      "the file the failed install replaced is back to what is installed"
    );
    assert_eq!(
      db_record(&fixture.app, "demo").expect("still recorded"),
      before,
      "the record still describes the version that is on the device"
    );
  }

  #[test]
  fn an_install_report_that_cannot_be_written_does_not_undo_the_install() {
    // The database is the record; the report explains it. A report that cannot
    // be written leaves a recorded install and a warning, not a rollback of
    // files that are in place. The report is blocked by occupying the temporary
    // name its atomic write uses — the file itself has to stay readable so the
    // startup initialisation, which writes it when it is missing, gets past.
    let mut fixture = PicaFixture::new();
    fs::create_dir_all(&fixture.app.paths.state_dir).expect("state dir");
    fs::write(&fixture.app.paths.report_file, "{\"schema\": 1, \"reports\": {}}")
      .expect("an existing report");
    let blocked = fixture.app.paths.report_file.with_file_name(format!(
      "{}.tmp-{}",
      fixture.app.paths.report_file.file_name().expect("name").to_string_lossy(),
      std::process::id()
    ));
    fs::create_dir_all(&blocked).expect("block the report's temporary name");

    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "overlay\n")],
        &[],
        &[],
      )
      .expect("the install itself is committed");

    assert_eq!(
      fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
      "overlay\n",
      "the file must survive a report that could not be written"
    );
    let record = db_record(&fixture.app, "demo").expect("the database owns the file");
    assert!(
      record["files"]
        .as_array()
        .expect("files")
        .iter()
        .any(|file| file.as_str().is_some_and(|path| path.ends_with("/etc/demo.conf"))),
      "the record must name what was installed: {record}"
    );
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

  #[test]
  fn an_env_copy_failure_rolls_back_the_payload_it_just_deployed() {
    let mut fixture = PicaFixture::new();

    // The env file's own path is a directory, so `fs::copy` fails with EISDIR.
    // That is deterministic, needs no privileges, and it is the step that runs
    // *after* the `root/` files are already on disk.
    let env_file = fixture.path("etc/pica/env.d/demo.env");
    fs::create_dir_all(&env_file).expect("create the blocking directory");

    let err =
      deploy_with_env(&mut fixture, true).expect_err("the env copy cannot land on a directory");
    assert!(err.message.contains("copy env file failed"), "{}", err.message);

    assert!(
      !fixture.path("etc/demo.conf").exists(),
      "the payload deployed before the failure must be removed again"
    );
    assert!(db_record(&fixture.app, "demo").is_none(), "a failed install leaves no record behind");
  }

  #[test]
  fn an_install_without_an_env_file_still_deploys_its_payload() {
    let mut fixture = PicaFixture::new();

    deploy_with_env(&mut fixture, false).expect("a package without scripts/.env installs");

    assert!(fixture.path("etc/demo.conf").is_file(), "the payload is deployed");
    assert!(db_record(&fixture.app, "demo").is_some(), "and recorded");
  }

  #[test]
  fn a_failed_env_copy_can_be_retried_and_the_retry_uninstalls_cleanly() {
    let mut fixture = PicaFixture::new();
    let env_file = fixture.path("etc/pica/env.d/demo.env");
    fs::create_dir_all(&env_file).expect("create the blocking directory");

    deploy_with_env(&mut fixture, true).expect_err("the first attempt fails");
    assert!(!fixture.path("etc/demo.conf").exists(), "the first attempt leaves no orphan behind");

    fs::remove_dir(&env_file).expect("clear the blocker");
    deploy_with_env(&mut fixture, true).expect("the retry installs");

    let record = db_record(&fixture.app, "demo").expect("the retry is recorded");
    assert!(record.to_string().contains("etc/demo.conf"), "{record}");

    crate::commands::remove::pkg(&mut fixture.app, "demo").expect("uninstall");
    assert!(!fixture.path("etc/demo.conf").exists(), "uninstall removes what the retry deployed");
    assert!(db_record(&fixture.app, "demo").is_none());
  }

  #[test]
  fn a_rollback_restores_the_content_of_a_file_it_replaced() {
    let mut fixture = PicaFixture::new();
    // The artifact ships this path, so the walk replaces it — but it existed
    // before the install started, which makes it not the rollback's to remove.
    // Its content, on the other hand, is the rollback's to put back.
    let existing = fixture.path("etc/demo.conf");
    fs::create_dir_all(existing.parent().expect("parent")).expect("mkdir");
    fs::write(&existing, "original\n").expect("write the pre-existing file");

    let env_file = fixture.path("etc/pica/env.d/demo.env");
    fs::create_dir_all(&env_file).expect("create the blocking directory");

    deploy_with_env(&mut fixture, true).expect_err("the env copy fails");

    assert!(
      existing.exists(),
      "a path that was there before the install must survive the rollback"
    );
    assert_eq!(
      fs::read_to_string(&existing).expect("read"),
      "original\n",
      "the copy taken before the walk replaced it"
    );
  }

  #[test]
  fn an_env_dir_failure_rolls_back_the_payload_it_just_deployed() {
    let mut fixture = PicaFixture::new();
    // `/etc/pica` is a regular file here, so creating `/etc/pica/env.d` fails
    // with ENOTDIR — deterministic, and it needs neither privileges nor a full
    // disk. It is the step right after the `root/` files land.
    let blocker = fixture.path("etc/pica");
    fs::create_dir_all(blocker.parent().expect("parent")).expect("mkdir");
    fs::write(&blocker, "not a directory\n").expect("write the blocker");

    deploy_with_env(&mut fixture, true).expect_err("the env directory cannot be created");

    assert!(
      !fixture.path("etc/demo.conf").exists(),
      "the payload deployed before the failure is rolled back"
    );
    assert!(db_record(&fixture.app, "demo").is_none());
  }

  #[test]
  fn a_failed_copy_that_created_the_target_records_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("demo.env");
    let mut tx = InstallTransaction::new();

    let err = copy_recording_new_target(&mut tx, &target, false, || {
      // The failure a real filesystem only produces on a bad day: the target is
      // opened and written, then the write dies.
      fs::write(&target, b"half-written").expect("write the partial file");
      Err(std::io::Error::other("simulated write failure"))
    })
    .expect_err("the copy fails");

    assert_eq!(err.kind(), std::io::ErrorKind::Other);
    assert_eq!(
      tx.created_records.len(),
      1,
      "the file the failed copy left behind has to reach the rollback"
    );
    assert_eq!(tx.created_records[0].files, vec![target.display().to_string()]);
  }

  #[test]
  fn a_failed_copy_over_an_existing_target_is_not_recorded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("demo.env");
    fs::write(&target, b"original").expect("write the original file");
    let mut tx = InstallTransaction::new();

    copy_recording_new_target(&mut tx, &target, true, || {
      fs::write(&target, b"replaced").expect("overwrite the file");
      Err(std::io::Error::other("simulated write failure"))
    })
    .expect_err("the copy fails");

    assert!(
      tx.created_records.is_empty(),
      "a path that was already on the device is not the rollback's to remove"
    );
    assert!(target.is_file(), "and it is not removed either");
  }

  #[test]
  fn a_failed_copy_that_created_nothing_records_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("demo.env");
    let mut tx = InstallTransaction::new();

    copy_recording_new_target(&mut tx, &target, false, || {
      Err(std::io::Error::other("simulated open failure"))
    })
    .expect_err("the copy fails");

    assert!(tx.created_records.is_empty(), "there is no path to remove");
  }

  #[test]
  fn a_failed_env_copy_over_an_existing_file_records_nothing_and_keeps_the_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("demo.env");
    fs::write(&target, b"original\n").expect("write the original env file");
    let mut tx = InstallTransaction::new();

    copy_recording_new_target(&mut tx, &target, true, || {
      // Truncate and rewrite, then die — what a partial copy leaves behind.
      fs::write(&target, b"partial").expect("rewrite the target");
      Err(std::io::Error::other("simulated write failure"))
    })
    .expect_err("the copy fails");

    assert!(
      tx.created_records.is_empty(),
      "a path that existed before the copy is not this install's to remove"
    );

    // What the record alone does with this path: nothing. The bytes stay as the
    // failed copy left them, which is why the *transaction* keeps a copy of the
    // env file before copying it — see
    // `a_failed_env_copy_restores_the_file_that_was_already_there`.
    let mut recorded = PayloadInstall::default();
    for record in &tx.created_records {
      recorded.files.extend(record.files.iter().cloned());
      recorded.dirs.extend(record.dirs.iter().cloned());
    }
    let _ = remove_payload(&recorded);

    assert!(target.is_file(), "the pre-existing path survives the rollback");
    assert_eq!(
      fs::read_to_string(&target).expect("read the env file"),
      "partial",
      "the record on its own restores no content"
    );
  }

  #[test]
  fn a_mid_payload_failure_restores_a_file_that_was_already_there() {
    let mut fixture = PicaFixture::new();
    // The walk goes in name order: `aaa.conf` lands, `etc/demo.conf` — already
    // on the device — is replaced, and then the escaping symlink fails it. The
    // path this install created is removed, and the one it replaced goes back to
    // the copy taken before the walk started.
    let existing = fixture.path("etc/demo.conf");
    fs::create_dir_all(existing.parent().expect("parent")).expect("mkdir");
    fs::write(&existing, "original\n").expect("write the pre-existing file");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("aaa.conf", "copied first\n"), ("etc/demo.conf", "payload\n")],
        &[("zzz-escape", "/etc/passwd")],
        &[],
      )
      .expect_err("the escaping symlink fails the walk");

    assert!(err.message.contains("escapes the target root"), "{}", err.message);
    assert!(
      existing.exists(),
      "a path that was there before the install is not the rollback's to remove"
    );
    assert_eq!(
      fs::read_to_string(&existing).expect("read"),
      "original\n",
      "and its content is not the artifact's either"
    );
    assert!(
      !fixture.path("aaa.conf").exists(),
      "the file this walk created before failing is removed"
    );
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

  #[test]
  fn a_rollback_after_a_full_deploy_restores_every_file_it_replaced() {
    let fixture = PicaFixture::new();
    for (relative, contents) in
      [("etc/demo.conf", "operator one\n"), ("etc/app/settings.json", "operator two\n")]
    {
      write_file(&fixture.path(relative), contents);
    }

    let mut deployed = Deployed::new(
      fixture,
      &[("etc/demo.conf", "payload one\n"), ("etc/app/settings.json", "payload two\n")],
      &[],
    )
    .expect("a payload of plain files deploys");
    assert_eq!(
      fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"),
      "payload one\n",
      "the deploy really did replace what was there"
    );

    let leftovers = deployed.rollback();
    assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
    assert_eq!(fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"), "operator one\n");
    assert_eq!(
      fs::read_to_string(deployed.path("etc/app/settings.json")).expect("read"),
      "operator two\n",
      "every file the walk replaced comes back, not only the first"
    );
  }

  #[test]
  fn a_rollback_restores_the_env_file_the_install_overwrote() {
    let fixture = PicaFixture::new();
    write_file(
      &fixture.path("etc/pica/env.d/demo.env"),
      "PICA_KEEP_SCRIPT_INSTALL=true\noperator\n",
    );
    write_file(&fixture.path("etc/demo.conf"), "operator\n");

    let mut deployed = Deployed::new(
      fixture,
      &[("etc/demo.conf", "payload\n")],
      &[("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\nnew\n")],
    )
    .expect("the payload and the env file both land");
    assert_eq!(
      fs::read_to_string(deployed.path("etc/pica/env.d/demo.env")).expect("read"),
      "PICA_KEEP_SCRIPT_INSTALL=true\nnew\n",
      "the artifact's env file replaced the operator's"
    );

    let leftovers = deployed.rollback();
    assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
    assert_eq!(
      fs::read_to_string(deployed.path("etc/pica/env.d/demo.env")).expect("read"),
      "PICA_KEEP_SCRIPT_INSTALL=true\noperator\n",
      "a file Pica owns is still the operator's file, and it goes back like any other"
    );
    assert_eq!(fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"), "operator\n");
  }

  #[test]
  fn a_symlink_the_payload_replaced_comes_back_as_a_symlink() {
    let fixture = PicaFixture::new();
    write_file(&fixture.path("etc/keep.conf"), "operator\n");
    symlink("keep.conf", fixture.path("etc/demo.conf")).expect("create the pre-existing link");

    let mut deployed =
      Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
    assert!(
      !deployed.path("etc/demo.conf").is_symlink(),
      "the rename replaced the link with the artifact's file"
    );

    let leftovers = deployed.rollback();
    assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
    assert_eq!(
      fs::read_link(deployed.path("etc/demo.conf")).expect("read the link"),
      Path::new("keep.conf"),
      "what comes back is the link itself, not a copy of what it pointed at"
    );
    assert_eq!(
      fs::read_to_string(deployed.path("etc/keep.conf")).expect("read"),
      "operator\n",
      "and nothing was ever written through it"
    );
  }

  #[test]
  fn an_empty_directory_the_payload_replaced_comes_back_as_a_directory() {
    let fixture = PicaFixture::new();
    fs::create_dir_all(fixture.path("etc")).expect("create the parent");
    fs::create_dir(fixture.path("etc/demo.conf")).expect("create the empty directory");

    let mut deployed =
      Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
    assert!(deployed.path("etc/demo.conf").is_file(), "a file took the empty directory's place");

    let leftovers = deployed.rollback();
    assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
    assert!(
      deployed.path("etc/demo.conf").is_dir(),
      "the path is a directory again, which is what it was"
    );
  }

  #[test]
  fn a_hard_linked_destination_keeps_its_other_name_and_its_content() {
    let fixture = PicaFixture::new();
    let original = fixture.path("etc/demo.conf");
    write_file(&original, "operator\n");
    let alias = fixture.path("etc/alias.conf");
    fs::hard_link(&original, &alias).expect("create the second name");

    let mut deployed =
      Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
    assert_eq!(
      fs::read_to_string(&alias).expect("read"),
      "operator\n",
      "the rename put a new inode at the destination, so the other name never changed"
    );

    let leftovers = deployed.rollback();
    assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
    assert_eq!(fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"), "operator\n");
    assert_eq!(
      fs::read_to_string(&alias).expect("read"),
      "operator\n",
      "both names hold the bytes the device had; that they are now two inodes is what \
       restoring one file by rename means"
    );
  }

  #[test]
  fn a_restore_that_cannot_put_a_file_back_keeps_the_copies_and_names_the_path() {
    let fixture = PicaFixture::new();
    write_file(&fixture.path("etc/demo.conf"), "operator\n");
    let mut deployed =
      Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
    let stores = backup_stores(&deployed.fixture);
    assert_eq!(stores.len(), 1, "the deploy kept exactly one store: {stores:?}");

    deployed.make_the_restore_fail("etc/demo.conf");
    let leftovers = deployed.rollback();

    assert_eq!(leftovers.len(), 1, "the file that could not go back is reported: {leftovers:?}");
    assert!(leftovers[0].starts_with("content:"), "it is named as content: {leftovers:?}");
    assert!(leftovers[0].contains("etc/demo.conf"), "with its path: {leftovers:?}");
    assert_eq!(
      backup_stores(&deployed.fixture),
      stores,
      "the copies stay where the message says they are — they are the only version of that \
       file left on the device"
    );
  }

  #[test]
  fn a_completed_restore_leaves_nothing_to_do_a_second_time() {
    let fixture = PicaFixture::new();
    write_file(&fixture.path("etc/demo.conf"), "operator\n");
    let mut deployed =
      Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");

    assert!(deployed.rollback().is_empty(), "the first rollback undid everything");
    let second = deployed.rollback();
    assert!(
      second.is_empty(),
      "a rollback of an install already rolled back reports nothing: {second:?}"
    );
    assert_eq!(
      fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"),
      "operator\n",
      "and does not disturb what is already back"
    );
    assert!(backup_stores(&deployed.fixture).is_empty(), "the store went with the first rollback");
  }

  #[test]
  fn a_successful_install_removes_the_copies_it_took() {
    let mut fixture = PicaFixture::new();
    write_file(&fixture.path("etc/demo.conf"), "operator\n");

    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "payload\n")],
        &[],
        &[],
      )
      .expect("the install completes");

    assert_eq!(
      fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
      "payload\n",
      "the install is the one that wins"
    );
    assert!(backup_stores(&fixture).is_empty(), "a committed install collects its own copies");
  }

  #[test]
  fn a_recorded_install_names_the_file_it_replaced_and_says_nothing_about_the_copies() {
    let mut fixture = PicaFixture::new();
    write_file(&fixture.path("etc/demo.conf"), "operator\n");

    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "payload\n")],
        &[],
        &[],
      )
      .expect("the install completes");

    // The record is the same one it always was: the file this install wrote,
    // whether or not that file was already there. Keeping a copy of the previous
    // content adds no path to it, which is why the copy is not installed, not
    // counted, and not something `-R` can delete.
    let record = db_record(&fixture.app, "demo").expect("the install is recorded");
    let text = record.to_string();
    assert!(text.contains("etc/demo.conf"), "the replaced path is owned: {text}");
    assert!(!text.contains("cache/backup"), "the copy is not: {text}");

    crate::commands::remove::pkg(&mut fixture.app, "demo").expect("uninstall");
    assert!(
      !fixture.path("etc/demo.conf").exists(),
      "and the uninstall that follows the existing ownership policy still removes it"
    );
  }

  #[test]
  fn an_install_that_replaces_nothing_never_makes_a_backup_directory() {
    let mut fixture = PicaFixture::new();

    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "payload\n")],
        &[],
        &[],
      )
      .expect("a first install completes");

    assert!(
      !fixture.app.paths.cache_dir.join("backup").exists(),
      "an install that destroys nothing copies nothing, so it creates no directory either"
    );
  }

  #[cfg(unix)]
  #[test]
  fn the_copies_are_only_readable_by_the_user_who_installed_them() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = PicaFixture::new();
    let original = fixture.path("etc/demo.conf");
    write_file(&original, "operator\n");
    fs::set_permissions(&original, fs::Permissions::from_mode(0o644))
      .expect("make the device file world-readable");

    let mut deployed =
      Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
    // A restore that fails keeps the store, which is the only moment the copies
    // are on disk while a test can still look at them.
    deployed.make_the_restore_fail("etc/demo.conf");
    assert_eq!(deployed.rollback().len(), 1, "kept so the copies are still there");

    let store = backup_stores(&deployed.fixture).into_iter().next().expect("one store");
    let mode = fs::metadata(&store).expect("stat the store").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "the store itself is the installer's alone");
    let copies: Vec<PathBuf> =
      fs::read_dir(&store).expect("read the store").flatten().map(|entry| entry.path()).collect();
    assert!(!copies.is_empty(), "the copy of the replaced file is in the store");
    for copy in copies {
      assert_eq!(
        fs::symlink_metadata(&copy).expect("stat the copy").permissions().mode() & 0o777,
        0o600,
        "{} was world-readable on the device; the copy of it is not",
        copy.display()
      );
    }
  }

  #[test]
  fn a_backup_that_cannot_be_written_stops_the_install_before_anything_is_written() {
    let mut fixture = PicaFixture::new();
    write_file(&fixture.path("etc/demo.conf"), "operator\n");
    write_file(&fixture.path("aaa.conf"), "operator too\n");
    // The cache directory's backup root is a regular file, so making a store in
    // it fails with `ENOTDIR` for any user, root included.
    let blocker = fixture.app.paths.cache_dir.join("backup");
    write_file(&blocker, "not a directory\n");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "payload\n"), ("aaa.conf", "payload too\n")],
        &[],
        &[],
      )
      .expect_err("the copies cannot be kept");

    assert!(
      err.message.contains("backup"),
      "the message names the store it could not make: {}",
      err.message
    );
    assert_eq!(
      fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
      "operator\n",
      "the file the walk would have replaced is untouched"
    );
    assert_eq!(
      fs::read_to_string(fixture.path("aaa.conf")).expect("read"),
      "operator too\n",
      "and so is the one it would have replaced"
    );
    assert!(db_record(&fixture.app, "demo").is_none(), "and nothing was recorded");
  }

  #[cfg(unix)]
  #[test]
  fn a_socket_at_a_payload_destination_stops_the_install_before_anything_is_written() {
    use std::os::unix::fs::FileTypeExt;

    let mut fixture = PicaFixture::new();
    let blocker = fixture.path("etc/demo.conf");
    write_file(&fixture.path("etc/other.conf"), "operator\n");
    std::os::unix::net::UnixListener::bind(&blocker).expect("bind a socket at the destination");

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[("etc/demo.conf", "payload\n"), ("aaa.conf", "payload too\n")],
        &[],
        &[],
      )
      .expect_err("a socket cannot be put back from a copy");

    assert!(err.message.contains("neither a file, a symlink nor a directory"), "{}", err.message);
    assert!(
      fs::symlink_metadata(&blocker).expect("stat").file_type().is_socket(),
      "the socket is exactly what it was"
    );
    assert!(
      !fixture.path("aaa.conf").exists(),
      "not even the file the walk would have reached first was written"
    );
    assert_eq!(
      fs::read_to_string(fixture.path("etc/other.conf")).expect("read"),
      "operator\n",
      "the device kept every byte it had"
    );
    assert!(db_record(&fixture.app, "demo").is_none());
  }

  #[test]
  fn a_finish_that_cannot_remove_the_copies_leaves_them_and_says_so() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut app = test_app(dir.path());
    let original = dir.path().join("device.conf");
    fs::write(&original, "operator\n").expect("write the device file");

    let mut backup = PayloadBackup::new(&app, "demo");
    let metadata = fs::symlink_metadata(&original).expect("stat the device file");
    backup.back_up(&original, &metadata).expect("keep a copy");
    let store = backup.dir.as_ref().expect("the copy made a store").path().to_path_buf();

    let mut tx = InstallTransaction::new();
    tx.payload_backup = Some(backup);
    tx.finish_with(
      &mut app,
      |_| Ok(()),
      |_| Ok(()),
      |_| Err(std::io::Error::other("simulated copy removal failure")),
    );

    assert!(store.is_dir(), "the install completed, but the copies were not deleted with it");
    assert_eq!(
      fs::read_dir(&store).expect("read the store").flatten().count(),
      1,
      "and the copy it made of the file it replaced is still inside"
    );
  }

  #[test]
  fn a_failed_env_copy_restores_the_file_that_was_already_there() {
    let dir = tempfile::tempdir().expect("temp dir");
    let app = test_app(dir.path());
    let target = dir.path().join("demo.env");
    fs::write(&target, "original\n").expect("write the env file the operator left");

    // The same store step 7 hands the env file to. It is taken directly here
    // because the copy failure this test needs — a write that dies after
    // truncating — is not something a filesystem can be made to do on demand.
    let mut backup = PayloadBackup::new(&app, "demo");
    backup
      .back_up(&target, &fs::symlink_metadata(&target).expect("stat the env file"))
      .expect("keep the copy");
    let mut tx = InstallTransaction::new();
    tx.payload_backup = Some(backup);

    copy_recording_new_target(&mut tx, &target, true, || {
      fs::write(&target, "partial").expect("truncate and rewrite the env file");
      Err(std::io::Error::other("simulated write failure"))
    })
    .expect_err("the copy fails");
    assert_eq!(
      fs::read_to_string(&target).expect("read"),
      "partial",
      "the record on its own leaves the half-written file in place"
    );

    let failed = tx.payload_backup.as_mut().expect("the transaction holds the store").restore();
    assert!(failed.is_empty(), "the env file goes back: {failed:?}");
    assert_eq!(
      fs::read_to_string(&target).expect("read"),
      "original\n",
      "the copy taken before the copy is what puts it back"
    );
    assert!(
      tx.created_records.is_empty(),
      "and the env file, which existed already, is still not this install's to remove"
    );
  }

  #[test]
  fn a_failure_after_the_first_kept_script_is_copied_rolls_it_back() {
    let mut fixture = PicaFixture::new();
    // `update` is copied first; `remove` is declared but absent from the
    // artifact, so the loop stops right after one successful copy. No lifecycle
    // hook runs on a first install here, which keeps the rollback limited to
    // the file this install actually wrote.
    let manifest = format!(
      "{MANIFEST_DEMO}pkgmgr = pica\nscript_update = scripts/update\n\
       script_remove = scripts/remove\n"
    );
    let err = fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[
          ("scripts/update", "#!/bin/sh\nexit 0\n"),
          ("scripts/.env", "PICA_KEEP_SCRIPT_UPDATE=true\nPICA_KEEP_SCRIPT_REMOVE=true\n"),
        ],
      )
      .expect_err("the declared remove script is not in the artifact");

    assert!(err.message.contains("script not found in package"), "{}", err.message);
    assert!(
      !fixture.path("usr/lib/pica/scripts/demo/scripts/update").exists(),
      "the script copied before the failure is rolled back"
    );
  }

  #[test]
  fn a_failed_upgrade_restores_the_previous_scripts() {
    let mut fixture = PicaFixture::new();
    let manifest = format!(
      "{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n\
       script_remove = scripts/remove\n"
    );
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\nPICA_KEEP_SCRIPT_REMOVE=true\n");
    let install_script = "usr/lib/pica/scripts/demo/scripts/install";
    let remove_script = "usr/lib/pica/scripts/demo/scripts/remove";

    // Version 1 keeps both scripts, so the upgrade has old files to replace.
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[
          ("scripts/install", "#!/bin/sh\necho old-install\n"),
          ("scripts/remove", "#!/bin/sh\necho old-remove\n"),
          env,
        ],
      )
      .expect("the first install keeps both scripts");

    // Version 2 drops `scripts/remove`: the new `install` is copied, then the
    // loop stops. The old directory was moved aside, not deleted.
    let err = fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new-install\n"), env],
      )
      .expect_err("version 2 does not ship the declared remove script");

    assert!(err.message.contains("script not found in package"), "{}", err.message);
    assert_eq!(
      fs::read_to_string(fixture.path(install_script)).expect("read the old install script"),
      "#!/bin/sh\necho old-install\n",
      "the old script is back, not the new copy"
    );
    assert_eq!(
      fs::read_to_string(fixture.path(remove_script)).expect("read the old remove script"),
      "#!/bin/sh\necho old-remove\n",
      "the script the new version no longer ships is restored too"
    );
    assert!(
      !fixture.path("usr/lib/pica/scripts/demo.rollback").exists(),
      "the backup is put back, not left behind"
    );
    assert!(
      !fixture.path("usr/lib/pica/scripts/demo.rollback.json").exists(),
      "and the marker that described it is gone too"
    );
  }

  #[test]
  fn a_failure_after_the_scripts_are_kept_still_restores_the_old_ones() {
    let mut fixture = PicaFixture::new();
    let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
    let kept = "usr/lib/pica/scripts/demo/scripts/install";

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho old\n"), env],
      )
      .expect("the first install keeps the script");

    // The upgrade replaces the kept scripts and *then* fails on the payload.
    // The backup belongs to the transaction, not to this one step, so the old
    // script still comes back.
    let err = fixture
      .install_without_containers(
        &manifest,
        &[("aaa.conf", "overlay\n")],
        &[("zzz-escape", "/etc/passwd")],
        &[("scripts/install", "#!/bin/sh\necho new\n"), env],
      )
      .expect_err("the escaping symlink fails the payload");

    assert!(err.message.contains("escapes the target root"), "{}", err.message);
    assert_eq!(
      fs::read_to_string(fixture.path(kept)).expect("read the kept script"),
      "#!/bin/sh\necho old\n",
      "a failure after the scripts were kept must still put the old ones back"
    );
    assert!(!fixture.path("usr/lib/pica/scripts/demo.rollback").exists());
  }

  #[test]
  fn a_backup_without_a_marker_is_moved_aside_and_kept() {
    let mut fixture = PicaFixture::new();
    let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho old\n"), env],
      )
      .expect("the first install keeps the script");

    // A backup nobody can classify: it may be the only copy of those scripts,
    // so it is moved to a name of its own and never deleted.
    let backup = fixture.path("usr/lib/pica/scripts/demo.rollback");
    fs::create_dir_all(&backup).expect("create the untracked backup");
    fs::write(backup.join("stale"), "old copy").expect("write a file in it");

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new\n"), env],
      )
      .expect("the upgrade still runs");

    assert!(!backup.exists(), "the canonical name is free again");
    let moved = moved_aside(&fixture, "demo.rollback");
    assert_eq!(moved.len(), 1, "exactly one backup was moved aside: {moved:?}");
    assert_eq!(
      fs::read_to_string(moved[0].join("stale")).expect("read the moved copy"),
      "old copy",
      "and the only copy is kept"
    );
  }

  #[test]
  fn a_pending_backup_is_moved_aside_and_kept() {
    let mut fixture = PicaFixture::new();
    let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho old\n"), env],
      )
      .expect("the first install keeps the script");

    // What a crash between the rename and the database write leaves behind.
    let backup = fixture.path("usr/lib/pica/scripts/demo.rollback");
    fs::create_dir_all(&backup).expect("create the pending backup");
    fs::write(backup.join("stale"), "old copy").expect("write a file in it");
    write_marker(&fixture, "demo.rollback.json", "cafe", ScriptsState::Pending);

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new\n"), env],
      )
      .expect("the upgrade still runs");

    assert!(!backup.exists(), "the canonical name is free again");
    let stale = fixture.path("usr/lib/pica/scripts/demo.rollback.cafe");
    assert_eq!(
      fs::read_to_string(stale.join("stale")).expect("read the moved copy"),
      "old copy",
      "a pending backup is never deleted"
    );
    assert!(
      fixture.path("usr/lib/pica/scripts/demo.rollback.cafe.json").is_file(),
      "its marker travels with it"
    );
  }

  #[test]
  fn a_committed_backup_left_behind_is_removed_before_the_next_upgrade() {
    let mut fixture = PicaFixture::new();
    let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho old\n"), env],
      )
      .expect("the first install keeps the script");

    // What a crash between the database write and the cleanup leaves behind:
    // the marker says the record landed, so the backup is superseded.
    let backup = fixture.path("usr/lib/pica/scripts/demo.rollback");
    fs::create_dir_all(&backup).expect("create the stale backup");
    fs::write(backup.join("stale"), "old copy").expect("write a file in it");
    write_marker(&fixture, "demo.rollback.json", "beef", ScriptsState::Committed);

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new\n"), env],
      )
      .expect("the upgrade still runs");

    assert!(!backup.exists(), "the committed backup is removed");
    assert!(
      !fixture.path("usr/lib/pica/scripts/demo.rollback.json").exists(),
      "and so is its marker"
    );
    assert_eq!(
      fs::read_to_string(fixture.path("usr/lib/pica/scripts/demo/scripts/install"))
        .expect("read the kept script"),
      "#!/bin/sh\necho new\n"
    );
  }

  #[test]
  fn an_unreadable_marker_is_no_reason_to_delete_a_backup() {
    let mut fixture = PicaFixture::new();
    let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho old\n"), env],
      )
      .expect("the first install keeps the script");

    let backup = fixture.path("usr/lib/pica/scripts/demo.rollback");
    fs::create_dir_all(&backup).expect("create the backup");
    fs::write(backup.join("stale"), "old copy").expect("write a file in it");
    // A schema this build does not know: it must not be guessed to be done.
    fs::write(
      fixture.path("usr/lib/pica/scripts/demo.rollback.json"),
      r#"{"schema":99,"state":"committed"}"#,
    )
    .expect("write the unknown marker");

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new\n"), env],
      )
      .expect("the upgrade still runs");

    let moved = moved_aside(&fixture, "demo.rollback");
    assert_eq!(moved.len(), 1, "the unknown backup is kept under another name: {moved:?}");
    assert_eq!(
      fs::read_to_string(moved[0].join("stale")).expect("read the moved copy"),
      "old copy"
    );
    assert_eq!(
      fs::read_to_string(PathBuf::from(format!("{}.json", moved[0].display())))
        .expect("read the moved marker"),
      r#"{"schema":99,"state":"committed"}"#,
      "the marker this build cannot read travels with the backup instead of being overwritten"
    );
    assert!(
      !fixture.path("usr/lib/pica/scripts/demo.rollback.json").exists(),
      "and its canonical name is free"
    );
  }

  #[test]
  fn a_corrupt_marker_moves_with_its_backup() {
    let mut fixture = PicaFixture::new();
    let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho old\n"), env],
      )
      .expect("the first install keeps the script");

    let backup = fixture.path("usr/lib/pica/scripts/demo.rollback");
    fs::create_dir_all(&backup).expect("create the backup");
    fs::write(backup.join("stale"), "old copy").expect("write a file in it");
    // Not even valid JSON: the state is unknown, but the file is still the
    // description of the backup beside it.
    fs::write(fixture.path("usr/lib/pica/scripts/demo.rollback.json"), "{ not json")
      .expect("write the corrupt marker");

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new\n"), env],
      )
      .expect("the upgrade still runs");

    assert!(!backup.exists(), "the canonical backup name is free again");
    assert!(
      !fixture.path("usr/lib/pica/scripts/demo.rollback.json").exists(),
      "and so is the marker name"
    );
    let moved = moved_aside(&fixture, "demo.rollback");
    assert_eq!(moved.len(), 1, "the backup is kept under another name: {moved:?}");
    assert_eq!(fs::read_to_string(moved[0].join("stale")).expect("read"), "old copy");
    assert_eq!(
      fs::read_to_string(PathBuf::from(format!("{}.json", moved[0].display())))
        .expect("read the moved marker"),
      "{ not json",
      "the corrupt marker travels with the backup instead of being overwritten"
    );
  }

  #[test]
  fn a_committed_backup_whose_marker_cannot_be_removed_blocks_the_next_backup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let backup = dir.path().join("scripts.rollback");
    fs::create_dir_all(&backup).expect("mkdir");
    fs::write(backup.join("stale"), "old copy").expect("write");
    let marker_path = dir.path().join("scripts.rollback.json");
    fs::write(&marker_path, "anything").expect("write the marker");
    let mut app = test_app(dir.path());

    let free = delete_committed_backup(&mut app, &backup, &marker_path, |_| {
      Err(std::io::Error::other("simulated marker removal failure"))
    });

    assert!(!free, "the canonical marker name is not free");
    assert!(!backup.exists(), "the backup itself was removed");
    assert!(marker_path.is_file(), "the marker it could not remove is still there");
  }

  #[test]
  fn an_orphan_marker_whose_name_is_taken_is_left_alone() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker_path = dir.path().join("scripts.rollback.json");
    fs::write(&marker_path, "marker").expect("write the marker");
    let blocker = dir.path().join("scripts.rollback.json.orphan.cafe");
    fs::write(&blocker, "not ours").expect("write the file in the way");
    let mut app = test_app(dir.path());

    let free = move_orphan_marker_aside(&mut app, &marker_path, "cafe");

    assert!(!free, "the canonical marker name is still taken");
    assert_eq!(
      fs::read_to_string(&blocker).expect("read the file in the way"),
      "not ours",
      "the occupied name is untouched"
    );
    assert_eq!(
      fs::read_to_string(&marker_path).expect("read the marker"),
      "marker",
      "and the orphan marker stays where it was"
    );
  }

  #[test]
  fn a_matching_version_does_not_make_a_pending_backup_committed() {
    let pending = ScriptsMarker {
      txid: "t".to_string(),
      pkgname: "demo".to_string(),
      pkgver: "1.0.0".to_string(),
      pkgrel: "1".to_string(),
      state: ScriptsState::Pending,
    };
    assert_eq!(
      backup_disposition(Some(&pending)),
      BackupDisposition::MoveAside,
      "the database version says nothing about which transaction made the backup"
    );
    let committed = ScriptsMarker { state: ScriptsState::Committed, ..pending };
    assert_eq!(backup_disposition(Some(&committed)), BackupDisposition::DeleteCommitted);
    assert_eq!(backup_disposition(None), BackupDisposition::MoveAside);
    assert!(ScriptsMarker::parse("not json").is_none());
    assert!(
      ScriptsMarker::parse(r#"{"schema":1,"state":"committed"}"#).is_none(),
      "fields missing"
    );
  }

  #[test]
  fn a_marker_that_cannot_be_written_puts_the_old_directory_back() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("scripts");
    fs::create_dir_all(&target).expect("mkdir");
    fs::write(target.join("install"), "old").expect("write the old script");
    let backup = dir.path().join("scripts.rollback");
    let mut app = test_app(dir.path());
    let marker = ScriptsMarker {
      txid: "t".to_string(),
      pkgname: "demo".to_string(),
      pkgver: "2.0.0".to_string(),
      pkgrel: "1".to_string(),
      state: ScriptsState::Pending,
    };

    let start = begin_scripts_backup(&mut app, &target, &backup, marker, |_, _| {
      Err(std::io::Error::other("simulated write failure"))
    })
    .expect("the failure is handled, not fatal");

    assert!(matches!(start, BackupStart::None), "no untracked backup is left active");
    assert_eq!(
      fs::read_to_string(target.join("install")).expect("read the old script"),
      "old",
      "the old directory is back where it was"
    );
    assert!(!backup.exists(), "and no backup is left behind");
  }

  #[test]
  fn a_failed_restore_keeps_the_backup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("scripts");
    let backup = dir.path().join("scripts.rollback");
    fs::create_dir_all(&backup).expect("mkdir backup");
    fs::write(backup.join("install"), "old").expect("write the only copy");
    fs::create_dir_all(&target).expect("mkdir target");
    fs::write(target.join("partial"), "new").expect("write a partial script");

    let err = restore_scripts_backup(&target, &backup, |_, _| {
      Err(std::io::Error::other("simulated rename failure"))
    })
    .expect_err("the rename fails");

    assert_eq!(err.kind(), std::io::ErrorKind::Other);
    assert_eq!(
      fs::read_to_string(backup.join("install")).expect("read the backup"),
      "old",
      "the only copy is still there"
    );
  }

  #[test]
  fn a_finish_that_cannot_remove_the_backup_keeps_it_and_its_marker() {
    let dir = tempfile::tempdir().expect("temp dir");
    let backup = dir.path().join("scripts.rollback");
    fs::create_dir_all(&backup).expect("mkdir");
    let marker = ScriptsMarker {
      txid: "t".to_string(),
      pkgname: "demo".to_string(),
      pkgver: "2.0.0".to_string(),
      pkgrel: "1".to_string(),
      state: ScriptsState::Committed,
    };
    let marker_path = dir.path().join("scripts.rollback.json");
    fs::write(&marker_path, marker.to_json()).expect("write the marker");

    let mut app = test_app(dir.path());
    let mut tx = InstallTransaction::new();
    tx.record_scripts_backup(dir.path().join("scripts"), backup.clone(), marker);
    tx.finish_with(
      &mut app,
      |_| Err(std::io::Error::other("simulated removal failure")),
      |_| Ok(()),
      |dir| fs::remove_dir_all(dir),
    );

    assert!(backup.is_dir(), "the backup is still there");
    assert!(marker_path.is_file(), "and so is the marker that classifies it");
  }

  #[test]
  fn a_finish_that_cannot_remove_the_marker_leaves_it_behind() {
    let dir = tempfile::tempdir().expect("temp dir");
    let backup = dir.path().join("scripts.rollback");
    fs::create_dir_all(&backup).expect("mkdir");
    let marker = ScriptsMarker {
      txid: "t".to_string(),
      pkgname: "demo".to_string(),
      pkgver: "2.0.0".to_string(),
      pkgrel: "1".to_string(),
      state: ScriptsState::Committed,
    };
    let marker_path = dir.path().join("scripts.rollback.json");
    fs::write(&marker_path, marker.to_json()).expect("write the marker");

    let mut app = test_app(dir.path());
    let mut tx = InstallTransaction::new();
    tx.record_scripts_backup(dir.path().join("scripts"), backup.clone(), marker);
    tx.finish_with(
      &mut app,
      |dir| fs::remove_dir_all(dir),
      |_| Err(std::io::Error::other("simulated marker removal failure")),
      |dir| fs::remove_dir_all(dir),
    );

    assert!(!backup.exists(), "the backup is gone");
    assert!(marker_path.is_file(), "the marker it could not remove is still there");
  }

  #[test]
  fn an_orphan_marker_is_moved_aside_instead_of_overwritten() {
    let mut fixture = PicaFixture::new();
    let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho old\n"), env],
      )
      .expect("the first install keeps the script");

    // A marker whose directory is gone: what a crash between the two renames of
    // a move-aside leaves, or a marker whose removal failed.
    write_marker(&fixture, "demo.rollback.json", "cafe", ScriptsState::Pending);

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new\n"), env],
      )
      .expect("the upgrade still runs");

    let orphans: Vec<PathBuf> = fs::read_dir(fixture.path("usr/lib/pica/scripts"))
      .expect("read the scripts dir")
      .flatten()
      .map(|entry| entry.path())
      .filter(|path| {
        path
          .file_name()
          .and_then(|name| name.to_str())
          .is_some_and(|name| name.starts_with("demo.rollback.json.orphan."))
      })
      .collect();
    assert_eq!(orphans.len(), 1, "the orphan marker is kept under another name");
    let text = fs::read_to_string(&orphans[0]).expect("read the orphan marker");
    assert_eq!(
      ScriptsMarker::parse(&text).map(|marker| marker.txid),
      Some("cafe".to_string()),
      "and it still names its own transaction"
    );
  }

  #[test]
  fn a_non_empty_directory_at_the_move_aside_name_is_left_alone() {
    let mut fixture = PicaFixture::new();
    let manifest = format!(
      "{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n\
       script_remove = scripts/remove\n"
    );
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\nPICA_KEEP_SCRIPT_REMOVE=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[
          ("scripts/install", "#!/bin/sh\necho old-install\n"),
          ("scripts/remove", "#!/bin/sh\necho old-remove\n"),
          env,
        ],
      )
      .expect("the first install keeps both scripts");

    // The name the marker asks for is already a non-empty directory, so the
    // move-aside fails without needing permissions.
    let backup = fixture.path("usr/lib/pica/scripts/demo.rollback");
    fs::create_dir_all(&backup).expect("create the pending backup");
    fs::write(backup.join("stale"), "old copy").expect("write a file in it");
    write_marker(&fixture, "demo.rollback.json", "cafe", ScriptsState::Pending);
    let blocker = fixture.path("usr/lib/pica/scripts/demo.rollback.cafe");
    fs::create_dir_all(&blocker).expect("create the blocker");
    fs::write(blocker.join("keep"), "not ours").expect("write the blocker");

    // Version 2 fails after its first script, with no backup taken.
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new-install\n"), env],
      )
      .expect_err("version 2 does not ship the declared remove script");

    assert_eq!(
      fs::read_to_string(backup.join("stale")).expect("read the old backup"),
      "old copy",
      "the backup that could not be moved is untouched"
    );
    assert_eq!(
      fs::read_to_string(blocker.join("keep")).expect("read the blocker"),
      "not ours",
      "and the name in the way is untouched too"
    );
  }

  /// Installs the first version with two kept scripts, leaves a pending backup
  /// behind, lets `create` occupy the name the move-aside would use, and then
  /// runs an upgrade that fails after its first script.
  fn failing_upgrade_with_pending_backup(
    fixture: &mut PicaFixture,
    create: impl FnOnce(&Path),
  ) -> (PathBuf, PathBuf) {
    let manifest = format!(
      "{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n\
       script_remove = scripts/remove\n"
    );
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\nPICA_KEEP_SCRIPT_REMOVE=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[
          ("scripts/install", "#!/bin/sh\necho old-install\n"),
          ("scripts/remove", "#!/bin/sh\necho old-remove\n"),
          env,
        ],
      )
      .expect("the first install keeps both scripts");

    let backup = fixture.path("usr/lib/pica/scripts/demo.rollback");
    fs::create_dir_all(&backup).expect("create the pending backup");
    fs::write(backup.join("stale"), "old copy").expect("write the only copy");
    write_marker(fixture, "demo.rollback.json", "cafe", ScriptsState::Pending);
    let blocker = fixture.path("usr/lib/pica/scripts/demo.rollback.cafe");
    create(&blocker);

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new-install\n"), env],
      )
      .expect_err("version 2 does not ship the declared remove script");

    (backup, blocker)
  }

  #[test]
  fn an_empty_directory_at_the_move_aside_name_is_left_alone() {
    let mut fixture = PicaFixture::new();
    let (backup, blocker) = failing_upgrade_with_pending_backup(&mut fixture, |path| {
      fs::create_dir(path).expect("create an empty directory");
    });

    assert!(blocker.is_dir(), "the empty directory is still there");
    assert_eq!(fs::read_dir(&blocker).expect("read it").count(), 0, "and still empty");
    assert_eq!(
      fs::read_to_string(backup.join("stale")).expect("read the backup"),
      "old copy",
      "the old backup is untouched"
    );
    assert!(
      fixture.path("usr/lib/pica/scripts/demo.rollback.json").is_file(),
      "and so is its marker"
    );
  }

  #[test]
  fn a_file_at_the_move_aside_name_is_left_alone() {
    let mut fixture = PicaFixture::new();
    let (backup, blocker) = failing_upgrade_with_pending_backup(&mut fixture, |path| {
      fs::write(path, "not ours").expect("write the file in the way");
    });

    assert!(blocker.is_file(), "the file is still there");
    assert_eq!(fs::read_to_string(&blocker).expect("read it"), "not ours", "with its content");
    assert_eq!(fs::read_to_string(backup.join("stale")).expect("read the backup"), "old copy");
  }

  #[test]
  fn a_marker_that_cannot_follow_the_backup_is_not_overwritten() {
    let mut fixture = PicaFixture::new();
    let manifest = format!(
      "{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n\
       script_remove = scripts/remove\n"
    );
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\nPICA_KEEP_SCRIPT_REMOVE=true\n");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[
          ("scripts/install", "#!/bin/sh\necho old-install\n"),
          ("scripts/remove", "#!/bin/sh\necho old-remove\n"),
          env,
        ],
      )
      .expect("the first install keeps both scripts");

    let backup = fixture.path("usr/lib/pica/scripts/demo.rollback");
    fs::create_dir_all(&backup).expect("create the pending backup");
    fs::write(backup.join("stale"), "old copy").expect("write the only copy");
    write_marker(&fixture, "demo.rollback.json", "cafe", ScriptsState::Pending);
    // The backup's new name is free, but its marker's destination is taken.
    let blocking_marker = fixture.path("usr/lib/pica/scripts/demo.rollback.cafe.json");
    fs::write(&blocking_marker, "not ours").expect("write the file in the way");

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new-install\n"), env],
      )
      .expect_err("version 2 does not ship the declared remove script");

    assert_eq!(
      fs::read_to_string(&blocking_marker).expect("read the file in the way"),
      "not ours",
      "the occupied marker name is left untouched"
    );
    assert!(
      !fixture.path("usr/lib/pica/scripts/demo.rollback.cafe").exists(),
      "the directory move was undone instead of leaving the pair split"
    );
    assert_eq!(
      fs::read_to_string(backup.join("stale")).expect("read the backup"),
      "old copy",
      "the backup is back under its canonical name"
    );
    let marker_text = fs::read_to_string(fixture.path("usr/lib/pica/scripts/demo.rollback.json"))
      .expect("read the canonical marker");
    assert_eq!(
      ScriptsMarker::parse(&marker_text).map(|marker| marker.txid),
      Some("cafe".to_string()),
      "and the marker that could not follow is still there"
    );
  }

  #[test]
  fn a_successful_upgrade_drops_the_backup() {
    let mut fixture = PicaFixture::new();
    let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
    let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
    let kept = "usr/lib/pica/scripts/demo/scripts/install";
    let backup = "usr/lib/pica/scripts/demo.rollback";

    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho old\n"), env],
      )
      .expect("the first install keeps the script");
    fixture
      .install_without_containers(
        &manifest,
        &[],
        &[],
        &[("scripts/install", "#!/bin/sh\necho new\n"), env],
      )
      .expect("the upgrade keeps it too");

    assert_eq!(
      fs::read_to_string(fixture.path(kept)).expect("read the kept script"),
      "#!/bin/sh\necho new\n",
      "the upgrade replaces the old bytes"
    );
    assert!(!fixture.path(backup).exists(), "and the old copy is cleaned up");
    assert!(
      !fixture.path("usr/lib/pica/scripts/demo.rollback.json").exists(),
      "its marker is cleaned up with it"
    );
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

  #[test]
  fn an_unreadable_installed_list_keeps_what_the_transaction_already_recorded() {
    let installed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let reads = std::rc::Rc::new(std::cell::RefCell::new(0));
    let stub = ListingStub {
      installed,
      reads,
      // The read after `base` answers; the one after `app` does not.
      fail_from_call: 1,
      fail_install_label: "app",
    };

    let work = tempfile::tempdir().expect("temp dir");
    let mut app = test_app(work.path());
    app.package_manager = Some(Box::new(stub));

    let mut tx = InstallTransaction::new();
    tx.tx_before = Some(Vec::new());

    install_phase(&mut app, &mut tx, "base", &["base-dep".to_string()]).expect("base installs");
    assert_eq!(tx.tx_added, vec!["base-dep".to_string()]);

    install_phase(&mut app, &mut tx, "app", &["app-dep".to_string()])
      .expect_err("the app install fails");

    assert_eq!(
      tx.tx_added,
      vec!["base-dep".to_string()],
      "a listing that stopped answering must not erase what was already confirmed"
    );
    assert!(tx.tx_changed.is_empty(), "and it must not invent version changes");
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
    let err =
      pkgfile_into(&mut app, &apk, None, EntrySource::Local, IndexSizes::default(), &target)
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
    assert_eq!(
      record["files"].as_array().map(Vec::len),
      Some(1),
      "the record holds the file the artifact deployed: {record}"
    );
  }

  #[test]
  fn the_header_names_which_of_the_four_things_this_is() {
    assert_eq!(transaction_header("demo", "1.0.0-1", None), "Installing demo (1.0.0-1)");
    assert_eq!(
      transaction_header("demo", "1.1.0-1", Some("1.0.0-1")),
      "Upgrading demo (1.0.0-1 -> 1.1.0-1)"
    );
    assert_eq!(
      transaction_header("demo", "0.9.0-1", Some("1.0.0-1")),
      "Downgrading demo (1.0.0-1 -> 0.9.0-1)"
    );
    // The same version on disk is a reinstall, not an upgrade.
    assert_eq!(
      transaction_header("demo", "1.0.0-1", Some("1.0.0-1")),
      "demo 1.0.0-1 is already installed; reinstalling"
    );
  }

  #[test]
  fn only_published_sizes_are_shown() {
    assert!(size_lines(IndexSizes::default()).is_empty(), "nothing published, nothing to say");
    // A zero is a feed declining to measure, not a package of no size.
    assert!(size_lines(IndexSizes { installed: Some(0), download: Some(0) }).is_empty());

    assert_eq!(
      size_lines(IndexSizes { installed: Some(2 * 1024 * 1024), download: Some(1536) }),
      vec!["  Package installed size: 2.0 MiB".to_string(), "  Download size: 1.5 KiB".to_string(),]
    );
  }

  #[test]
  fn the_summary_counts_the_files_the_payload_wrote() {
    // Directories are not files, and the size is the bytes actually written.
    let payload = PayloadInstall {
      files: vec!["/etc/demo.conf".to_string(), "/usr/bin/demo".to_string()],
      dirs: vec!["/etc".to_string()],
      size: 1536,
    };

    assert_eq!(install_summary(Lang::EnUs, &payload), "Installed 2 files, 1.5 KiB");
  }

  #[test]
  fn the_precheck_summary_names_each_source_it_found() {
    let report = json!({
      "kmod": [{"name": "kmod-tun", "status": "feed"}],
      "base": [{"name": "present", "status": "installed"}],
      "app": [
        {"name": "helper", "status": "packaged"},
        {"name": "extra", "status": "feed", "optional": true},
      ],
    });
    let deps = DeclaredDeps { kmod: vec![], base: vec![], app: vec![] };
    // An empty plan covers no optional dependency, which is the default when the
    // user was never asked.
    let optional = optional::Plan::unavailable(&deps, "this backend cannot install them");

    assert_eq!(
      dependency_source_lines(&report, &optional),
      vec!["  kmod-tun (feed)".to_string(), "  helper (packaged)".to_string()],
      "an installed dependency needs nothing, and a declined optional one is not being brought in"
    );
  }
}
