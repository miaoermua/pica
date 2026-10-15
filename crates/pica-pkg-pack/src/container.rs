//! Unpacking of the pre-built containers a package ships in `pkgs/`.
//!
//! Containers are expanded **at build time**, not on the device. `pica-pack`
//! merges them, through [`PayloadRoot`], into the artifact's `root/`, so the
//! router only ever copies plain files and never has to spare room for a second
//! copy of a package it is about to lay down.
//!
//! Two layouts are understood:
//!
//! - `.ipk` — an outer tar holding `control` and a `data.tar.<compression>`
//!   member; the data member holds the files.
//! - `.apk` — three gzip streams concatenated (signature, control, data), so the
//!   payload shares one archive with the control entries and is told apart by
//!   name afterwards: apk keeps every control entry as a dot-file at the archive
//!   root. This is the layout of apk-tools 2.x and earlier.
//!
//! apk-tools 3.0 replaced that layout with ADB (the apk database format), which
//! opens with the `ADBd` signature block instead of a gzip stream and cannot be
//! unpacked by any tar. Such a container is reported rather than passed to tar,
//! which would fail without saying why.
//!
//! Dependency metadata inside a container is deliberately ignored. Pica installs
//! files; it does not solve dependency graphs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::{copy_symlink, ensure_dir, make_temp_dir, symlink_escapes_root};

use crate::build::ROOT;

/// The signature block of the apk-tools 3.x (ADB) format, which tar cannot open.
const ADB_SIGNATURE: &[u8; 4] = b"ADBd";

/// A temporary directory removed when the value goes out of scope.
#[derive(Debug)]
struct Scratch(PathBuf);

impl Scratch {
  fn new(prefix: &str) -> PicaResult<Self> {
    Ok(Self(make_temp_dir(prefix)?))
  }

