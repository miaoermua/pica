//! Staging the maintainer's `root/` into the artifact.
//!
//! A `pica` artifact carries files and nothing else: no container, no metadata
//! for another package manager to interpret. That makes `root/` the whole
//! payload, and it is copied entry by entry rather than with a recursive copy
//! for two reasons — the artifact must carry the links the maintainer wrote
//! instead of the files their targets happen to hold on the build machine, and a
//! link resolving outside the payload has to be refused here for the same reason
//! the device refuses it when it lays the tree down
//! ([`pica_pkg_core::io::symlink_escapes_root`]).
//!
//! Nothing here reads an `opkg` or `apk` container: a package that needs files
//! from one is built for the backend that installs it, so that backend's
//! database keeps owning them.

use std::fs;
use std::path::{Path, PathBuf};

use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::{copy_symlink, ensure_dir, symlink_escapes_root};
use pica_pkg_core::layout::ROOT;

/// Copies `source` into `destination`, which becomes the artifact's `root/`.
///
/// An absent `source` stages nothing; an empty one still creates `destination`,
/// because a package that ships an empty `root/` said so.
///
/// # Errors
/// Returns an error for an unreadable tree, a link that leaves the payload, and
/// an entry that is neither a file, a directory nor a symlink.
pub(crate) fn stage_root(source: &Path, destination: &Path) -> PicaResult<()> {
  if !source.is_dir() {
    return Ok(());
  }
  ensure_dir(destination)?;
  stage_dir(source, destination, Path::new(""))
}

