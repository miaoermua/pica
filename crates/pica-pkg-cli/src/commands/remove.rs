use crate::app::{ensure_dirs, App, CliError, CliResult, E_ARG_INVALID};
use crate::pkgmgr::PackageManagerKind;
use crate::state::{db_del_installed, read_json_file};
use crate::system::run_command_capture_output;
use pica_pkg_core::io::{remove_payload, PayloadInstall};
use pica_pkg_core::manifest::{
  get_array_for as manifest_get_array_for, get_scalar as manifest_get_scalar,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const SCRIPTS_DIR: &str = "/usr/lib/pica/scripts";

pub fn pkg(app: &mut App, pkgname: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let db = read_json_file(&app.paths.db_file)?;
  let entry = db
    .get("installed")
    .and_then(Value::as_object)
    .and_then(|installed| installed.get(pkgname))
    .cloned()
    .ok_or_else(|| CliError::new(E_ARG_INVALID, format!("not installed: {pkgname}")))?;
  let manifest = entry
    .get("manifest")
    .cloned()
    .ok_or_else(|| CliError::new(E_ARG_INVALID, format!("not installed: {pkgname}")))?;

  let script_remove = manifest_get_scalar(&manifest, "script_remove");
  // How the package was installed is the only correct way to uninstall it, so
  // the recorded backend wins over any current CLI/config selection.
  let recorded = manifest_get_scalar(&manifest, "pkgmgr");
  let pkgmgr = if recorded.is_empty() { app.backend().as_str().to_string() } else { recorded };
  let kind = PackageManagerKind::parse(&pkgmgr).ok_or_else(|| {
    CliError::new(E_ARG_INVALID, format!("{pkgname}: unknown pkgmgr in db: {pkgmgr}"))
  })?;

  app.select_package_manager(kind.as_str())?;
  app.log_info(format!("Removing {pkgname}..."));

  let remove_target = if script_remove.starts_with('/') || script_remove.is_empty() {
    script_remove.clone()
  } else {
    format!("{pkgname}/{script_remove}")
  };
  run_script(app, &remove_target, "remove")?;

  let mut remove_set = BTreeSet::new();
  for app_name in manifest_get_array_for(&manifest, "app", kind.as_str()) {
    let trimmed = app_name.trim();
    if !trimmed.is_empty() {
      remove_set.insert(trimmed.to_string());
    }
  }

  let app_i18n_template = manifest_get_scalar(&manifest, "app_i18n");
  if !app_i18n_template.is_empty() {
    let lang = read_json_file(&app.paths.conf_file)
      .ok()
      .and_then(|value| value.get("i18n").and_then(Value::as_str).map(ToString::to_string))
      .unwrap_or_else(|| "zh-cn".to_string());
    if lang == "zh-cn" {
      remove_set.insert(app_i18n_template.replace("{lang}", &lang));
    }
  }

  if kind.is_native() {
    for package_name in remove_set {
      app.log_info(format!("removing {pkgmgr} package: {package_name}"));
      app.package_manager()?.remove(&package_name)?;
    }
  } else {
    app.log_info(format!("skip package-manager remove (pkgmgr={pkgmgr})"));
  }

  // Everything the payload created is replayed from the database record, so
  // removal cannot drift from what was actually installed. Entries installed by
  // a native backend record package names instead of paths, and `remove_payload`
  // ignores those; non-absolute entries are dropped here for clarity.
  let files = json_string_list(&entry, "files")
    .into_iter()
    .filter(|path| path.starts_with('/'))
    .collect::<Vec<_>>();
  let payload = PayloadInstall {
    files: exclude_native_owned(app, kind, files),
    dirs: json_string_list(&entry, "dirs"),
    size: 0,
  };
  remove_payload(&payload);

  db_del_installed(&app.paths.db_file, pkgname)?;
  app.log_info("Transaction completed");
  Ok(())
}

fn json_string_list(entry: &Value, key: &str) -> Vec<String> {
  entry
    .get(key)
    .and_then(Value::as_array)
    .map(|items| items.iter().filter_map(Value::as_str).map(ToString::to_string).collect())
    .unwrap_or_default()
}

/// Drops payload paths the native backend still attributes to another package.
///
/// A payload may have overwritten a file the backend tracks (the usually
/// read-only filesystem makes this rare, but a user can install into a writable
/// overlay). Deleting it would leave that package inconsistent, and Pica cannot
/// restore the original, so the file is left in place and reported.
fn exclude_native_owned(app: &mut App, kind: PackageManagerKind, files: Vec<String>) -> Vec<String> {
  if !kind.is_native() {
    return files;
  }

  // Ownership is resolved first so the backend borrow ends before logging.
  let owners: Vec<Option<String>> = match app.package_manager() {
    Ok(manager) => files.iter().map(|path| manager.owns_file(path)).collect(),
    Err(_) => return files,
  };

  let mut kept = Vec::new();
  for (path, owner) in files.into_iter().zip(owners) {
    match owner {
      Some(owner) => app.log_warn(format!("keeping {path}: still owned by {owner}")),
      None => kept.push(path),
    }
  }
  kept
}

fn run_script(app: &mut App, script_rel: &str, label: &str) -> CliResult<()> {
  if script_rel.is_empty() || script_rel.ends_with('/') {
    return Ok(());
  }

  let script_path = if script_rel.starts_with('/') {
    PathBuf::from(script_rel)
  } else {
    Path::new(SCRIPTS_DIR).join(script_rel)
  };

  if !script_path.is_file() {
    app.log_info(format!(
      "Skip {label} script: {script_rel} (not found at {})",
      script_path.display()
    ));
    return Ok(());
  }

  app.log_info(format!("Running {label} script: {script_rel}"));
  run_command_capture_output("sh", &[script_path.to_string_lossy().as_ref()]).map(|_| ())
}