  fn path(&self) -> &Path {
    &self.0
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

/// Runs tar, tolerating builds whose tar lacks `--ignore-zeros`.
///
/// The flag goes after the archive because `-f` consumes the argument that
/// follows it: `tar -xzf --ignore-zeros ARCHIVE` asks tar to open a member
/// called `--ignore-zeros`. Where it is unknown the plain form is retried,
/// which still unpacks single-member containers.
fn run_tar(flags: &[&str], extra: &[&str], archive: &Path, target: &Path) -> PicaResult<()> {
  let mut last = None;
  for suffix in [extra, &[]] {
    let mut command = Command::new("tar");
    // The order matters: `-f` consumes the argument that follows it, so
    // `tar -xzf --ignore-zeros ARCHIVE` would ask tar to open a member called
    // `--ignore-zeros`. The extra flags therefore follow the archive.
    command.args(flags).arg(archive).args(suffix).arg("-C").arg(target);
    match command.output() {
      Ok(output) if output.status.success() => return Ok(()),
      Ok(output) => {
        last = Some(
          String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("tar failed")
            .to_string(),
        );
      }
      Err(err) => return Err(PicaError::msg(format!("tar could not run: {err}"))),
    }
  }
  Err(PicaError::msg(format!(
    "{}: {}",
    archive.display(),
    last.unwrap_or_else(|| "tar failed".to_string())
  )))
}

/// A container's payload, unpacked somewhere on disk.
///
/// The directory belongs to the value: dropping it removes the tree, including
/// the one an `.ipk`'s data member was extracted into. A caller that needs the
/// files to outlive it has to merge them somewhere of its own first, which is
/// what [`PayloadRoot::merge_containers`] does.
#[derive(Debug)]
pub struct UnpackedContainer {
  /// Name recorded for the container, taken from its control metadata.
  pub pkgname: String,
  payload_root: PathBuf,
  /// Held so that `payload_root` stays valid for as long as this value does.
  ///
  /// Never read: what this field does is happen, and then stop happening. An
  /// `.apk`'s payload is this directory, and an `.ipk`'s was extracted into it.
  #[allow(dead_code, reason = "held for its Drop, which removes the tree")]
  scratch: Scratch,
}

impl UnpackedContainer {
  /// The directory holding the files the payload carried.
  #[must_use]
  pub fn payload_root(&self) -> &Path {
    &self.payload_root
  }

  fn new(pkgname: String, scratch: Scratch) -> Self {
    let payload_root = scratch.path().to_path_buf();
    Self { pkgname, payload_root, scratch }
  }
}

/// Unpacks one container and returns where its files landed.
///
/// # Errors
/// Returns an error for an unknown container format, an ADB (apk-tools 3.x)
/// container, a container whose payload cannot be found, and a failed extract.
pub fn unpack_container(container: &Path) -> PicaResult<UnpackedContainer> {
  let extension =
    container.extension().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
  if extension != "ipk" && extension != "apk" {
    return Err(PicaError::msg(format!(
      "unsupported container format: {} (expected .ipk or .apk)",
      container.display()
    )));
  }

  if extension == "apk" && is_adb_container(container) {
    return Err(PicaError::msg(format!(
      "{} is an apk-tools 3.x (ADB) container, which tar cannot unpack. \
       Repack it as a legacy concatenated-gzip .apk, or build the artifact with \
       `targets = apk` so the device's own apk installs it instead of Pica.",
      container.display()
    )));
  }

  let outer = Scratch::new("pica-container")?;
  // `--ignore-zeros` matters for a legacy `.apk`: without it tar stops at the
  // first end-of-archive marker and would unpack the signature member only.
  run_tar(&["-xzf"], &["--ignore-zeros"], container, outer.path())?;

  if extension == "ipk" {
    let member = find_data_member(outer.path()).ok_or_else(|| {
      PicaError::msg(format!("container carries no data.tar member: {}", container.display()))
    })?;
    let data = Scratch::new("pica-container-data")?;
    // An ipk's data member names its own compression, which the builder chose.
    run_tar(&compression_flags(&member), &[], &member, data.path())?;
    // `control` lives inside `control.tar.gz`, not loose in the outer archive.
    let control = read_ipk_control(outer.path());
    // `outer` goes out of scope here and takes the container's own members with
    // it; the payload is in `data`, which the returned value keeps alive.
    return Ok(UnpackedContainer::new(package_name(&control, container), data));
  }

  strip_apk_control(outer.path())?;
  let control = read_text(&outer.path().join(".PKGINFO"));
  Ok(UnpackedContainer::new(package_name(&control, container), outer))
}

/// The `control` text of an `.ipk`: the outer archive holds `control.tar.gz`, and
/// the `control` file is a member of that.
///
/// It is unpacked into a directory of its own rather than beside the payload:
/// a `control.tar.gz` also carries the maintainer scripts (`preinst`,
/// `postinst`, `prerm`, `postrm`, `control`), and those are not payload. Sharing
/// the payload's directory would hand them to the device's root.
fn read_ipk_control(outer: &Path) -> String {
  let Some(member) = find_control_member(outer) else {
    // Some builders put a plain `control` file loose in the outer archive.
    return read_text(&outer.join("control"));
  };
  let control_dir = Scratch::new("pica-container-control");
  let Ok(scratch) = control_dir else { return read_text(&outer.join("control")) };
  if run_tar(&["-xzf"], &[], &member, scratch.path()).is_err() {
    return read_text(&outer.join("control"));
  }
  read_text(&scratch.path().join("control"))
}

/// The `control.tar.*` member of an `.ipk`, if it has one.
fn find_control_member(outer: &Path) -> Option<PathBuf> {
  let entries = fs::read_dir(outer).ok()?;
  let mut candidates: Vec<PathBuf> = entries
    .flatten()
    .map(|entry| entry.path())
    .filter(|path| {
      let named = path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("control.tar"));
      // A real file, not a link: a symlinked member would send tar to whatever
      // it points at, which is a file on the build machine rather than part of
      // the container.
      named && is_real_file(path)
    })
    .collect();
  candidates.sort();
  candidates.into_iter().next()
}

/// Whether `path` starts with the apk-tools 3.x signature block.
///
/// The ADB formats introduced in apk-tools 3.0 open with `ADBd`. A legacy
/// container opens with a gzip stream (`1f 8b`) or a tar header (`ustar`), so
/// the four bytes decide it without reading the whole file.
pub fn is_adb_container(path: &Path) -> bool {
  use std::io::Read;

  let Ok(mut file) = fs::File::open(path) else { return false };
  let mut magic = [0u8; 4];
  matches!(file.read_exact(&mut magic), Ok(())) && &magic == ADB_SIGNATURE
}

/// The artifact's `root/`, built from the sources a package has.
///
/// Every path that goes in is remembered together with the source that put it
/// there. That is what lets a genuine conflict name both sides instead of
/// letting whichever source happened to be processed last quietly win: a merge
/// that cannot say who provided a path cannot say whether overwriting it was
/// intended.
#[derive(Debug)]
pub struct PayloadRoot {
  root: PathBuf,
  entries: BTreeMap<PathBuf, Entry>,
}

#[derive(Debug)]
struct Entry {
  kind: EntryKind,
  source: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
  Dir,
  File,
  Symlink,
}

impl EntryKind {
  fn describe(self) -> &'static str {
    match self {
      Self::Dir => "directory",
      Self::File => "file",
      Self::Symlink => "symlink",
    }
  }
}

/// Which of a package's sources a path came from.
#[derive(Clone, Debug)]
enum Source {
  /// The maintainer's `root/`.
  Overlay,
  /// A container from `pkgs/`, named as its control metadata names it.
  Container(String),
}

impl Source {
  fn describe(&self) -> String {
    match self {
      Self::Overlay => "root/".to_string(),
      Self::Container(name) => format!("container {name}"),
    }
  }
}

impl PayloadRoot {
  #[must_use]
  pub fn new(root: PathBuf) -> Self {
    Self { root, entries: BTreeMap::new() }
  }

  /// Where the merged payload lives.
  #[must_use]
  pub fn root(&self) -> &Path {
    &self.root
  }

