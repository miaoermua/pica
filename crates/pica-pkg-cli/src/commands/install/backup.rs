//! Copies of what an install replaces, and putting them back.

use crate::app::{App, CliError, CliResult, E_IO};
use crate::i18n::Key;
use crate::i18n::tr;
use pica_pkg_core::io::{copy_symlink, deploy_file};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Names the copies inside one transaction's backup directory. Process-wide so
/// that two transactions — two packages in one run, or two `pica` processes on
/// one device — cannot pick the same name for different files.
///
/// `usize` rather than `u64`: 32-bit MIPS has no 64-bit atomics, and a store
/// never holds anywhere near `u32::MAX` copies.
pub(super) static BACKUP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// What a transaction keeps of the files it is about to replace.
///
/// A payload file goes in by renaming a draft over the destination: atomic, and
/// final — the previous content is unreachable the moment the rename happens.
/// This store is the copy taken *before* the walk starts, so a deploy that fails
/// partway puts the device back the way it found it instead of leaving this
/// artifact's bytes in a file the transaction never finished.
///
/// Three things it deliberately is not:
///
/// - **Not a journal.** It lives in this process and names no state a later run
///   can resume. A process killed mid-deploy leaves both the device's files and
///   the copies exactly where they are, and nothing across runs reads them: the
///   guarantee is *in-process* rollback, not crash recovery.
/// - **Not the payload.** Nothing here is recorded as an installed file, so the
///   database list, the size accounting and what `-R` may delete are unchanged.
///   The copies live under Pica's own cache directory, not inside the target
///   rootfs they came from.
/// - **Not a general-purpose archiver.** Only a path the walk is about to
///   destroy gets a copy, and only in the three forms the walk can destroy —
///   see [`PayloadBackup::back_up`] for what is refused instead.
pub(super) struct PayloadBackup {
  /// `<cache>/backup`, where this store's own directory is made.
  pub(super) parent: PathBuf,
  /// Names which package the copies belong to.
  pub(super) prefix: String,
  /// Made with the first file that needs storing, so an install that replaces
  /// nothing — the common first install — creates no directory at all.
  /// `tempfile` gives it a name no other transaction can pick, and Pica asks it
  /// for mode `0700` wherever the platform has a mode to ask for: the copies are
  /// the device's own files and one of them may be a configuration file only
  /// root may read. The copies themselves are `0600` by the way they are made.
  pub(super) dir: Option<tempfile::TempDir>,
  pub(super) entries: Vec<BackupEntry>,
}

/// One destination kept as it was, and how to put it back.
pub(super) struct BackupEntry {
  pub(super) destination: PathBuf,
  pub(super) kind: BackupKind,
  /// Whether the payload walk is what writes this destination. Only such a file
  /// can be skipped on the way back: the walk replaces it in one rename, so a walk
  /// that never reached it left it exactly as the copy found it.
  pub(super) walked: bool,
}

pub(super) enum BackupKind {
  /// A regular file. Its bytes are at `stored` and `permissions` is the mode it
  /// had; a restore puts both back with one rename, the same way a deploy does.
  File { stored: PathBuf, permissions: fs::Permissions },
  /// A symlink, kept as a symlink rather than as what it points at.
  Link { stored: PathBuf },
  /// A directory that held nothing. The walk only replaces one after removing
  /// it, so putting it back is removing what took its place and making the
  /// directory again, with the mode it had — a directory Pica did not create is
  /// not one Pica gets to re-permission, and `mkdir` would apply the umask.
  EmptyDir { permissions: fs::Permissions },
}

impl PayloadBackup {
  pub(super) fn new(app: &App, pkgname: &str) -> Self {
    Self {
      parent: app.paths.cache_dir.join("backup"),
      prefix: format!("{pkgname}-"),
      dir: None,
      entries: Vec::new(),
    }
  }

  /// Keeps `destination` as it stands right now, before anything can replace it.
  ///
  /// A path that does not exist yet needs no copy: this install created it, so
  /// removing it is the whole undo, and a copy of it would be a file the record
  /// does not name. A directory the walk cannot empty needs no copy either — it
  /// survives, which is what a non-empty one deserves.
  ///
  /// Anything that is neither a file, a symlink nor a directory is refused here
  /// rather than replaced: a socket, a fifo or a device node cannot be recreated
  /// from a copy of its bytes, and this is the last moment at which refusing
  /// costs nothing — after the walk starts, the destination is already gone.
  pub(super) fn back_up(&mut self, destination: &Path, metadata: &fs::Metadata) -> CliResult<()> {
    let kind = if metadata.is_symlink() {
      BackupKind::Link { stored: self.copy_link(destination)? }
    } else if metadata.is_file() {
      BackupKind::File { stored: self.copy_file(destination)?, permissions: metadata.permissions() }
    } else if metadata.is_dir() {
      if dir_has_entries(destination)? {
        return Ok(());
      }
      BackupKind::EmptyDir { permissions: metadata.permissions() }
    } else {
      return Err(CliError::new(E_IO, tr(Key::BackupUnsupportedKind, &[&destination.display()])));
    };

    self.entries.push(BackupEntry {
      destination: destination.to_path_buf(),
      kind,
      // Only `snapshot_payload` sets this: everything else here is a file this
      // install writes itself, and a copy of it may be the only version left even
      // if the write failed.
      walked: false,
    });
    Ok(())
  }