fn stage_dir(source: &Path, destination: &Path, relative: &Path) -> PicaResult<()> {
  let mut entries: Vec<PathBuf> = fs::read_dir(source)
    .map_err(|err| PicaError::msg(format!("read {} failed: {err}", source.display())))?
    .flatten()
    .map(|entry| entry.path())
    .collect();
  entries.sort();

  for path in entries {
    let Some(name) = path.file_name() else { continue };
    let child = relative.join(name);
    let to = destination.join(&child);
    // `symlink_metadata` and `file_type` both stop at the link itself. Asking
    // `is_dir`/`is_file` instead would follow it, and a package could then have
    // the build read whatever the link points at on the build machine.
    let metadata = fs::symlink_metadata(&path)
      .map_err(|err| PicaError::msg(format!("stat {} failed: {err}", path.display())))?;
    let kind = metadata.file_type();

    if kind.is_dir() {
      ensure_dir(&to)?;
      stage_dir(&path, destination, &child)?;
    } else if kind.is_symlink() {
      let link = fs::read_link(&path)
        .map_err(|err| PicaError::msg(format!("read link {} failed: {err}", path.display())))?;
      let parent = to.parent().unwrap_or(destination).to_path_buf();
      // A link has to resolve inside the payload: outside is a path the device
      // refuses when it lays the payload down — the same rule, asked of the same
      // function — and, while staging, a path that leaves the artifact entirely.
      if symlink_escapes_root(&link, &parent, destination) {
        return Err(PicaError::msg(format!(
          "symlink {} -> {} escapes the payload: links must stay inside {ROOT}",
          child.display(),
          link.display()
        )));
      }
      copy_symlink(&path, &to)?;
    } else if kind.is_file() {
      fs::copy(&path, &to)
        .map_err(|err| PicaError::msg(format!("copy {} failed: {err}", path.display())))?;
    } else {
      // Sockets, fifos and device nodes. The device lays down files, so shipping
      // one would be building a package that cannot fully install.
      return Err(PicaError::msg(format!(
        "{ROOT}/: {} is neither a file, a directory nor a symlink, which is all Pica deploys",
        child.display()
      )));
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::stage_root;
  use pica_pkg_core::io::make_temp_dir;
  use pretty_assertions::assert_eq;
  use std::fs;
  use std::os::unix::fs::symlink;
  use std::path::Path;

  #[test]
  fn a_file_link_survives_as_a_link() {
    let dir = make_temp_dir("pica-root-link").expect("temp dir");
    let root = dir.join("root");
    fs::create_dir_all(root.join("usr/lib")).expect("mkdir root");
    fs::write(root.join("usr/lib/libdemo.so.1"), "payload\n").expect("write file");
    symlink("libdemo.so.1", root.join("usr/lib/libdemo.so")).expect("create link");

    let out = dir.join("out");
    stage_root(&root, &out).expect("stage");

    let link = out.join("usr/lib/libdemo.so");
    assert!(
      fs::symlink_metadata(&link).expect("stat").file_type().is_symlink(),
      "a shared library link must stay a link, or the deployment differs from the native package"
    );
    assert_eq!(fs::read_link(&link).expect("read link"), Path::new("libdemo.so.1"));
  }

  #[test]
  fn a_directory_link_is_not_expanded() {
    let dir = make_temp_dir("pica-root-dir-link").expect("temp dir");
    let root = dir.join("root");
    fs::create_dir_all(root.join("srv/data")).expect("mkdir root");
    fs::write(root.join("srv/data/file"), "payload\n").expect("write file");
    symlink("data", root.join("srv/alias")).expect("create link");

    let out = dir.join("out");
    stage_root(&root, &out).expect("stage");

    let alias = out.join("srv/alias");
    assert!(
      fs::symlink_metadata(&alias).expect("stat").file_type().is_symlink(),
      "expanding a directory link would clone the tree instead of shipping the link"
    );
    assert_eq!(
      fs::read_to_string(alias.join("file")).expect("read through the link"),
      "payload\n",
      "the link still resolves, inside the payload"
    );
  }

  #[test]
  fn a_link_that_leaves_the_payload_is_refused() {
    let dir = make_temp_dir("pica-root-escape").expect("temp dir");
    let root = dir.join("root");
    fs::create_dir_all(root.join("etc")).expect("mkdir root");
    fs::write(root.join("etc/keep"), "x\n").expect("write file");
    symlink("/etc", root.join("etc/escape")).expect("create link");

    let err = stage_root(&root, &dir.join("out")).expect_err("must refuse the link");
    let message = err.to_string();
    assert!(message.contains("escapes the payload"), "{message}");
    assert!(message.contains("etc/escape"), "the error must name the link: {message}");
  }

  #[test]
  fn a_relative_link_cannot_climb_out_of_the_payload() {
    let dir = make_temp_dir("pica-root-climb").expect("temp dir");
    let root = dir.join("root");
    fs::create_dir_all(root.join("etc")).expect("mkdir root");
    fs::write(root.join("etc/keep"), "x\n").expect("write file");
    symlink("../../host", root.join("etc/escape")).expect("create link");

    let err = stage_root(&root, &dir.join("out")).expect_err("must refuse the link");
    assert!(err.to_string().contains("escapes the payload"), "{err}");
  }

  #[test]
  fn a_link_cannot_pull_a_file_off_the_build_machine_into_the_payload() {
    let dir = make_temp_dir("pica-root-leak").expect("temp dir");

    // Something that exists only where the build runs. A package that can make
    // the build read it can ship it.
    let host = dir.join("host-secrets");
    fs::create_dir_all(&host).expect("mkdir host dir");
    fs::write(host.join("id_rsa"), "private key\n").expect("write host file");

    let root = dir.join("root");
    fs::create_dir_all(root.join("etc")).expect("mkdir root");
    symlink(&host, root.join("etc/leak")).expect("create link");

    let out = dir.join("out");
    let err = stage_root(&root, &out).expect_err("a link to the build machine is not payload");

    assert!(err.to_string().contains("escapes the payload"), "{err}");
    assert!(
      !out.join("etc/leak/id_rsa").exists(),
      "the build machine's file must not be copied into the payload"
    );
  }

  #[test]
  fn an_absent_root_stages_nothing_and_an_empty_one_still_exists() {
    let dir = make_temp_dir("pica-root-empty").expect("temp dir");

    let absent = dir.join("absent");
    let out = dir.join("out-absent");
    stage_root(&absent, &out).expect("staging an absent root");
    assert!(!out.exists(), "nothing to stage means no root/");

    let empty = dir.join("empty");
    fs::create_dir_all(&empty).expect("mkdir empty");
    let out = dir.join("out-empty");
    stage_root(&empty, &out).expect("staging an empty root");
    assert!(out.is_dir(), "a package that ships an empty root/ said so");
  }
}
