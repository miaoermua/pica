use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_DB_INVALID, E_INDEX_INVALID, E_RUNTIME, Paths,
  ensure_dirs,
};
use crate::i18n::Key;
use crate::i18n::tr;
use pica_pkg_core::io::now_unix_secs;
use pica_pkg_core::manifest::get_first as manifest_get_first;
use pica_pkg_core::model::config::Config;
use pica_pkg_core::model::db::{Db, DbEntry};
use pica_pkg_core::model::local_index::LocalIndex;
use serde_json::{Map, Value, json};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub fn report_set_install_result(
  paths: &Paths,
  pkgname: &str,
  selector: &str,
  manifest: &Value,
  precheck: &Value,
  tx_added: &[String],
  app_added: &[String],
) -> CliResult<()> {
  ensure_dirs(paths)?;

  let mut report = read_json_file(&paths.report_file).unwrap_or_else(|_| {
    json!({
        "schema": 1,
        "reports": {},
    })
  });

  report["schema"] = json!(1);
  ensure_json_object_field(&mut report, "reports")?;

  let appname = manifest_get_first(manifest, "appname");

  let program_url = {
    let value = manifest_get_first(manifest, "url");
    if value.is_empty() { manifest_get_first(manifest, "origin") } else { value }
  };

  report["reports"][pkgname] = json!({
      "updated_at": now_unix_secs(),
      "selector": selector,
      "package": {
          "pkgname": manifest_get_first(manifest, "pkgname"),
          "appname": appname,
          "url": program_url,
          "luci_url": manifest_get_first(manifest, "luci_url"),
          "os": manifest_get_first(manifest, "os"),
          "branch": manifest_get_first(manifest, "branch"),
          "protocol": manifest_get_first(manifest, "protocol"),
          "pkgver": manifest_get_first(manifest, "pkgver"),
          "pkgrel": manifest_get_first(manifest, "pkgrel"),
      },
      "precheck": precheck,
      "dependency_diff": {
          "transaction_added": tx_added,
          "app_stage_added": app_added,
      }
  });

  write_json_atomic_pretty(&paths.report_file, &report)
}

/// Reads the installed-package database.
///
/// # Errors
/// Returns `E_DB_INVALID` when the file exists but is not a database.
pub fn read_db(paths: &Paths) -> CliResult<Db> {
  let value = read_json_file(&paths.db_file)?;
  serde_json::from_value(value).map_err(|err| {
    CliError::new(E_DB_INVALID, tr(Key::NotPicaDatabase, &[&paths.db_file.display(), &err]))
  })
}

/// Writes the installed-package database in one atomic replace.
///
/// # Errors
/// Returns an error when the file cannot be written.
pub fn write_db(paths: &Paths, db: &Db) -> CliResult<()> {
  let value = serde_json::to_value(db)
    .map_err(|err| CliError::new(E_DB_INVALID, tr(Key::DatabaseEncodeFailed, &[&err])))?;
  write_json_atomic_pretty(&paths.db_file, &value)
}

/// Reads the sync cache.
///
/// # Errors
/// Returns `E_INDEX_INVALID` when the cache exists but was not written by a
/// sync, which says to run `pica -S`.
pub fn read_local_index(paths: &Paths) -> CliResult<LocalIndex> {
  let value = read_json_file(&paths.index_file)?;
  serde_json::from_value(value)
    .map_err(|err| CliError::new(E_INDEX_INVALID, tr(Key::IndexMissing, &[&err])))
}

/// Reads the configuration. A file that does not parse as one is an error the
/// caller decides how to treat.
///
/// # Errors
/// Returns `E_CONFIG_INVALID` when the file is not a configuration object.
pub fn read_config(paths: &Paths) -> CliResult<Config> {
  let value = read_json_file(&paths.conf_file)?;
  serde_json::from_value(value)
    .map_err(|err| CliError::new(E_CONFIG_INVALID, format!("{}: {err}", paths.conf_file.display())))
}

/// The language-pack code `luci-i18n-*` packages are named with: the configured
/// `i18n`, or the default when the configuration says nothing or cannot be read.
pub fn luci_i18n(paths: &Paths) -> String {
  read_config(paths)
    .ok()
    .and_then(|conf| conf.i18n)
    .unwrap_or_else(|| pica_pkg_core::layout::DEFAULT_LUCI_I18N.to_string())
}

