use crate::error::{PicaError, PicaResult};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[must_use]
pub fn now_unix_secs() -> u64 {
  SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs())
}

#[must_use]
pub fn now_unix_nanos() -> u128 {
  SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_nanos())
}

/// # Errors
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn read_json_file(path: &Path) -> PicaResult<serde_json::Value> {
  let content = fs::read_to_string(path)?;
  Ok(serde_json::from_str(&content)?)
}

/// # Errors
/// Returns an error if serialization fails or the file cannot be written.
pub fn write_json_file_pretty(path: &Path, value: &serde_json::Value) -> PicaResult<()> {
  let content = serde_json::to_string_pretty(value)?;
  write_atomic(path, content.as_bytes())
}

/// # Errors
/// Returns an error if the temporary file cannot be written or renamed. The
/// previous file is left in place when it existed, and no temporary file is left
/// behind.
pub fn write_atomic(path: &Path, content: &[u8]) -> PicaResult<()> {
  let tmp = temp_path_for(path);
  if let Some(parent) = tmp.parent() {
    fs::create_dir_all(parent)?;
  }
  // The draft is named for this process, so two writers of the same path cannot
  // overwrite each other's half-written file, and a failure below takes the draft
  // away instead of leaving a file nobody owns.
  if let Err(err) = fs::write(&tmp, content).and_then(|()| fs::rename(&tmp, path)) {
    let _ = fs::remove_file(&tmp);
    return Err(PicaError::from(err));
  }
  Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
  let mut candidate = path.as_os_str().to_os_string();
  candidate.push(format!(".tmp-{}", process::id()));
  PathBuf::from(candidate)
}

/// # Errors
/// Returns an error if the directory cannot be created.
pub fn ensure_dir(path: &Path) -> PicaResult<()> {
  fs::create_dir_all(path)?;
  Ok(())
}

#[must_use]
pub fn sha256_hex(content: &[u8]) -> String {
  let hash = Sha256::digest(content);
  format!("{hash:x}")
}

/// # Errors
/// Returns an error if the file cannot be read.
pub fn sha256_file(path: &Path) -> PicaResult<String> {
  let content = fs::read(path)?;
  Ok(sha256_hex(&content))
}

/// Creates a temporary directory with a unique name.
///
/// Note: This returns an owned `PathBuf` rather than using `tempfile::TempDir`
/// because callers (e.g., install's `TempDirGuard`) need ownership of the path
/// and manage the directory lifetime themselves.
///
/// # Errors
/// Returns an error if the temporary directory cannot be created.
pub fn make_temp_dir(prefix: &str) -> PicaResult<PathBuf> {
  // The name has to be unique, and the clock does not guarantee that: it can
  // report the same tick to two callers, which in one process means the same
  // pid as well. `create_dir` refuses a path that is taken — including a
  // leftover of an earlier, killed run — so a failed attempt just moves on
  // instead of handing out a directory that is not empty and not ours.
  for attempt in 0..64u32 {
    let path =
      env::temp_dir().join(format!("{prefix}-{}-{}-{attempt}", process::id(), now_unix_nanos()));
    match fs::create_dir(&path) {
      Ok(()) => return Ok(path),
      Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
      Err(err) => return Err(PicaError::from(err)),
    }
  }

  Err(PicaError::msg(format!("could not create a temporary directory for {prefix}")))
}

/// # Errors
/// Returns an error if the source is not a directory or any file operation fails.
pub fn copy_dir_recursive(source: &Path, target: &Path) -> PicaResult<()> {
  if !source.is_dir() {
    return Err(PicaError::msg(format!("not a directory: {}", source.display())));
  }

  ensure_dir(target)?;
  for entry in fs::read_dir(source)? {
    let entry = entry?;
    let from = entry.path();
    let to = target.join(entry.file_name());
    let metadata = fs::symlink_metadata(&from)?;

    if metadata.is_dir() {
      copy_dir_recursive(&from, &to)?;
    } else if metadata.is_file() {
      fs::copy(&from, &to)?;
    } else if metadata.file_type().is_symlink() {
      copy_symlink(&from, &to)?;
    }
  }

  Ok(())
}

/// Recreates `source` at `target` as a link to the same place, rather than
/// copying what it points at.
///
/// # Errors
/// Returns an error if the link cannot be read or created. Where the platform
/// has no symlinks the file is copied instead, which is the closest thing to
/// the same meaning.
#[cfg(unix)]
pub fn copy_symlink(source: &Path, target: &Path) -> PicaResult<()> {
  use std::os::unix::fs::symlink;

  let link_target = fs::read_link(source)?;
  symlink(link_target, target)?;
  Ok(())
}

/// Recreates `source` at `target` as a link to the same place, rather than
/// copying what it points at.
///
/// # Errors
/// Returns an error if the link cannot be read or created. Where the platform
/// has no symlinks the file is copied instead, which is the closest thing to
/// the same meaning.
#[cfg(not(unix))]
pub fn copy_symlink(source: &Path, target: &Path) -> PicaResult<()> {
  let _ = target;
  fs::copy(source, target)?;
  Ok(())
}

/// # Errors
/// Returns an error if the executable path cannot be resolved.
pub fn resolve_script_dir_from_exe() -> PicaResult<PathBuf> {
  let exe = env::current_exe()?;
  let Some(parent) = exe.parent() else {
    return Err(PicaError::msg("cannot resolve executable directory"));
  };
  Ok(parent.to_path_buf())
}

