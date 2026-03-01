use crate::state::{db_del_installed, read_json_file};
use crate::system::{opkg_remove_pkg, run_command_capture_output};
use crate::{
  ensure_dirs, manifest_get_array, manifest_get_scalar, App, CliError, CliResult, E_ARG_INVALID,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub fn pkg(app: &mut App, pkgname: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let db = read_json_file(&app.paths.db_file)?;
  let manifest = db
    .get("installed")
    .and_then(Value::as_object)
    .and_then(|installed| installed.get(pkgname))
    .and_then(|entry| entry.get("manifest"))
    .cloned()
    .ok_or_else(|| CliError::new(E_ARG_INVALID, format!("not installed: {pkgname}")))?;

  let cmd_remove = manifest_get_scalar(&manifest, "cmd_remove");
  let pkgmgr = {
    let value = manifest_get_scalar(&manifest, "pkgmgr");
    if value.is_empty() {
      "opkg".to_string()
    } else {
      value
    }
  };

  app.log_info(format!("Removing {pkgname}..."));

  let remove_target = if cmd_remove.starts_with('/') || cmd_remove.is_empty() {
    cmd_remove.clone()
  } else {
    format!("{pkgname}/{cmd_remove}")
  };
  run_cmd_install_tree(app, &remove_target, "remove")?;

  let mut remove_set = BTreeSet::new();
  for app_name in manifest_get_array(&manifest, "app") {
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

  if pkgmgr == "opkg" {
    for opkg_name in remove_set {
      app.log_info(format!("removing opkg package: {opkg_name}"));
      opkg_remove_pkg(&opkg_name)?;
    }
  } else {
    app.log_info(format!("skip package-manager remove (pkgmgr={pkgmgr})"));
  }

  let env_file = Path::new("/etc/pica/env.d").join(format!("{pkgname}.env"));
  if env_file.is_file() {
    app.log_info(format!("removing env: {}", env_file.display()));
    let _ = fs::remove_file(env_file);
  }

  let src_dir = Path::new("/usr/lib/pica/src").join(pkgname);
  if src_dir.is_dir() {
    app.log_info(format!("removing src dir: {}", src_dir.display()));
    let _ = fs::remove_dir_all(src_dir);
  }

  let cmd_dir = Path::new("/usr/lib/pica/cmd").join(pkgname);
  if cmd_dir.is_dir() {
    app.log_info(format!("removing cmd dir: {}", cmd_dir.display()));
    let _ = fs::remove_dir_all(cmd_dir);
  }

  db_del_installed(&app.paths.db_file, pkgname)?;
  app.log_info("Transaction completed");
  Ok(())
}

fn run_cmd_install_tree(app: &mut App, cmd_rel: &str, label: &str) -> CliResult<()> {
  if cmd_rel.is_empty() {
    return Ok(());
  }

  if cmd_rel.ends_with('/') {
    return Ok(());
  }

  let cmd_path = if cmd_rel.starts_with('/') {
    PathBuf::from(cmd_rel)
  } else {
    Path::new("/usr/lib/pica/cmd").join(cmd_rel)
  };

  if !cmd_path.is_file() {
    app.log_info(format!("Skip {label} cmd: {cmd_rel} (not found at {})", cmd_path.display()));
    return Ok(());
  }

  app.log_info(format!("Running {label} cmd: {cmd_rel}"));
  run_command_capture_output("sh", &[cmd_path.to_string_lossy().as_ref()]).map(|_| ())
}
