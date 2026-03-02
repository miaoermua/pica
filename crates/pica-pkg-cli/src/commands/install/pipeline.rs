use super::feed::{install_via_feeds_or_ipk, pkg_list_diff_added, reorder_app_list};
use super::precheck::{build_precheck_report, summarize_missing_precheck, validate_package};
use crate::app::{
  ensure_dirs, resolve_lang, App, CliError, CliResult, E_CONFIG_INVALID, E_IO, E_MANIFEST_INVALID,
  E_PACKAGE_INVALID, E_RUNTIME, E_VERSION_INCOMPATIBLE,
};
use crate::state::{db_has_installed, db_set_installed, report_set_install_result};
use crate::system::{
  opkg_is_installed, opkg_snapshot_installed, opkg_update_ignore, run_hook, run_tar_extract,
};
use pica_pkg_core::io::copy_dir_recursive;
use pica_pkg_core::io::make_temp_dir;
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::version::{pkgver_cmp_key, ver_ge};
use pica_pkg_core::PICA_VERSION;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

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
pub(super) struct HookConfig {
  pub(super) cmd_install: String,
  pub(super) cmd_update: String,
  pub(super) cmd_remove: String,
  pub(super) keep_all: bool,
  pub(super) keep_install: bool,
  pub(super) keep_update: bool,
  pub(super) keep_remove: bool,
}

