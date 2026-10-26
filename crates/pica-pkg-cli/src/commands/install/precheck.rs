use super::pipeline::{PackageFields, installs_declared_dependencies};
use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_MANIFEST_INVALID, E_PACKAGE_INVALID,
  E_PLATFORM_UNSUPPORTED, E_VERSION_INCOMPATIBLE,
};
use crate::platform::{
  ArchVerdict, check_arch, detect_luci_variant, detect_os, detect_package_arches, normalize_uname,
};
use crate::system::run_command_text;
use pica_pkg_core::constraint::{self, Constraint};
use pica_pkg_core::manifest::{Manifest, SOURCE_AVAILABILITY_VALUES, is_valid_source_availability};
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

/// Parses one dependency list, naming the manifest key in the error.
///
/// [`constraint::resolve`] runs here rather than at each reader so that the
/// `?` marker and the required-wins rule are decided once: every consumer sees
/// one entry per package, and a name declared both ways cannot be installed as
/// optional by one path and skipped by another.
pub(super) fn parse_group(items: &[String], field: &str) -> CliResult<Vec<Constraint>> {
  constraint::resolve(items)
    .map_err(|err| CliError::new(E_MANIFEST_INVALID, format!("{field}: {err}")))
}

/// The parsed dependency declarations of the artifact being installed.
///
/// One parse feeds three readers — the precheck, the install-report, and the
/// step-5 install — so a declaration cannot be accepted by one and refused by
/// another.
pub(crate) struct DeclaredDeps {
  pub(crate) kmod: Vec<Constraint>,
  pub(crate) base: Vec<Constraint>,
  pub(crate) app: Vec<Constraint>,
}

impl DeclaredDeps {
  /// Parses `kmod`, `base` and `app` for `backend`.
  ///
  /// `app_decls` is passed in because the app list is not only the manifest's:
  /// `app_i18n` adds one name at install time, and the caller is the only place
  /// that knows whether it applies to this install.
  pub(crate) fn parse(manifest: &Manifest, backend: &str, app_decls: &[String]) -> CliResult<Self> {
    Ok(Self {
      kmod: parse_group(&manifest.get_array_for("kmod", backend), "kmod")?,
      base: parse_group(&manifest.get_array_for("base", backend), "base")?,
      app: parse_group(app_decls, "app")?,
    })
  }
}

/// The status of every declaration in `deps`, in the order they were declared.
///
/// A bound is enforced here only when the dependency is already installed: that
/// is the one case where pica knows which version the backend will leave alone.
/// A dependency that is not installed yet may still arrive at the right version,
/// so the bound is recorded in the report and checked again after step 5 (see
/// `pipeline::recheck_constraints`). What is never done is putting the bound on
/// the backend's command line — the name is all a backend is asked for.
///
/// An optional declaration that is missing or too old is recorded and warned
/// about, never raised to an error: the manifest itself said the install may
/// proceed without it. Its entry carries `"optional": true` so the report, the
/// selector and the missing-dependency summary can tell the two apart.
fn dep_entries(
  app: &mut App,
  requester: &str,
  group: &str,
  deps: &[Constraint],
  pkgs_dir: &Path,
) -> CliResult<Vec<Value>> {
  let mut entries = Vec::new();
  for dep in deps {
    let status = match precheck_dep_source(app, &dep.name, pkgs_dir) {
      Ok(status) => status,
      // A feed index that cannot be read is an error for a required dependency:
      // "missing" would be a claim about the feeds that pica cannot make. For an
      // optional one it is only a reason not to offer it.
      Err(err) if dep.optional => {
        app.log_warn(format!("optional dependency {} was skipped: {}", dep.name, err.message));
        "unknown".to_string()
      }
      Err(err) => return Err(err),
    };
    // Only an installed dependency has a version on disk to compare against; for
    // the rest the bound travels to the post-install recheck instead. `None`
    // therefore means "not asked", which is why the key is omitted rather than
    // written as `false`; `Some(None)` means it was asked and the backend would
    // not say which version is installed.
    let bound = (status == "installed" && dep.has_bound())
      .then(|| crate::constraints::verdict(app, requester, group, dep));
    match &bound {
      Some(Some(violations)) => {
        if let Some(message) = violations.first() {
          if dep.optional {
            app.log_warn(format!("{}: {message}", crate::constraints::BOUND_UNMET));
          } else {
            return Err(CliError::new(E_VERSION_INCOMPATIBLE, message.clone()));
          }
        }
      }
      // Installed, bounded, and no version to compare: unchecked, not satisfied.
      // Recording `true` here would assert something nobody verified.
      Some(None) => app.log_warn(format!(
        "installed version of {} is unknown, so the declared {} was not checked",
        dep.name,
        dep.requirement()
      )),
      None => {}
    }

    let mut entry = json!({"name": dep.name, "status": status});
    if dep.optional {
      entry["optional"] = json!(true);
    }
    if dep.has_bound() {
      // The report is what an install leaves behind for auditing: it records
      // that the artifact asserted a version, even when the dependency was not
      // installed yet and the assertion could not be checked.
      entry["constraint"] = json!(dep.requirement());
    }
    // The verdict, when one was reached. Its absence is not "satisfied": it says
    // the question could not be put to a version this install knows about.
    if let Some(Some(violations)) = &bound {
      entry["bound_satisfied"] = json!(violations.is_empty());
    }
    entries.push(entry);
  }
  Ok(entries)
}