/// # Errors
/// Returns an error if the command cannot be spawned.
pub fn run_command_capture(mut command: Command) -> PicaResult<Output> {
  let output = command.output()?;
  Ok(output)
}

/// # Errors
/// Returns an error if the command fails or exits with a non-zero status.
pub fn run_command_success(mut command: Command, context: &str) -> PicaResult<()> {
  let output = command.output()?;
  if output.status.success() {
    return Ok(());
  }
  let stderr = String::from_utf8_lossy(&output.stderr);
  let stdout = String::from_utf8_lossy(&output.stdout);
  let detail =
    if stderr.trim().is_empty() { stdout.trim().to_string() } else { stderr.trim().to_string() };
  Err(PicaError::msg(format!("{context}: {detail}")))
}

/// What a package's `root/` payload created on the target system, recorded so
/// that removal can undo exactly this and nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PayloadInstall {
  pub files: Vec<String>,
  /// Directories created by this install, parents before children.
  pub dirs: Vec<String>,
  pub size: u64,
}

/// What `remove_payload` managed to take away.
///
/// Both figures count files, which is what the record holds: a path recorded as
/// a file is unlinked whether it is a regular file or a symlink. Directories are
/// not counted, because they are only removed when they end up empty and the
/// record does not say which ones did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PayloadRemoval {
  /// Files this call unlinked.
  pub removed: usize,
  /// Files that are still there: the unlink failed for some reason other than
  /// the file having been absent already.
  pub kept: usize,
}

/// Mirrors `source_root` onto `target_root`, preserving file modes and
/// recreating symlinks instead of following them.
///
/// # Errors
/// Returns an error if a payload path or symlink would escape `target_root`,
/// or if any filesystem operation fails.
pub fn install_payload(source_root: &Path, target_root: &Path) -> PicaResult<PayloadInstall> {
  let mut install = PayloadInstall { files: Vec::new(), dirs: Vec::new(), size: 0 };
  install_payload_with(source_root, target_root, &mut install)?;
  Ok(install)
}

/// The same walk, recording into a caller-owned result.
///
/// A walk that fails leaves the paths it already wrote in `install`, which is the
/// one thing a plain `Result` cannot report: a rollback needs to know which
/// destinations the walk actually replaced, or it will put a copy back over a file
/// nothing touched. Directories and symlinks in a partial result name places the
/// walk may have cleared before failing, so they are the caller's to inspect.
///
/// # Errors
/// Returns an error if a payload path or symlink would escape `target_root`, or if
/// any filesystem operation fails.
pub fn install_payload_with(
  source_root: &Path,
  target_root: &Path,
  install: &mut PayloadInstall,
) -> PicaResult<()> {
  walk_payload(source_root, source_root, target_root, install)
}

fn walk_payload(
  base: &Path,
  dir: &Path,
  target_root: &Path,
  install: &mut PayloadInstall,
) -> PicaResult<()> {
  let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
  entries.sort_by_key(fs::DirEntry::file_name);

  for entry in entries {
    let from = entry.path();
    let relative = from.strip_prefix(base).map_err(|_| {
      PicaError::msg(format!("payload path outside {}: {}", base.display(), from.display()))
    })?;
    let to = resolve_under(target_root, relative)?;
    let metadata = fs::symlink_metadata(&from)?;

    if metadata.is_dir() {
      if !to.is_dir() {
        fs::create_dir_all(&to)?;
        install.dirs.push(to.display().to_string());
      }
      walk_payload(base, &from, target_root, install)?;
    } else if metadata.is_symlink() {
      let link = fs::read_link(&from)?;
      let parent = to.parent().unwrap_or(target_root).to_path_buf();
      if symlink_escapes_root(&link, &parent, target_root) {
        return Err(PicaError::msg(format!(
          "payload symlink escapes the target root: {} -> {}",
          relative.display(),
          link.display()
        )));
      }
      if to.symlink_metadata().is_ok() {
        fs::remove_file(&to)?;
      }
      copy_symlink(&from, &to)?;
      install.files.push(to.display().to_string());
    } else if metadata.is_file() {
      if let Some(parent) = to.parent() {
        ensure_dir(parent)?;
      }
      deploy_file(&from, &to, &metadata.permissions())?;
      install.size += metadata.len();
      install.files.push(to.display().to_string());
    }
  }

  Ok(())
}