/// A new database record for what an install wrote.
pub fn new_entry(manifest: &Value, pkgfile: &str, files: &[String], dirs: &[String]) -> DbEntry {
  DbEntry {
    manifest: manifest.clone(),
    pkgfile: pkgfile.to_string(),
    files: files.to_vec(),
    dirs: dirs.to_vec(),
    installed_at: now_unix_secs(),
    extra: Map::new(),
  }
}

pub fn read_json_file(path: &Path) -> CliResult<Value> {
  pica_pkg_core::io::read_json_file(path).map_err(CliError::from)
}

pub fn write_json_atomic_pretty(path: &Path, value: &Value) -> CliResult<()> {
  pica_pkg_core::io::write_json_file_pretty(path, value).map_err(CliError::from)
}

/// Files that must become visible together.
///
/// Nothing touches the disk while a plan is being built; [`Transaction::commit`]
/// writes each staged file to a sibling temp file and renames it into place, so
/// every file is either the old one or the new one. Across files, all-or-nothing
/// comes from remembering the previous bytes and putting them back when a later
/// write fails. A recovery that also fails is reported with both causes rather
/// than hidden, because a half-applied state must never look like success.
#[derive(Default)]
pub(crate) struct Transaction {
  entries: Vec<Entry>,
}

struct Entry {
  path: PathBuf,
  next: Vec<u8>,
  previous: Option<Vec<u8>>,
}

impl Transaction {
  pub(crate) fn new() -> Self {
    Self { entries: Vec::new() }
  }

  /// Records `path` as it is now and as it should be afterwards.
  ///
  /// # Errors
  /// Returns an error if the current contents cannot be read or the parent
  /// directory cannot be created.
  pub(crate) fn stage(&mut self, app: &App, path: &Path, next: Vec<u8>) -> CliResult<()> {
    let previous = match fs::read(path) {
      Ok(bytes) => Some(bytes),
      Err(err) if err.kind() == io::ErrorKind::NotFound => None,
      Err(err) => {
        let shown = path.display().to_string();
        let cause = err.to_string();
        return Err(CliError::new(E_RUNTIME, app.args(Key::CommitReadFailed, &[&shown, &cause])));
      }
    };

    if let Some(parent) = path.parent()
      && let Err(err) = fs::create_dir_all(parent)
    {
      let shown = path.display().to_string();
      let cause = err.to_string();
      return Err(CliError::new(E_RUNTIME, app.args(Key::CommitReadFailed, &[&shown, &cause])));
    }

    self.entries.push(Entry { path: path.to_path_buf(), next, previous });
    Ok(())
  }

  /// Stages `value` as pretty JSON, the format every file here is written in.
  ///
  /// # Errors
  /// Returns an error if the value cannot be serialized or the path cannot be
  /// staged.
  pub(crate) fn stage_json(&mut self, app: &App, path: &Path, value: &Value) -> CliResult<()> {
    let text = serde_json::to_string_pretty(value)
      .map_err(|err| CliError::new(E_RUNTIME, err.to_string()))?;
    self.stage(app, path, text.into_bytes())
  }

  /// Writes every staged file, or puts back what was there before.
  ///
  /// # Errors
  /// Returns an error naming the failed write, and either the fact that the
  /// previous state was restored or the reason it could not be. A failure on
  /// the first file says so plainly: nothing had been touched yet, and
  /// claiming a restore would report an undo that never happened.
  pub(crate) fn commit(self, app: &App) -> CliResult<()> {
    for index in 0..self.entries.len() {
      let entry = &self.entries[index];
      if let Err(err) = replace_file(&entry.path, &entry.next) {
        let cause = err.message;
        let shown = entry.path.display().to_string();
        if index == 0 {
          return Err(CliError::new(err.code, app.args(Key::CommitFailed, &[&shown, &cause])));
        }
        return match restore(&self.entries[..index]) {
          Ok(()) => Err(CliError::new(err.code, app.args(Key::CommitRestored, &[&cause]))),
          Err(restore) => {
            Err(CliError::new(E_RUNTIME, app.args(Key::CommitBroken, &[&cause, &restore.message])))
          }
        };
      }
    }
    Ok(())
  }
}

/// Puts the previous state of `entries` back, newest first.
fn restore(entries: &[Entry]) -> CliResult<()> {
  for entry in entries.iter().rev() {
    match &entry.previous {
      Some(bytes) => replace_file(&entry.path, bytes)?,
      None => match fs::remove_file(&entry.path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
          return Err(CliError::new(E_RUNTIME, format!("{}: {err}", entry.path.display())));
        }
      },
    }
  }
  Ok(())
}