  /// Expands every container under `pkgs/` into the payload, returning the
  /// names that were laid down.
  ///
  /// This is what a `pica`-backend artifact needs at build time: the device then
  /// only copies files, and `-R` can take exactly those away again.
  ///
  /// # Errors
  /// Returns an error for an unreadable or unsupported container, and for a path
  /// that two containers both provide.
  pub fn merge_containers(&mut self, pkgs_dir: &Path) -> PicaResult<Vec<String>> {
    if !pkgs_dir.is_dir() {
      return Ok(Vec::new());
    }

    let mut containers: Vec<PathBuf> = Vec::new();
    collect_files(pkgs_dir, &mut containers)?;
    containers.sort();

    let mut names = Vec::new();
    for container in &containers {
      let unpacked = unpack_container(container)?;
      self.merge(unpacked.payload_root(), &Source::Container(unpacked.pkgname.clone()))?;
      names.push(unpacked.pkgname);
    }
    Ok(names)
  }

  /// Merges the maintainer's `root/`.
  ///
  /// It goes in last and may replace what a container supplied: those files are
  /// the ones a maintainer wrote on purpose, and patching a file inside a
  /// container is otherwise impossible to express.
  ///
  /// # Errors
  /// Returns an error for an unreadable overlay, a symlink that escapes it, and
  /// a path that contradicts a container's entry with the other kind.
  pub fn merge_overlay(&mut self, overlay: &Path) -> PicaResult<()> {
    if !overlay.is_dir() {
      return Ok(());
    }
    self.merge(overlay, &Source::Overlay)
  }

  fn merge(&mut self, src: &Path, source: &Source) -> PicaResult<()> {
    self.merge_dir(src, Path::new(""), source)
  }

  fn merge_dir(&mut self, dir: &Path, relative: &Path, source: &Source) -> PicaResult<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
      .map_err(|err| PicaError::msg(format!("read {} failed: {err}", dir.display())))?
      .flatten()
      .map(|entry| entry.path())
      .collect();
    entries.sort();

    for path in entries {
      let Some(name) = path.file_name() else { continue };
      let child = relative.join(name);
      // `symlink_metadata` and `file_type` both stop at the link itself. Asking
      // `is_dir`/`is_file` instead would follow it, and a package could then
      // have the build read whatever the link points at on the build machine.
      let metadata = fs::symlink_metadata(&path)
        .map_err(|err| PicaError::msg(format!("stat {} failed: {err}", path.display())))?;
      let kind = metadata.file_type();

      if kind.is_dir() {
        self.claim(&child, EntryKind::Dir, source)?;
        self.place_dir(&child)?;
        self.merge_dir(&path, &child, source)?;
      } else if kind.is_symlink() {
        let link = fs::read_link(&path)
          .map_err(|err| PicaError::msg(format!("read link {} failed: {err}", path.display())))?;
        self.claim(&child, EntryKind::Symlink, source)?;
        self.place_symlink(&path, &child, &link)?;
      } else if kind.is_file() {
        self.claim(&child, EntryKind::File, source)?;
        self.place_file(&path, &child)?;
      } else {
        // Sockets, fifos and device nodes. The device lays down files, so
        // shipping one would be building a package that cannot fully install.
        return Err(PicaError::msg(format!(
          "{}: {} is neither a file, a directory nor a symlink, which is all Pica deploys",
          source.describe(),
          child.display()
        )));
      }
    }
    Ok(())
  }

  /// Records that `source` is about to provide `relative`, refusing the ways a
  /// payload can contradict itself.
  ///
  /// Every path arrives through the directory holding it, so its ancestors are
  /// claimed before it is: a source that names `etc/demo` as a link and then
  /// offers `etc/demo/config` is refused when the second claim finds a link
  /// where it needs a directory. Without that, the file would be written through
  /// the link — onto whatever the link points at, which during a build is a path
  /// on the machine doing the build.
  fn claim(&mut self, relative: &Path, kind: EntryKind, source: &Source) -> PicaResult<()> {
    let Some(existing) = self.entries.get(relative) else {
      self.entries.insert(relative.to_path_buf(), Entry { kind, source: source.clone() });
      return Ok(());
    };

    if existing.kind != kind {
      return Err(PicaError::msg(format!(
        "{} cannot provide {} as a {}: {} already provides it as a {}",
        source.describe(),
        relative.display(),
        kind.describe(),
        existing.source.describe(),
        existing.kind.describe()
      )));
    }

    // Directories merge, whoever names them: a container shipping
    // `etc/demo/lib` names `etc/demo` as well, so two packages sharing a
    // directory is the ordinary case rather than two claims on one path. Only a
    // file or a link has a single provider. The entry is left as first recorded,
    // so a later contradiction is still attributed to whoever made it that kind.
    if kind == EntryKind::Dir {
      return Ok(());
    }

    match (source, &existing.source) {
      // The maintainer's own files, merged last, are the one source allowed to
      // have the last word.
      (Source::Overlay, _) => Ok(()),
      // The same archive may name a path twice; that is one copy either way.
      (Source::Container(one), Source::Container(other)) if one == other => Ok(()),
      // Two containers naming one path is a packaging mistake. Neither is more
      // authoritative, and resolving it by archive order would make the result
      // depend on file names.
      (Source::Container(one), Source::Container(other)) => Err(PicaError::msg(format!(
        "containers {one} and {other} both provide {}: keep one copy, or put the file in \
         {ROOT}/ to say which one wins",
        relative.display()
      ))),
      // Unreachable while the overlay is merged last, and an error rather than a
      // silent overwrite so that ordering stays a fact rather than an assumption.
      (Source::Container(name), Source::Overlay) => Err(PicaError::msg(format!(
        "container {name} cannot provide {}: {ROOT}/ already does",
        relative.display()
      ))),
    }
  }

  fn place_dir(&self, relative: &Path) -> PicaResult<()> {
    let to = self.root.join(relative);
    // A link where the directory goes would swallow the whole subtree:
    // `create_dir_all` follows it, and the files would land wherever it points.
    remove_link(&to)?;
    ensure_dir(&to)
  }

  fn place_file(&self, from: &Path, relative: &Path) -> PicaResult<()> {
    let to = self.root.join(relative);
    // `fs::copy` writes *through* a destination symlink. Without this line the
    // build would write wherever a link already in the payload points, which is
    // the difference between a package that contains a link and a build machine
    // that follows it.
    remove_link(&to)?;
    if let Some(parent) = to.parent() {
      ensure_dir(parent)?;
    }
    fs::copy(from, &to)
      .map_err(|err| PicaError::msg(format!("copy {} failed: {err}", from.display())))?;
    Ok(())
  }

  fn place_symlink(&self, from: &Path, relative: &Path, link: &Path) -> PicaResult<()> {
    let to = self.root.join(relative);
    let parent = to.parent().unwrap_or(&self.root).to_path_buf();
    // A link has to resolve inside the payload. Outside is either a link the
    // device refuses when it lays the payload down — the same rule, asked of the
    // same function — or, while merging, a path that leaves the artifact
    // altogether.
    if symlink_escapes_root(link, &parent, &self.root) {
      return Err(PicaError::msg(format!(
        "symlink {} -> {} escapes the payload: links must stay inside {}",
        relative.display(),
        link.display(),
        ROOT
      )));
    }
    remove_link(&to)?;
    if let Some(parent) = to.parent() {
      ensure_dir(parent)?;
    }
    copy_symlink(from, &to)
  }
}

