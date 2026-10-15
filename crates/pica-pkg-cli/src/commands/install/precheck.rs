use super::pipeline::{PackageFields, installs_declared_dependencies};
use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_PACKAGE_INVALID, E_PLATFORM_UNSUPPORTED,
};
use crate::platform::{
  ArchVerdict, check_arch, detect_luci_variant, detect_os, detect_package_arches, detect_platform,
  normalize_uname,
};
use crate::system::run_command_text;
use pica_pkg_core::manifest::Manifest;
use serde_json::{Value, json};
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

/// Where one declared dependency would come from, as the precheck report says it.
///
/// `Err` when the feeds cannot be read at all: "missing" is an answer *about*
/// the feeds, and a backend whose index was never fetched would give it for
/// every name there is.
pub(crate) fn precheck_dep_source(app: &App, dep: &str, pkgs_dir: &Path) -> CliResult<String> {
  let Ok(package_manager) = app.package_manager() else {
    // No backend at all is not a failure: it means no feeds, and the containers
    // are the only source that could name this dependency.
    return Ok(packaged_or_missing(pkgs_dir, dep));
  };
  if package_manager.is_installed(dep) {
    return Ok("installed".to_string());
  }
  if package_manager.package_exists(dep)? {
    return Ok("feed".to_string());
  }
  Ok(packaged_or_missing(pkgs_dir, dep))
}

/// The source left when the feeds do not have `dep`.
fn packaged_or_missing(pkgs_dir: &Path, dep: &str) -> String {
  if pkgs_dir_has_pkg(pkgs_dir, dep) { "packaged".to_string() } else { "missing".to_string() }
}

/// The status of every name in `deps`, in the order they were declared.
fn dep_entries(app: &App, deps: Vec<String>, pkgs_dir: &Path) -> CliResult<Vec<Value>> {
  let mut entries = Vec::new();
  for dep in deps.into_iter().filter(|dep| !dep.is_empty()) {
    let status = precheck_dep_source(app, &dep, pkgs_dir)?;
    entries.push(json!({"name": dep, "status": status}));
  }
  Ok(entries)
}

pub(crate) fn build_precheck_report(
  app: &App,
  manifest: &Manifest,
  pkgs_dir: &Path,
  app_list: &[String],
) -> CliResult<Value> {
  let backend = app.backend().as_str();

  // kmods are always resolved through feeds: the OpenWrt kernel module packages
  // cannot be shipped inside an application artifact, so their `pkgs_dir` is
  // empty on purpose.
  let kmod = dep_entries(app, manifest.get_array_for("kmod", backend), Path::new(""))?;
  let base = dep_entries(app, manifest.get_array_for("base", backend), pkgs_dir)?;
  let app = dep_entries(app, app_list.to_vec(), pkgs_dir)?;

  Ok(json!({
    "kmod": kmod,
    "base": base,
    "app": app,
  }))
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

/// Whether installing this package would change anything on disk.
///
/// `root/` and `scripts/` are payload by being there: `root/` is laid down file
/// by file and a hook runs. `pkgs/` is not — an empty one installs nothing — so
/// it counts only when it holds a file, which is the same view
/// `install_pkgs_dir` takes. The difference matters because an artifact whose
/// only payload is an empty `pkgs/` used to pass validation and then fail in the
/// middle of the install with "no packages found in packaged dir".
fn has_payload(tmpdir: &Path) -> bool {
  ["root", "scripts"].iter().any(|name| tmpdir.join(name).is_dir())
    || dir_has_file(&tmpdir.join("pkgs"), 0)
}

/// True when `dir` holds a file, at any depth.
///
/// The packer flattens containers into `pkgs/` — one level is what an artifact
/// has — but a nested layout is still a payload, so the search descends. The
/// depth limit is for the archive that points a directory symlink at one of its
/// ancestors: without it the recursion would follow the cycle forever.
fn dir_has_file(dir: &Path, depth: usize) -> bool {
  /// Levels below `pkgs/` that are still considered payload.
  const MAX_DEPTH: usize = 8;

  if depth > MAX_DEPTH {
    return false;
  }
  let Ok(entries) = fs::read_dir(dir) else {
    return false;
  };
  entries.flatten().any(|entry| {
    let path = entry.path();
    if path.is_dir() { dir_has_file(&path, depth + 1) } else { path.is_file() }
  })
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
      "package would be empty: it ships no root/ or scripts/, and its pkgs/ holds no packages",
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