/// Writes `content` to a temp file next to `path`, then renames it over `path`.
///
/// The temp file is a sibling so the rename stays within one filesystem, and it
/// carries the process id so two runs cannot collide. A failed write takes its
/// own temp file with it.
fn replace_file(path: &Path, content: &[u8]) -> CliResult<()> {
  let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
  let temp = path.with_file_name(format!(".{name}.pica-{}", std::process::id()));

  for step in [fs::write(&temp, content), fs::rename(&temp, path)] {
    if let Err(err) = step {
      let _ = fs::remove_file(&temp);
      return Err(CliError::new(E_RUNTIME, format!("{}: {err}", path.display())));
    }
  }
  Ok(())
}

pub fn ensure_json_object_field(value: &mut Value, key: &str) -> CliResult<()> {
  let Some(obj) = value.as_object_mut() else {
    return Err(CliError::new(E_RUNTIME, tr(Key::JsonRootNotObject, &[])));
  };

  if !obj.contains_key(key) {
    obj.insert(key.to_string(), Value::Object(Map::new()));
  }

  if !obj.get(key).is_some_and(Value::is_object) {
    return Err(CliError::new(E_RUNTIME, tr(Key::JsonFieldNotObject, &[&key])));
  }

  Ok(())
}

/// Drops the cached packages left by this run.
///
/// Silent on success: the cache is Pica's own bookkeeping, and a user who asked
/// to install something learns nothing from being told it was tidied. Failures
/// are still reported, because a cache that cannot be cleaned is a fact about
/// the machine.
pub(crate) fn cleanup_pkg_cache(app: &mut App) {
  let cache_pkgs_dir = app.paths.cache_dir.join("pkgs");
  if !cache_pkgs_dir.is_dir() {
    return;
  }

  let entries = match fs::read_dir(&cache_pkgs_dir) {
    Ok(entries) => entries,
    Err(err) => {
      app.log_warn(tr(Key::CacheCleanupSkipped, &[&cache_pkgs_dir.display(), &err]));
      return;
    }
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let result = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };

    if let Err(err) = result {
      app.log_warn(tr(Key::CacheCleanupFailed, &[&path.display(), &err]));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{ensure_json_object_field, new_entry, read_db, write_db};
  use crate::app::{E_DB_INVALID, E_RUNTIME, Paths};
  use pretty_assertions::assert_eq;
  use serde_json::json;
  use std::path::Path;

  fn paths(root: &Path) -> Paths {
    Paths {
      etc_dir: root.to_path_buf(),
      conf_file: root.join("pica.json"),
      state_dir: root.to_path_buf(),
      db_file: root.join("db.json"),
      index_file: root.join("index.json"),
      cache_dir: root.join("cache"),
      repos_cache_dir: root.join("cache/repos"),
      report_file: root.join("install-report.json"),
      lock_file: root.join("db.lck"),
    }
  }

  #[test]
  fn ensure_json_object_field_creates_and_validates() {
    let mut value = json!({"schema": 1});
    ensure_json_object_field(&mut value, "installed").expect("field should be created");
    assert!(value["installed"].is_object());

    let mut invalid = json!({"installed": []});
    let err = ensure_json_object_field(&mut invalid, "installed").expect_err("must reject array");
    assert_eq!(err.code, E_RUNTIME);
  }

  #[test]
  fn a_database_round_trips_through_the_store() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(dir.path());
    std::fs::write(&paths.db_file, r#"{"schema":1,"installed":{},"kept":"yes"}"#).expect("seed");

    let mut db = read_db(&paths).expect("read");
    db.installed.insert(
      "demo".to_string(),
      new_entry(&json!({"pkgname": "demo"}), "/tmp/demo.pkg.tar.gz", &["/etc/demo".into()], &[]),
    );
    write_db(&paths, &db).expect("write");

    let again = read_db(&paths).expect("read back");
    assert_eq!(again.installed["demo"].files, ["/etc/demo"]);
    assert_eq!(again.extra["kept"], json!("yes"), "unknown fields survive");
  }

  #[test]
  fn a_database_that_is_not_one_is_reported_as_such() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = paths(dir.path());
    std::fs::write(&paths.db_file, r#"{"installed":[]}"#).expect("seed");
    assert_eq!(read_db(&paths).expect_err("not a database").code, E_DB_INVALID);
  }
}