/// Puts one file in place at `to`: the bytes go into a temporary file in the
/// destination's own directory and are renamed into place, so the destination is
/// never absent and never half-written.
///
/// This is the only way Pica lays a file down, and both callers share it on
/// purpose: a package's `root/` walk and a rollback that puts a replaced file
/// back have to agree on what "in place" means, or the undo of a deploy is a
/// second, different deploy.
///
/// The order matters twice over. Unlinking first and copying onto the name is
/// what avoided `ETXTBSY` — a running daemon's binary is a file the kernel still
/// holds open — but it also opened a window in which the previous content was
/// already gone while the new one was still incomplete, and a failure inside
/// that window destroyed a file this install could not put back. A same-directory
/// temporary file keeps the avoidance, because the rename swaps in a fresh inode
/// while the running process holds the old one, and closes the window, because
/// the destination changes content exactly once.
///
/// The temporary file has to be created in the destination's directory. The
/// staging directory comes from `env::temp_dir()`, which on a device is usually
/// a different filesystem, and a rename between filesystems fails.
///
/// `tempfile` creates the draft `0600` and owner-only, so nobody else can write
/// into it while it is being filled. The mode the caller asks for is applied to
/// the draft *before* the rename rather than after it: a rename carries the
/// draft's mode with it, so this is what makes the installed file's mode change
/// atomically with its content — a process killed between the two steps cannot
/// leave a daemon's binary without its execute bit. The trade-off is that a
/// payload whose own mode is group- or world-writable is briefly writable by
/// others while it is still a draft, which is the permission the installed file
/// is being asked to have anyway.
///
/// A directory standing at the destination is removed when it is empty, exactly
/// as the payload walk has always done, and left alone when it holds anything: the
/// rename then fails and the caller reports it, rather than this function deleting
/// contents it does not own. That removal happens *after* the draft is complete
/// and carries its mode, because it is the one step that takes something away
/// before the rename puts something back: doing it first would let a copy that
/// fails — an unreadable source, a full filesystem — cost the empty directory
/// instead of leaving the destination as it was.
///
/// An existing destination is replaced by the rename instead of written through,
/// so a symlink standing there is never followed into whatever it names.
///
/// # Errors
/// Returns an error if the temporary file cannot be created, if its contents
/// cannot be written, or if the rename fails. In every case the destination is
/// left as it was and no temporary file is left behind.
pub fn deploy_file(source: &Path, to: &Path, permissions: &fs::Permissions) -> PicaResult<()> {
  let parent = to
    .parent()
    .ok_or_else(|| PicaError::msg(format!("file has no parent directory: {}", to.display())))?;

  let mut temp = tempfile::NamedTempFile::new_in(parent)
    .map_err(|err| PicaError::msg(format!("cannot create a temporary file: {err}")))?;

  let copied =
    fs::File::open(source).and_then(|mut content| std::io::copy(&mut content, &mut temp));
  if let Err(err) = copied {
    let _ = temp.close();
    return Err(PicaError::msg(format!(
      "file {} could not be copied to {}: {err}",
      source.display(),
      to.display()
    )));
  }

  // `tempfile` creates `0600`, and a rename carries the mode of the file being
  // renamed: without this an executable or an init script would arrive
  // unreadable to anyone but root.
  if let Err(err) = temp.as_file().set_permissions(permissions.clone()) {
    let _ = temp.close();
    return Err(PicaError::msg(format!("file {} could not take its mode: {err}", to.display())));
  }

  // The draft is complete and has its mode, so the only thing left to clear is a
  // directory occupying the name. Doing it here rather than before the copy means
  // a copy that fails cannot cost an empty directory: `remove_existing` leaves a
  // directory with contents alone, and the rename below then reports it.
  if fs::symlink_metadata(to).is_ok_and(|existing| existing.is_dir()) {
    remove_existing(to)?;
  }

  // Dropping a failed persist removes the temporary file, so a rename that does
  // not land leaves the destination exactly as it was.
  temp.persist(to).map_err(|err| {
    PicaError::msg(format!("file {} could not be put in place: {}", to.display(), err.error))
  })?;

  Ok(())
}

/// Removes whatever is at `path`, whether it is a file, a symlink or a
/// directory left by an earlier package.
///
/// A directory is only removed when it is empty: a payload file can legitimately
/// replace something an earlier install put there, but nothing this walk
/// touches owns a directory that has contents of its own.
fn remove_existing(path: &Path) -> PicaResult<()> {
  match fs::symlink_metadata(path) {
    Ok(metadata) if metadata.is_dir() => match fs::remove_dir(path) {
      Ok(()) => Ok(()),
      // Something is still inside it, and this walk has no claim on it.
      Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
      Err(err) => Err(PicaError::from(err)),
    },
    Ok(_) => Ok(fs::remove_file(path)?),
    // Already gone, which is the state the caller asked for.
    Err(_) => Ok(()),
  }
}

/// Undoes a recorded payload install. Directories are removed only when empty,
/// deepest first, so shared directories survive.
///
/// Only absolute paths are touched. Payload paths are always absolute because
/// they are resolved against a target root; anything else in the record (an
/// install delegated to a native package manager stores package names there) is
/// not a file Pica created and must never be unlinked relative to the CWD.
#[must_use]
pub fn remove_payload(payload: &PayloadInstall) -> PayloadRemoval {
  let mut outcome = PayloadRemoval::default();

  for file in &payload.files {
    if !Path::new(file).is_absolute() {
      continue;
    }
    match fs::remove_file(file) {
      Ok(()) => outcome.removed += 1,
      // Already absent is the state the caller asked for; it is neither a file
      // this call removed nor one it had to leave behind.
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
      Err(_) => outcome.kept += 1,
    }
  }
  for dir in payload.dirs.iter().rev() {
    let path = Path::new(dir);
    if !path.is_absolute() {
      continue;
    }
    if fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none()) {
      let _ = fs::remove_dir(path);
    }
  }

  outcome
}

