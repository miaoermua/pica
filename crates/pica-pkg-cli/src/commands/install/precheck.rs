use super::pipeline::{installs_declared_dependencies, PackageFields};
use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_PACKAGE_INVALID, E_PLATFORM_UNSUPPORTED,
};
use crate::platform::{
  check_arch, detect_luci_variant, detect_package_arches, detect_os, detect_platform,
  normalize_uname, ArchVerdict,
};
use crate::system::run_command_text;
use pica_pkg_core::manifest::Manifest;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// True when `pkgs/` ships a native package for `pkg`.
///
/// The packer already filtered `pkgs/` down to one backend, so the only thing
/// left to match is the package name. Names are matched on a `-`/`_` boundary so
/// `foo` does not match `foo-extra`.
pub(crate) fn pkgs_dir_has_pkg(dir: &Path, pkg: &str) -> bool {
  let Ok(entries) = fs::read_dir(dir) else {
    return false;
  };

  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      if pkgs_dir_has_pkg(&path, pkg) {
        return true;
      }
      continue;
    }
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
      continue;
    };
    if name.starts_with(&format!("{pkg}_")) || name.starts_with(&format!("{pkg}-")) {
      return true;
    }
  }

  false
}

pub(crate) fn precheck_dep_source(app: &App, dep: &str, pkgs_dir: &Path) -> String {
  if app.package_manager().is_ok_and(|manager| manager.is_installed(dep)) {
    return "installed".to_string();
  }
  if app.package_manager().is_ok_and(|manager| manager.package_exists(dep)) {
    return "feed".to_string();
  }
  if pkgs_dir_has_pkg(pkgs_dir, dep) {
    return "packaged".to_string();
  }
  "missing".to_string()
}

pub(crate) fn build_precheck_report(
  app: &App,
  manifest: &Manifest,
  pkgs_dir: &Path,
  app_list: &[String],
) -> Value {
  let backend = app.backend().as_str();

  let kmod = manifest
    .get_array_for("kmod", backend)
    .into_iter()
    .filter(|dep| !dep.is_empty())
    // kmods are always resolved through feeds: the OpenWrt kernel module
    // packages cannot be shipped inside an application artifact.
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(app, &dep, Path::new(""))}))
    .collect::<Vec<Value>>();

  let base = manifest
    .get_array_for("base", backend)
    .into_iter()
    .filter(|dep| !dep.is_empty())
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(app, &dep, pkgs_dir)}))
    .collect::<Vec<Value>>();

  let app = app_list
    .iter()
    .filter(|dep| !dep.is_empty())
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(app, dep, pkgs_dir)}))
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

/// A package is empty when it declares no dependency group either, since then
/// installing it would change nothing on disk.
fn has_payload(tmpdir: &Path) -> bool {
  ["root", "scripts", "pkgs"].iter().any(|name| tmpdir.join(name).is_dir())
}

/// Gates a package's declared `arch` against what this host can say about
/// itself.
///
/// "Cannot tell" must not read as "incompatible". A host driven through the
/// `pica` backend has no package manager to ask, and a backend whose command
/// failed cannot answer either, so an unanswered question — not an empty list —
/// refuses nothing. A weaker answer is accepted too, but said out loud, because
/// an unchecked `arch` is a risk the user is taking knowingly.
fn check_package_arch(app: &mut App, pkg: &PackageFields) -> CliResult<()> {
  if pkg.arch == "all" {
    return Ok(());
  }

  let host_arches = detect_package_arches(app);
  match check_arch(&pkg.arch, &host_arches) {
    ArchVerdict::Backend | ArchVerdict::Kernel => Ok(()),
    ArchVerdict::Related => {
      app.log_warn(format!(
        "arch {} was accepted on a family match alone ({})",
        pkg.arch,
        host_arches.describe()
      ));
      Ok(())
    }
    ArchVerdict::Unverified => {
      app.log_warn(format!(
        "cannot determine this host's package architecture, so arch={} was not checked ({})",
        pkg.arch,
        host_arches.describe()
      ));
      Ok(())
    }
    ArchVerdict::Mismatch => Err(CliError::new(
      E_PLATFORM_UNSUPPORTED,
      format!("unsupported arch: pkg={} ({})", pkg.arch, host_arches.describe()),
    )),
  }
}