/// Removes `path` when it is a symlink, so the write that follows creates its own
/// entry instead of writing through the link.
fn remove_link(path: &Path) -> PicaResult<()> {
  let Ok(metadata) = fs::symlink_metadata(path) else { return Ok(()) };
  if metadata.file_type().is_symlink() {
    fs::remove_file(path)
      .map_err(|err| PicaError::msg(format!("remove {} failed: {err}", path.display())))?;
  }
  Ok(())
}

/// Recursively lists the files under `dir`, sorted so builds are reproducible.
///
/// A symlink is never followed, not even one that looks like a directory or a
/// container: `pkgs/` is meant to say what to ship, and a link would silently
/// ship whatever it points at instead — a file from the build machine's own
/// filesystem. Saying so is more useful than quietly dropping it.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> PicaResult<()> {
  let mut entries: Vec<PathBuf> = fs::read_dir(dir)
    .map_err(|err| PicaError::msg(format!("read {} failed: {err}", dir.display())))?
    .flatten()
    .map(|entry| entry.path())
    .collect();
  entries.sort();

  for path in entries {
    let metadata = fs::symlink_metadata(&path)
      .map_err(|err| PicaError::msg(format!("stat {} failed: {err}", path.display())))?;
    if metadata.is_dir() {
      collect_files(&path, out)?;
    } else if metadata.is_file() {
      out.push(path);
    } else {
      return Err(PicaError::msg(format!(
        "{} is a symlink: {} must hold the containers themselves, not links to files elsewhere",
        path.display(),
        dir.display()
      )));
    }
  }
  Ok(())
}

/// The `data.tar.<compression>` member of an `.ipk`, if the container has one.
fn find_data_member(outer: &Path) -> Option<PathBuf> {
  let entries = fs::read_dir(outer).ok()?;
  let mut candidates: Vec<PathBuf> = entries
    .flatten()
    .map(|entry| entry.path())
    .filter(|path| {
      let named = path
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with("data.tar"));
      named && is_real_file(path)
    })
    .collect();
  candidates.sort();
  candidates.into_iter().next()
}

/// Whether `path` is a regular file rather than a link to one.
///
/// A container that names a member through a symlink is asking tar to read
/// whatever the link points at, which is not part of the container and may be a
/// file on the machine doing the build.
fn is_real_file(path: &Path) -> bool {
  fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

/// The tar flags implied by an `data.tar.<compression>` member's name.
///
/// The name is folded above, so the `.tgz`-style shortenings are matched
/// case-insensitively already; the lint that suggests folding them does not
/// follow the binding it is looking at.
#[allow(clippy::case_sensitive_file_extension_comparisons, reason = "the name is folded above")]
fn compression_flags(member: &Path) -> Vec<&'static str> {
  let name = member.file_name().and_then(|value| value.to_str()).unwrap_or("").to_ascii_lowercase();
  if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
    vec!["-xzf"]
  } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
    vec!["-xJf"]
  } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") {
    vec!["-xjf"]
  } else {
    vec!["-xf"]
  }
}

