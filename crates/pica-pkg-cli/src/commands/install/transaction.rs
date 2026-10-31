//! What an install changed, how to undo it, and recording it (step 8).

use super::backup::{PayloadBackup, back_up_tree_into};
use super::payload::snapshot_payload;
use super::pipeline::{DependencyPrecheck, InstallContext, canonicalize_display, install_summary};
use crate::app::{App, CliError, CliResult};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::state::{new_entry, read_db, report_set_install_result, write_db};
use crate::system::{TargetRoot, run_hook};
use pica_pkg_core::io::{PayloadInstall, remove_payload};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// What one install has done so far, so that it can be undone.
///
/// Every field is filled in *before* the step that could fail, so a step that
/// dies halfway still leaves the transaction holding whatever it managed to do.
/// Nothing here is written to the database: the record is step 8, and until it
/// exists this struct is the only description of the install.
pub(super) struct InstallTransaction {
  /// Every installed package and the version the backend reported for it,
  /// taken before step 5. Names alone cannot tell a dependency this install
  /// added from one it upgraded.
  pub(super) tx_before: Option<Vec<(String, String)>>,
  /// Packages step 5 added, diffed against `tx_before` after every phase.
  /// Rollback removes exactly these.
  pub(super) tx_added: Vec<String>,
  /// Packages that were already installed and changed version during step 5, as
  /// `(name, before, after)`. Rollback cannot undo these — pica has no previous
  /// version to put back — so they are reported instead of silently accepted.
  pub(super) tx_changed: Vec<(String, String, String)>,
  /// The part of `tx_added` that came from the `app` stage, which is the `app`
  /// list when the dependencies came from feeds. A container batch is installed
  /// as one request covering `base` and `app` at once, so it appears in
  /// `tx_added` only.
  pub(super) app_added: Vec<String>,
  /// Paths deployed, in deployment order so removal can replay them backwards.
  /// This is what the database records, so it lists every file the install
  /// wrote — including the ones it replaced.
  pub(super) payload_records: Vec<PayloadInstall>,
  /// The subset of `payload_records` this transaction **created**. Rollback
  /// removes only these: a file the install replaced was already on the device,
  /// and deleting it would take the path itself away on top of the content.
  pub(super) created_records: Vec<PayloadInstall>,
  /// What this transaction has copied out of the way, ready to go back: the kept
  /// scripts an upgrade replaces (step 6), the payload's destinations and the env
  /// file (step 7). Emptied when the install commits.
  pub(super) payload_backup: Option<PayloadBackup>,
  /// The destinations the payload walk actually wrote, in the order it wrote
  /// them. A walk that fails halfway leaves what it reached here, which is how a
  /// rollback tells a file it replaced from one it never got to: putting a copy
  /// back over the latter would rewrite a file nothing touched.
  pub(super) deployed: HashSet<PathBuf>,
  /// Labels of the hooks that ran, in the order they ran.
  pub(super) executed_hooks: Vec<String>,
}

impl InstallTransaction {
  pub(super) fn new() -> Self {
    Self {
      tx_before: None,
      tx_added: Vec::new(),
      tx_changed: Vec::new(),
      app_added: Vec::new(),
      payload_records: Vec::new(),
      created_records: Vec::new(),
      payload_backup: None,
      deployed: HashSet::new(),
      executed_hooks: Vec::new(),
    }
  }

  pub(super) fn record_payload(&mut self, payload: PayloadInstall) {
    self.payload_records.push(payload);
  }

  /// Notes every destination a walk reached, finished or not.
  pub(super) fn note_deployed(&mut self, walked: &PayloadInstall) {
    self.deployed.extend(walked.files.iter().map(PathBuf::from));
  }

