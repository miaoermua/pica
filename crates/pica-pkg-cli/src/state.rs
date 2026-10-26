use crate::app::{App, CliError, CliResult, E_DB_INVALID, E_RUNTIME, Paths, ensure_dirs};
use crate::i18n::Key;
use pica_pkg_core::io::now_unix_secs;
use pica_pkg_core::manifest::get_first as manifest_get_first;
use pica_pkg_core::selector::Selector;
use pica_pkg_core::version::pkgver_cmp_key;
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

/// Records an installation.
///
/// `files` and `dirs` are the payload this install created; removal replays them
/// in reverse. Installs delegated to a native package manager pass `packages` in
/// `files` instead, because that backend, not Pica, owns the real file list.
pub fn db_set_installed(
  db_file: &Path,
  pkgname: &str,
  manifest: &Value,
  pkgfile: &str,
  files: &[String],
  dirs: &[String],
) -> CliResult<()> {
  let mut db = read_json_file(db_file)?;
  ensure_json_object_field(&mut db, "installed")?;

  let installed = db
    .get_mut("installed")
    .and_then(Value::as_object_mut)
    .ok_or_else(|| CliError::new(E_DB_INVALID, "db installed is not object"))?;

  installed.insert(
    pkgname.to_string(),
    json!({
        "manifest": manifest,
        "pkgfile": pkgfile,
        "files": files,
        "dirs": dirs,
        "installed_at": now_unix_secs(),
    }),
  );

  write_json_atomic_pretty(db_file, &db)
}

/// Adds the paths a failed install left behind without replacing what the
/// database already says about the package.
///
/// A first install that fails leaves files no entry names, so they are written
/// as a new one: without it `-R` would have nothing to act on and the files
/// would be unreachable. An upgrade that fails is not that case. Its rollback
/// put the previous version's files back, and the entry for the package still
/// describes that version; writing the new manifest over it would claim a
/// version that is not on the device and drop the paths that *are*. The remnant
/// is merged into the entry that is already there, keeping its manifest,
/// `pkgfile` and `installed_at`.
///
/// Returns whether the package already had an entry.
pub fn db_record_remnant(
  db_file: &Path,
  pkgname: &str,
  manifest: &Value,
  pkgfile: &str,
  files: &[String],
  dirs: &[String],
) -> CliResult<bool> {
  let mut db = read_json_file(db_file)?;
  ensure_json_object_field(&mut db, "installed")?;

  let installed = db
    .get_mut("installed")
    .and_then(Value::as_object_mut)
    .ok_or_else(|| CliError::new(E_DB_INVALID, "db installed is not object"))?;

  if let Some(Value::Object(entry)) = installed.get_mut(pkgname) {
    merge_owned_paths(entry, "files", files);
    merge_owned_paths(entry, "dirs", dirs);
    write_json_atomic_pretty(db_file, &db)?;
    return Ok(true);
  }

  installed.insert(
    pkgname.to_string(),
    json!({
        "manifest": manifest,
        "pkgfile": pkgfile,
        "files": files,
        "dirs": dirs,
        "installed_at": now_unix_secs(),
    }),
  );

  write_json_atomic_pretty(db_file, &db)?;
  Ok(false)
}

/// Appends the paths that are not already named, in the order given.
fn merge_owned_paths(entry: &mut Map<String, Value>, key: &str, add: &[String]) {
  if add.is_empty() {
    return;
  }

  let list = entry.entry(key.to_string()).or_insert_with(|| json!([]));
  let Some(items) = list.as_array_mut() else {
    *list = json!(add);
    return;
  };

  for path in add {
    if !items.iter().any(|item| item.as_str() == Some(path.as_str())) {
      items.push(json!(path));
    }
  }
}