/// Removes apk's control entries from an unpacked container.
///
/// apk keeps `.PKGINFO`, `.SIGN.*` and the lifecycle scripts it declares as
/// dot-files at the archive root, while the payload always lives under a real
/// directory. Only top-level dot *files* are dropped, so payload such as
/// `root/.profile` stays in place — that entry is `root/`, not `.profile`.
fn strip_apk_control(outer: &Path) -> PicaResult<()> {
  let entries = fs::read_dir(outer).map_err(|err| PicaError::msg(format!("{err}")))?;

  for entry in entries.flatten() {
    let path = entry.path();
    // Not `is_file()`: that follows links, and an entry that is a link to a file
    // elsewhere is a control entry all the same. `remove_file` drops the link
    // itself, never its target.
    let named =
      path.file_name().and_then(|value| value.to_str()).is_some_and(|name| name.starts_with('.'));
    let Ok(metadata) = fs::symlink_metadata(&path) else { continue };
    if named && !metadata.is_dir() {
      let _ = fs::remove_file(&path);
    }
  }
  Ok(())
}

fn read_text(path: &Path) -> String {
  fs::read_to_string(path).unwrap_or_default()
}

/// The package's own name: the control field, or the file name as a fallback.
fn package_name(control: &str, container: &Path) -> String {
  // An ipk uses `Package:`, apk v2 `.PKGINFO` uses `pkgname =`.
  for line in control.lines() {
    let line = line.trim();
    if let Some(value) = line.strip_prefix("Package:") {
      let value = value.trim();
      if !value.is_empty() {
        return value.to_string();
      }
    }
    if let Some(value) = line.strip_prefix("pkgname") {
      if let Some(value) = value.trim_start().strip_prefix('=') {
        let value = value.trim();
        if !value.is_empty() {
          return value.to_string();
        }
      }
    }
  }
  container.file_stem().and_then(|value| value.to_str()).unwrap_or("unknown").to_string()
}

#[cfg(test)]
mod tests {
  use super::{PayloadRoot, compression_flags, is_adb_container, package_name, unpack_container};
  use pica_pkg_core::error::PicaResult;
  use pica_pkg_core::io::make_temp_dir;
  use pretty_assertions::assert_eq;
  use std::fs;
  use std::io::Write;
  use std::os::unix::fs::symlink;
  use std::path::{Path, PathBuf};

  #[test]
  fn reads_the_ipk_package_field() {
    let control = "Package: luci-app-demo\nVersion: 1.0.0\nDescription: a: b\n";
    assert_eq!(package_name(control, Path::new("demo.ipk")), "luci-app-demo");
  }

  #[test]
  fn reads_the_apk_pkgname_field() {
    let control = "pkgname = demo\npkgver = 1.0.0\n";
    assert_eq!(package_name(control, Path::new("demo.apk")), "demo");
  }

  #[test]
  fn falls_back_to_the_container_file_name() {
    assert_eq!(package_name("", Path::new("demo_1.0_all.ipk")), "demo_1.0_all");
    assert_eq!(package_name("pkgname =\n", Path::new("/tmp/x/demo.apk")), "demo");
  }

  #[test]
  fn data_member_compression_is_taken_from_its_name() {
    assert_eq!(compression_flags(Path::new("data.tar.gz")), vec!["-xzf"]);
    assert_eq!(compression_flags(Path::new("data.tar.xz")), vec!["-xJf"]);
    assert_eq!(compression_flags(Path::new("data.tar.bz2")), vec!["-xjf"]);
    assert_eq!(compression_flags(Path::new("data.tar")), vec!["-xf"]);
  }

  #[test]
  fn recognises_the_apk_tools_3x_signature_block() {
    let dir = pica_pkg_core::io::make_temp_dir("pica-adb-test").expect("temp dir");
    let path = dir.join("demo.apk");

    let mut file = fs::File::create(&path).expect("create");
    file.write_all(b"ADBd\x8c\xbc\x07XSI").expect("write adb magic");
    drop(file);
    assert!(is_adb_container(&path), "an apk-tools 3.x container starts with ADBd");

    // A legacy container opens with a gzip stream instead.
    let legacy = dir.join("legacy.apk");
    fs::write(&legacy, b"\x1f\x8b\x08\x00\x00\x00\x00\x00").expect("write gzip magic");
    assert!(!is_adb_container(&legacy), "a legacy apk opens with gzip");
  }

  #[test]
  fn an_adb_container_reports_the_cause_instead_of_a_tar_failure() {
    let dir = pica_pkg_core::io::make_temp_dir("pica-adb-err").expect("temp dir");
    let path = dir.join("demo.apk");
    fs::write(&path, b"ADBd\x8c\xbc\x07XSImore bytes that are not a gzip stream").expect("write");

    let err = unpack_container(&path).expect_err("must refuse the ADB container");
    let message = err.to_string();
    assert!(message.contains("apk-tools 3.x"), "the error must name the format: {message}");
    assert!(message.contains("targets = apk"), "the error must offer a way forward: {message}");
  }