  /// Takes the copies of everything step 7 is about to replace, before it
  /// replaces any of it.
  ///
  /// The order is the whole point: a cache directory that cannot be written, a
  /// file that cannot be read and a destination that cannot be put back (a
  /// socket, a fifo, a device node) all fail *here*, while the device still has
  /// every byte it had.
  pub(super) fn prepare_payload_backup(
    &mut self,
    app: &App,
    pkgname: &str,
    target: &TargetRoot,
    root_dir: &Path,
  ) -> CliResult<()> {
    let backup = self.backup_store(app, pkgname);
    if root_dir.is_dir() {
      snapshot_payload(target, root_dir, root_dir, backup)?;
    }
    Ok(())
  }

  /// The transaction's one backup store, made on first use.
  pub(super) fn backup_store(&mut self, app: &App, pkgname: &str) -> &mut PayloadBackup {
    self.payload_backup.get_or_insert_with(|| PayloadBackup::new(app, pkgname))
  }

  /// Keeps every file and link under `dir`, which this install is about to
  /// clear, and returns their paths.
  pub(super) fn back_up_tree(
    &mut self,
    app: &App,
    pkgname: &str,
    dir: &Path,
  ) -> CliResult<HashSet<PathBuf>> {
    let mut kept = HashSet::new();
    back_up_tree_into(self.backup_store(app, pkgname), dir, &mut kept)?;
    Ok(kept)
  }

  /// Keeps one more path that step 7 is about to overwrite.
  ///
  /// The env file is Pica's own metadata, but it is a destination this install
  /// replaces exactly like a payload file does, so it is backed up by the same
  /// store and restored by the same rollback.
  pub(super) fn back_up_destination(&mut self, path: &Path) -> CliResult<()> {
    let Some(backup) = self.payload_backup.as_mut() else {
      return Ok(());
    };
    let Ok(metadata) = fs::symlink_metadata(path) else {
      return Ok(());
    };
    backup.back_up(path, &metadata)
  }

  /// Removes the copies this transaction kept, once the install is complete. A
  /// failure is reported, not hidden: they are the last copy of what was on the
  /// device before, and the message says where they are.
  pub(super) fn finish(&mut self, app: &mut App) {
    self.finish_with(app, |dir| fs::remove_dir_all(dir));
  }

  pub(super) fn finish_with(
    &mut self,
    app: &mut App,
    remove_payload_backup: impl FnOnce(&Path) -> std::io::Result<()>,
  ) {
    if let Some(backup) = self.payload_backup.take().and_then(PayloadBackup::release) {
      if let Err(err) = remove_payload_backup(&backup) {
        app.log_warn(tr(Key::BackupCleanupFailed, &[&backup.display(), &err]));
      }
    }
  }

  /// Records the paths this transaction created, which rollback may remove.
  ///
  /// Kept apart from [`Self::record_payload`] on purpose: the database record
  /// has to name every file the install wrote, while a rollback may only take
  /// away paths that were not there before it started.
  pub(super) fn record_created(&mut self, payload: PayloadInstall) {
    self.created_records.push(payload);
  }

  pub(super) fn record_hook(&mut self, label: &str) {
    self.executed_hooks.push(label.to_string());
  }

  /// Diffs the backend's installed set against the snapshot from step 4, keeping
  /// an addition and a version change apart: only the first can be undone.
  ///
  /// Called after every phase *and* on its error path. A backend that fails
  /// halfway through a request has still installed part of it, so the exit
  /// status is not evidence that nothing landed.
  ///
  /// A list that cannot be read keeps what earlier phases confirmed. Treating
  /// the failure as "nothing installed" is what makes a transaction forget the
  /// packages it just added — and those are exactly the ones rollback is for.
  pub(super) fn record_backend_changes(&mut self, app: &mut App) {
    let Some(before) = &self.tx_before else {
      return;
    };
    let mut after = match app.package_manager().and_then(crate::pkgmgr::PackageManager::installed) {
      Ok(after) => after,
      Err(err) => {
        app.log_warn(tr(Key::InstalledListUnreadable, &[&err.message]));
        return;
      }
    };
    after.sort();

    let mut added = Vec::new();
    let mut changed = Vec::new();
    for (name, version) in &after {
      match before.iter().find(|(known, _)| known == name) {
        None => added.push(name.clone()),
        Some((_, previous)) if previous != version => {
          changed.push((name.clone(), previous.clone(), version.clone()));
        }
        Some(_) => {}
      }
    }
    self.tx_added = added;
    self.tx_changed = changed;
  }

