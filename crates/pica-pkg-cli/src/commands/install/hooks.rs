//! Step 6: lifecycle hooks and the scripts a package keeps.

use super::payload::push_missing_ancestors;
use super::pipeline::{InstallContext, ensure_dir};
use super::transaction::InstallTransaction;
use crate::app::{App, CliError, CliResult, E_IO, E_PACKAGE_INVALID};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::system::{TargetRoot, run_hook};
use pica_pkg_core::io::PayloadInstall;
use pica_pkg_core::layout::SCRIPTS_DIR;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

pub(super) fn kept_scripts_dir(target: &TargetRoot, pkgname: &str) -> PathBuf {
  target.resolve(SCRIPTS_DIR).join(pkgname)
}

pub(super) fn execute_scripts(
  app: &mut App,
  ctx: &InstallContext<'_>,
  tx: &mut InstallTransaction,
) -> CliResult<()> {
  let scripts = ctx.scripts;
  let (label, hook) = ctx.lifecycle_hook();
  if !hook.is_empty() {
    // Recorded before it runs: a hook that dies halfway has still run, and the
    // package's own `remove` hook is the only thing that can undo what it did.
    tx.record_hook(label);
    run_hook(app, ctx.tmpdir, hook, label)?;
  }

  let target_dir = kept_scripts_dir(ctx.target, ctx.pkgname);
  let dir_existed = target_dir.is_dir();

  // An upgrade replaces the kept scripts as a set instead of merging into them,
  // so a script the new version no longer keeps does not linger. The old ones go
  // into the transaction's backup first — the same store, and the same rollback,
  // as every file the payload replaces — and only then is the directory cleared.
  // A path in `previous` is put back by the backup, so it must never also be
  // recorded as created: rollback would delete it again after restoring it.
  let previous = if dir_existed {
    let previous = tx.back_up_tree(app, ctx.pkgname, &target_dir)?;
    fs::remove_dir_all(&target_dir)
      .map_err(|err| CliError::new(E_IO, tr(Key::ClearFailed, &[&target_dir.display(), &err])))?;
    previous
  } else {
    HashSet::new()
  };

  let mut items: Vec<(&str, bool)> = Vec::new();
  if !scripts.install.is_empty() && !scripts.install.starts_with('/') {
    items.push((&scripts.install, scripts.keep_all || scripts.keep_install));
  }
  if !scripts.update.is_empty() && !scripts.update.starts_with('/') {
    items.push((&scripts.update, scripts.keep_all || scripts.keep_update));
  }
  if !scripts.remove.is_empty() && !scripts.remove.starts_with('/') {
    items.push((&scripts.remove, scripts.keep_all || scripts.keep_remove));
  }

  let mut kept_any = false;
  for (relative, keep) in &items {
    let source = ctx.tmpdir.join(relative);
    if !source.is_file() {
      return Err(CliError::new(E_PACKAGE_INVALID, tr(Key::ScriptNotFound, &[&relative])));
    }

    if !*keep {
      continue;
    }

    let target = target_dir.join(relative);
    let mut created = PayloadInstall::default();
    push_missing_ancestors(ctx.target, &target, &mut created);
    // Directories are recorded only when the kept-scripts directory did not
    // exist before this install: an upgrade must not turn a pre-existing
    // directory into something `-R` may delete later. A rollback of an upgrade
    // removes the whole directory and renames the backup over it instead.
    if !dir_existed {
      tx.record_created(created);
    }

    if let Some(parent) = target.parent() {
      ensure_dir(parent)?;
    }

    let path = target.display().to_string();
    match fs::copy(&source, &target) {
      Ok(_) => {
        if !previous.contains(&target) {
          tx.record_created(PayloadInstall { files: vec![path], ..PayloadInstall::default() });
        }
        kept_any = true;
      }
      Err(err) => {
        // A copy opens the destination first, so a failed one can leave a file
        // behind. It is this install's own unless the backup is putting an old
        // file back at the same path.
        if !previous.contains(&target) && target.symlink_metadata().is_ok() {
          tx.record_created(PayloadInstall { files: vec![path], ..PayloadInstall::default() });
        }
        return Err(CliError::new(E_IO, tr(Key::CopyScriptFailed, &[&err])));
      }
    }
  }

  if !kept_any && target_dir.is_dir() {
    let _ = fs::remove_dir_all(&target_dir);
  }

  Ok(())
}