/// Where `relative` lands under `root` once the symlinks standing in its way are
/// followed.
///
/// The walk does not choose where a path goes: the kernel does. `create_dir_all`
/// on a name that is already a symlink to a directory descends into the
/// directory it names, and a file written below it lands there too, so a payload
/// path is only understood once every existing component has been resolved. This
/// answers that question without writing anything.
///
/// Only symlinks that are already on disk are followed; a component that does not
/// exist is a name the walk would create as a plain directory, and stays as it is
/// spelled. An absolute link is resolved the way the kernel would resolve it, and
/// has to stay under `root` for the same reason [`symlink_escapes_root`] insists
/// on it: on the device `root` *is* `/`.
///
/// `None` is not a failure: it says the path leaves `root`, which is an answer a
/// guard can use — nothing Pica owns lives outside its own root. What cannot be
/// answered *is* an error: a link that cannot be read, one that never settles
/// within [`MAX_LINK_HOPS`] steps, and a path that climbs with `..` instead of
/// naming something. A caller that guards something must refuse the payload on
/// this error rather than guess.
///
/// # Errors
/// Returns an error if `relative` is not a plain path under `root`, or if the
/// symlinks in it cannot be followed.
pub fn resolve_in_root(root: &Path, relative: &Path) -> PicaResult<Option<PathBuf>> {
  let mut pending: VecDeque<OsString> = VecDeque::new();
  for component in relative.components() {
    match component {
      Component::Normal(name) => pending.push_back(name.to_os_string()),
      Component::CurDir => {}
      _ => {
        return Err(PicaError::msg(format!(
          "payload path escapes the target root: {}",
          relative.display()
        )));
      }
    }
  }

  let mut out = root.to_path_buf();
  let mut hops = 0usize;
  while let Some(name) = pending.pop_front() {
    out.push(name);
    let Ok(metadata) = fs::symlink_metadata(&out) else {
      // Nothing there yet: the walk would make it, and would make it real.
      continue;
    };
    if !metadata.is_symlink() {
      continue;
    }

    hops += 1;
    if hops > MAX_LINK_HOPS {
      return Err(PicaError::msg(format!(
        "symbolic links do not settle while resolving {}",
        relative.display()
      )));
    }
    let link = fs::read_link(&out).map_err(|err| {
      PicaError::msg(format!("cannot read the symbolic link {}: {err}", out.display()))
    })?;
    let target = if link.is_absolute() {
      lexical_normalize(&link)
    } else {
      lexical_normalize(&out.parent().unwrap_or(root).join(&link))
    };
    if !target.starts_with(root) {
      return Ok(None);
    }

    let mut rest: VecDeque<OsString> = VecDeque::new();
    if let Ok(tail) = target.strip_prefix(root) {
      for component in tail.components() {
        if let Component::Normal(name) = component {
          rest.push_back(name.to_os_string());
        }
      }
    }
    rest.extend(pending);
    pending = rest;
    out = root.to_path_buf();
  }

  Ok(Some(out))
}

/// Where a symlink being *created* would lead, resolved under `root`.
///
/// [`resolve_in_root`] follows links that are already on disk, so it says nothing
/// about a link that is still only a name in a payload. That link is the one a
/// later install would write through, which is why it has to be answered before it
/// is made: `parent` is where it will sit (already resolved), and `link` is what
/// the payload says it points at.
///
/// # Errors
/// Returns an error if the rest of the chain cannot be resolved.
pub fn resolve_link(root: &Path, parent: &Path, link: &Path) -> PicaResult<Option<PathBuf>> {
  let joined = if link.is_absolute() {
    lexical_normalize(link)
  } else {
    lexical_normalize(&parent.join(link))
  };
  let Ok(tail) = joined.strip_prefix(root) else {
    return Ok(None);
  };
  resolve_in_root(root, tail)
}

fn resolve_under(target_root: &Path, relative: &Path) -> PicaResult<PathBuf> {
  let mut out = target_root.to_path_buf();
  for component in relative.components() {
    match component {
      Component::Normal(name) => out.push(name),
      Component::CurDir => {}
      Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
        return Err(PicaError::msg(format!(
          "payload path escapes the target root: {}",
          relative.display()
        )));
      }
    }
  }
  Ok(out)
}

/// Whether `link` would resolve outside `root`, the directory whose contents it
/// is allowed to name.
///
/// Both stages of a package's life have to answer this the same way: the build
/// merges a container's files into an artifact, and the device lays that
/// artifact's `root/` down. A link the build accepts but the device refuses
/// builds a package that only fails where it is used, and a link the device
/// accepts but the build follows leads out of the payload and into whatever
/// machine is building it. The rule lives here so the two cannot drift.
/// How many symlinks [`resolve_in_root`] follows before it calls the chain a loop.
///
/// The kernel's own limit is 40; stopping below it keeps the answer coming from
/// here rather than from a failed `stat`, and none of it is on a path a device
/// walks in normal use.
const MAX_LINK_HOPS: usize = 32;

#[must_use]
pub fn symlink_escapes_root(link: &Path, link_parent: &Path, root: &Path) -> bool {
  let resolved = if link.is_absolute() {
    lexical_normalize(link)
  } else {
    lexical_normalize(&link_parent.join(link))
  };
  !resolved.starts_with(root)
}

