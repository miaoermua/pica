use super::install;
use crate::app::{App, CliError, CliResult, ensure_dirs};
use crate::candidate::find_pica_candidates_in_index;
use crate::i18n::Key;
use crate::i18n::tr;
use crate::installed;
use crate::state::read_db;
use pica_pkg_core::constraint;
use pica_pkg_core::manifest::get_array_for;
use pica_pkg_core::selector::Selector;
use pica_pkg_core::version::pkgver_ge;

/// One installed record a repository could upgrade.
pub(crate) struct Tracked {
  pub(crate) pkgname: String,
  pub(crate) selector: String,
  pub(crate) installed: String,
  pub(crate) kind: crate::pkgmgr::PackageManagerKind,
}

/// Every record that came from a repository, with the backend it is handled
/// with. A local file or URL has no index entry to compare against.
pub(crate) fn tracked(app: &App) -> CliResult<Vec<Tracked>> {
  let db = read_db(&app.paths)?;
  let mut out = Vec::new();
  for (pkgname, entry) in &db.installed {
    if !installed::entry_source(entry).is_repo_resolvable() {
      continue;
    }
    // The recorded backend decides which variant of the artifact this entry
    // actually is, and candidates are filtered by it, so the lookup and the
    // install both run with it. An explicit `--pkgmgr` still wins.
    let kind = app.backend_for_record(installed::entry_backend(entry).kind);
    let selector = Selector {
      raw: String::new(),
      norm: String::new(),
      appname: entry.appname_or(pkgname),
      branch: entry.field("branch"),
    }
    .to_canonical_string();
    out.push(Tracked { pkgname: pkgname.clone(), selector, installed: entry.version(), kind });
  }
  Ok(out)
}

/// The newest version the synced sources offer for `record` on this host, or
/// `None` when none of them publishes it.
///
/// # Errors
/// Fails when the sync cache cannot be read or the record's backend is missing.
pub(crate) fn newest_offered(app: &mut App, record: &Tracked) -> CliResult<Option<String>> {
  let found =
    app.with_backend(record.kind, |app| find_pica_candidates_in_index(app, &record.selector))?;
  let mut best: Option<String> = None;
  for candidate in found {
    if best.as_deref().is_none_or(|current| pkgver_ge(&candidate.cmpver, current)) {
      best = Some(candidate.cmpver);
    }
  }
  Ok(best)
}

pub fn all(app: &mut App) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let records = tracked(app)?;
  if records.is_empty() {
    app.info(Key::UpgradeNone);
    return Ok(());
  }

  // 1. The plan, from the synced index alone: nothing is downloaded yet. One
  //    package's failure is that package's: the rest still get their turn, and
  //    the summary says which ones did not make it.
  let mut outcome = Outcome::default();
  let mut pending = Vec::new();
  for record in records {
    match newest_offered(app, &record) {
      Ok(Some(newest)) if pkgver_ge(&record.installed, &newest) => {
        app.info_args(Key::UpgradeUpToDate, &[&record.pkgname, &record.installed]);
      }
      Ok(Some(newest)) => pending.push((record, newest)),
      Ok(None) => {
        app.warn_args(Key::UpgradeSkip, &[&record.pkgname]);
        outcome.skipped.push(record.pkgname);
      }
      Err(err) => {
        app.warn_args(Key::UpgradeFailed, &[&record.pkgname, &err.message]);
        outcome.failed.push((record.pkgname, err));
      }
    }
  }

  if !pending.is_empty() {
    app.info_args(Key::PlanUpgrades, &[&pending.len().to_string()]);
    for (record, newest) in &pending {
      app.log_info(format!("  {} {} -> {newest}", record.pkgname, record.installed));
    }
    if app.stop_for_dry_run() {
      return outcome.into_result(app);
    }
    // One answer covers the whole list, so each install does not ask again.
    app.confirm()?;
    app.confirmed = true;
  }

  // 2. The upgrades themselves.
  for (record, _) in pending {
    // The install path prints the transaction header from the database record,
    // so the upgrade loop does not announce the same upgrade a second time.
    match app.with_backend(record.kind, |app| install::pica_from_repo(app, &record.selector)) {
      Ok(()) => outcome.upgraded.push(record.pkgname),
      Err(err) => {
        app.warn_args(Key::UpgradeFailed, &[&record.pkgname, &err.message]);
        outcome.failed.push((record.pkgname, err));
      }
    }
  }

  if !outcome.upgraded.is_empty() {
    recheck_constraints(app)?;
  }
  outcome.report(app)
}

