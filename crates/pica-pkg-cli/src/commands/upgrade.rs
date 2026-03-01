use super::install;
use crate::app::{ensure_dirs, App, CliResult};
use crate::candidate::find_pica_candidates_in_index;
use crate::state::read_json_file;
use pica_pkg_core::manifest::get_first as manifest_get_first;
use pica_pkg_core::version::{pkgver_cmp_key, pkgver_ge};
use serde_json::Value;

pub fn all(app: &mut App) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let db = read_json_file(&app.paths.db_file)?;
  let Some(installed) = db.get("installed").and_then(Value::as_object) else {
    app.log_info("No pica packages to upgrade");
    return Ok(());
  };

  let mut candidates = Vec::new();
  for (pkgname, entry) in installed {
    let manifest = entry.get("manifest").unwrap_or(&Value::Null);
    if manifest_get_first(manifest, "source") != "pica" {
      continue;
    }

    let appname = {
      let value = manifest_get_first(manifest, "appname");
      if value.is_empty() {
        let fallback = manifest_get_first(manifest, "pkgname");
        if fallback.is_empty() {
          pkgname.clone()
        } else {
          fallback
        }
      } else {
        value
      }
    };

    let branch = manifest_get_first(manifest, "branch");
    let installed_ver = pkgver_cmp_key(
      &manifest_get_first(manifest, "pkgver"),
      &manifest_get_first(manifest, "pkgrel"),
    );

    let mut selector = appname;
    if !branch.is_empty() {
      selector.push('(');
      selector.push_str(&branch);
      selector.push(')');
    }

    candidates.push((pkgname.clone(), selector, installed_ver));
  }

  if candidates.is_empty() {
    app.log_info("No pica packages to upgrade");
    return Ok(());
  }

  let mut updated_any = false;

  for (pkgname, selector, installed_ver) in candidates {
    let repo_candidates = find_pica_candidates_in_index(app, &selector)?;
    if repo_candidates.is_empty() {
      app.log_warn(format!("Skip {pkgname}: not found in index"));
      continue;
    }

    let mut best_ver = String::new();
    for candidate in &repo_candidates {
      if pkgver_ge(&candidate.cmpver, &best_ver) {
        best_ver.clone_from(&candidate.cmpver);
      }
    }

    if pkgver_ge(&installed_ver, &best_ver) {
      app.log_info(format!("Up to date: {pkgname} ({installed_ver})"));
      continue;
    }

    app.log_info(format!("Upgrading {pkgname}: {installed_ver} -> {best_ver}"));
    install::pica_from_repo(app, &selector)?;
    updated_any = true;
  }

  if !updated_any {
    app.log_info("All packages are up to date");
  }

  Ok(())
}