  /// Every path this install created, as one payload.
  ///
  /// `root/` is deployed first and the kept scripts after it, so parents come
  /// before children: `remove_payload` walks directories in reverse and only
  /// removes the ones that ended up empty.
  pub(super) fn merged_payload(&self) -> PayloadInstall {
    let mut merged = PayloadInstall::default();
    for record in &self.payload_records {
      merged.files.extend(record.files.iter().cloned());
      merged.dirs.extend(record.dirs.iter().cloned());
      merged.size += record.size;
    }
    merged
  }

  /// Undoes everything recorded so far, and returns what could not be undone.
  ///
  /// The order mirrors the install backwards: the content this transaction
  /// replaced goes back first, then the paths it created come off (deepest
  /// payload last), then the hooks are compensated, and finally the packages
  /// step 5 added are handed back to the backend that installed them.
  pub(super) fn rollback(&mut self, app: &mut App, ctx: &InstallContext<'_>) -> Vec<String> {
    let mut leftovers = Vec::new();

    // Content first. A path this install replaced is not its own to delete, so
    // the only way back is the copy taken before the walk — and putting it back
    // has to happen before anything is removed, because a removal can empty a
    // directory this transaction never created.
    if let Some(mut backup) = self.payload_backup.take() {
      let unrestored = backup.restore_replacing(&self.deployed);
      for path in &unrestored {
        leftovers.push(format!("content:{path}"));
      }
      if let Some(dir) = backup.release() {
        // A copy that could not go back is the only version of that file anyone
        // has left, so the store stays where it is and the message says where.
        let paths = unrestored.join(" ");
        let paths = if paths.is_empty() { tr(Key::TheReplacedFiles, &[]) } else { paths };
        app.log_warn(tr(Key::RestoreFailed, &[&paths, &dir.display()]));
      }
    }

    for record in self.created_records.iter().rev() {
      // Its count is dropped because it does not answer the question below: a
      // directory may survive too, and that is not a file.
      let _ = remove_payload(record);
    }

    // `remove_payload` is best effort, so what survived is found by looking
    // rather than by trusting a return value. The survivors are both reported
    // and kept as this install's payload record, because a `-R` driven by that
    // record is the only way left to clean them.
    //
    // A recorded directory is one `remove_payload` was told this install
    // created, so a directory that is still there is a survivor too: it is
    // either not empty (something else — a hook's file, another package's file
    // — moved in) or could not be removed. Ignoring it would report a clean
    // rollback while leaving a record nothing points `-R` at.
    //
    // Only `created_records` is walked: a file the install *replaced* is not a
    // survivor of this rollback, and listing it here would make `-R` delete a
    // path the transaction did not create.
    // Asked with `symlink_metadata` rather than `exists`: a created symlink whose
    // target is gone is a survivor too, and `exists` follows the link when it is
    // exactly what may not be followed.
    let present = |path: &String| Path::new(path).symlink_metadata().is_ok();
    let mut remnant = PayloadInstall::default();
    for record in &self.created_records {
      for file in &record.files {
        if present(file) {
          leftovers.push(file.clone());
          remnant.files.push(file.clone());
        }
      }
      for dir in &record.dirs {
        if present(dir) {
          leftovers.push(dir.clone());
          remnant.dirs.push(dir.clone());
        }
      }
    }
    self.payload_records = vec![remnant.clone()];
    self.created_records = vec![remnant];

    if !self.executed_hooks.is_empty() {
      // A hook may have changed things only the package knows how to undo, so
      // the package's own `remove` hook is the compensation. It runs in reverse
      // order, once per hook that ran.
      for label in self.executed_hooks.iter().rev() {
        if let Err(err) = run_hook(app, ctx.tmpdir, &ctx.scripts.remove, "rollback-remove") {
          leftovers.push(format!("hook:{label} ({})", err.message));
        }
      }
      self.executed_hooks.clear();
    }

    if app.backend().requires_system_binary() {
      for name in std::mem::take(&mut self.tx_added).into_iter().rev() {
        if let Err(err) = app.package_manager().and_then(|manager| manager.remove(&name)) {
          leftovers.push(format!("pkg:{name} ({})", err.message));
        }
      }
    }
    self.tx_added.clear();
    self.app_added.clear();

    leftovers
  }
}