fn cleanup_previous_hook_dir(pkgname: &str) {
  let hook_dir = Path::new("/usr/lib/pica/cmd").join(pkgname);
  if hook_dir.is_dir() {
    let _ = fs::remove_dir_all(&hook_dir);
  }
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
      "PICA_KEEP_CMD_ALL" => keep_all = parsed,
      "PICA_KEEP_CMD_INSTALL" => keep_install = parsed,
      "PICA_KEEP_CMD_UPDATE" => keep_update = parsed,
      "PICA_KEEP_CMD_REMOVE" => keep_remove = parsed,
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

fn canonicalize_display(path: &Path) -> String {
  fs::canonicalize(path).map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}

pub(super) fn pkgfile(app: &mut App, pkgfile: &Path, selector: Option<String>) -> CliResult<()> {
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

  let manifest_file = tmpdir.join("manifest");
  if !manifest_file.is_file() {
    return Err(CliError::new(E_PACKAGE_INVALID, "package missing manifest"));
  }
  if !tmpdir.join("cmd").is_dir() {
    return Err(CliError::new(E_PACKAGE_INVALID, "package missing cmd/"));
  }
  let manifest = Manifest::from_file(&manifest_file)
    .map_err(|err| CliError::new(E_MANIFEST_INVALID, format!("invalid manifest: {err}")))?;

  let pkgname = required_manifest_field(&manifest, "pkgname")?;
  let _appname = required_manifest_field(&manifest, "appname")?;
  let pica_required = manifest.get_first("pica");
  let pkgver = required_manifest_field(&manifest, "pkgver")?;
  let pkgrel = manifest.get_first("pkgrel");
  let pkgmgr_raw = manifest.get_first("pkgmgr");

  let pkg = PackageFields {
    pkgname: pkgname.clone(),
    pkgver_display: pkgver_cmp_key(&pkgver, &pkgrel),
    platform: required_manifest_field(&manifest, "platform")?,
    os: required_manifest_field(&manifest, "os")?,
    arch: required_manifest_field(&manifest, "arch")?,
    uname: manifest.get_first("uname"),
    luci: manifest.get_first("luci"),
    pkgmgr: if pkgmgr_raw.is_empty() { "opkg".to_string() } else { pkgmgr_raw },
    visibility: manifest.get_first("visibility"),
  };

  let env_file = tmpdir.join("cmd/.env");
  let (env_keep_all, env_keep_install, env_keep_update, env_keep_remove) =
    if env_file.is_file() { read_env_keep_flags(&env_file) } else { (None, None, None, None) };

  let hooks = HookConfig {
    cmd_install: manifest.get_scalar("cmd_install"),
    cmd_update: manifest.get_scalar("cmd_update"),
    cmd_remove: manifest.get_scalar("cmd_remove"),
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

  execute_hooks(app, &tmpdir, &pkgname, is_upgrade, &hooks)?;

  let installed_files = deploy_files(&tmpdir, &pkgname)?;

  record_installation(app, &pkgname, &selector, &manifest, pkgfile, &deps, &installed_files)
}

fn resolve_dependencies(
  app: &mut App,
  manifest: &Manifest,
  tmpdir: &Path,
  pkgmgr: &str,
) -> CliResult<DependencyResult> {
  let depend_dir = tmpdir.join("depend");
  let binary_dir = tmpdir.join("binary");

  let mut app_list = manifest.get_array("app");
  let lang = resolve_lang(&app.paths.conf_file);
  let i18n_template = manifest.get_first("app_i18n");
  if !i18n_template.is_empty() && lang == "zh-cn" {
    app_list.push(i18n_template.replace("{lang}", &lang));
  }
  app_list = reorder_app_list(app_list);

  let mut precheck = json!({"kmod": [], "base": [], "app": []});
  let mut tx_added = Vec::new();
  let mut app_added = Vec::new();

  if pkgmgr == "opkg" {
    app.log_info("Resolving opkg dependencies and app list...");

    let kmod_list =
      manifest.get_array("kmod").into_iter().filter(|item| !item.is_empty()).collect::<Vec<_>>();
    let base_list =
      manifest.get_array("base").into_iter().filter(|item| !item.is_empty()).collect::<Vec<_>>();

    opkg_update_ignore();

    precheck = build_precheck_report(manifest, &depend_dir, &binary_dir, &app_list);
    let missing = summarize_missing_precheck(&precheck);
    if !missing.is_empty() {
      app.log_warn(format!("dependency precheck: missing {}", missing.join(" ")));
      return Err(CliError::new(
        E_RUNTIME,
        format!("dependency precheck failed, missing: {}", missing.join(" ")),
      ));
    }

    let snap_before_tx = opkg_snapshot_installed();

    for dep in &kmod_list {
      if dep.is_empty() {
        continue;
      }
      if !opkg_is_installed(dep) {
        crate::system::opkg_install_pkg("kmod", dep)?;
      }
    }

    let has_depend_dir = depend_dir.is_dir();
    install_via_feeds_or_ipk(app, "base", &base_list, &depend_dir, has_depend_dir)?;

    let snap_before_app = opkg_snapshot_installed();

    install_via_feeds_or_ipk(app, "app", &app_list, &binary_dir, true)?;

    let snap_after_app = opkg_snapshot_installed();
    let snap_after_tx = opkg_snapshot_installed();

    tx_added = pkg_list_diff_added(&snap_before_tx, &snap_after_tx);
    app_added = pkg_list_diff_added(&snap_before_app, &snap_after_app);
  }

  Ok(DependencyResult { precheck, tx_added, app_added })
}

fn execute_hooks(
  app: &mut App,
  tmpdir: &Path,
  pkgname: &str,
  is_upgrade: bool,
  hooks: &HookConfig,
) -> CliResult<()> {
  if is_upgrade {
    run_hook(app, tmpdir, &hooks.cmd_update, "update")?;
  } else {
    run_hook(app, tmpdir, &hooks.cmd_install, "install")?;
  }

  cleanup_previous_hook_dir(pkgname);

  let mut hook_items: Vec<(&str, bool)> = Vec::new();
  if !hooks.cmd_install.is_empty() && !hooks.cmd_install.starts_with('/') {
    hook_items.push((&hooks.cmd_install, hooks.keep_all || hooks.keep_install));
  }
  if !hooks.cmd_update.is_empty() && !hooks.cmd_update.starts_with('/') {
    hook_items.push((&hooks.cmd_update, hooks.keep_all || hooks.keep_update));
  }
  if !hooks.cmd_remove.is_empty() && !hooks.cmd_remove.starts_with('/') {
    hook_items.push((&hooks.cmd_remove, hooks.keep_all || hooks.keep_remove));
  }

  let mut kept_any_hook = false;
  for (hook_rel, keep) in &hook_items {
    let source = tmpdir.join(hook_rel);
    if !source.is_file() {
      return Err(CliError::new(
        E_PACKAGE_INVALID,
        format!("cmd script not found in package: {hook_rel}"),
      ));
    }

    if !*keep {
      continue;
    }

    let target = Path::new("/usr/lib/pica/cmd").join(pkgname).join(hook_rel);
    if let Some(parent) = target.parent() {
      ensure_dir(parent)?;
    }
    fs::copy(&source, &target)
      .map_err(|err| CliError::new(E_IO, format!("copy hook failed: {err}")))?;
    kept_any_hook = true;
  }

  if !kept_any_hook {
    let hook_dir = Path::new("/usr/lib/pica/cmd").join(pkgname);
    if hook_dir.is_dir() {
      let _ = fs::remove_dir_all(&hook_dir);
    }
  }

  Ok(())
}

fn deploy_files(tmpdir: &Path, pkgname: &str) -> CliResult<Vec<String>> {
  let mut installed_files = Vec::new();

  let cmd_dir = tmpdir.join("cmd");
  if cmd_dir.is_dir() {
    ensure_dir(Path::new("/usr/bin"))?;
    copy_dir_recursive(&cmd_dir, Path::new("/usr/bin")).map_err(CliError::from)?;
    if let Ok(entries) = fs::read_dir(&cmd_dir) {
      for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
          if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
            if name != ".env" {
              installed_files.push(Path::new("/usr/bin").join(name).display().to_string());
            }
          }
        }
      }
    }
  }

  let src_dir = tmpdir.join("src");
  if src_dir.is_dir() {
    let src_target_dir = Path::new("/usr/lib/pica/src").join(pkgname);
    ensure_dir(&src_target_dir)?;
    copy_dir_recursive(&src_dir, &src_target_dir).map_err(CliError::from)?;
    installed_files.push(src_target_dir.display().to_string());
  }

  let env_file = tmpdir.join("cmd/.env");
  if env_file.is_file() {
    ensure_dir(Path::new("/etc/pica/env.d"))?;
    let target = Path::new("/etc/pica/env.d").join(format!("{pkgname}.env"));
    fs::copy(env_file, target)
      .map_err(|err| CliError::new(E_IO, format!("copy env file failed: {err}")))?;
    installed_files
      .push(Path::new("/etc/pica/env.d").join(format!("{pkgname}.env")).display().to_string());
  }

  let hook_dir = Path::new("/usr/lib/pica/cmd").join(pkgname);
  if hook_dir.is_dir() {
    if let Ok(entries) = fs::read_dir(&hook_dir) {
      for entry in entries.flatten() {
        if entry.path().is_file() {
          installed_files.push(entry.path().display().to_string());
        }
      }
    }
  }

  Ok(installed_files)
}

fn record_installation(
  app: &mut App,
  pkgname: &str,
  selector: &str,
  manifest: &Manifest,
  pkgfile: &Path,
  deps: &DependencyResult,
  installed_files: &[String],
) -> CliResult<()> {
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
  if manifest_stored.get("source").is_none() {
    manifest_stored["source"] = json!("pica");
  }
  if manifest_stored.get("url").is_none() {
    let origin = manifest_stored.get("origin").and_then(Value::as_str).unwrap_or("").to_string();
    manifest_stored["url"] = json!(origin);
  }
  if manifest_stored.get("pkgmgr").is_none() {
    manifest_stored["pkgmgr"] = json!("opkg");
  }
  for key in ["branch", "protocol", "url", "luci_url", "luci_desc", "pkgmgr", "visibility"] {
    if manifest_stored.get(key).is_none() {
      manifest_stored[key] = json!("");
    }
  }

  db_set_installed(
    &app.paths.db_file,
    pkgname,
    &manifest_stored,
    &canonicalize_display(pkgfile),
    installed_files,
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
