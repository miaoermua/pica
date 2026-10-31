//! Step 7: laying `root/` and the env file onto the target.

use super::backup::PayloadBackup;
use super::guard::refuse_pica_state;
use super::hooks::kept_scripts_dir;
use super::pipeline::{InstallContext, ensure_dir};
use super::transaction::InstallTransaction;
use crate::app::{App, CliError, CliResult, E_IO};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::system::TargetRoot;
use pica_pkg_core::io::{PayloadInstall, install_payload_with};
use pica_pkg_core::layout::{ENV_DIR, ROOT, SCRIPTS};
use std::fs;
use std::path::Path;

/// Step 7: installs the artifact's declarative payload, recording every path it
/// creates in `tx` so removal can undo exactly this install.
///
/// `root/` is mirrored onto the target rootfs (so `root/etc/init.d/demo` becomes
/// `/etc/init.d/demo`), while kept lifecycle scripts and the env file are copied
/// into Pica's own directories.
pub(super) fn deploy_payload(
  app: &App,
  ctx: &InstallContext<'_>,
  tx: &mut InstallTransaction,
) -> CliResult<()> {
  let target = ctx.target;
  let mut payload = PayloadInstall::default();

  let root_dir = ctx.tmpdir.join(ROOT);
  // Before the copies, not merely before the walk: a refusal that happened after
  // the snapshot would leave every replaced file copied out and rewritten back on
  // the way out, so "nothing on the device has changed" would be false for a file
  // whose bytes and inode quietly did.
  refuse_pica_state(&app.paths, target, &root_dir)?;
  // The copies go first, and the store exists even for a payload that turns out
  // to replace nothing: the env file below is a destination too.
  tx.prepare_payload_backup(app, ctx.pkgname, target, &root_dir)?;
  if root_dir.is_dir() {
    // Enumerated *before* the walk: only a path that does not exist yet can be
    // one the walk creates, and only such a path may be removed again.
    let intended = intended_payload(target, &root_dir);
    // The walk records into `walked` as it goes, so a walk that stops on an error
    // still says which destinations it got to — which is exactly what the rollback
    // needs, and what a plain `Result` throws away.
    let mut walked = PayloadInstall::default();
    let walked_error = install_payload_with(&root_dir, target.as_path(), &mut walked).err();
    tx.note_deployed(&walked);
    match walked_error {
      None => {
        let installed = walked;
        // Recorded here, not at the end of this function: everything below can
        // fail, and a failure there must not lose the files that are already on
        // disk. Only the paths this walk created may be removed again — the
        // result also lists the files it replaced, and deleting one of those
        // would take away a path that existed before the install. `intended`
        // holds the paths that were missing before the walk, so the intersection
        // is exactly what this install created; the ones it replaced go back
        // from the backup instead.
        tx.record_created(PayloadInstall {
          files: installed
            .files
            .iter()
            .filter(|file| intended.files.contains(file))
            .cloned()
            .collect(),
          // The walker only pushes the directories it had to create.
          dirs: installed.dirs.clone(),
          size: installed.size,
        });
        payload.files.extend(installed.files);
        payload.dirs.extend(installed.dirs);
        payload.size += installed.size;
      }
      Some(err) => {
        // `install_payload` validates as it copies, so a payload it rejects can
        // still have created files before it got there. The paths that are on disk now are recorded:
        // every one of them was missing before the walk, so every one of them is
        // this install's own to remove, and a path that turns out not to have
        // been reached is left out rather than claimed. What the walk *did*
        // replace is not in this list at all — the backup puts that back.
        let landed = existing_only(intended);
        tx.record_payload(landed.clone());
        tx.record_created(landed);
        return Err(CliError::from(err));
      }
    }
  }

  let env_file = ctx.tmpdir.join(SCRIPTS).join(".env");
  if env_file.is_file() {
    let env_dir = target.resolve(ENV_DIR);
    let file = env_dir.join(format!("{}.env", ctx.pkgname));
    // A directory this install had to create is its own; recording it before
    // the copy means a copy that fails does not leave it behind. Directories
    // that already existed are not recorded, and `remove_payload` only removes
    // a recorded one when it ends up empty.
    let mut created_dirs = PayloadInstall::default();
    push_missing_ancestors(target, &file, &mut created_dirs);
    ensure_dir(&env_dir)?;
    tx.record_created(created_dirs);

    let existed = file.symlink_metadata().is_ok();
    if existed {
      // The env file is Pica's own metadata, but a package replacing it is the
      // same destruction as replacing a payload file: the operator's copy goes
      // into the same store and comes back from the same rollback.
      tx.back_up_destination(&file)?;
    }
    let written = copy_recording_new_target(tx, &file, existed, || fs::copy(&env_file, &file))
      .map_err(|err| CliError::new(E_IO, tr(Key::CopyEnvFailed, &[&err])))?;
    payload.files.push(file.display().to_string());
    payload.size += written;
  }

  let scripts_dir = kept_scripts_dir(target, ctx.pkgname);
  if scripts_dir.is_dir() {
    payload.dirs.push(scripts_dir.display().to_string());
    if let Ok(entries) = fs::read_dir(&scripts_dir) {
      for entry in entries.flatten() {
        if entry.path().is_file() {
          // Kept scripts are Pica's own files, so their bytes belong in what
          // this install wrote — the payload walk did not count them.
          payload.size += entry.metadata().map_or(0, |meta| meta.len());
          payload.files.push(entry.path().display().to_string());
        }
      }
    }
  }

  tx.record_payload(payload);

  Ok(())
}

