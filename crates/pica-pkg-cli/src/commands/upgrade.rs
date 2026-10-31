use super::install;
use crate::app::{App, CliResult, ensure_dirs};
use crate::candidate::find_pica_candidates_in_index;
use crate::i18n::Key;
use crate::installed;
use crate::state::read_json_file;
use pica_pkg_core::constraint;
use pica_pkg_core::manifest::{get_array_for, get_first as manifest_get_first};
use pica_pkg_core::selector::Selector;
use pica_pkg_core::version::{pkgver_cmp_key, pkgver_ge};
use serde_json::Value;

pub fn all(app: &mut App) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let db = read_json_file(&app.paths.db_file)?;
  let Some(installed) = db.get("installed").and_then(Value::as_object) else {
    app.info(Key::UpgradeNone);
    return Ok(());
  };

  let mut candidates = Vec::new();
  for (pkgname, entry) in installed {
    // Only repository installs can be re-resolved: a local file or URL has no
    // index entry to compare against.
    if !installed::entry_source(entry).is_repo_resolvable() {
      continue;
    }

    // The recorded backend decides which variant of the artifact this entry
    // actually is, and candidates are filtered by it, so it must be selected
    // before the lookup. An explicit `--pkgmgr` still wins.
    if app.options.pkgmgr.is_none() {
      app.select_package_manager(installed::entry_backend(entry).kind.as_str())?;
    }

    let manifest = entry.get("manifest").unwrap_or(&Value::Null);

    let appname = {
      let value = manifest_get_first(manifest, "appname");
      if value.is_empty() {
        let fallback = manifest_get_first(manifest, "pkgname");
        if fallback.is_empty() { pkgname.clone() } else { fallback }
      } else {
        value
      }
    };

    let branch = manifest_get_first(manifest, "branch");
    let installed_ver = pkgver_cmp_key(
      &manifest_get_first(manifest, "pkgver"),
      &manifest_get_first(manifest, "pkgrel"),
    );

    let selector =
      Selector { raw: String::new(), norm: String::new(), appname, branch }.to_canonical_string();

    candidates.push((pkgname.clone(), selector, installed_ver));
  }

  if candidates.is_empty() {
    app.info(Key::UpgradeNone);
    return Ok(());
  }

  let mut updated_any = false;

  for (pkgname, selector, installed_ver) in candidates {
    let repo_candidates = find_pica_candidates_in_index(app, &selector)?;
    if repo_candidates.is_empty() {
      let name = pkgname.clone();
      app.warn_args(Key::UpgradeSkip, &[&name]);
      continue;
    }

    let mut best_ver = String::new();
    for candidate in &repo_candidates {
      if pkgver_ge(&candidate.cmpver, &best_ver) {
        best_ver.clone_from(&candidate.cmpver);
      }
    }

    if pkgver_ge(&installed_ver, &best_ver) {
      let name = pkgname.clone();
      let version = installed_ver.clone();
      app.info_args(Key::UpgradeUpToDate, &[&name, &version]);
      continue;
    }

    // The install path prints the transaction header from the database record,
    // so the upgrade loop does not announce the same upgrade a second time.
    install::pica_from_repo(app, &selector)?;
    updated_any = true;
  }

  if updated_any {
    recheck_constraints(app)?;
  } else {
    app.info(Key::UpgradeAllUpToDate);
  }

  Ok(())
}

/// Re-checks every installed package's bounds once the backend is done.
///
/// `-Su` hands each version choice to the backend and keeps the result: a
/// version the backend picked is not pica's to undo, so a broken bound is a
/// warning, never a downgrade. Saying it here is still worth the query — the
/// alternative is a mismatch that only surfaces later, at the next `-U`
/// precheck, as a refusal to install something unrelated to what `-Su` did.
fn recheck_constraints(app: &mut App) -> CliResult<()> {
  let db = read_json_file(&app.paths.db_file)?;
  let Some(entries) = db.get("installed").and_then(Value::as_object) else {
    return Ok(());
  };

  for (pkgname, entry) in entries {
    let backend = installed::entry_backend(entry);
    if app.options.pkgmgr.is_none() && app.select_package_manager(backend.kind.as_str()).is_err() {
      // A backend whose command is gone cannot say what is installed. Skipping
      // the entry keeps the upgrades that did succeed from being reported as a
      // failed run; the next `-U` that needs this backend says so directly.
      continue;
    }

    let manifest = entry.get("manifest").unwrap_or(&Value::Null);
    for group in crate::constraints::GROUPS {
      let declarations = get_array_for(manifest, group, backend.kind.as_str());
      let constraints = match constraint::parse_all(&declarations) {
        Ok(constraints) => constraints,
        // Not reachable for an artifact this pica installed (the precheck would
        // have refused it), but a row written by an older pica may hold any
        // text. Reporting it beats silently treating it as unconstrained.
        Err(err) => {
          app.log_warn(format!("{pkgname}: cannot read {group} constraints: {err}"));
          continue;
        }
      };

      for message in crate::constraints::unmet(app, pkgname, group, &constraints) {
        app.log_warn(message);
      }
    }
  }

  Ok(())
}