fn lexical_normalize(path: &Path) -> PathBuf {
  let mut out = PathBuf::new();
  for component in path.components() {
    match component {
      Component::ParentDir => {
        if !out.pop() {
          out.push(component.as_os_str());
        }
      }
      Component::CurDir => {}
      other => out.push(other.as_os_str()),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::{
    PayloadInstall, deploy_file, install_payload, lexical_normalize, make_temp_dir, remove_payload,
    resolve_in_root, resolve_link, write_atomic,
  };
  use std::fs;
  use std::path::{Path, PathBuf};

  #[test]
  fn an_executable_is_replaced_rather_than_written_over() {
    // Writing over a file the kernel still holds open fails with ETXTBSY, and
    // a running daemon's binary is exactly that. A package that ships its own
    // executable has to be able to replace the one a previous install left.
    let dir = make_temp_dir("pica-test").expect("temp dir");
    let source = dir.join("payload");
    let target = dir.join("target");
    fs::create_dir_all(source.join("usr/sbin")).expect("mkdir payload");
    fs::create_dir_all(target.join("usr/sbin")).expect("mkdir target");

    fs::write(source.join("usr/sbin/daemon"), "new build").expect("write payload file");
    fs::write(target.join("usr/sbin/daemon"), "old build").expect("write an existing file");

    let installed = install_payload(&source, &target).expect("install over an existing file");
    assert_eq!(
      fs::read_to_string(target.join("usr/sbin/daemon")).expect("read"),
      "new build",
      "the file the payload carries must be what is there afterwards"
    );
    assert_eq!(installed.files.len(), 1);
  }

  #[test]
  fn a_directory_the_payload_replaces_is_left_alone_when_it_holds_anything() {
    let dir = make_temp_dir("pica-test").expect("temp dir");
    let source = dir.join("payload");
    let target = dir.join("target");
    fs::create_dir_all(source.join("opt")).expect("mkdir payload");
    fs::create_dir_all(target.join("opt/kept")).expect("mkdir target");
    fs::write(source.join("opt/app"), "a file where a directory was").expect("write payload");
    fs::write(target.join("opt/kept/other"), "not ours").expect("write a bystander");

    install_payload(&source, &target).expect("install");
    assert!(
      target.join("opt/kept/other").is_file(),
      "this walk has no claim on what an earlier install left inside that directory"
    );
    assert!(target.join("opt/app").is_file(), "the payload's own file is still laid down");
  }

  #[test]
  fn an_empty_directory_survives_a_copy_that_fails_before_the_rename() {
    // Moving an empty directory out of the way is the only destructive step
    // before the rename, so it waits for a complete draft. A source that cannot
    // be read must cost the draft, not the directory that was standing there.
    let dir = make_temp_dir("pica-test").expect("temp dir");
    let source = dir.join("missing");
    let target = dir.join("target");
    fs::create_dir_all(target.join("opt/app")).expect("mkdir target");
    let mode = fs::metadata(&target).expect("meta").permissions();

    let err = deploy_file(&source, &target.join("opt/app"), &mode)
      .expect_err("a source that does not exist cannot be deployed");

    assert!(err.to_string().contains("missing"), "{err}");
    assert!(target.join("opt/app").is_dir(), "the empty directory must still be there");
    assert_eq!(
      fs::read_dir(target.join("opt")).expect("read opt").count(),
      1,
      "the draft must be gone with the failure"
    );
  }

  #[test]
  fn every_temporary_directory_is_a_new_one() {
    // Callers unpack into these and expect them to be empty, so two calls that
    // land on the same clock tick must not hand out the same directory.
    let first = make_temp_dir("pica-test").expect("first temp dir");
    let second = make_temp_dir("pica-test").expect("second temp dir");

    assert_ne!(first, second);
    assert!(first.is_dir());
    assert!(second.is_dir());
    assert_eq!(fs::read_dir(&first).expect("read first").count(), 0);

    let _ = fs::remove_dir_all(&first);
    let _ = fs::remove_dir_all(&second);
  }

  #[cfg(unix)]
  use std::os::unix::fs::symlink;

  fn payload(tmp: &Path) -> std::path::PathBuf {
    let root = tmp.join("root");
    fs::create_dir_all(root.join("usr/bin")).expect("mkdir usr/bin");
    fs::create_dir_all(root.join("etc/init.d")).expect("mkdir etc/init.d");
    fs::write(root.join("usr/bin/app"), b"binary").expect("write app");
    fs::write(root.join("etc/init.d/app"), b"#!/bin/sh\n").expect("write init");
    root
  }

  #[test]
  fn mirrors_paths_and_records_everything() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = payload(tmp.path());
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).expect("mkdir target");

    let installed = install_payload(&root, &target).expect("install payload");

    assert!(target.join("usr/bin/app").is_file());
    assert!(target.join("etc/init.d/app").is_file());
    assert_eq!(installed.size, 6 + 10);
    assert!(installed.files.iter().any(|f| f.ends_with("usr/bin/app")));
    assert!(installed.dirs.iter().any(|d| d.ends_with("usr/bin")));

    let outcome = remove_payload(&installed);
    assert_eq!(outcome.removed, installed.files.len(), "every recorded file was unlinked");
    assert_eq!(outcome.kept, 0);
    assert!(!target.join("usr/bin/app").exists());
    assert!(!target.join("usr").exists(), "directories we created are pruned");
    assert!(target.is_dir(), "the target root itself survives");
  }

  #[test]
  fn keeps_pre_existing_directories() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = payload(tmp.path());
    let target = tmp.path().join("target");
    fs::create_dir_all(target.join("usr/bin")).expect("pre-create usr/bin");
    fs::write(target.join("usr/bin/other"), b"keep me").expect("write neighbour");

    let installed = install_payload(&root, &target).expect("install payload");
    assert!(!installed.dirs.iter().any(|d| d.ends_with("usr/bin")));

    assert_eq!(remove_payload(&installed).removed, installed.files.len());
    assert!(target.join("usr/bin/other").is_file());
    assert!(target.join("usr/bin").is_dir());
  }

  #[test]
  fn removal_counts_what_it_did_rather_than_what_the_record_claimed() {
    let tmp = tempfile::tempdir().expect("create temp dir");

    // A recorded path that is now a non-empty directory cannot be unlinked.
    let blocked = tmp.path().join("blocked");
    fs::create_dir_all(&blocked).expect("mkdir");
    fs::write(blocked.join("child"), b"x").expect("write child");

    let payload = PayloadInstall {
      files: vec![
        blocked.display().to_string(),
        // Already gone: the caller asked for absence and got it, so this is
        // neither removed nor kept.
        tmp.path().join("gone").display().to_string(),
      ],
      dirs: Vec::new(),
      size: 0,
    };

    let outcome = remove_payload(&payload);
    assert_eq!(outcome.removed, 0);
    assert_eq!(outcome.kept, 1);
    assert!(blocked.join("child").is_file(), "the failed removal changed nothing");
  }

  #[cfg(unix)]
  #[test]
  fn recreates_symlinks_without_following_them() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("usr/bin")).expect("mkdir");
    fs::write(root.join("usr/bin/real"), b"real").expect("write real");
    symlink("real", root.join("usr/bin/link")).expect("symlink");

    let target = tmp.path().join("target");
    let installed = install_payload(&root, &target).expect("install payload");

    let link = target.join("usr/bin/link");
    assert!(link.symlink_metadata().expect("stat").file_type().is_symlink());
    assert_eq!(fs::read_link(&link).expect("read link"), Path::new("real"));
    assert_eq!(installed.size, 4, "symlinks contribute no size");
  }

  #[cfg(unix)]
  #[test]
  fn rejects_symlink_escaping_the_target_root() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("usr/bin")).expect("mkdir");
    symlink("../../../../etc/passwd", root.join("usr/bin/evil")).expect("symlink");

    let target = tmp.path().join("target");
    let err = install_payload(&root, &target).expect_err("must reject escaping symlink");
    assert!(err.to_string().contains("escapes the target root"));
    assert!(!tmp.path().join("etc/passwd").exists());
  }

  #[cfg(unix)]
  #[test]
  fn resolve_in_root_follows_the_links_that_are_already_there() {
    // What the kernel would do with `usr/lib/demo/db.json` when `usr/lib/demo` is a
    // symlink: descend into the directory it names. The name is not the path.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let target = tmp.path().join("target");
    fs::create_dir_all(target.join("var/lib/pica")).expect("mkdir state");
    fs::create_dir_all(target.join("usr/lib")).expect("mkdir usr/lib");
    symlink("../../var/lib/pica", target.join("usr/lib/demo")).expect("link state");

    let landed = resolve_in_root(&target, Path::new("usr/lib/demo/db.json"))
      .expect("resolve")
      .expect("inside the root");
    assert_eq!(landed, target.join("var/lib/pica/db.json"));

    // A component that does not exist yet stays as spelled: the walk would create it
    // as a real directory, not follow anything.
    let fresh = resolve_in_root(&target, Path::new("etc/new.d/x.conf"))
      .expect("resolve")
      .expect("inside the root");
    assert_eq!(fresh, target.join("etc/new.d/x.conf"));
  }

  #[cfg(unix)]
  #[test]
  fn resolve_in_root_reports_a_link_out_of_the_root_as_none() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).expect("mkdir");
    symlink("/etc/passwd", target.join("away")).expect("link out");

    // `None`, not an error: "it leaves the root" is an answer, and it is the answer
    // that keeps a guard from reporting a protected path it cannot reach.
    assert_eq!(resolve_in_root(&target, Path::new("away")).expect("resolve"), None);
    assert_eq!(resolve_link(&target, &target, Path::new("/etc/passwd")).expect("resolve"), None);
  }

  #[cfg(unix)]
  #[test]
  fn resolve_in_root_refuses_what_it_cannot_answer() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).expect("mkdir");
    // A loop the kernel would give up on after 40 hops; this stops at 32 and says so.
    symlink("b", target.join("a")).expect("link a");
    symlink("a", target.join("b")).expect("link b");

    let looped = resolve_in_root(&target, Path::new("a")).expect_err("a loop cannot be resolved");
    assert!(looped.to_string().contains("do not settle"), "{looped}");

    // `..` is not a path under the root, it is a way of spelling one that leaves it.
    let climbing = resolve_in_root(&target, Path::new("../outside")).expect_err("must refuse");
    assert!(climbing.to_string().contains("escapes the target root"), "{climbing}");
  }

  #[test]
  fn lexical_normalize_collapses_parent_segments() {
    assert_eq!(lexical_normalize(Path::new("/usr/bin/../lib")), PathBuf::from("/usr/lib"));
    assert_eq!(lexical_normalize(Path::new("/a/../../b")), PathBuf::from("/../b"));
    assert_eq!(lexical_normalize(Path::new("./a/./b")), PathBuf::from("a/b"));
  }

  #[test]
  fn empty_payload_is_a_no_op() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(&root).expect("mkdir");
    let installed = install_payload(&root, &tmp.path().join("target")).expect("install");
    assert_eq!(installed, PayloadInstall::default());
  }

  /// Every name in `dir`, sorted, so a test can say what a failed deploy left
  /// behind — including a temporary file it was supposed to remove.
  fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
      .expect("read dir")
      .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
      .collect();
    names.sort();
    names
  }

  #[cfg(unix)]
  #[test]
  fn a_replaced_executable_takes_the_mode_the_payload_carries() {
    use std::os::unix::fs::PermissionsExt;

    // The temporary file is created `0600` and a rename carries that mode into
    // place, so an install that replaces an existing executable has to take the
    // mode from the archive or the daemon would come back unrunnable.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("usr/sbin")).expect("mkdir payload");
    fs::write(root.join("usr/sbin/daemon"), b"new build").expect("write payload");
    fs::set_permissions(root.join("usr/sbin/daemon"), fs::Permissions::from_mode(0o755))
      .expect("chmod payload");

    let target = tmp.path().join("target");
    fs::create_dir_all(target.join("usr/sbin")).expect("mkdir target");
    let installed_file = target.join("usr/sbin/daemon");
    fs::write(&installed_file, b"old build").expect("write the previous build");
    fs::set_permissions(&installed_file, fs::Permissions::from_mode(0o644)).expect("chmod target");

    install_payload(&root, &target).expect("install over an existing executable");

    assert_eq!(fs::read(&installed_file).expect("read"), b"new build");
    assert_eq!(
      fs::symlink_metadata(&installed_file).expect("stat").permissions().mode() & 0o777,
      0o755,
      "the mode the archive carries is the mode that lands"
    );
  }

  #[cfg(unix)]
  #[test]
  fn a_payload_file_that_cannot_be_read_leaves_the_previous_one_untouched() {
    use std::os::unix::fs::PermissionsExt;

    // Assumption: this runs as an unprivileged user, so mode `000` on the source
    // really does refuse the read. As root the open would succeed and the test
    // would fail rather than silently pass, which is the honest outcome.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("usr/sbin")).expect("mkdir payload");
    let source = root.join("usr/sbin/daemon");
    fs::write(&source, b"new build").expect("write payload");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).expect("chmod payload");

    let target = tmp.path().join("target");
    fs::create_dir_all(target.join("usr/sbin")).expect("mkdir target");
    let installed_file = target.join("usr/sbin/daemon");
    fs::write(&installed_file, b"old build").expect("write the previous build");

    install_payload(&root, &target).expect_err("the payload file cannot be read");

    assert_eq!(
      fs::read_to_string(&installed_file).expect("read"),
      "old build",
      "a copy that never happened must not have taken the previous build away"
    );
    assert_eq!(
      names_in(&target.join("usr/sbin")),
      vec!["daemon".to_string()],
      "the temporary file the failed copy abandoned is removed"
    );
  }

  #[test]
  fn a_rename_that_cannot_land_leaves_the_previous_file_untouched() {
    // A non-empty directory at the destination is the deterministic way to make
    // the rename itself fail: this walk may not delete contents it does not own,
    // so it has to put the file down nowhere and report it.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("opt")).expect("mkdir payload");
    fs::write(root.join("opt/app"), b"new build").expect("write payload");

    let target = tmp.path().join("target");
    fs::create_dir_all(target.join("opt/app")).expect("mkdir the blocking directory");
    fs::write(target.join("opt/app/other"), b"not ours").expect("write a bystander");

    install_payload(&root, &target).expect_err("the rename has nowhere to land");

    assert!(target.join("opt/app").is_dir(), "the directory that blocked it is still a directory");
    assert_eq!(
      fs::read_to_string(target.join("opt/app/other")).expect("read"),
      "not ours",
      "and what was inside it is untouched"
    );
    assert_eq!(
      names_in(&target.join("opt")),
      vec!["app".to_string()],
      "the temporary file whose rename failed is removed"
    );
  }

  #[test]
  fn a_new_file_that_cannot_be_laid_down_is_not_recorded() {
    // Nothing existed at the destination here, so the failure has to leave no
    // path that a later uninstall would try to remove: `install_payload` returns
    // no list at all, and the half-file is never created.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("usr/bin")).expect("mkdir payload");
    fs::write(root.join("usr/bin/app"), b"binary").expect("write payload");

    let target = tmp.path().join("target");
    fs::create_dir_all(target.join("usr")).expect("mkdir target");
    // `usr/bin` is a regular file, so the parent the payload needs cannot be
    // created — ENOTDIR, no privileges and no full disk involved.
    fs::write(target.join("usr/bin"), b"not a directory").expect("write the blocker");

    let err = install_payload(&root, &target).expect_err("the parent is not a directory");
    assert!(
      target.join("usr/bin/app").symlink_metadata().is_err(),
      "nothing was created at the path this install failed to lay down: {err}"
    );
    assert_eq!(
      fs::read_to_string(target.join("usr/bin")).expect("read"),
      "not a directory",
      "the bystander that blocked the install is still there"
    );
    assert_eq!(names_in(&target.join("usr")), vec!["bin".to_string()]);
  }

  #[cfg(unix)]
  #[test]
  fn a_payload_file_replaces_a_symlink_without_writing_through_it() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("usr/bin")).expect("mkdir payload");
    fs::write(root.join("usr/bin/app"), b"new build").expect("write payload");

    let target = tmp.path().join("target");
    fs::create_dir_all(target.join("usr/bin")).expect("mkdir target");
    let elsewhere = target.join("usr/elsewhere");
    fs::write(&elsewhere, b"someone else's file").expect("write the link target");
    symlink("../elsewhere", target.join("usr/bin/app")).expect("symlink");

    let installed = install_payload(&root, &target).expect("install");

    let laid = target.join("usr/bin/app");
    assert!(
      !laid.symlink_metadata().expect("stat").file_type().is_symlink(),
      "the link is replaced by the payload's own file"
    );
    assert_eq!(fs::read(&laid).expect("read"), b"new build");
    assert_eq!(
      fs::read_to_string(&elsewhere).expect("read"),
      "someone else's file",
      "writing through the link would have reached outside the payload's own file"
    );
    assert!(installed.files.iter().any(|f| f.ends_with("usr/bin/app")));
  }

  #[cfg(unix)]
  #[test]
  fn a_new_file_lands_with_the_mode_the_payload_carries() {
    use std::os::unix::fs::PermissionsExt;

    // Nothing stood in the way here, so this is the path that goes through the
    // same temporary file as a replace: the mode must come from the payload, not
    // from whatever `tempfile` or the umask would have given.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("etc/init.d")).expect("mkdir payload");
    let source = root.join("etc/init.d/app");
    fs::write(&source, b"#!/bin/sh\n").expect("write payload");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).expect("chmod payload");

    let target = tmp.path().join("target");
    fs::create_dir_all(&target).expect("mkdir target");
    install_payload(&root, &target).expect("install a new file");

    let laid = target.join("etc/init.d/app");
    assert_eq!(fs::read(&laid).expect("read"), b"#!/bin/sh\n");
    assert_eq!(
      fs::symlink_metadata(&laid).expect("stat").permissions().mode() & 0o777,
      0o755,
      "an init script that arrives unrunnable is not an init script"
    );
    assert_eq!(names_in(&target.join("etc/init.d")), vec!["app".to_string()]);
  }

  #[test]
  fn write_atomic_replaces_the_file_and_leaves_no_draft() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let path = tmp.path().join("db.json");
    write_atomic(&path, b"{\"a\":1}").expect("first write");
    write_atomic(&path, b"{\"a\":2}").expect("second write");

    assert_eq!(fs::read(&path).expect("read"), b"{\"a\":2}");
    assert_eq!(
      names_in(tmp.path()),
      vec!["db.json".to_string()],
      "the draft the rename consumed is not left beside the file"
    );
  }

  #[cfg(unix)]
  #[test]
  fn a_write_atomic_that_cannot_land_keeps_the_previous_file_and_no_draft() {
    use std::os::unix::fs::PermissionsExt;

    // A directory standing at the destination refuses both the write and the
    // rename, which is the deterministic way to fail this call: whatever happens,
    // the caller's file has to stay exactly as it was.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let blocker = tmp.path().join("index.json");
    fs::create_dir(&blocker).expect("mkdir the blocking directory");
    fs::set_permissions(&blocker, fs::Permissions::from_mode(0o555)).expect("chmod blocker");

    write_atomic(&blocker, b"index").expect_err("a directory cannot be replaced");

    assert!(blocker.is_dir(), "the destination is still the directory it was");
    assert_eq!(
      names_in(tmp.path()),
      vec!["index.json".to_string()],
      "the failed write took its draft with it"
    );
  }

  #[test]
  fn a_replaced_path_and_a_created_path_are_both_in_the_returned_list() {
    // The caller tells "created by this install" from "already there" by
    // intersecting this list with the paths that were missing before the walk
    // (see `intended_payload` in the CLI), so both kinds have to be reported, and
    // a replaced path must still hold the new content rather than a temp file's.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root");
    fs::create_dir_all(root.join("etc/conf.d")).expect("mkdir payload");
    fs::write(root.join("etc/conf.d/app"), b"new").expect("write the replaced file");
    fs::write(root.join("etc/conf.d/fresh"), b"brand new").expect("write the created file");

    let target = tmp.path().join("target");
    fs::create_dir_all(target.join("etc/conf.d")).expect("mkdir target");
    fs::write(target.join("etc/conf.d/app"), b"old").expect("write the previous file");

    let installed = install_payload(&root, &target).expect("install");

    let mut listed: Vec<String> = installed.files.clone();
    listed.sort();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(fs::read_to_string(target.join("etc/conf.d/app")).expect("read"), "new");
    assert_eq!(fs::read_to_string(target.join("etc/conf.d/fresh")).expect("read"), "brand new");
    assert_eq!(
      names_in(&target.join("etc/conf.d")),
      vec!["app".to_string(), "fresh".to_string()],
      "no temporary file is left among the installed names"
    );
    assert_eq!(installed.size, 3 + 9, "the sizes come from the payload, not from temp files");
  }
}
