//! The guard that keeps a payload out of Pica's own state.

use crate::app::{CliError, CliResult, E_PACKAGE_INVALID};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::system::TargetRoot;
use pica_pkg_core::layout::{ENV_DIR, SCRIPTS_DIR};
use std::fs;
use std::path::{Path, PathBuf};

/// Refuses a payload that would write Pica's own state.
///
/// `etc/pica/pica.json`, `var/lib/pica/db.json` and the rest are the files this
/// install is about to read and rewrite. A package that ships one of them would
/// otherwise be deployed like any other file: the database it replaced is the
/// one the commit then loads, and a rollback would put the package's copy back
/// instead of the record that was there.
///
/// The directories matter as much as the files. The lock lives in
/// `<state>/db.lck.d/pid`, and a payload that writes a pid there makes the lock
/// answer for a process that is not running, so the next `pica` reclaims it while
/// this install is still going. `<scripts>/<pkgname>/` holds the hooks `-R` and
/// `-U` run for a package, so a payload landing there replaces a hook nobody
/// asked it to replace. Nothing legitimate installs any of these paths — a
/// package's own hooks and env file reach the device through `scripts/`, never
/// through `root/`.
///
/// Every path is checked where it *lands*, not where it is spelled. A payload
/// naming `usr/lib/demo/db.json` writes the database when `usr/lib/demo` is a
/// symlink to the state directory, and it lands on `install-report.json` when it
/// plants `opt/alias -> ../var/lib/pica` first; comparing the spelled path would
/// miss both. So the components above the entry are resolved through the symlinks
/// that are already there, the entry's own link target is resolved too, and a path
/// whose location cannot be established — an unreadable link, a loop, one that
/// leads out of the target root — refuses the payload before anything is written
/// rather than being guessed at. None of this depends on the order the walk takes
/// the entries in: the checks are about the destination as a whole.
///
/// This is a collision guard, not a privilege boundary. A lifecycle hook is
/// already arbitrary code running as root and can write wherever root can; what
/// this stops is a package's *payload* quietly overwriting the files this
/// transaction is reading, the lock it holds, or another package's hooks.
pub(super) fn refuse_pica_state(
  paths: &crate::app::Paths,
  target: &TargetRoot,
  root_dir: &Path,
) -> CliResult<()> {
  let protected = [
    (Key::GuardConfigFile, paths.conf_file.as_path()),
    (Key::GuardDatabase, paths.db_file.as_path()),
    (Key::GuardIndex, paths.index_file.as_path()),
    (Key::GuardInstallReport, paths.report_file.as_path()),
    (Key::GuardLockFile, paths.lock_file.as_path()),
  ];

  let mut lock_dir = paths.lock_file.as_os_str().to_os_string();
  lock_dir.push(".d");
  let lock_dir = PathBuf::from(lock_dir);
  let scripts_dir = target.resolve(SCRIPTS_DIR);
  let env_dir = target.resolve(ENV_DIR);
  // Compared by component, so `/var/lib/pica` does not claim `/var/lib/pica-old`.
  // The narrowest directory comes first so the message names the thing that is
  // actually at risk: the lock directory is inside the state directory, and the
  // kept scripts are not described by "a directory Pica owns".
  let protected_dirs = [
    (Key::GuardLockDir, lock_dir.as_path()),
    (Key::GuardKeptScripts, scripts_dir.as_path()),
    (Key::GuardEnvFragments, env_dir.as_path()),
    (Key::GuardStateDir, paths.state_dir.as_path()),
    (Key::GuardConfigDir, paths.etc_dir.as_path()),
  ];

  let mut nodes = Vec::new();
  payload_nodes(root_dir, root_dir, &mut nodes);

  for (relative, node) in &nodes {
    // A directory entry is a no-op when a directory is already there, and makes an
    // empty one otherwise: it overwrites nothing, and everything written *below* it
    // comes through as a file or a link and is checked on its own. Checking it here
    // would report the directory holding Pica's database instead of the database.
    if matches!(node, PayloadNode::Dir) {
      continue;
    }

    // Where the entry itself is written. `deploy_file` renames over the name and
    // `copy_symlink` clears it first, so neither follows a symlink standing at the
    // last component; the components above it are another matter, and they are the
    // ones this resolves.
    let parent = relative.parent().unwrap_or(Path::new(""));
    // `None` here means the parent chain leaves the target root. Pica's files are
    // inside it, so there is nothing this entry could overwrite that the walk is
    // about to depend on — and the walk refuses a payload that leads out of the
    // root before it writes the link in any case.
    let Some(landing_parent) = resolve_in_root(target, parent, relative)? else {
      continue;
    };
    let mut landing = landing_parent.clone();
    if let Some(name) = relative.file_name() {
      landing.push(name);
    }
    refuse_protected(&protected, &protected_dirs, &landing)?;

    // A symlink is a name the next install would write through, so it is refused
    // when it *points* into Pica's state, not only when it sat there. Directories
    // need no check of their own: one that lands on a directory is a no-op, and one
    // that lands nowhere only makes an empty directory, while everything written
    // *below* it is a file or a link and is checked as such — which is also what
    // keeps the message naming the database rather than the directory holding it.
    if let PayloadNode::Link(points_at) = node {
      let resolved = pica_pkg_core::io::resolve_link(target.as_path(), &landing_parent, points_at)
        .map_err(|err| {
          CliError::new(
            E_PACKAGE_INVALID,
            tr(Key::GuardSymlinkUnknown, &[&relative.display(), &err]),
          )
        })?;
      if let Some(resolved) = resolved {
        refuse_protected(&protected, &protected_dirs, &resolved)?;
      }
    }
  }

  Ok(())
}

