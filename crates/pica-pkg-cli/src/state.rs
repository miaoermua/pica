use crate::{
  ensure_dirs, manifest_get_first, CliError, CliResult, Selector, E_DB_INVALID, E_IO,
  E_JSON_INVALID, E_RUNTIME,
};
use pica_pkg_core::io::now_unix_secs;
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub fn report_set_install_result(
  paths: &crate::Paths,
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
    if value.is_empty() {
      manifest_get_first(manifest, "origin")
    } else {
      value
    }
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

pub fn db_set_installed(
  db_file: &Path,
  pkgname: &str,
  manifest: &Value,
  pkgfile: &str,
  files: &[String],
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
        "installed_at": now_unix_secs(),
    }),
  );

  write_json_atomic_pretty(db_file, &db)
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

pub fn db_has_installed(db_file: &Path, pkgname: &str) -> CliResult<bool> {
  let db = read_json_file(db_file)?;
  let has = db
    .get("installed")
    .and_then(Value::as_object)
    .is_some_and(|installed| installed.contains_key(pkgname));
  Ok(has)
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
  let content = fs::read_to_string(path)
    .map_err(|err| CliError::new(E_IO, format!("read {} failed: {err}", path.display())))?;

  serde_json::from_str(&content)
    .map_err(|err| CliError::new(E_JSON_INVALID, format!("parse {} failed: {err}", path.display())))
}

pub fn write_json_atomic_pretty(path: &Path, value: &Value) -> CliResult<()> {
  let mut tmp_name = OsString::from(path.as_os_str());
  tmp_name.push(".tmp");
  let tmp_path = PathBuf::from(tmp_name);

  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)
      .map_err(|err| CliError::new(E_IO, format!("mkdir {} failed: {err}", parent.display())))?;
  }

  let content = serde_json::to_string_pretty(value)
    .map_err(|err| CliError::new(E_JSON_INVALID, err.to_string()))?;
  fs::write(&tmp_path, content)
    .map_err(|err| CliError::new(E_IO, format!("write {} failed: {err}", tmp_path.display())))?;
  fs::rename(&tmp_path, path)
    .map_err(|err| CliError::new(E_IO, format!("rename {} failed: {err}", path.display())))?;

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

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn unique_tmp_path(name: &str) -> PathBuf {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    std::env::temp_dir().join(format!("pica-state-test-{name}-{now}-{}", std::process::id()))
  }

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
  fn db_find_installed_pkgname_by_selector_matches_branch() {
    let db_file = unique_tmp_path("selector");
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

    let _ = fs::remove_file(&db_file);
  }
}
