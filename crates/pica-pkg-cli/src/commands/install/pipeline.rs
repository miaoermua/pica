use super::feed::{install_via_feeds_or_pkgs, pkg_list_diff_added, reorder_app_list};
use super::precheck::{build_precheck_report, summarize_missing_precheck, validate_package};
use crate::app::{
  ensure_dirs, resolve_lang, App, CliError, CliResult, E_CONFIG_INVALID, E_IO, E_MANIFEST_INVALID,
  E_PACKAGE_INVALID, E_RUNTIME, E_VERSION_INCOMPATIBLE,
};
use crate::installed::EntrySource;
use crate::pkgmgr::PackageManagerKind;
use crate::state::{db_has_installed, db_set_installed, report_set_install_result};
use crate::system::{run_hook, run_tar_extract};
use pica_pkg_core::io::make_temp_dir;
use pica_pkg_core::io::{install_payload, PayloadInstall};
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::version::{pkgver_cmp_key, ver_ge};
use pica_pkg_core::PICA_VERSION;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// Pkgdir layout inside a built artifact. See docs/standard.md.
const MANIFEST: &str = "manifest";
const SCRIPTS: &str = "scripts";
const ROOT: &str = "root";
const PKGS: &str = "pkgs";

/// Where kept lifecycle scripts and env files live on the target system.
const SCRIPTS_DIR: &str = "/usr/lib/pica/scripts";
const ENV_DIR: &str = "/etc/pica/env.d";

pub(super) struct TempDirGuard {
  path: std::path::PathBuf,
}