/// Rolls a failed install back and reports what could not be undone.
///
/// A rollback that finishes cleanly leaves no trace, so the original error is
/// returned unchanged. One that cannot finish keeps a database record of what is
/// left: without it `-R` would have nothing to act on and the files would be
/// unreachable.
pub(super) fn abandon(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &mut InstallTransaction,
  err: CliError,
) -> CliError {
  let name = ctx.pkgname.to_string();
  app.warn_args(Key::InstallRollingBack, &[&name]);

  let leftovers = tx.rollback(app, ctx);
  if leftovers.is_empty() {
    app.warn_args(Key::InstallRolledBack, &[&name]);
    return err;
  }

  let count = leftovers.len().to_string();
  let paths = leftovers.join(" ");
  app.warn_args(Key::InstallRollbackLeftovers, &[&name, &count, &paths]);

  if let Err(record_err) = record_installation(app, ctx, precheck, tx, true).map(drop) {
    let message = record_err.message.clone();
    app.warn_args(Key::InstallRollbackRecordFailed, &[&name, &message]);
  }

  CliError::new(err.code, app.args(Key::InstallRollbackIncomplete, &[&err.message]))
}

/// Step 8: writes the completed install to the database.
pub(super) fn record_installation(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &InstallTransaction,
  failed: bool,
) -> CliResult<Vec<String>> {
  // The precheck object is stored verbatim, so the optional dependencies are an
  // extra key inside it: a reader that predates `?` sees the same kmod/base/app
  // arrays it always did, and ignores the new key.
  let mut report = precheck.report.clone();
  if !precheck.optional.is_empty() {
    report["optional"] = precheck.optional.report(if failed { "failed" } else { "installed" });
  }

  let mut manifest_stored = ctx.manifest.value.clone();
  // `source` describes how *this* install obtained the package, not where the
  // artifact was built, so it is overwritten instead of defaulted: a URL
  // download stays `url` even when the packer stamped `pica` into the manifest.
  manifest_stored["source"] = json!(ctx.source.as_str());
  // `pkgmgr` is the backend that actually owns the files. Recording the one in
  // use (rather than the artifact's declared value) keeps `-R` from driving a
  // package manager that never touched this install, as happens with
  // `--pkgmgr pica` over an `opkg` artifact.
  manifest_stored["pkgmgr"] = json!(app.backend().as_str());
  if manifest_stored.get("url").is_none() {
    let origin = manifest_stored.get("origin").and_then(Value::as_str).unwrap_or("").to_string();
    manifest_stored["url"] = json!(origin);
  }
  for key in ["branch", "protocol", "url", "luci_url", "luci_desc", "source_availability"] {
    if manifest_stored.get(key).is_none() {
      manifest_stored[key] = json!("");
    }
  }

  let payload = tx.merged_payload();
  let pkgfile = canonicalize_display(ctx.pkgfile);
  let mut db = read_db(&app.paths)?;
  // What the version being replaced wrote and this one does not: once this
  // record lands nothing names those paths any more, so they are handed back
  // to the caller to remove after the commit.
  let orphaned = if failed {
    Vec::new()
  } else {
    db.installed.get(ctx.pkgname).map_or_else(Vec::new, |previous| {
      let mut paths: Vec<String> = previous
        .owned_paths()
        .filter(|path| !payload.files.iter().any(|file| file == path))
        .map(ToString::to_string)
        .collect();
      paths.extend(previous.dirs.iter().filter(|dir| !payload.dirs.contains(dir)).cloned());
      paths
    })
  };
  match db.installed.get_mut(ctx.pkgname) {
    // The rollback has already put the previous version's files back when there
    // was one, so the entry that describes it must survive: only the paths this
    // install could not remove are added to it, keeping its manifest, `pkgfile`
    // and `installed_at`.
    Some(entry) if failed => entry.merge_paths(&payload.files, &payload.dirs),
    // A first install that fails leaves files no entry names, so they get one:
    // without it `-R` would have nothing to act on.
    _ => {
      db.installed.insert(
        ctx.pkgname.to_string(),
        new_entry(&manifest_stored, &pkgfile, &payload.files, &payload.dirs),
      );
    }
  }
  write_db(&app.paths, &db)?;

  // The database is what makes an install an install: `-Qi`, `-R` and `-U` read
  // it, and none of them reads the report. Writing the report second means a
  // report that cannot be written leaves a recorded install rather than a report
  // describing one the database never took; and since the install is already
  // recorded and its files are in place, that failure is a warning instead of a
  // reason to undo them.
  if let Err(err) = report_set_install_result(
    &app.paths,
    ctx.pkgname,
    ctx.selector,
    &ctx.manifest.value,
    &report,
    &tx.tx_added,
    &tx.app_added,
  ) {
    let message = err.message.clone();
    app.warn_args(Key::InstallReportFailed, &[ctx.pkgname, &message]);
  }

  // What Pica itself wrote, and only when Pica is the one that wrote it: a
  // native backend prints its own summary, and counting Pica's containers for it
  // would be a second, different answer to the same question.
  if !failed && !app.backend().requires_system_binary() && !payload.files.is_empty() {
    let summary = install_summary(app.lang, &payload);
    app.log_info(summary);
  }

  // This function is also the writer of a failed attempt's remnant record, and
  // that attempt has just been reported as a rollback. Saying "transaction
  // completed" on the way out would contradict the error printed right after it.
  if !failed {
    app.info(Key::TransactionCompleted);
  }

  Ok(orphaned)
}