/// What one `-Su` did, package by package.
#[derive(Default)]
struct Outcome {
  upgraded: Vec<String>,
  skipped: Vec<String>,
  failed: Vec<(String, CliError)>,
}

impl Outcome {
  /// Prints the summary, and fails the run when any package failed.
  ///
  /// The error carries the code of the first failure, so `--json-errors` says
  /// what kind of thing went wrong; the message names every failed package.
  fn report(self, app: &mut App) -> CliResult<()> {
    let upgraded = self.upgraded.len().to_string();
    let skipped = self.skipped.len().to_string();
    let failed = self.failed.len().to_string();
    if self.upgraded.is_empty() && self.failed.is_empty() && self.skipped.is_empty() {
      app.info(Key::UpgradeAllUpToDate);
    } else {
      app.info_args(Key::UpgradeSummary, &[&upgraded, &skipped, &failed]);
    }
    self.into_result(app)
  }

  /// Fails the run when any package failed, without the summary line.
  fn into_result(self, app: &App) -> CliResult<()> {
    let Some((_, first)) = self.failed.first() else {
      return Ok(());
    };
    let names: Vec<&str> = self.failed.iter().map(|(name, _)| name.as_str()).collect();
    Err(CliError::new(first.code, app.args(Key::UpgradeSomeFailed, &[&names.join(", ")])))
  }
}

/// Re-checks every installed package's bounds once the backend is done.
///
/// `-Su` hands each version choice to the backend and keeps the result: a
/// version the backend picked is not pica's to undo, so a broken bound is a
/// warning, never a downgrade. Saying it here is still worth the query — the
/// alternative is a mismatch that only surfaces later, at the next `-U`
/// precheck, as a refusal to install something unrelated to what `-Su` did.
fn recheck_constraints(app: &mut App) -> CliResult<()> {
  let db = read_db(&app.paths)?;
  for (pkgname, entry) in &db.installed {
    let backend = installed::entry_backend(entry);
    let kind = app.backend_for_record(backend.kind);
    // A backend whose command is gone cannot say what is installed. Skipping
    // the entry keeps the upgrades that did succeed from being reported as a
    // failed run; the next `-U` that needs this backend says so directly.
    let _ = app.with_backend(kind, |app| {
      recheck_entry(app, pkgname, entry, backend.kind);
      Ok(())
    });
  }

  Ok(())
}

/// Warns about each bound of one record that the installed system breaks.
fn recheck_entry(
  app: &mut App,
  pkgname: &str,
  entry: &pica_pkg_core::model::db::DbEntry,
  recorded: crate::pkgmgr::PackageManagerKind,
) {
  for group in crate::constraints::GROUPS {
    let declarations = get_array_for(&entry.manifest, group, recorded.as_str());
    let constraints = match constraint::parse_all(&declarations) {
      Ok(constraints) => constraints,
      // Not reachable for an artifact this pica installed (the precheck would
      // have refused it), but a row written by an older pica may hold any
      // text. Reporting it beats silently treating it as unconstrained.
      Err(err) => {
        app.log_warn(tr(Key::ConstraintsUnreadable, &[&pkgname, &group, &err]));
        continue;
      }
    };

    for message in crate::constraints::unmet(app, pkgname, group, &constraints) {
      app.log_warn(message);
    }
  }
}