pub(crate) fn build_precheck_report(
  app: &mut App,
  manifest: &Manifest,
  pkgs_dir: &Path,
  deps: &DeclaredDeps,
) -> CliResult<Value> {
  let requester = manifest.get_first("pkgname");

  // kmods are always resolved through feeds: the OpenWrt kernel module packages
  // cannot be shipped inside an application artifact, so their `pkgs_dir` is
  // empty on purpose.
  let kmod = dep_entries(app, &requester, "kmod", &deps.kmod, Path::new(""))?;
  let base = dep_entries(app, &requester, "base", &deps.base, pkgs_dir)?;
  let app = dep_entries(app, &requester, "app", &deps.app, pkgs_dir)?;

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
      // An optional dependency is not a prerequisite: listing it here would
      // turn "optional and absent" into the same refusal as "required and absent".
      if entry.get("optional").and_then(Value::as_bool).unwrap_or(false) {
        continue;
      }
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

/// Refuses declarations the chosen backend cannot resolve.
///
/// The `pica` backend owns files but does not resolve dependencies, so a
/// package that declares any would install cleanly while silently skipping
/// them. The predicate is shared with the install-time gate in `pipeline`, so
/// the check and the gate cannot disagree about who installs these.
///
/// Only required declarations are refused. An optional one is a suggestion the
/// backend cannot take up, which is exactly the "unavailable, warn and skip"
/// case: refusing the whole artifact would make it mandatory.
fn check_resolvable_dependencies(
  app: &mut App,
  manifest: &Manifest,
  pkg: &PackageFields,
) -> CliResult<()> {
  if installs_declared_dependencies(&pkg.pkgmgr) {
    return Ok(());
  }

  let mut required = Vec::new();
  let mut optional = Vec::new();
  for key in ["app", "base", "kmod"] {
    let Ok(constraints) = constraint::resolve(&manifest.get_array_for(key, &pkg.pkgmgr)) else {
      // The same list is parsed again, with the manifest key in the error, by
      // `DeclaredDeps::parse`. Not repeating that here keeps one message.
      continue;
    };
    for dep in constraints {
      if dep.optional { optional.push(dep.raw) } else { required.push(dep.raw) }
    }
  }

  if !required.is_empty() {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      format!(
        "package declares {} but pkgmgr={} cannot resolve dependencies; \
         rebuild it with pkgmgr=opkg or pkgmgr=apk, or drop the dependency",
        required.join(" "),
        pkg.pkgmgr
      ),
    ));
  }

  if !optional.is_empty() {
    app.log_warn(format!(
      "pkgmgr={} cannot install optional dependencies; skipping {}",
      pkg.pkgmgr,
      optional.join(" ")
    ));
  }

  Ok(())
}

pub(super) fn validate_package(
  app: &mut App,
  manifest: &Manifest,
  tmpdir: &Path,
  pkg: &PackageFields,
) -> CliResult<()> {
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

  check_resolvable_dependencies(app, manifest, pkg)?;

  // `pkgmgr` is validated while resolving the backend; re-checking it here would
  // duplicate the rule in two places and let them drift.
  if !has_payload(tmpdir) {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      "package would be empty: it ships no root/ or scripts/, and its pkgs/ holds no packages",
    ));
  }

  if !is_valid_source_availability(&pkg.source_availability) {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!(
        "invalid source_availability value: {} (supported: {})",
        pkg.source_availability,
        SOURCE_AVAILABILITY_VALUES.join(", ")
      ),
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

  // What this artifact declares about itself — version, os, arch,
  // source_availability, type — is in the manifest and in `-Qi`; the transaction
  // header already said which package and which version is being installed.
  // Printing the fields one per line here only buried the phase that followed,
  // and every check above already refused a mismatch with an error naming both
  // sides.

  Ok(())
}