pub fn db_del_installed(db_file: &Path, pkgname: &str) -> CliResult<()> {
  let mut db = read_json_file(db_file)?;
  ensure_json_object_field(&mut db, "installed")?;

  let installed = db
    .get_mut("installed")
    .and_then(Value::as_object_mut)
    .ok_or_else(|| CliError::new(E_DB_INVALID, "db installed is not object"))?;
  installed.remove(pkgname);

  write_json_atomic_pretty(db_file, &db)
}

/// The version the database records for `pkgname`, if it records the package.
///
/// It is the same string the index and the transaction header print, built from
/// the stored `pkgver`/`pkgrel` by the same function the install used, so an
/// upgrade can be recognised without parsing anything twice.
pub fn db_installed_version(db_file: &Path, pkgname: &str) -> CliResult<Option<String>> {
  let db = read_json_file(db_file)?;
  let Some(entry) = db.get("installed").and_then(Value::as_object).and_then(|i| i.get(pkgname))
  else {
    return Ok(None);
  };

  let manifest = entry.get("manifest").unwrap_or(&Value::Null);
  let pkgver = manifest_get_first(manifest, "pkgver");
  if pkgver.is_empty() {
    // Recorded without a version: say so rather than inventing one.
    return Ok(Some(String::new()));
  }
  let pkgrel = manifest_get_first(manifest, "pkgrel");

  Ok(Some(pkgver_cmp_key(&pkgver, &pkgrel)))
}

pub fn db_find_installed_pkgname_by_selector(
  db_file: &Path,
  selector: &Selector,
) -> CliResult<Option<String>> {
  let db = read_json_file(db_file)?;
  let Some(installed) = db.get("installed").and_then(Value::as_object) else {
    return Ok(None);
  };

  for (pkgname, entry) in installed {
    let manifest = entry.get("manifest").unwrap_or(&Value::Null);

    let key_matches = pkgname == &selector.appname
      || manifest_get_first(manifest, "appname") == selector.appname
      || manifest_get_first(manifest, "pkgname") == selector.appname;
    if !key_matches {
      continue;
    }

    if !selector.branch.is_empty() && manifest_get_first(manifest, "branch") != selector.branch {
      continue;
    }

    return Ok(Some(pkgname.clone()));
  }

  Ok(None)
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
    return Err(CliError::new(E_RUNTIME, "json root is not object"));
  };

  if !obj.contains_key(key) {
    obj.insert(key.to_string(), Value::Object(Map::new()));
  }

  if !obj.get(key).is_some_and(Value::is_object) {
    return Err(CliError::new(E_RUNTIME, format!("json field '{key}' is not object")));
  }

  Ok(())
}