/// Where `relative` would land under `root_dir`, following the symlinks already
/// standing in its way.
///
/// Refusing is the answer to every question this cannot answer: a link that cannot
/// be read, one that leads out of the target root, and one that never settles. A
/// guard that guessed here would be guarding the wrong path.
pub(super) fn resolve_in_root(
  target: &TargetRoot,
  relative: &Path,
  spelled: &Path,
) -> CliResult<Option<PathBuf>> {
  pica_pkg_core::io::resolve_in_root(target.as_path(), relative).map_err(|err| {
    CliError::new(E_PACKAGE_INVALID, tr(Key::GuardPathUnknown, &[&spelled.display(), &err]))
  })
}

pub(super) fn refuse_protected(
  files: &[(Key, &Path)],
  dirs: &[(Key, &Path)],
  landing: &Path,
) -> CliResult<()> {
  if let Some((label, _)) = files.iter().find(|(_, guard)| *guard == landing) {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      tr(Key::GuardPicaState, &[&tr(*label, &[]), &landing.display()]),
    ));
  }
  if let Some((label, _)) = dirs.iter().find(|(_, dir)| landing.starts_with(dir)) {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      tr(Key::GuardPicaState, &[&tr(*label, &[]), &landing.display()]),
    ));
  }
  Ok(())
}

/// One entry of a `root/` payload, as the walk sees it.
pub(super) enum PayloadNode {
  /// A directory, merged into whatever is at the destination.
  Dir,
  /// A regular file, put in place by a rename over the destination's name.
  File,
  /// A symlink, created at the destination's name after clearing it. What it
  /// points at is kept: that is the path a later write would take.
  Link(PathBuf),
}

/// Every path the payload would write, whether or not something is there now.
///
/// [`intended_payload`] answers a different question — which paths the walk can
/// create — so a payload that overwrites Pica's database is invisible to it for
/// the very reason that matters: the database already exists.
///
/// The paths are relative to `base` on purpose: where they land is not a matter
/// of string concatenation, because the target may have symlinks a component
/// passes through. Resolving them is the caller's job, once, against a real
/// filesystem.
pub(super) fn payload_nodes(base: &Path, dir: &Path, nodes: &mut Vec<(PathBuf, PayloadNode)>) {
  let Ok(entries) = fs::read_dir(dir) else {
    return;
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
      nodes.push((relative.to_path_buf(), PayloadNode::Dir));
      payload_nodes(base, &path, nodes);
    } else if metadata.is_symlink() {
      let Ok(link) = fs::read_link(&path) else {
        continue;
      };
      nodes.push((relative.to_path_buf(), PayloadNode::Link(link)));
    } else if metadata.is_file() {
      nodes.push((relative.to_path_buf(), PayloadNode::File));
    }
  }
}