/// Every path a `root/` payload could still create, mirroring the walker's
/// mapping without copying anything.
///
/// Only used to make a *failed* deploy reversible, and only for paths that do
/// not exist yet: those are the ones the walk can create, so they are the ones
/// its failure can have half-created. A path that is already there is left out
/// on purpose — the walk may never have reached it, and removing a file or a
/// directory this install did not create is destruction the rollback has no
/// right to perform. Only the paths of this list that are actually on disk when
/// the walk fails are recorded; see [`existing_only`].
///
/// The other half of a failed deploy — a pre-existing file the walk overwrote
/// before it failed — is not this list's business at all. [`PayloadBackup`] took
/// a copy of it before the walk started, and `rollback` puts that back.
pub(super) fn intended_payload(target: &TargetRoot, root_dir: &Path) -> PayloadInstall {
  let mut payload = PayloadInstall::default();
  collect_intended(target, root_dir, root_dir, &mut payload);
  payload
}

/// What the payload would write, for the plan: how many entries other than
/// directories, and the bytes of the regular files among them.
pub(super) fn payload_extent(root_dir: &Path) -> (usize, u64) {
  fn walk(dir: &Path, count: &mut usize, bytes: &mut u64) {
    let Ok(entries) = fs::read_dir(dir) else {
      return;
    };
    for entry in entries.flatten() {
      let path = entry.path();
      let Ok(meta) = path.symlink_metadata() else {
        continue;
      };
      if meta.is_dir() {
        walk(&path, count, bytes);
      } else {
        *count += 1;
        if meta.is_file() {
          *bytes += meta.len();
        }
      }
    }
  }
  let (mut count, mut bytes) = (0, 0);
  walk(root_dir, &mut count, &mut bytes);
  (count, bytes)
}

/// Copies a file, and keeps the transaction honest when the copy dies after
/// creating its destination.
///
/// A copy truncates and writes the target, so a failure partway leaves a file
/// behind that no `Ok` value will ever describe. Only a destination that did
/// not exist before the copy is this install's to remove; one that did is left
/// where it is, and its previous content comes back from the backup the
/// transaction took before the copy started.
///
/// The copy itself is a parameter so a test can fail it at the one point a real
/// filesystem cannot be made to fail on demand.
pub(super) fn copy_recording_new_target(
  tx: &mut InstallTransaction,
  to: &Path,
  existed: bool,
  copy: impl FnOnce() -> std::io::Result<u64>,
) -> std::io::Result<u64> {
  match copy() {
    Ok(written) => Ok(written),
    Err(err) => {
      if !existed && to.symlink_metadata().is_ok() {
        tx.record_created(PayloadInstall {
          files: vec![to.display().to_string()],
          ..PayloadInstall::default()
        });
      }
      Err(err)
    }
  }
}