pub(super) fn validate_package(
  app: &mut App,
  manifest: &Manifest,
  tmpdir: &Path,
  pkg: &PackageFields,
) -> CliResult<()> {
  let host_platform = detect_platform();
  let host_os = detect_os();
  let host_uname_raw = run_command_text("uname", &["-m"]).unwrap_or_else(|_| "unknown".to_string());
  let host_uname = normalize_uname(&host_uname_raw);
  let pkg_uname_norm = normalize_uname(&pkg.uname);

  if !pkg.uname.is_empty() && pkg_uname_norm != host_uname {
    return Err(CliError::new(
      E_PLATFORM_UNSUPPORTED,
      format!("unsupported uname: pkg={} host={host_uname_raw}", pkg.uname),
    ));
  }

  if pkg.os != "all" && pkg.os != host_os {
    return Err(CliError::new(
      E_PLATFORM_UNSUPPORTED,
      format!("unsupported os: pkg={} host={host_os}", pkg.os),
    ));
  }

  check_package_arch(app, pkg)?;

  // The `pica` backend owns files but does not resolve dependencies, so a
  // package that declares any would install cleanly while silently skipping
  // them. The predicate is shared with the install-time gate in `pipeline`, so
  // the check and the gate cannot disagree about who installs these.
  if !installs_declared_dependencies(&pkg.pkgmgr) {
    let declared: Vec<String> = ["app", "base", "kmod"]
      .iter()
      .flat_map(|key| manifest.get_array_for(key, &pkg.pkgmgr))
      .filter(|dep| !dep.trim().is_empty())
      .collect();
    if !declared.is_empty() {
      return Err(CliError::new(
        E_PACKAGE_INVALID,
        format!(
          "package declares {} but pkgmgr={} cannot resolve dependencies; \
           rebuild it with pkgmgr=opkg or pkgmgr=apk, or drop the dependency",
          declared.join(" "),
          pkg.pkgmgr
        ),
      ));
    }
  }

  // `pkgmgr` is validated while resolving the backend; re-checking it here would
  // duplicate the rule in two places and let them drift.
  if !has_payload(tmpdir) {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      "package would be empty: it ships none of root/, scripts/ or pkgs/",
    ));
  }

  if !pkg.visibility.is_empty()
    && pkg.visibility != "open"
    && pkg.visibility != "mix"
    && pkg.visibility != "closed"
  {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("invalid visibility value: {} (supported: open, mix, closed)", pkg.visibility),
    ));
  }

  if manifest.has_type("luci") {
    if pkg.luci.is_empty() {
      return Err(CliError::new(E_CONFIG_INVALID, "type=luci requires luci=<lua1|js2>"));
    }
    if pkg.luci != "lua1" && pkg.luci != "js2" {
      return Err(CliError::new(E_CONFIG_INVALID, format!("invalid luci value: {}", pkg.luci)));
    }
    let host_luci = detect_luci_variant(app);
    if host_luci == "unknown" {
      return Err(CliError::new(
        E_PLATFORM_UNSUPPORTED,
        format!("luci variant required ({}) but cannot detect host", pkg.luci),
      ));
    }
    if host_luci != pkg.luci {
      return Err(CliError::new(
        E_PLATFORM_UNSUPPORTED,
        format!("unsupported luci variant: pkg={} host={host_luci}", pkg.luci),
      ));
    }
  }

  app.log_info(format!("Installing {}...", pkg.pkgname));
  app.log_info(format!("  version: {}", pkg.pkgver_display));
  app.log_info(format!("  os: {} (host: {host_os})", pkg.os));
  app.log_info(format!("  platform: {} (host: {host_platform})", pkg.platform));
  app.log_info(format!("  arch: {}", pkg.arch));
  app.log_info(format!("  pkgmgr: {}", pkg.pkgmgr));
  app.log_info(format!("  visibility: {}", pkg.visibility));
  if !pkg.uname.is_empty() {
    app.log_info(format!("  uname: {} (host: {host_uname_raw})", pkg.uname));
  }
  let pkg_type = manifest.get_first("type");
  if !pkg_type.is_empty() {
    app.log_info(format!("  type: {pkg_type}"));
  }
  if !pkg.luci.is_empty() {
    app.log_info(format!("  luci: {}", pkg.luci));
  }

  Ok(())
}