impl TempDirGuard {
  pub(super) fn new(path: std::path::PathBuf) -> Self {
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

pub(super) struct DependencyResult {
  pub(super) precheck: Value,
  pub(super) tx_added: Vec<String>,
  pub(super) app_added: Vec<String>,
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

fn kept_scripts_dir(pkgname: &str) -> std::path::PathBuf {
  Path::new(SCRIPTS_DIR).join(pkgname)
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
/// selected in [`App::new`]. A native override that contradicts a native
/// artifact is rejected instead of silently installing into the wrong database;
/// `--pkgmgr none` is always allowed and means "extract `root/` ourselves".
fn select_backend_for_package(app: &mut App, manifest: &Manifest) -> CliResult<String> {
  let declared = required_manifest_field(manifest, "pkgmgr")?;
  let declared_kind = PackageManagerKind::parse(&declared)
    .ok_or_else(|| CliError::new(E_MANIFEST_INVALID, format!("invalid manifest pkgmgr: {declared}")))?;

  let kind = match app.options.pkgmgr.clone() {
    Some(value) => {
      let override_kind = PackageManagerKind::parse(&value)
        .ok_or_else(|| CliError::new(E_CONFIG_INVALID, format!("invalid --pkgmgr: {value}")))?;
      if override_kind.is_native() && declared_kind.is_native() && override_kind != declared_kind {
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

/// Installs an artifact file.
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
  if !pkgfile.is_file() {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      format!("pkgfile not found: {}", pkgfile.display()),
    ));
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

  validate_package(app, &manifest, &tmpdir, &pkg)?;

  let deps = resolve_dependencies(app, &manifest, &tmpdir, &pkg.pkgmgr)?;

  execute_scripts(app, &tmpdir, &pkgname, is_upgrade, &scripts)?;

  let payload = deploy_payload(&tmpdir, &pkgname)?;

  record_installation(
    app,
    &InstallRecord {
      pkgname: &pkgname,
      selector: &selector,
      manifest: &manifest,
      pkgfile,
      deps: &deps,
      payload: &payload,
      source,
    },
  )
}

fn resolve_dependencies(
  app: &mut App,
  manifest: &Manifest,
  tmpdir: &Path,
  pkgmgr: &str,
) -> CliResult<DependencyResult> {
  let pkgs_dir = tmpdir.join(PKGS);

  let non_empty = |items: Vec<String>| items.into_iter().filter(|item| !item.is_empty()).collect::<Vec<_>>();
  let mut app_list = non_empty(manifest.get_array_for("app", pkgmgr));
  let lang = resolve_lang(&app.paths.conf_file);
  let i18n_template = manifest.get_first("app_i18n");
  if !i18n_template.is_empty() && lang == "zh-cn" {
    app_list.push(i18n_template.replace("{lang}", &lang));
  }
  app_list = reorder_app_list(app_list);

  let mut precheck = json!({"kmod": [], "base": [], "app": []});
  let mut tx_added = Vec::new();
  let mut app_added = Vec::new();

  // The `none` backend has no dependency resolver and no feeds: it installs the
  // payload and stops, so no precheck or feed work is attempted.
  if PackageManagerKind::parse(pkgmgr).is_some_and(PackageManagerKind::is_native) {
    app.log_info(format!("Resolving {pkgmgr} dependencies and app list..."));

    let kmod_list = non_empty(manifest.get_array_for("kmod", pkgmgr));
    let base_list = non_empty(manifest.get_array_for("base", pkgmgr));

    let package_manager = app.package_manager()?;
    package_manager.update_index();

    precheck = build_precheck_report(app, manifest, &pkgs_dir, &app_list);
    let missing = summarize_missing_precheck(&precheck);
    if !missing.is_empty() {
      app.log_warn(format!("dependency precheck: missing {}", missing.join(" ")));
      return Err(CliError::new(
        E_RUNTIME,
        format!("dependency precheck failed, missing: {}", missing.join(" ")),
      ));
    }

    let snap_before_tx = package_manager.snapshot_installed();

    for dep in &kmod_list {
      if dep.is_empty() {
        continue;
      }
      if !package_manager.is_installed(dep) {
        package_manager.install("kmod", dep)?;
      }
    }

    let has_pkgs_dir = pkgs_dir.is_dir();
    install_via_feeds_or_pkgs(app, "base", &base_list, &pkgs_dir, has_pkgs_dir)?;

    let snap_before_app = package_manager.snapshot_installed();

    install_via_feeds_or_pkgs(app, "app", &app_list, &pkgs_dir, true)?;

    let snap_after_app = package_manager.snapshot_installed();
    let snap_after_tx = package_manager.snapshot_installed();

    tx_added = pkg_list_diff_added(&snap_before_tx, &snap_after_tx);
    app_added = pkg_list_diff_added(&snap_before_app, &snap_after_app);
  }

  Ok(DependencyResult { precheck, tx_added, app_added })
}

fn execute_scripts(
  app: &mut App,
  tmpdir: &Path,
  pkgname: &str,
  is_upgrade: bool,
  scripts: &ScriptConfig,
) -> CliResult<()> {
  if is_upgrade {
    run_hook(app, tmpdir, &scripts.update, "update")?;
  } else {
    run_hook(app, tmpdir, &scripts.install, "install")?;
  }

  let target_dir = kept_scripts_dir(pkgname);
  if target_dir.is_dir() {
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
    let source = tmpdir.join(relative);
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

/// Installs the artifact's declarative payload.
///
/// `root/` is mirrored onto the real rootfs (so `root/etc/init.d/demo` becomes
/// `/etc/init.d/demo`), while kept lifecycle scripts and the env file are copied
/// into Pica's own directories. Everything created is recorded so removal can
/// undo exactly this install.
fn deploy_payload(tmpdir: &Path, pkgname: &str) -> CliResult<PayloadInstall> {
  let mut payload = PayloadInstall::default();

  let root_dir = tmpdir.join(ROOT);
  if root_dir.is_dir() {
    let installed = install_payload(&root_dir, Path::new("/")).map_err(CliError::from)?;
    payload.files.extend(installed.files);
    payload.dirs.extend(installed.dirs);
    payload.size += installed.size;
  }

  let env_file = tmpdir.join(SCRIPTS).join(".env");
  if env_file.is_file() {
    ensure_dir(Path::new(ENV_DIR))?;
    let target = Path::new(ENV_DIR).join(format!("{pkgname}.env"));
    fs::copy(&env_file, &target)
      .map_err(|err| CliError::new(E_IO, format!("copy env file failed: {err}")))?;
    payload.files.push(target.display().to_string());
  }

  let scripts_dir = kept_scripts_dir(pkgname);
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

  Ok(payload)
}

/// Everything the database has to remember about one artifact install.
///
/// Grouping it keeps the growing set of per-install values out of a long
/// argument list that is easy to transpose at the call site.
struct InstallRecord<'a> {
  pkgname: &'a str,
  selector: &'a str,
  manifest: &'a Manifest,
  pkgfile: &'a Path,
  deps: &'a DependencyResult,
  payload: &'a PayloadInstall,
  source: EntrySource,
}

fn record_installation(app: &mut App, record: &InstallRecord<'_>) -> CliResult<()> {
  let InstallRecord { pkgname, selector, manifest, pkgfile, deps, payload, source } = *record;

  report_set_install_result(
    &app.paths,
    pkgname,
    selector,
    &manifest.value,
    &deps.precheck,
    &deps.tx_added,
    &deps.app_added,
  )?;

  let mut manifest_stored = manifest.value.clone();
  // `source` describes how *this* install obtained the package, not where the
  // artifact was built, so it is overwritten instead of defaulted: a URL
  // download stays `url` even when the packer stamped `pica` into the manifest.
  manifest_stored["source"] = json!(source.as_str());
  // `pkgmgr` is the backend that actually owns the files. Recording the one in
  // use (rather than the artifact's declared value) keeps `-R` from driving a
  // package manager that never touched this install, as happens with
  // `--pkgmgr none` over a native artifact.
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

  db_set_installed(
    &app.paths.db_file,
    pkgname,
    &manifest_stored,
    &canonicalize_display(pkgfile),
    &payload.files,
    &payload.dirs,
  )?;

  app.log_info("Transaction completed");

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::TempDirGuard;
  use std::fs;

  #[test]
  fn temp_dir_guard_cleans_on_drop() {
    let parent = tempfile::tempdir().expect("create temp dir");
    let path = parent.path().join("guard-test");
    fs::create_dir_all(&path).expect("create test dir");
    fs::write(path.join("marker"), "x").expect("write marker");

    {
      let _guard = TempDirGuard::new(path.clone());
    }

    assert!(!path.exists());
  }
}
