use crate::error::{PicaError, PicaResult};
use sha2::{Digest, Sha256};
use std::env;
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
/// Returns an error if the temporary file cannot be written or renamed.
pub fn write_atomic(path: &Path, content: &[u8]) -> PicaResult<()> {
  let tmp = temp_path_for(path);
  if let Some(parent) = tmp.parent() {
    fs::create_dir_all(parent)?;
  }
  fs::write(&tmp, content)?;
  fs::rename(&tmp, path)?;
  Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
  let mut candidate = path.as_os_str().to_os_string();
  candidate.push(".tmp");
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

#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path) -> PicaResult<()> {
  use std::os::unix::fs::symlink;

  let link_target = fs::read_link(source)?;
  symlink(link_target, target)?;
  Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(source: &Path, target: &Path) -> PicaResult<()> {
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

/// Mirrors `source_root` onto `target_root`, preserving file modes and
/// recreating symlinks instead of following them.
///
/// # Errors
/// Returns an error if a payload path or symlink would escape `target_root`,
/// or if any filesystem operation fails.
pub fn install_payload(source_root: &Path, target_root: &Path) -> PicaResult<PayloadInstall> {
  let mut files: Vec<PathBuf> = Vec::new();
  let mut dirs: Vec<PathBuf> = Vec::new();
  let mut size = 0u64;
  walk_payload(source_root, source_root, target_root, &mut files, &mut dirs, &mut size)?;

  Ok(PayloadInstall {
    files: files.iter().map(|path| path.display().to_string()).collect(),
    dirs: dirs.iter().map(|path| path.display().to_string()).collect(),
    size,
  })
}

fn walk_payload(
  base: &Path,
  dir: &Path,
  target_root: &Path,
  files: &mut Vec<PathBuf>,
  dirs: &mut Vec<PathBuf>,
  size: &mut u64,
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
        dirs.push(to.clone());
      }
      walk_payload(base, &from, target_root, files, dirs, size)?;
    } else if metadata.is_symlink() {
      let link = fs::read_link(&from)?;
      let parent = to.parent().unwrap_or(target_root).to_path_buf();
      if symlink_escapes(&link, &parent, target_root) {
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
      files.push(to);
    } else if metadata.is_file() {
      if let Some(parent) = to.parent() {
        ensure_dir(parent)?;
      }
      fs::copy(&from, &to)?;
      *size += metadata.len();
      files.push(to);
    }
  }

  Ok(())
}

/// Undoes a recorded payload install. Directories are removed only when empty,
/// deepest first, so shared directories survive.
///
/// Only absolute paths are touched. Payload paths are always absolute because
/// they are resolved against a target root; anything else in the record (an
/// install delegated to a native package manager stores package names there) is
/// not a file Pica created and must never be unlinked relative to the CWD.
pub fn remove_payload(payload: &PayloadInstall) {
  for file in &payload.files {
    if Path::new(file).is_absolute() {
      let _ = fs::remove_file(file);
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

fn symlink_escapes(link: &Path, link_parent: &Path, target_root: &Path) -> bool {
  let resolved = if link.is_absolute() {
    lexical_normalize(link)
  } else {
    lexical_normalize(&link_parent.join(link))
  };
  !resolved.starts_with(target_root)
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
  use super::{PayloadInstall, install_payload, lexical_normalize, make_temp_dir, remove_payload};
  use std::fs;
  use std::path::{Path, PathBuf};

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

    remove_payload(&installed);
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

    remove_payload(&installed);
    assert!(target.join("usr/bin/other").is_file());
    assert!(target.join("usr/bin").is_dir());
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
}