  #[test]
  fn an_ipk_maintainer_scripts_are_not_payload() {
    // `control.tar.gz` carries `preinst`, `postinst`, `prerm`, `postrm` and
    // `control`. They belong to the package manager, not to the rootfs, and
    // unpacking them beside the payload would hand the device a `/postinst`.
    let dir = make_temp_dir("pica-ipk-scripts").expect("temp dir");
    let container = build_ipk_with_scripts(&dir, "demo-lib");

    let unpacked = unpack_container(&container).expect("unpack");
    let leaked: Vec<String> = ["control", "preinst", "postinst", "prerm", "postrm"]
      .into_iter()
      .filter(|name| unpacked.payload_root().join(name).exists())
      .map(ToString::to_string)
      .collect();
    assert!(leaked.is_empty(), "maintainer scripts must not be payload: {leaked:?}");

    assert_eq!(unpacked.pkgname, "demo-lib");
    assert!(unpacked.payload_root().join("etc/demo").is_dir(), "the payload is still there");
  }

  /// An `.ipk` whose `control.tar.gz` also holds the maintainer scripts a real
  /// package carries, which is what must not be mistaken for payload.
  fn build_ipk_with_scripts(dir: &Path, pkgname: &str) -> std::path::PathBuf {
    let data = dir.join("data/etc/demo");
    fs::create_dir_all(&data).expect("mkdir data");
    fs::write(data.join("config"), "payload\n").expect("write payload");
    let data_tar = dir.join("data.tar.gz");
    run(&[
      "-czf",
      data_tar.to_str().expect("path"),
      "-C",
      dir.join("data").to_str().expect("path"),
      ".",
    ]);

    let control = dir.join("ctl");
    fs::create_dir_all(&control).expect("mkdir control");
    fs::write(control.join("control"), format!("Package: {pkgname}\nVersion: 1.0\n"))
      .expect("write control");
    for script in ["preinst", "postinst", "prerm", "postrm"] {
      fs::write(control.join(script), "#!/bin/sh\nexit 0\n").expect("write script");
    }
    let control_tar = dir.join("control.tar.gz");
    run(&[
      "-czf",
      control_tar.to_str().expect("path"),
      "-C",
      control.to_str().expect("path"),
      "control",
      "preinst",
      "postinst",
      "prerm",
      "postrm",
    ]);

    let ipk = dir.join("demo_1.0_all.ipk");
    run(&[
      "-czf",
      ipk.to_str().expect("path"),
      "-C",
      dir.to_str().expect("path"),
      "control.tar.gz",
      "data.tar.gz",
    ]);
    ipk
  }

  fn run(args: &[&str]) {
    let status = std::process::Command::new("tar").args(args).status().expect("run tar");
    assert!(status.success(), "tar {args:?} failed");
  }

  /// A real `.ipk`: an outer tar holding a `control.tar.gz` (which names the
  /// package) and a `data.tar.gz` (which carries `files`).
  fn build_ipk(dir: &Path, pkgname: &str, files: &[(&str, &str)]) -> PathBuf {
    build_ipk_with_links(dir, pkgname, files, &[])
  }

  /// The same, with links in the payload. `links` are `(relative path, target)`.
  fn build_ipk_with_links(
    dir: &Path,
    pkgname: &str,
    files: &[(&str, &str)],
    links: &[(&str, &str)],
  ) -> PathBuf {
    let data = dir.join(format!("data-{pkgname}"));
    for (relative, contents) in files {
      let path = data.join(relative);
      fs::create_dir_all(path.parent().expect("payload parent")).expect("mkdir payload");
      fs::write(&path, contents).expect("write payload file");
    }
    for (relative, target) in links {
      let path = data.join(relative);
      fs::create_dir_all(path.parent().expect("link parent")).expect("mkdir payload");
      symlink(target, &path).expect("create payload link");
    }
    // The members are added under the names a real `.ipk` uses, whatever the
    // staging directories are called.
    let outer = dir.join(format!("outer-{pkgname}"));
    fs::create_dir_all(&outer).expect("mkdir outer");
    let data_tar = outer.join("data.tar.gz");
    run(&["-czf", data_tar.to_str().expect("path"), "-C", data.to_str().expect("path"), "."]);

    let control = dir.join(format!("control-{pkgname}"));
    fs::create_dir_all(&control).expect("mkdir control");
    fs::write(control.join("control"), format!("Package: {pkgname}\nVersion: 1.0\n"))
      .expect("write control");
    let control_tar = outer.join("control.tar.gz");
    run(&[
      "-czf",
      control_tar.to_str().expect("path"),
      "-C",
      control.to_str().expect("path"),
      "control",
    ]);

    let ipk = dir.join(format!("{pkgname}.ipk"));
    run(&[
      "-czf",
      ipk.to_str().expect("path"),
      "-C",
      outer.to_str().expect("path"),
      "control.tar.gz",
      "data.tar.gz",
    ]);
    ipk
  }