/// Removes what the replaced version wrote and the new one does not, once the
/// new record is committed.
///
/// A path another pica record lists, or the system backend owns, is kept: it is
/// someone else's file now. A failure is a warning — the upgrade is already
/// complete, and the path is named so it can be removed by hand.
pub(super) fn remove_orphans(app: &mut App, pkgname: &str, orphaned: Vec<String>) {
  if orphaned.is_empty() {
    return;
  }
  let Ok(db) = read_db(&app.paths) else {
    return;
  };
  let mut files = Vec::new();
  let mut dirs = Vec::new();
  for path in orphaned {
    if db.owner_of(&path, pkgname).is_some()
      || app.package_manager().ok().and_then(|manager| manager.owns_file(&path)).is_some()
    {
      continue;
    }
    if std::path::Path::new(&path).is_dir() { dirs.push(path) } else { files.push(path) }
  }
  let outcome = remove_payload(&PayloadInstall { files: files.clone(), dirs, size: 0 });
  // The removal reports counts; a file still present is one it could not take.
  for path in files.iter().filter(|path| std::path::Path::new(path).symlink_metadata().is_ok()) {
    app.warn_args(Key::InstallOrphanKept, &[pkgname, path]);
  }
  if outcome.removed > 0 {
    let count = outcome.removed.to_string();
    app.info_args(Key::InstallOrphansRemoved, &[&count]);
  }
}
