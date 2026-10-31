use crate::app::{App, CliError, CliResult, E_ARG_INVALID, ensure_dirs};
use crate::format;
use crate::i18n::tr;
use crate::i18n::{Key, Lang};
use crate::installed::entry_backend;
use crate::pkgmgr::PackageManagerKind;
use crate::state::{luci_i18n, read_db, write_db};
use crate::system::run_command_capture_output;
use pica_pkg_core::io::{PayloadInstall, remove_payload};
use pica_pkg_core::layout::{DEFAULT_LUCI_I18N, SCRIPTS_DIR};
use pica_pkg_core::manifest::get_array_for as manifest_get_array_for;
use pica_pkg_core::model::db::{Db, DbEntry};
use pica_pkg_core::version::pkgver_cmp_key;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn pkg(app: &mut App, pkgname: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let mut db = read_db(&app.paths)?;
  let entry = db
    .installed
    .get(pkgname)
    .cloned()
    .ok_or_else(|| CliError::new(E_ARG_INVALID, app.args(Key::NotInstalled, &[pkgname])))?;
  // How the package was installed is the only correct way to uninstall it, so
  // the recorded backend wins over any current CLI/config selection. Rows from
  // releases that stored the backend elsewhere resolve through the same ladder
  // instead of silently falling back to whatever this invocation selected.
  let backend = entry_backend(&entry);
  let kind = backend.kind;
  let pkgmgr = kind.as_str().to_string();
  if backend.is_inferred() {
    let origin = backend.origin.text();
    let name = pkgname.to_string();
    let using = pkgmgr.clone();
    app.warn_args(Key::RemoveBackendInferred, &[&name, &origin, &using]);
  }

  app.with_backend(kind, |app| remove_with_backend(app, pkgname, &entry, kind, &mut db))
}

/// The removal itself, with the backend that installed the package in use.
fn remove_with_backend(
  app: &mut App,
  pkgname: &str,
  entry: &DbEntry,
  kind: PackageManagerKind,
  db: &mut Db,
) -> CliResult<()> {
  let script_remove = entry.scalar("script_remove");
  let pkgmgr = kind.as_str().to_string();
  // The version is the one the database recorded for the package being taken
  // away, not whatever the index offers now.
  let header = removal_header(app.lang, pkgname, &recorded_version(entry));
  app.log_info(header);

  let remove_set = backend_packages(app, entry, kind);
  let files = entry.owned_paths().map(ToString::to_string).collect::<Vec<_>>();

  // The plan: what the record says will go. Nothing has run yet, so a dry run
  // or a "no" leaves the package exactly as it was.
  if !files.is_empty() {
    app.info_args(Key::PlanRemoveFiles, &[&files.len().to_string()]);
  }
  if kind.requires_system_binary() && !remove_set.is_empty() {
    let names = remove_set.iter().map(String::as_str).collect::<Vec<_>>().join(" ");
    app.info_args(Key::PlanRemoveBackend, &[&pkgmgr, &names]);
  }
  if app.stop_for_dry_run() {
    return Ok(());
  }
  app.confirm()?;

  let remove_target = if script_remove.starts_with('/') || script_remove.is_empty() {
    script_remove.clone()
  } else {
    format!("{pkgname}/{script_remove}")
  };
  run_script(app, &remove_target, "remove")?;

  if kind.requires_system_binary() {
    for package_name in remove_set {
      app.info_args(Key::RemovePackage, &[&pkgmgr, &package_name]);
      app.package_manager()?.remove(&package_name)?;
    }
  }

  // Everything the payload created is replayed from the database record, so
  // removal cannot drift from what was actually installed. Entries installed by
  // a native backend record package names instead of paths, and `remove_payload`
  // ignores those; non-absolute entries are dropped here for clarity.
  let (to_remove, kept_owned) = exclude_system_owned(app, kind, files);
  let payload = PayloadInstall { files: to_remove, dirs: entry.dirs.clone(), size: 0 };
  let outcome = remove_payload(&payload);

  // What actually happened, not what the record claimed: a path that was gone
  // already is counted as neither removed nor kept.
  let summary = removal_summary(app.lang, outcome.removed, kept_owned, outcome.kept, &pkgmgr);
  if let Some(line) = summary {
    app.log_info(format!("  {line}"));
  }

  db.installed.remove(pkgname);
  write_db(&app.paths, db)?;
  app.info(Key::TransactionCompleted);
  Ok(())
}