pub(super) fn collect_intended(
  target: &TargetRoot,
  base: &Path,
  dir: &Path,
  payload: &mut PayloadInstall,
) {
  let Ok(entries) = fs::read_dir(dir) else {
    return;
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(relative) = path.strip_prefix(base) else {
      continue;
    };
    let to = target.as_path().join(relative);

    let Ok(metadata) = fs::symlink_metadata(&path) else {
      continue;
    };

    // The walker calls `create_dir_all` for a file's parent, so a missing
    // ancestor is created even when the payload has no entry for it. Walking up
    // from the root records those in the order removal expects (parents before
    // children) instead of leaving the rollback to trip over them.
    push_missing_ancestors(target, &to, payload);

    let missing = fs::symlink_metadata(&to).is_err();
    if metadata.is_dir() {
      if missing {
        payload.dirs.push(to.display().to_string());
      }
      collect_intended(target, base, &path, payload);
    } else if missing {
      payload.files.push(to.display().to_string());
    }
  }
}

/// Adds every ancestor of `path` that does not exist yet, outermost first.
pub(super) fn push_missing_ancestors(
  target: &TargetRoot,
  path: &Path,
  payload: &mut PayloadInstall,
) {
  let root = target.as_path();
  let Ok(relative) = path.strip_prefix(root) else {
    return;
  };

  let mut current = root.to_path_buf();
  for component in relative.components().take(relative.components().count().saturating_sub(1)) {
    current.push(component);
    let missing = fs::symlink_metadata(&current).is_err();
    let text = current.display().to_string();
    if missing && !payload.dirs.contains(&text) {
      payload.dirs.push(text);
    }
  }
}

/// Copies out everything this `root/` walk is about to replace, before it
/// replaces any of it.
///
/// The traversal mirrors [`collect_intended`] — the same mapping the deploy
/// makes — on purpose: a backup taken of the wrong path restores the wrong file.
/// A destination that is not there yet is skipped, because this install owns that
/// one and removing it is the whole undo.
pub(super) fn snapshot_payload(
  target: &TargetRoot,
  base: &Path,
  dir: &Path,
  backup: &mut PayloadBackup,
) -> CliResult<()> {
  let Ok(entries) = fs::read_dir(dir) else {
    return Ok(());
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let Ok(relative) = path.strip_prefix(base) else {
      continue;
    };
    let Ok(metadata) = fs::symlink_metadata(&path) else {
      continue;
    };

    if metadata.is_dir() {
      snapshot_payload(target, base, &path, backup)?;
      continue;
    }
    // Only a file or a symlink puts something at its own destination; anything
    // else in the payload the walker ignores, so nothing of the device's is at
    // risk for it.
    if !metadata.is_file() && !metadata.is_symlink() {
      continue;
    }

    let to = target.as_path().join(relative);
    let Ok(existing) = fs::symlink_metadata(&to) else {
      continue;
    };
    backup.back_up(&to, &existing)?;
    if let Some(entry) = backup.entries.last_mut() {
      entry.walked = true;
    }
  }

  Ok(())
}

/// The paths of a payload that are actually on disk.
///
/// `install_payload` reports nothing when it fails, so a deploy that died
/// partway is described only by looking: of the paths this install could still
/// create, the ones that are there are the ones it did create — and the only
/// ones its rollback has a right to remove.
pub(super) fn existing_only(payload: PayloadInstall) -> PayloadInstall {
  let present = |path: &String| Path::new(path).symlink_metadata().is_ok();
  PayloadInstall {
    files: payload.files.into_iter().filter(present).collect(),
    dirs: payload.dirs.into_iter().filter(present).collect(),
    size: 0,
  }
}
