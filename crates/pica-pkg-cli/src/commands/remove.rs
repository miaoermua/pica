use crate::app::{App, CliError, CliResult, E_ARG_INVALID, ensure_dirs};
use crate::format;
use crate::installed::entry_backend;
use crate::pkgmgr::PackageManagerKind;
use crate::state::{db_del_installed, read_json_file};
use crate::system::run_command_capture_output;
use pica_pkg_core::io::{PayloadInstall, remove_payload};
use pica_pkg_core::manifest::{
  get_array_for as manifest_get_array_for, get_scalar as manifest_get_scalar,
};
use pica_pkg_core::version::pkgver_cmp_key;
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
  // the recorded backend wins over any current CLI/config selection. Rows from
  // releases that stored the backend elsewhere resolve through the same ladder
  // instead of silently falling back to whatever this invocation selected.
  let backend = entry_backend(&entry);
  let kind = backend.kind;
  let pkgmgr = kind.as_str().to_string();
  if backend.is_inferred() {
    app.log_warn(format!("{pkgname}: backend {}; using {pkgmgr}", backend.origin.as_str()));
  }

  app.select_package_manager(kind.as_str())?;
  // The version is the one the database recorded for the package being taken
  // away, not whatever the index offers now.
  app.log_info(removal_header(pkgname, &recorded_version(&manifest)));

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

  if kind.requires_system_binary() {
    for package_name in remove_set {
      app.log_info(format!("Removing {pkgmgr} package: {package_name}"));
      app.package_manager()?.remove(&package_name)?;
    }
  }

  // Everything the payload created is replayed from the database record, so
  // removal cannot drift from what was actually installed. Entries installed by
  // a native backend record package names instead of paths, and `remove_payload`
  // ignores those; non-absolute entries are dropped here for clarity.
  let files = json_string_list(&entry, "files")
    .into_iter()
    .filter(|path| path.starts_with('/'))
    .collect::<Vec<_>>();
  let (to_remove, kept_owned) = exclude_system_owned(app, kind, files);
  let payload =
    PayloadInstall { files: to_remove, dirs: json_string_list(&entry, "dirs"), size: 0 };
  let outcome = remove_payload(&payload);

  // What actually happened, not what the record claimed: a path that was gone
  // already is counted as neither removed nor kept.
  if let Some(line) = removal_summary(outcome.removed, kept_owned, outcome.kept, &pkgmgr) {
    app.log_info(format!("  {line}"));
  }

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

/// Drops payload paths the system backend still attributes to another package.
///
/// A payload may have overwritten a file the backend tracks (the usually
/// read-only filesystem makes this rare, but a user can install into a writable
/// overlay). Deleting it would leave that package inconsistent, and Pica cannot
/// restore the original, so the file is left in place and reported.
///
/// Returns the paths to remove and how many were kept for that reason.
fn exclude_system_owned(
  app: &mut App,
  kind: PackageManagerKind,
  files: Vec<String>,
) -> (Vec<String>, usize) {
  if !kind.requires_system_binary() {
    return (files, 0);
  }

  // Ownership is resolved first so the backend borrow ends before logging.
  let owners: Vec<Option<String>> = match app.package_manager() {
    Ok(manager) => files.iter().map(|path| manager.owns_file(path)).collect(),
    Err(_) => return (files, 0),
  };

  let mut kept = Vec::new();
  let mut dropped = 0;
  for (path, owner) in files.into_iter().zip(owners) {
    match owner {
      Some(owner) => {
        app.log_warn(format!("keeping {path}: still owned by {owner}"));
        dropped += 1;
      }
      None => kept.push(path),
    }
  }
  (kept, dropped)
}

/// `Removing tailscale (1.104.1-1)`, or just the name when the record has no
/// version to show.
fn removal_header(pkgname: &str, version: &str) -> String {
  if version.is_empty() {
    format!("Removing {pkgname}")
  } else {
    format!("Removing {pkgname} ({version})")
  }
}

/// The version the database recorded for this package.
///
/// The manifest the record keeps is the one the install wrote, so this is the
/// version that was installed, built the same way the header of an install is.
fn recorded_version(manifest: &Value) -> String {
  let pkgver = manifest_get_scalar(manifest, "pkgver");
  if pkgver.is_empty() {
    return String::new();
  }
  pkgver_cmp_key(&pkgver, &manifest_get_scalar(manifest, "pkgrel"))
}

/// `Removed 4 files; kept 1 file owned by opkg`, or `None` when the record held
/// no paths at all (a package a native backend installed owns its own files).
///
/// `removed` and `kept_failed` come from what the removal did, `kept_owned` from
/// ownership that refused to delete. The reason is only stated when every kept
/// file has it: a failed unlink is not "owned by opkg".
fn removal_summary(
  removed: usize,
  kept_owned: usize,
  kept_failed: usize,
  pkgmgr: &str,
) -> Option<String> {
  if removed + kept_owned + kept_failed == 0 {
    return None;
  }

  let mut line = format!("Removed {}", format::files(removed));
  let kept = kept_owned + kept_failed;
  if kept > 0 {
    let reason = if kept_failed == 0 { format!(" owned by {pkgmgr}") } else { String::new() };
    line = format!("{line}; kept {}{reason}", format::files(kept));
  }
  Some(line)
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

#[cfg(test)]
mod tests {
  use super::{recorded_version, removal_header, removal_summary};
  use pretty_assertions::assert_eq;
  use serde_json::json;

  #[test]
  fn the_header_names_the_version_the_record_holds() {
    assert_eq!(removal_header("demo", "1.0.0-1"), "Removing demo (1.0.0-1)");
    assert_eq!(removal_header("demo", ""), "Removing demo");
  }

  #[test]
  fn the_summary_says_what_was_removed_and_what_survived() {
    assert_eq!(
      removal_summary(4, 1, 0, "opkg").as_deref(),
      Some("Removed 4 files; kept 1 file owned by opkg")
    );
    assert_eq!(removal_summary(4, 0, 1, "pica").as_deref(), Some("Removed 4 files; kept 1 file"));
    assert_eq!(removal_summary(0, 0, 0, "pica"), None);
  }

  #[test]
  fn the_recorded_version_is_built_from_the_stored_pkgver_and_pkgrel() {
    assert_eq!(recorded_version(&json!({"pkgver": "1.2.3", "pkgrel": "4"})), "1.2.3-4");
    assert_eq!(recorded_version(&json!({"pkgver": "", "pkgrel": "4"})), "");
  }
}