  /// Stages `containers` under `pkgs/` and merges them the way a build does.
  fn merge_pkgs(dir: &Path, pkgs: &Path, containers: &[PathBuf]) -> PicaResult<PayloadRoot> {
    fs::create_dir_all(pkgs).expect("mkdir pkgs");
    for container in containers {
      let name = container.file_name().expect("container name");
      fs::copy(container, pkgs.join(name)).expect("stage container");
    }
    let mut payload = PayloadRoot::new(dir.join("out"));
    payload.merge_containers(pkgs)?;
    Ok(payload)
  }

  #[test]
  fn an_ipk_payload_is_flattened_into_the_target() {
    let dir = make_temp_dir("pica-unpack-test").expect("temp dir");
    let ipk = build_ipk(&dir, "demo", &[("etc/init.d/demo", "#!/bin/sh\n")]);

    let mut payload = PayloadRoot::new(dir.join("payload"));
    let pkgs = dir.join("pkgs");
    fs::create_dir_all(&pkgs).expect("mkdir pkgs");
    fs::copy(&ipk, pkgs.join("demo_1.0_all.ipk")).expect("copy in");
    let names = payload.merge_containers(&pkgs).expect("unpack");

    assert_eq!(names, vec!["demo".to_string()]);
    assert_eq!(
      fs::read_to_string(payload.root().join("etc/init.d/demo")).expect("read"),
      "#!/bin/sh\n"
    );
  }

  // -- What a payload may carry ------------------------------------------------
  //
  // Every test below is about a container file arriving in the payload as what
  // it was, and never as a way to reach a file that was never in the package.

  #[test]
  fn a_file_link_survives_as_a_link() {
    let dir = make_temp_dir("pica-link-file").expect("temp dir");
    let ipk = build_ipk_with_links(
      &dir,
      "demo-lib",
      &[("usr/lib/libdemo.so.1", "payload\n")],
      &[("usr/lib/libdemo.so", "libdemo.so.1")],
    );

    let payload = merge_pkgs(&dir, &dir.join("pkgs"), &[ipk]).expect("merge");

    let link = payload.root().join("usr/lib/libdemo.so");
    assert!(
      fs::symlink_metadata(&link).expect("stat").file_type().is_symlink(),
      "a shared library link must stay a link, or the deployment differs from the native package"
    );
    assert_eq!(fs::read_link(&link).expect("read link"), Path::new("libdemo.so.1"));
  }

  #[test]
  fn a_directory_link_is_not_expanded() {
    let dir = make_temp_dir("pica-link-dir").expect("temp dir");
    let ipk = build_ipk_with_links(
      &dir,
      "demo",
      &[("srv/data/file", "payload\n")],
      &[("srv/alias", "data")],
    );

    let payload = merge_pkgs(&dir, &dir.join("pkgs"), &[ipk]).expect("merge");

    let alias = payload.root().join("srv/alias");
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
    let dir = make_temp_dir("pica-link-escape").expect("temp dir");
    let ipk = build_ipk_with_links(&dir, "demo", &[("etc/keep", "x\n")], &[("etc/escape", "/etc")]);

    let err = merge_pkgs(&dir, &dir.join("pkgs"), &[ipk]).expect_err("must refuse the link");
    let message = err.to_string();
    assert!(message.contains("escapes the payload"), "{message}");
    assert!(message.contains("etc/escape"), "the error must name the link: {message}");
  }

  #[test]
  fn a_relative_link_cannot_climb_out_of_the_payload() {
    let dir = make_temp_dir("pica-link-climb").expect("temp dir");
    let ipk =
      build_ipk_with_links(&dir, "demo", &[("etc/keep", "x\n")], &[("etc/escape", "../../host")]);

    let err = merge_pkgs(&dir, &dir.join("pkgs"), &[ipk]).expect_err("must refuse the link");
    assert!(err.to_string().contains("escapes the payload"), "{err}");
  }

  #[test]
  fn a_link_cannot_pull_a_file_off_the_build_machine_into_the_payload() {
    let dir = make_temp_dir("pica-link-leak").expect("temp dir");

    // Something that exists only where the build runs. A package that can make
    // the build read it can ship it.
    let host = dir.join("host-secrets");
    fs::create_dir_all(&host).expect("mkdir host dir");
    fs::write(host.join("id_rsa"), "private key\n").expect("write host file");

    let ipk = build_ipk_with_links(
      &dir,
      "demo",
      &[("etc/keep", "x\n")],
      &[("etc/leak", host.to_str().expect("path"))],
    );

    let mut payload = PayloadRoot::new(dir.join("out"));
    let pkgs = dir.join("pkgs");
    fs::create_dir_all(&pkgs).expect("mkdir pkgs");
    fs::copy(&ipk, pkgs.join("demo.ipk")).expect("stage");
    let err = payload
      .merge_containers(&pkgs)
      .expect_err("a link to the build machine is not part of the package");

    assert!(err.to_string().contains("escapes the payload"), "{err}");
    assert!(
      !payload.root().join("etc/leak/id_rsa").exists(),
      "the build machine's file must not be copied into the payload"
    );
  }

  // -- Conflicts ---------------------------------------------------------------
  //
  // A package with two sources has to say what happens when they disagree, and
  // the answer has to be the same file contents every time.