/// The backend packages a record brought in: its `app` list and, where the
/// `LuCI` language asks for one, its translation package.
fn backend_packages(app: &App, entry: &DbEntry, kind: PackageManagerKind) -> BTreeSet<String> {
  let mut remove_set = BTreeSet::new();
  for app_name in manifest_get_array_for(&entry.manifest, "app", kind.as_str()) {
    let trimmed = app_name.trim();
    if !trimmed.is_empty() {
      remove_set.insert(trimmed.to_string());
    }
  }

  let app_i18n_template = entry.scalar("app_i18n");
  if !app_i18n_template.is_empty() {
    let lang = luci_i18n(&app.paths);
    if lang == DEFAULT_LUCI_I18N {
      remove_set.insert(app_i18n_template.replace("{lang}", &lang));
    }
  }
  remove_set
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
        app.log_warn(tr(Key::KeepingOwnedFile, &[&path, &owner]));
        dropped += 1;
      }
      None => kept.push(path),
    }
  }
  (kept, dropped)
}

/// `Removing tailscale (1.104.1-1)`, or just the name when the record has no
/// version to show.
fn removal_header(lang: Lang, pkgname: &str, version: &str) -> String {
  if version.is_empty() {
    lang.args(Key::RemoveHeader, &[pkgname])
  } else {
    lang.args(Key::RemoveHeaderVersioned, &[pkgname, version])
  }
}

/// The version the database recorded for this package.
///
/// The manifest the record keeps is the one the install wrote, so this is the
/// version that was installed, built the same way the header of an install is.
fn recorded_version(entry: &DbEntry) -> String {
  let pkgver = entry.scalar("pkgver");
  if pkgver.is_empty() {
    return String::new();
  }
  pkgver_cmp_key(&pkgver, &entry.scalar("pkgrel"))
}

/// `Removed 4 files; kept 1 file owned by opkg`, or `None` when the record held
/// no paths at all (a package a native backend installed owns its own files).
///
/// `removed` and `kept_failed` come from what the removal did, `kept_owned` from
/// ownership that refused to delete. The reason is only stated when every kept
/// file has it: a failed unlink is not "owned by opkg".
fn removal_summary(
  lang: Lang,
  removed: usize,
  kept_owned: usize,
  kept_failed: usize,
  pkgmgr: &str,
) -> Option<String> {
  if removed + kept_owned + kept_failed == 0 {
    return None;
  }

  let removed_text = format::files(lang, removed);
  let kept = kept_owned + kept_failed;
  if kept == 0 {
    return Some(lang.args(Key::RemoveSummaryOnly, &[&removed_text]));
  }

  let kept_text = format::files(lang, kept);
  let kept_text = if kept_failed == 0 {
    lang.args(Key::RemoveSummaryOwned, &[&kept_text, pkgmgr])
  } else {
    kept_text
  };
  Some(lang.args(Key::RemoveSummary, &[&removed_text, &kept_text]))
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
    let path = script_path.display().to_string();
    app.info_args(Key::RemoveSkipScript, &[label, script_rel, &path]);
    return Ok(());
  }

  app.info_args(Key::RemoveRunScript, &[label, script_rel]);
  run_command_capture_output("sh", &[script_path.to_string_lossy().as_ref()]).map(|_| ())
}

#[cfg(test)]
mod tests {
  use super::{DbEntry, recorded_version, removal_header, removal_summary};
  use crate::i18n::Lang;
  use pretty_assertions::assert_eq;
  use serde_json::json;

  #[test]
  fn the_header_names_the_version_the_record_holds() {
    assert_eq!(removal_header(Lang::EnUs, "demo", "1.0.0-1"), "Removing demo (1.0.0-1)");
    assert_eq!(removal_header(Lang::EnUs, "demo", ""), "Removing demo");
  }

  #[test]
  fn the_summary_says_what_was_removed_and_what_survived() {
    assert_eq!(
      removal_summary(Lang::EnUs, 4, 1, 0, "opkg").as_deref(),
      Some("Removed 4 files; kept 1 file owned by opkg")
    );
    assert_eq!(
      removal_summary(Lang::EnUs, 4, 0, 1, "pica").as_deref(),
      Some("Removed 4 files; kept 1 file")
    );
    assert_eq!(removal_summary(Lang::EnUs, 0, 0, 0, "pica"), None);
  }

  #[test]
  fn the_recorded_version_is_built_from_the_stored_pkgver_and_pkgrel() {
    let entry = |manifest| DbEntry { manifest, ..DbEntry::default() };
    assert_eq!(recorded_version(&entry(json!({"pkgver": "1.2.3", "pkgrel": "4"}))), "1.2.3-4");
    assert_eq!(recorded_version(&entry(json!({"pkgver": "", "pkgrel": "4"}))), "");
  }
}