/// Every package list one synced repo entry holds: one list per backend index.
#[must_use]
pub(crate) fn repo_package_sets(repo: &Value) -> Vec<&[Value]> {
  let mut sets = Vec::new();

  if let Some(indexes) = repo.get("indexes").and_then(Value::as_object) {
    for index in indexes.values() {
      if let Some(packages) = index.get("packages").and_then(Value::as_array) {
        sets.push(packages.as_slice());
      }
    }
  }

  sets
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
      app.log_warn(format!(
        "cleanup cache skipped: cannot read {}: {err}",
        cache_pkgs_dir.display()
      ));
      return;
    }
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let result = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };

    if let Err(err) = result {
      app.log_warn(format!("cleanup cache failed: {}: {err}", path.display()));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{
    db_find_installed_pkgname_by_selector, db_record_remnant, db_set_installed,
    ensure_json_object_field, read_json_file, write_json_atomic_pretty,
  };
  use crate::app::E_RUNTIME;
  use pica_pkg_core::selector::Selector;
  use pretty_assertions::assert_eq;
  use serde_json::{Value, json};

  #[test]
  fn ensure_json_object_field_creates_and_validates() {
    let mut value = json!({"schema": 1});
    ensure_json_object_field(&mut value, "installed").expect("must create object field");
    assert!(value.get("installed").is_some_and(Value::is_object));

    let mut invalid = json!({"installed": []});
    let err = ensure_json_object_field(&mut invalid, "installed").expect_err("must fail");
    assert_eq!(err.code, E_RUNTIME);
  }

  #[test]
  fn a_remnant_is_added_to_the_entry_of_the_version_that_is_still_installed() {
    // A failed upgrade restores the old files but cannot always remove the new
    // ones. The entry still describes what is on the device, so it keeps its
    // manifest and gains only the paths `-R` would otherwise never see.
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_file = dir.path().join("db.json");
    write_json_atomic_pretty(&db_file, &json!({"schema": 1, "installed": {}})).expect("empty db");
    db_set_installed(
      &db_file,
      "demo",
      &json!({"pkgname": "demo", "pkgver": "1.0"}),
      "demo-1.0.ipk",
      &["usr/bin/demo".to_string()],
      &[],
    )
    .expect("record the first version");
    let installed_at = read_json_file(&db_file)
      .expect("read db")
      .pointer("/installed/demo/installed_at")
      .cloned()
      .expect("installed_at");

    let existed = db_record_remnant(
      &db_file,
      "demo",
      &json!({"pkgname": "demo", "pkgver": "2.0"}),
      "demo-2.0.ipk",
      &["usr/bin/demo".to_string(), "etc/demo.conf".to_string()],
      &["etc".to_string()],
    )
    .expect("record the remnant");

    assert!(existed, "the package had an entry");
    let db = read_json_file(&db_file).expect("read db");
    let entry = db.pointer("/installed/demo").expect("entry");
    assert_eq!(entry["manifest"]["pkgver"], json!("1.0"), "the old version is what is on disk");
    assert_eq!(entry["pkgfile"], json!("demo-1.0.ipk"));
    assert_eq!(entry["installed_at"], installed_at, "the install itself is not new");
    assert_eq!(
      entry["files"],
      json!(["usr/bin/demo", "etc/demo.conf"]),
      "the surviving path is added once"
    );
    assert_eq!(entry["dirs"], json!(["etc"]));
  }

  #[test]
  fn a_first_install_that_fails_is_recorded_so_its_files_can_be_removed() {
    // Nothing on the device names these files: without an entry `-R` has
    // nothing to read and the leftovers stay forever.
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_file = dir.path().join("db.json");
    write_json_atomic_pretty(&db_file, &json!({"schema": 1, "installed": {}})).expect("empty db");

    let existed = db_record_remnant(
      &db_file,
      "demo",
      &json!({"pkgname": "demo", "pkgver": "2.0"}),
      "demo-2.0.ipk",
      &["etc/demo.conf".to_string()],
      &[],
    )
    .expect("record the remnant");

    assert!(!existed, "there was no entry to merge into");
    let db = read_json_file(&db_file).expect("read db");
    let entry = db.pointer("/installed/demo").expect("entry");
    assert_eq!(entry["manifest"]["pkgver"], json!("2.0"));
    assert_eq!(entry["files"], json!(["etc/demo.conf"]));
  }

  #[test]
  fn db_find_installed_pkgname_by_selector_matches_branch() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_file = dir.path().join("selector-db.json");
    let db = json!({
        "schema": 1,
        "installed": {
            "hello": {
                "manifest": {
                    "pkgname": "hello",
                    "appname": "hello-app",
                    "branch": "stable",
                    "pkgver": "1.2.3",
                    "pkgrel": "4"
                }
            }
        }
    });
    write_json_atomic_pretty(&db_file, &db).expect("write db");

    let selector = Selector::parse("hello-app(stable)").expect("parse selector");
    let found = db_find_installed_pkgname_by_selector(&db_file, &selector)
      .expect("query selector")
      .expect("must match");
    assert_eq!(found, "hello");

    let selector_miss = Selector::parse("hello-app(dev)").expect("parse selector");
    let miss = db_find_installed_pkgname_by_selector(&db_file, &selector_miss).expect("query miss");
    assert!(miss.is_none());
  }
}
