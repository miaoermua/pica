use crate::system::{opkg_has_package, opkg_is_installed};
use pica_pkg_core::manifest::Manifest;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub(crate) fn ipk_dir_has_pkg(dir: &Path, pkg: &str) -> bool {
  let Ok(entries) = fs::read_dir(dir) else {
    return false;
  };

  for entry in entries.flatten() {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("").to_string();
    if name.starts_with(&format!("{pkg}_")) || name.starts_with(&format!("{pkg}-")) {
      return true;
    }
  }

  false
}

pub(crate) fn precheck_dep_source(dep: &str, ipk_dir: &Path) -> String {
  if opkg_is_installed(dep) {
    return "installed".to_string();
  }
  if opkg_has_package(dep) {
    return "feed".to_string();
  }
  if ipk_dir_has_pkg(ipk_dir, dep) {
    return "packaged".to_string();
  }
  "missing".to_string()
}

pub(crate) fn build_precheck_report(
  manifest: &Manifest,
  depend_dir: &Path,
  binary_dir: &Path,
  app_list: &[String],
) -> Value {
  let kmod = manifest
    .get_array("kmod")
    .into_iter()
    .filter(|dep| !dep.is_empty())
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(&dep, Path::new(""))}))
    .collect::<Vec<Value>>();

  let base = manifest
    .get_array("base")
    .into_iter()
    .filter(|dep| !dep.is_empty())
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(&dep, depend_dir)}))
    .collect::<Vec<Value>>();

  let app = app_list
    .iter()
    .filter(|dep| !dep.is_empty())
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(dep, binary_dir)}))
    .collect::<Vec<Value>>();

  json!({
      "kmod": kmod,
      "base": base,
      "app": app,
  })
}

pub(super) fn summarize_missing_precheck(precheck: &Value) -> Vec<String> {
  let mut missing = Vec::new();

  for (group, key) in
    [(precheck.get("kmod"), "kmod"), (precheck.get("base"), "base"), (precheck.get("app"), "app")]
  {
    let Some(entries) = group.and_then(Value::as_array) else {
      continue;
    };

    for entry in entries {
      let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
      if status != "missing" {
        continue;
      }

      let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
      if !name.is_empty() {
        missing.push(format!("{key}:{name}"));
      }
    }
  }

  missing
}