  /// Takes the copies out, deepest path first, and puts each one back.
  ///
  /// Reverse order is the same rule `remove_payload` follows for directories: a
  /// path that has to be cleared before it can become a directory again is
  /// handled before its parent is made. Restoring is done by the same primitives
  /// as deploying — one file by rename, one link by recreating the link — so the
  /// undo of a deploy is not a second, different deploy, and a restore that
  /// already happened is a no-op rather than a hazard: every path left alone is
  /// still the path the entry names.
  ///
  /// The copies are kept when one of them cannot go back: they are the only copy
  /// of what the device had, so the paths that failed are returned for the caller
  /// to name, and the directory stays where the message says it is.
  #[cfg(test)]
  pub(super) fn restore(&mut self) -> Vec<String> {
    self.restore_reaching(|_| true)
  }

  /// The same restore, but only for destinations the walk got as far as writing.
  ///
  /// A copy taken of a file the walk never reached is a copy of a file that is
  /// still exactly as it was: putting it back is a rewrite with the same bytes —
  /// a new inode, broken hard links, a fresh mtime, and a full read and write of a
  /// file that may be large — with nothing to show for it. `deployed` is what the
  /// walk itself reported writing, so a file it did not reach is simply dropped
  /// from the record.
  ///
  /// Two kinds are restored whether or not the walk reported them. `deploy_file`
  /// and `copy_symlink` clear the destination before they make the new thing, so a
  /// deploy that fails after clearing leaves the name *missing*, and the entry is
  /// the only way back. Neither is expensive to redo: a link is one syscall, and an
  /// empty directory that is still a directory is the no-op case above.
  pub(super) fn restore_replacing(&mut self, deployed: &HashSet<PathBuf>) -> Vec<String> {
    let reached = |entry: &BackupEntry| match &entry.kind {
      // Only a destination the walk replaced atomically can be left alone. A file
      // Pica wrote itself — an env file or a kept script — is copied into place,
      // so a copy that failed may have left it truncated: it always goes back.
      BackupKind::File { .. } => !entry.walked || deployed.contains(&entry.destination),
      BackupKind::Link { .. } | BackupKind::EmptyDir { .. } => true,
    };
    self.restore_reaching(reached)
  }

  pub(super) fn restore_reaching(&mut self, reached: impl Fn(&BackupEntry) -> bool) -> Vec<String> {
    let mut failed = Vec::new();

    for entry in self.entries.iter().rev() {
      if !reached(entry) {
        continue;
      }
      let result = match &entry.kind {
        BackupKind::File { stored, permissions } => {
          ensure_parent(&entry.destination).and_then(|()| {
            deploy_file(stored, &entry.destination, permissions).map_err(CliError::from)
          })
        }
        BackupKind::Link { stored } => ensure_parent(&entry.destination).and_then(|()| {
          let _ = remove_any(&entry.destination);
          copy_symlink(stored, &entry.destination).map_err(CliError::from)
        }),
        BackupKind::EmptyDir { permissions } => {
          // A directory that is still a directory is what this entry kept. The
          // walk may have put files inside it, and those are the record's to
          // remove; recreating it here would only fail on a directory that is
          // not empty and report a path that never went missing.
          let still_a_directory =
            fs::symlink_metadata(&entry.destination).is_ok_and(|existing| existing.is_dir());
          if still_a_directory {
            Ok(())
          } else {
            remove_any(&entry.destination)
              .and_then(|()| fs::create_dir_all(&entry.destination))
              .and_then(|()| fs::set_permissions(&entry.destination, permissions.clone()))
              .map_err(|err| {
                CliError::new(
                  E_IO,
                  tr(Key::BackupRemakeDirFailed, &[&entry.destination.display(), &err]),
                )
              })
          }
        }
      };

      if let Err(err) = result {
        failed.push(format!("{} ({})", entry.destination.display(), err.message));
      }
    }

    if failed.is_empty() {
      self.entries.clear();
      self.dir = None;
    }
    failed
  }

  /// Hands the directory over and stops `tempfile` from cleaning it up
  /// (`TempDir::keep`), returning where it is.
  ///
  /// Whoever calls this decides what the copies are worth: a committed install
  /// removes the directory it gets back — and says so when it cannot — while a
  /// rollback that could not put everything back keeps it, because it is the only
  /// copy of what the device had. A restore that finished cleared the directory
  /// already, so this returns `None` there.
  pub(super) fn release(mut self) -> Option<PathBuf> {
    self.dir.take().map(tempfile::TempDir::keep)
  }