  #[test]
  fn the_overlay_replaces_a_containers_file() {
    let dir = make_temp_dir("pica-merge-overlay").expect("temp dir");
    let ipk = build_ipk(&dir, "demo-lib", &[("etc/demo.conf", "from-container\n")]);

    let mut payload = merge_pkgs(&dir, &dir.join("pkgs"), &[ipk]).expect("merge containers");
    let overlay = dir.join("root");
    fs::create_dir_all(overlay.join("etc")).expect("mkdir root");
    fs::write(overlay.join("etc/demo.conf"), "from-root\n").expect("write overlay");
    payload.merge_overlay(&overlay).expect("the overlay is allowed the last word");

    assert_eq!(
      fs::read_to_string(payload.root().join("etc/demo.conf")).expect("read"),
      "from-root\n",
      "root/ is where a maintainer says what the file must be"
    );
  }

  #[test]
  fn two_containers_providing_one_file_are_refused() {
    let dir = make_temp_dir("pica-merge-conflict").expect("temp dir");
    let a = build_ipk(&dir, "demo-a", &[("etc/demo.conf", "from-a\n")]);
    let b = build_ipk(&dir, "demo-b", &[("etc/demo.conf", "from-b\n")]);

    let err = merge_pkgs(&dir, &dir.join("pkgs"), &[a, b])
      .expect_err("neither container is more authoritative");
    let message = err.to_string();
    assert!(message.contains("demo-a") && message.contains("demo-b"), "must name both: {message}");
    assert!(message.contains("etc/demo.conf"), "must name the path: {message}");
    assert!(message.contains("root/"), "must say how to resolve it: {message}");
  }

  #[test]
  fn containers_may_share_the_directories_they_only_pass_through() {
    // Two packages installing into `etc/init.d` both name `etc` and `etc/init.d`.
    // Sharing a directory is the ordinary case; only a file has one provider.
    let dir = make_temp_dir("pica-merge-shared-dir").expect("temp dir");
    let a = build_ipk(&dir, "demo-a", &[("etc/init.d/a", "a\n")]);
    let b = build_ipk(&dir, "demo-b", &[("etc/init.d/b", "b\n")]);

    let payload = merge_pkgs(&dir, &dir.join("pkgs"), &[a, b]).expect("directories merge");

    assert_eq!(fs::read_to_string(payload.root().join("etc/init.d/a")).expect("read"), "a\n");
    assert_eq!(fs::read_to_string(payload.root().join("etc/init.d/b")).expect("read"), "b\n");
  }

  #[test]
  fn a_file_cannot_take_a_directorys_place() {
    let dir = make_temp_dir("pica-merge-kind").expect("temp dir");
    let ipk = build_ipk(&dir, "demo-a", &[("etc/demo/keep", "container\n")]);

    let mut payload = merge_pkgs(&dir, &dir.join("pkgs"), &[ipk]).expect("merge containers");
    let overlay = dir.join("root");
    fs::create_dir_all(overlay.join("etc")).expect("mkdir root");
    fs::write(overlay.join("etc/demo"), "not a directory\n").expect("write overlay");

    let err = payload.merge_overlay(&overlay).expect_err("a file cannot replace a directory");
    let message = err.to_string();
    assert!(message.contains("etc/demo"), "must name the path: {message}");
    assert!(message.contains("directory"), "must say what is already there: {message}");
  }

  #[test]
  fn a_link_cannot_take_a_files_place() {
    let dir = make_temp_dir("pica-merge-link-conflict").expect("temp dir");
    let ipk = build_ipk(&dir, "demo-a", &[("etc/demo.conf", "container\n")]);

    let mut payload = merge_pkgs(&dir, &dir.join("pkgs"), &[ipk]).expect("merge containers");
    let overlay = dir.join("root");
    fs::create_dir_all(overlay.join("etc")).expect("mkdir root");
    symlink("demo.other", overlay.join("etc/demo.conf")).expect("create overlay link");

    let err = payload.merge_overlay(&overlay).expect_err("a link cannot replace a file");
    let message = err.to_string();
    assert!(message.contains("etc/demo.conf"), "must name the path: {message}");
    assert!(message.contains("symlink"), "must say what it would become: {message}");
  }

  #[test]
  fn a_path_below_another_containers_link_is_refused() {
    // The dangerous shape: one container names `etc/demo` as a link, another
    // puts a file under it. Writing that file would create it wherever the link
    // points — outside the payload, if the link says so.
    let dir = make_temp_dir("pica-merge-under-link").expect("temp dir");
    let link =
      build_ipk_with_links(&dir, "demo-a", &[("etc/keep", "x\n")], &[("etc/demo", "real")]);
    let under = build_ipk(&dir, "demo-b", &[("etc/demo/config", "payload\n")]);

    let err = merge_pkgs(&dir, &dir.join("pkgs"), &[link, under])
      .expect_err("the path does not exist as a directory");
    let message = err.to_string();
    assert!(message.contains("etc/demo"), "must name the link: {message}");
    assert!(message.contains("symlink"), "must say what is in the way: {message}");
  }
}