  pub(super) fn copy_file(&mut self, destination: &Path) -> CliResult<PathBuf> {
    let dir = self.dir()?;
    let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    let stored = dir.join(format!("file-{seq}"));

    let mut draft = tempfile::NamedTempFile::new_in(&dir)
      .map_err(|err| write_back_up_failure(destination, &stored, err))?;
    let copied =
      fs::File::open(destination).and_then(|mut from| std::io::copy(&mut from, &mut draft));
    if let Err(err) = copied {
      return Err(read_back_up_failure(destination, &err));
    }
    // The draft is `0600` and owner-only, which is the point: the mode here is a
    // secret, not a permission bit, so the copy stays unreadable even when the
    // file on the device was world-readable. The mode that goes *back* onto the
    // device is the one kept in the entry.
    draft.persist(&stored).map_err(|err| write_back_up_failure(destination, &stored, err.error))?;
    Ok(stored)
  }

  pub(super) fn copy_link(&mut self, destination: &Path) -> CliResult<PathBuf> {
    let stored = self.next_path("link")?;
    // The destination's *own* link is kept, not what it points at, and
    // `fs::symlink` refuses a name that is already taken: a file planted at the
    // name can only make this fail, never be written through.
    copy_symlink(destination, &stored)
      .map_err(|err| write_back_up_failure(destination, &stored, err))?;
    Ok(stored)
  }

  /// A name in the backup directory for the next copy.
  pub(super) fn next_path(&mut self, label: &str) -> CliResult<PathBuf> {
    let dir = self.dir()?;
    let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("{label}-{seq}")))
  }

  pub(super) fn dir(&mut self) -> CliResult<PathBuf> {
    if let Some(dir) = &self.dir {
      return Ok(dir.path().to_path_buf());
    }

    fs::create_dir_all(&self.parent)
      .map_err(|err| CliError::new(E_IO, tr(Key::MkdirFailed, &[&self.parent.display(), &err])))?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(&self.prefix);
    // The copies are the device's own files, and one of them may be a
    // configuration file holding a secret, so the store is asked for owner-only
    // — a mode set *at creation*, so there is no moment in which it is not. A
    // platform with no mode to set keeps its default, and what is inside is
    // still `0600`.
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let made = builder.tempdir_in(&self.parent).map_err(|err| {
      CliError::new(E_IO, tr(Key::BackupStoreFailed, &[&self.parent.display(), &err]))
    })?;
    let path = made.path().to_path_buf();
    self.dir = Some(made);
    Ok(path)
  }
}

/// Whether `dir` holds anything, including a broken symlink.
/// Makes the directory a restored path lives in, when the install removed it:
/// an upgrade clears the kept scripts' directory as a whole.
pub(super) fn ensure_parent(path: &Path) -> CliResult<()> {
  match path.parent() {
    Some(parent) if !parent.exists() => fs::create_dir_all(parent).map_err(|err| {
      CliError::new(E_IO, tr(Key::BackupRecreateFailed, &[&parent.display(), &err]))
    }),
    _ => Ok(()),
  }
}

pub(super) fn back_up_tree_into(
  backup: &mut PayloadBackup,
  dir: &Path,
  kept: &mut HashSet<PathBuf>,
) -> CliResult<()> {
  let entries = fs::read_dir(dir).map_err(|err| read_back_up_failure(dir, &err))?;
  for entry in entries {
    let path = entry.map_err(|err| read_back_up_failure(dir, &err))?.path();
    let metadata = fs::symlink_metadata(&path).map_err(|err| read_back_up_failure(&path, &err))?;
    if metadata.is_dir() {
      back_up_tree_into(backup, &path, kept)?;
    } else {
      backup.back_up(&path, &metadata)?;
      kept.insert(path);
    }
  }
  Ok(())
}

pub(super) fn dir_has_entries(dir: &Path) -> CliResult<bool> {
  let mut entries = fs::read_dir(dir)
    .map_err(|err| CliError::new(E_IO, tr(Key::ReadFailed, &[&dir.display(), &err])))?;
  Ok(entries.next().is_some())
}

/// Takes `path` away whether it is a file, a link or a directory, and reports
/// only what is still standing afterwards: a path that is already gone was not
/// this install's to keep.
pub(super) fn remove_any(path: &Path) -> std::io::Result<()> {
  if fs::symlink_metadata(path).is_err() {
    return Ok(());
  }

  match fs::remove_file(path).or_else(|_| fs::remove_dir(path)) {
    Ok(()) => Ok(()),
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(err) => Err(err),
  }
}

pub(super) fn read_back_up_failure(destination: &Path, err: &std::io::Error) -> CliError {
  CliError::new(E_IO, tr(Key::BackupReadFailed, &[&destination.display(), &err]))
}

pub(super) fn write_back_up_failure(
  destination: &Path,
  stored: &Path,
  err: impl std::fmt::Display,
) -> CliError {
  CliError::new(E_IO, tr(Key::BackupCopyFailed, &[&destination.display(), &stored.display(), &err]))
}
