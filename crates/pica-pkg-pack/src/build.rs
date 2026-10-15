use crate::archive::{compute_sha256, run_tar_create, unpacked_size, write_sha256sum_entry};
use crate::backend::{BuildBackend, parse_targets};
use crate::container::{PayloadRoot, is_adb_container, unpack_container};
use crate::matrix::{collect_archs, collect_platforms, has_platform_arch_matrix};
use crate::rewrite::{BuiltFields, rewrite_manifest_for_build};
use crate::{msg, msg2};
use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::{
  copy_dir_recursive, ensure_dir, make_temp_dir, resolve_script_dir_from_exe, write_atomic,
};
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::repo::{Package, PackageIndex, REPO_SCHEMA, package_filename, parse_repo_json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Directory names inside a pkgdir. See docs/standard.md.
const MANIFEST: &str = "manifest";
const SCRIPTS: &str = "scripts";
pub(crate) const ROOT: &str = "root";
const PKGS: &str = "pkgs";
const LICENSE: &str = "LICENSE";

pub(crate) struct BuildRequest<'a> {
  pub pkgdir: &'a Path,
  pub output_dir: &'a Path,
  pub pkgname: &'a str,
  pub pkgver: &'a str,
  pub pkgrel: &'a str,
  pub build_platform: &'a str,
  pub build_arch: &'a str,
  pub pica: &'a str,
  pub has_matrix: bool,
  pub backend: BuildBackend,
}

pub(crate) fn main_build(
  pkgdir: &Path,
  outdir: Option<PathBuf>,
  pkgmgr: Option<&str>,
) -> PicaResult<()> {
  if !pkgdir.is_dir() {
    return Err(PicaError::msg(format!("not a directory: {}", pkgdir.display())));
  }

  let manifest_file = pkgdir.join(MANIFEST);
  if !manifest_file.is_file() {
    return Err(PicaError::msg(format!("missing {MANIFEST} in {}", pkgdir.display())));
  }

  let manifest = Manifest::from_file(&manifest_file)?;

  // `pkgmgr` is packer-owned: it names the single backend of a built artifact.
  // Accepting it here would reintroduce the "one field, two types" ambiguity.
  if manifest.has("pkgmgr") {
    return Err(PicaError::msg(
      "source manifest must not declare `pkgmgr`; use `targets` (the packer writes `pkgmgr` into each artifact)",
    ));
  }

  let pkgname = manifest.require_non_empty("pkgname")?;
  let _appname = manifest.require_non_empty("appname")?;
  let pkgver = manifest.require_non_empty("pkgver")?;
  let _pkg_os = manifest.require_non_empty("os")?;
  let pkgrel = manifest.get_first("pkgrel");
  let mut platform = manifest.get_first("platform");
  let mut arch = manifest.get_first("arch");
  let pica = manifest.get_first("pica");
  let backends = parse_targets(pkgmgr, &manifest.targets())?;

  let (pkgver, pkgrel) = resolve_version(pkgver, pkgrel);

  if platform.is_empty() {
    platform = "all".to_string();
  }
  if arch.is_empty() {
    arch = "all".to_string();
  }

  let pkgs_dir = pkgdir.join(PKGS);
  check_unbuilt_backends(&pkgs_dir, &backends)?;
  check_pica_declares_no_dependencies(&manifest, &backends)?;

  if !has_payload(pkgdir, &pkgs_dir, &backends) {
    return Err(PicaError::msg(format!(
      "package would be empty: provide at least one of {SCRIPTS}/, {ROOT}/ or {PKGS}/<backend>/"
    )));
  }

  let base_output = match outdir {
    Some(path) => path,
    None => resolve_script_dir_from_exe()?.join("bin").join(&pkgname),
  };

  for backend in backends {
    let output_dir = base_output.join(backend.output_subdir());
    ensure_dir(&output_dir)?;

    // Matrix layout is resolved per backend, so an arch-specific opkg build and
    // a noarch-only apk build can coexist in one pkgdir.
    let backend_root = pkgs_dir.join(backend.as_str());
    let has_matrix = backend.expects_containers() && has_platform_arch_matrix(&backend_root);

    if has_matrix {
      for build_platform in collect_platforms(&[backend_root.as_path()])? {
        for build_arch in collect_archs(&[backend_root.as_path()], &build_platform)? {
          build_one(&BuildRequest {
            pkgdir,
            output_dir: &output_dir,
            pkgname: &pkgname,
            pkgver: &pkgver,
            pkgrel: &pkgrel,
            build_platform: &build_platform,
            build_arch: &build_arch,
            pica: &pica,
            has_matrix,
            backend,
          })?;
        }
      }
    } else {
      build_one(&BuildRequest {
        pkgdir,
        output_dir: &output_dir,
        pkgname: &pkgname,
        pkgver: &pkgver,
        pkgrel: &pkgrel,
        build_platform: &platform,
        build_arch: &arch,
        pica: &pica,
        has_matrix,
        backend,
      })?;
    }
  }

  Ok(())
}

/// `pkgs/<backend>/` counts as payload for every backend that ships containers,
/// `pica` included: it is the containers, not `root/`, that make such a package
/// worth installing.
fn has_payload(pkgdir: &Path, pkgs_dir: &Path, backends: &[BuildBackend]) -> bool {
  if pkgdir.join(SCRIPTS).is_dir() || pkgdir.join(ROOT).is_dir() {
    return true;
  }
  backends
    .iter()
    .any(|backend| backend.expects_containers() && pkgs_dir.join(backend.as_str()).is_dir())
}

/// A pkgdir that ships `pkgs/apk/` but declares only `targets = opkg` is almost
/// always a forgotten declaration. Warning about it dropped those containers
/// from every artifact, so the build stops instead.
fn check_unbuilt_backends(pkgs_dir: &Path, backends: &[BuildBackend]) -> PicaResult<()> {
  let Ok(entries) = fs::read_dir(pkgs_dir) else { return Ok(()) };

  for entry in entries.flatten() {
    let path = entry.path();
    if !path.is_dir() {
      continue;
    }
    let name = entry.file_name().to_string_lossy().to_string();
    let known = BuildBackend::parse(&name).is_ok();
    let selected = backends.iter().any(|backend| backend.as_str() == name);
    if known && !selected {
      return Err(PicaError::msg(format!(
        "{PKGS}/{name}/ present but {name} is not in targets; add it to targets or remove the directory"
      )));
    }
  }

  Ok(())
}

/// Pica owns its files but does not resolve dependencies, so a `pica` target
/// that declares any would ship a package whose dependencies nobody installs.
/// Both the build and the install reject that combination (D3).
fn check_pica_declares_no_dependencies(
  manifest: &Manifest,
  backends: &[BuildBackend],
) -> PicaResult<()> {
  if !backends.contains(&BuildBackend::Pica) {
    return Ok(());
  }

  for key in ["app", "base", "kmod"] {
    let declared: Vec<String> = manifest
      .get_array_for(key, BuildBackend::Pica.as_str())
      .into_iter()
      .filter(|item| !item.trim().is_empty())
      .collect();
    if !declared.is_empty() {
      return Err(PicaError::msg(format!(
        "`{key}` declares {} but the pica backend cannot resolve dependencies; \
         build it with `targets = opkg` or `targets = apk`, or declare the dependency per backend (`{key}.opkg = ...`)",
        declared.join(" ")
      )));
    }
  }

  Ok(())
}

pub(crate) fn build_one(req: &BuildRequest<'_>) -> PicaResult<()> {
  let pkgfile = package_filename(
    req.pkgname,
    req.pkgver,
    req.pkgrel,
    req.build_platform,
    req.build_arch,
    req.backend.as_str(),
  );

  msg(format!(
    "Making package: {} {}-{} [{}]",
    req.pkgname,
    req.pkgver,
    req.pkgrel,
    req.backend.as_str()
  ));
  msg2(format!("Platform: {}", req.build_platform));
  msg2(format!("Arch: {}", req.build_arch));
  if req.pica.is_empty() {
    msg2("Pica requires: (not specified)");
  } else {
    msg2(format!("Pica requires >= {}", req.pica));
  }

  let tmpdir = make_temp_dir("pica-pack")?;
  let result = stage_and_pack(req, &tmpdir, &pkgfile);
  let _ = fs::remove_dir_all(&tmpdir);
  result
}

/// Normalizes `pkgver`/`pkgrel`.
///
/// A manifest may omit `pkgrel`, in which case a combined `pkgver = 1.0.0-2`
/// carries the revision and a bare `pkgver = 1.0.0` defaults to revision 1.
fn resolve_version(pkgver: String, pkgrel: String) -> (String, String) {
  if !pkgrel.is_empty() {
    return (pkgver, pkgrel);
  }
  match pkgver.split_once('-') {
    Some((version, revision)) => (version.to_string(), revision.to_string()),
    None => (pkgver, "1".to_string()),
  }
}

fn stage_and_pack(req: &BuildRequest<'_>, tmpdir: &Path, pkgfile: &str) -> PicaResult<()> {
  let scripts_src = req.pkgdir.join(SCRIPTS);
  if scripts_src.is_dir() {
    copy_dir_recursive(&scripts_src, &tmpdir.join(SCRIPTS))?;
  }

  // `root/` is assembled before the manifest is written, because the size the
  // manifest records is the size of what a device will deploy — and for a `pica`
  // artifact part of that comes out of the containers, so it is not known until
  // they have been expanded.
  let root_src = req.pkgdir.join(ROOT);
  let mut payload = PayloadRoot::new(tmpdir.join(ROOT));

  // A `pica`-backend artifact has to be self-contained: the device only ever
  // copies files, so any container it ships is expanded here at build time and
  // its payload becomes part of `root/`. `opkg`/`apk` artifacts keep `pkgs/`
  // and let the system package manager install it, which is the only way those
  // formats get their dependencies handled.
  if req.backend.expects_containers() {
    let pkgs_src = req.pkgdir.join(PKGS);
    if req.backend == BuildBackend::Pica {
      // Only this backend's own containers: `pkgs/<backend>/` holds what the
      // device's package manager would install, and another backend's containers
      // are none of this artifact's business.
      let names = payload.merge_containers(&pkgs_src.join(req.backend.as_str()))?;
      if !names.is_empty() {
        msg2(format!("Expanded {} container(s): {}", names.len(), names.join(" ")));
      }
    } else {
      stage_packages(req, &tmpdir.join(PKGS))?;
    }
  }

  // Last, so that the maintainer's own file is what ends up in the artifact when
  // a container carries the same path: `root/` is where a package says what it
  // meant, and a container's copy is the thing being corrected. Two containers
  // disputing one path is a different matter, and the merge refuses it.
  payload.merge_overlay(&root_src)?;
  if root_src.is_dir() {
    // An empty `root/` is still a directory the package asked to ship.
    ensure_dir(payload.root())?;
  }

  // The containers an `opkg`/`apk` artifact ships are installed by the system
  // package manager, so they are part of what the package costs on a device —
  // and, as for a `pica` artifact's own containers, they only say how much once
  // they have been unpacked. A `pica` artifact expanded its containers into
  // `root/` above, so its `root/` is the whole payload already.
  let containers = match req.backend {
    BuildBackend::Pica => ContainerPayload::default(),
    _ => measure_containers(&tmpdir.join(PKGS))?,
  };

  let install_size = unpacked_size(payload.root()) + containers.bytes;
  msg2(format!("Install size: {install_size} bytes"));
  if !containers.counted.is_empty() {
    msg2(format!(
      "{} of those bytes are in {} container(s): {}",
      containers.bytes,
      containers.counted.len(),
      containers.counted.join(" ")
    ));
  }
  // A container pica cannot unpack is named instead of counted as nothing: a
  // package that installs files must not be advertised as installing no bytes.
  if !containers.unmeasured.is_empty() {
    msg2(format!(
      "WARNING: not counted, pica cannot unpack them: {}",
      containers.unmeasured.join(", ")
    ));
  }
  msg2("Creating archive...");

  rewrite_manifest_for_build(
    &req.pkgdir.join(MANIFEST),
    &tmpdir.join(MANIFEST),
    &BuiltFields {
      pkgver: req.pkgver,
      pkgrel: req.pkgrel,
      platform: req.build_platform,
      arch: req.build_arch,
      pkgmgr: req.backend.as_str(),
      size: install_size,
    },
  )?;

  let license_src = req.pkgdir.join(LICENSE);
  if license_src.is_file() {
    fs::copy(&license_src, tmpdir.join(LICENSE))?;
  }

  let mut tar_items = vec![MANIFEST.to_string()];
  for name in [SCRIPTS, ROOT, PKGS] {
    if tmpdir.join(name).is_dir() {
      tar_items.push(name.to_string());
    }
  }
  if tmpdir.join(LICENSE).is_file() {
    tar_items.push(LICENSE.to_string());
  }

  let final_pkg = req.output_dir.join(pkgfile);
  let final_tmp = req.output_dir.join(format!(".{pkgfile}.tmp-{}", std::process::id()));
  run_tar_create(tmpdir, &final_tmp, &tar_items)?;
  fs::rename(&final_tmp, &final_pkg)?;

  let csize = fs::metadata(&final_pkg)?.len();
  let sha256 = compute_sha256(&final_pkg)?;
  write_sha256sum_entry(&req.output_dir.join("SHA256SUMS"), pkgfile, &sha256)?;

  let built_manifest = Manifest::from_file(tmpdir.join(MANIFEST))?;
  write_repo_index(req, &built_manifest, pkgfile, &sha256, csize, install_size)?;

  msg(format!("Finished: {}", final_pkg.display()));
  println!("{}", final_pkg.display());
  Ok(())
}

/// Flattens `pkgs/<backend>/[<platform>/<arch>/]` into the archive's `pkgs/`.
/// Each artifact is already per-platform/per-arch, so nesting inside is noise.
fn stage_packages(req: &BuildRequest<'_>, dst: &Path) -> PicaResult<()> {
  let backend_root = req.pkgdir.join(PKGS).join(req.backend.as_str());
  if !backend_root.is_dir() {
    return Ok(());
  }

  let source = if req.has_matrix {
    let path = backend_root.join(req.build_platform).join(req.build_arch);
    if !path.is_dir() {
      return Err(PicaError::msg(format!(
        "missing {PKGS}/{}/{}/{}",
        req.backend.as_str(),
        req.build_platform,
        req.build_arch
      )));
    }
    path
  } else {
    backend_root
  };

  let extensions = req.backend.package_extensions();
  let copied = copy_packages_flat(&source, dst, extensions)?;
  if copied > 0 {
    return Ok(());
  }

  // Nothing matched: distinguish "directory was empty" from "wrong extensions",
  // which is the common mistake when a pkgdir ships both .ipk and .apk.
  let found = list_extensions(&source);
  if found.is_empty() {
    Ok(())
  } else {
    Err(PicaError::msg(format!(
      "no {} files found for the {} backend in {} (found: {})",
      extensions.join(" or "),
      req.backend.as_str(),
      source.display(),
      found.join(" ")
    )))
  }
}

fn copy_packages_flat(source: &Path, dst: &Path, extensions: &[&str]) -> PicaResult<usize> {
  let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
  collect_packages(source, extensions, &mut files)?;
  if files.is_empty() {
    return Ok(0);
  }

  ensure_dir(dst)?;
  for (name, from) in &files {
    fs::copy(from, dst.join(name))?;
  }
  Ok(files.len())
}

/// What the containers an `opkg`/`apk` artifact ships will install, and the
/// ones whose payload could not be read.
#[derive(Default)]
struct ContainerPayload {
  /// Bytes the counted containers put on a device, their payloads unpacked.
  bytes: u64,
  /// Names of the containers those bytes come from.
  counted: Vec<String>,
  /// Names of the containers that could not be unpacked, with the reason.
  unmeasured: Vec<String>,
}

/// Measures the payload of every container in `pkgs_dir`, which is the staged
/// copy of what the artifact ships.
///
/// A payload has to be expanded before it can be measured — an `.ipk` only says
/// what it installs inside its `data` member — which is why this cannot happen
/// before the artifact's contents are assembled. A container pica cannot unpack
/// is reported to the caller rather than counted as nothing, and rather than
/// failing the build: shipping a container is the maintainer's decision, and
/// accepting it is the device's package manager's business, not this one's.
fn measure_containers(pkgs_dir: &Path) -> PicaResult<ContainerPayload> {
  let mut measured = ContainerPayload::default();
  if !pkgs_dir.is_dir() {
    return Ok(measured);
  }

  let mut containers: Vec<PathBuf> = Vec::new();
  for entry in fs::read_dir(pkgs_dir)? {
    containers.push(entry?.path());
  }
  containers.sort();

  for container in containers {
    let name = container.file_name().unwrap_or_default().to_string_lossy().to_string();
    if is_adb_container(&container) {
      measured.unmeasured.push(format!("{name} (apk-tools 3.x ADB)"));
      continue;
    }
    match unpack_container(&container) {
      Ok(unpacked) => {
        measured.bytes += unpacked_size(unpacked.payload_root());
        measured.counted.push(name);
      }
      Err(err) => measured.unmeasured.push(format!("{name} ({err})")),
    }
  }

  Ok(measured)
}

fn collect_packages(
  dir: &Path,
  extensions: &[&str],
  out: &mut BTreeMap<String, PathBuf>,
) -> PicaResult<()> {
  for entry in fs::read_dir(dir)? {
    let entry = entry?;
    let path = entry.path();
    if path.is_dir() {
      collect_packages(&path, extensions, out)?;
      continue;
    }
    if !has_extension(&path, extensions) {
      continue;
    }
    let name = entry.file_name().to_string_lossy().to_string();
    if let Some(previous) = out.insert(name.clone(), path.clone()) {
      return Err(PicaError::msg(format!(
        "duplicate package file name {name}: {} and {}",
        previous.display(),
        path.display()
      )));
    }
  }
  Ok(())
}

fn list_extensions(dir: &Path) -> Vec<String> {
  let mut found: Vec<String> = Vec::new();
  let Ok(entries) = fs::read_dir(dir) else { return found };
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      for nested in list_extensions(&path) {
        if !found.contains(&nested) {
          found.push(nested);
        }
      }
    } else if let Some(ext) = path.extension().and_then(|value| value.to_str()) {
      let dotted = format!(".{ext}");
      if !found.contains(&dotted) {
        found.push(dotted);
      }
    }
  }
  found.sort();
  found
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
  path.extension().and_then(|value| value.to_str()).is_some_and(|value| {
    let dotted = format!(".{value}");
    extensions.iter().any(|extension| dotted.eq_ignore_ascii_case(extension))
  })
}

/// Writes the per-backend repo index. Top-level fields are filled from the
/// built manifest so the index is queryable without unpacking the artifact,
/// and the embedded manifest keeps full array semantics for repeated keys.
///
/// `install_size` is passed rather than read back from `manifest`: the index
/// advertises what an install will cost, and the manifest's own value is written
/// from the same number so the two cannot disagree.
fn write_repo_index(
  req: &BuildRequest<'_>,
  manifest: &Manifest,
  filename: &str,
  sha256: &str,
  csize: u64,
  install_size: u64,
) -> PicaResult<()> {
  let index_path = req.output_dir.join("repo.json");
  let mut index = if index_path.is_file() {
    let content = fs::read_to_string(&index_path)?;
    parse_repo_json(&content).unwrap_or(PackageIndex { schema: REPO_SCHEMA, packages: Vec::new() })
  } else {
    PackageIndex { schema: REPO_SCHEMA, packages: Vec::new() }
  };
  index.schema = REPO_SCHEMA;
  index.packages.retain(|package| package.filename != filename);

  let text = |key: &str| -> Option<String> {
    let value = manifest.get_first(key);
    (!value.is_empty()).then_some(value)
  };

  index.packages.push(Package {
    pkgname: req.pkgname.to_string(),
    pkgver: req.pkgver.to_string(),
    pkgrel: req.pkgrel.to_string(),
    pkgmgr: req.backend.as_str().to_string(),
    platform: req.build_platform.to_string(),
    arch: req.build_arch.to_string(),
    filename: filename.to_string(),
    sha256: sha256.to_string(),
    appname: text("appname"),
    os: text("os"),
    branch: text("branch"),
    protocol: text("protocol"),
    url: text("url"),
    origin: text("origin"),
    luci_url: text("luci_url"),
    luci_desc: text("luci_desc"),
    pkgdesc: text("pkgdesc"),
    license: text("license"),
    visibility: text("visibility"),
    pica: text("pica").or_else(|| (!req.pica.is_empty()).then(|| req.pica.to_string())),
    download_url: None,
    size: Some(install_size),
    csize: Some(csize),
    manifest: Some(manifest.value.clone()),
    extra: serde_json::Map::default(),
  });

  let content = serde_json::to_vec_pretty(&index)?;
  write_atomic(&index_path, &content)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{
    BuildRequest, copy_packages_flat, list_extensions, main_build, unpacked_size, write_repo_index,
  };
  use crate::backend::BuildBackend;
  use pica_pkg_core::repo::parse_repo_json;
  use std::fs;
  use std::path::Path;

  fn write_pkgdir(root: &Path, manifest: &str) {
    fs::create_dir_all(root).expect("mkdir pkgdir");
    fs::write(root.join("manifest"), manifest).expect("write manifest");
  }

  /// Builds a pkgdir with `manifest` and returns the failure message as a plain
  /// string, so assertions read without importing the error type.
  fn build_with_manifest(root: &Path, manifest: &str) -> Result<(), String> {
    write_pkgdir(root, manifest);
    super::main_build(root, Some(root.join("out")), None).map_err(|err| err.to_string())
  }

  /// Builds a real `.ipk` so the unpack path is exercised rather than mocked:
  /// an outer tar holding a `control.tar.gz` (which carries `Package:`) and a
  /// `data.tar.gz` (which carries the files).
  fn make_ipk(root: &Path, pkgname: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let staging = root.join("ipk-staging");
    let _ = fs::remove_dir_all(&staging);

    let data = staging.join("data");
    for (relative, contents) in files {
      let path = data.join(relative);
      fs::create_dir_all(path.parent().expect("payload parent")).expect("mkdir payload");
      fs::write(&path, contents).expect("write payload file");
    }
    let data_tar = root.join("data.tar.gz");
    tar(&["-czf", data_tar.to_str().expect("path"), "-C", data.to_str().expect("path"), "."]);

    let control = staging.join("control");
    fs::create_dir_all(&control).expect("mkdir control");
    fs::write(control.join("control"), format!("Package: {pkgname}\nVersion: 1.0\n"))
      .expect("write control");
    let control_tar = root.join("control.tar.gz");
    tar(&[
      "-czf",
      control_tar.to_str().expect("path"),
      "-C",
      control.to_str().expect("path"),
      "control",
    ]);

    let ipk = root.join(format!("{pkgname}.ipk"));
    tar(&[
      "-czf",
      ipk.to_str().expect("path"),
      "-C",
      root.to_str().expect("path"),
      "control.tar.gz",
      "data.tar.gz",
    ]);
    ipk
  }

  fn tar(args: &[&str]) {
    let status = std::process::Command::new("tar").args(args).status().expect("run tar");
    assert!(status.success(), "tar {args:?} failed");
  }

  /// Names of the entries inside a built artifact, as `tar -tzf` prints them.
  fn artifact_entries(artifact: &Path) -> String {
    let output =
      std::process::Command::new("tar").arg("-tzf").arg(artifact).output().expect("run tar");
    assert!(output.status.success(), "tar -tzf failed for {}", artifact.display());
    String::from_utf8_lossy(&output.stdout).to_string()
  }

  /// The contents of one file inside a built artifact.
  fn artifact_file(artifact: &Path, relative: &str) -> String {
    let output = std::process::Command::new("tar")
      .args(["-xzOf"])
      .arg(artifact)
      .arg(relative)
      .output()
      .expect("run tar");
    assert!(output.status.success(), "tar -xzOf {relative} failed for {}", artifact.display());
    String::from_utf8_lossy(&output.stdout).to_string()
  }

  /// The single artifact a build wrote under `out/<backend>/`.
  fn artifact(root: &Path, backend: &str) -> std::path::PathBuf {
    let dir = root.join("out").join(backend);
    let mut found: Vec<std::path::PathBuf> = fs::read_dir(&dir)
      .unwrap_or_else(|err| panic!("no artifacts in {}: {err}", dir.display()))
      .flatten()
      .map(|entry| entry.path())
      .filter(|path| path.to_string_lossy().ends_with(".pkg.tar.gz"))
      .collect();
    assert_eq!(found.len(), 1, "expected exactly one artifact in {}", dir.display());
    found.remove(0)
  }

  const BASE_MANIFEST: &str =
    "pkgname = hello\nappname = hello\npkgver = 1.0.0\npkgrel = 1\nos = linux\n";

  #[test]
  fn copies_only_matching_extension_and_flattens() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("apk").join("all").join("all");
    fs::create_dir_all(&source).expect("mkdir nested");
    fs::write(source.join("one.apk"), b"apk").expect("write apk");
    fs::write(source.join("one.ipk"), b"ipk").expect("write ipk");

    let dst = tmp.path().join("out");
    let copied =
      copy_packages_flat(tmp.path().join("apk").as_path(), &dst, &[".apk"]).expect("copy");
    assert_eq!(copied, 1);
    assert!(dst.join("one.apk").is_file());
    assert!(!dst.join("one.ipk").exists());
    assert_eq!(list_extensions(&dst), vec![".apk".to_string()]);
  }

  #[test]
  fn pica_keeps_both_container_formats() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("pica");
    fs::create_dir_all(&source).expect("mkdir nested");
    fs::write(source.join("one.apk"), b"apk").expect("write apk");
    fs::write(source.join("two.ipk"), b"ipk").expect("write ipk");
    fs::write(source.join("notes.txt"), b"text").expect("write stray file");

    let dst = tmp.path().join("out");
    let copied =
      copy_packages_flat(&source, &dst, BuildBackend::Pica.package_extensions()).expect("copy");
    assert_eq!(copied, 2, "pica unpacks both formats itself");
    assert!(dst.join("one.apk").is_file());
    assert!(dst.join("two.ipk").is_file());
    assert!(!dst.join("notes.txt").exists());
  }

  #[test]
  fn duplicate_package_names_are_rejected() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir_all(&a).expect("mkdir a");
    fs::create_dir_all(&b).expect("mkdir b");
    fs::write(a.join("same.apk"), b"x").expect("write a");
    fs::write(b.join("same.apk"), b"y").expect("write b");

    let err =
      copy_packages_flat(tmp.path(), &tmp.path().join("out"), &[".apk"]).expect_err("must fail");
    assert!(err.to_string().contains("duplicate package file name"));
  }

  #[test]
  fn unpacked_size_of_root_is_reported() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().join("root").join("usr").join("bin");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("app"), b"0123456789").expect("write");
    assert_eq!(unpacked_size(&tmp.path().join("root")), 10);
  }

  #[test]
  fn repo_index_carries_backend_and_manifest_arrays() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pkgdir = tmp.path().join("pkgdir");
    let output = tmp.path().join("output");
    fs::create_dir_all(&pkgdir).expect("mkdir pkgdir");
    fs::create_dir_all(&output).expect("mkdir output");
    fs::write(
      pkgdir.join("manifest"),
      "pkgname = luci-app-demo\nappname = demo\npkgver = 1.0.0\npkgrel = 1\nos = openwrt\n\
       branch = stable\nprotocol = luci\nurl = https://example.invalid/demo\npkgdesc = demo\n\
       visibility = open\napp = demo\napp = luci-app-demo\nbase = luci-base\nbase = rpcd\n\
       builddate = 1\nsize = 4096\n",
    )
    .expect("write manifest");

    let manifest =
      pica_pkg_core::manifest::Manifest::from_file(pkgdir.join("manifest")).expect("parse");
    let req = BuildRequest {
      pkgdir: &pkgdir,
      output_dir: &output,
      pkgname: "luci-app-demo",
      pkgver: "1.0.0",
      pkgrel: "1",
      build_platform: "all",
      build_arch: "all",
      pica: "",
      has_matrix: false,
      backend: BuildBackend::Apk,
    };
    write_repo_index(
      &req,
      &manifest,
      "luci-app-demo-1.0.0-1-all-apk.pkg.tar.gz",
      &"1".repeat(64),
      2048,
      4096,
    )
    .expect("write index");

    let text = fs::read_to_string(output.join("repo.json")).expect("read index");
    let parsed = parse_repo_json(&text).expect("index must satisfy schema 2");
    let pkg = &parsed.packages[0];

    assert_eq!(pkg.pkgmgr, "apk");
    assert_eq!(pkg.app_key(), "demo", "appname must come from the manifest, not pkgname");
    assert_eq!(pkg.branch.as_deref(), Some("stable"));
    assert_eq!(pkg.protocol.as_deref(), Some("luci"));
    assert_eq!(pkg.url.as_deref(), Some("https://example.invalid/demo"));
    assert_eq!(pkg.os.as_deref(), Some("openwrt"));
    assert_eq!(pkg.size, Some(4096));
    assert_eq!(pkg.csize, Some(2048));
    assert_eq!(
      pkg.manifest.as_ref().and_then(|m| m.get("app")).and_then(|v| v.as_array()).map(Vec::len),
      Some(2),
      "repeated manifest keys must not collapse in the index"
    );

    // Re-writing the same filename replaces rather than duplicates.
    write_repo_index(
      &req,
      &manifest,
      "luci-app-demo-1.0.0-1-all-apk.pkg.tar.gz",
      &"2".repeat(64),
      4096,
      4096,
    )
    .expect("rewrite index");
    let replaced =
      parse_repo_json(&fs::read_to_string(output.join("repo.json")).expect("read")).expect("valid");
    assert_eq!(replaced.packages.len(), 1);
    assert_eq!(replaced.packages[0].sha256, "2".repeat(64));
  }

  #[test]
  fn build_requires_os_field() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("scripts")).expect("mkdir scripts");
    fs::write(root.join("scripts/install"), "#!/bin/sh\nexit 0\n").expect("write script");
    write_pkgdir(
      &root,
      "pkgname = hello\nappname = hello\npkgver = 1.0.0\npkgrel = 1\nplatform = amd64\narch = x86_64\n",
    );

    let err = main_build(&root, Some(root.join("out")), None).expect_err("missing os must fail");
    assert!(err.to_string().contains("manifest missing os"));
  }

  #[test]
  fn build_rejects_pkgmgr_in_source_manifest() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("scripts")).expect("mkdir scripts");
    write_pkgdir(&root, &format!("{BASE_MANIFEST}pkgmgr = opkg,apk\n"));

    let err = main_build(&root, Some(root.join("out")), None).expect_err("pkgmgr must be rejected");
    assert!(err.to_string().contains("must not declare `pkgmgr`"));
  }

  #[test]
  fn build_rejects_empty_package() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    write_pkgdir(&root, BASE_MANIFEST);

    let err = main_build(&root, Some(root.join("out")), None).expect_err("empty package");
    assert!(err.to_string().contains("would be empty"));
  }

  #[test]
  fn builds_a_scripts_only_package_for_every_backend() {
    for backend in ["opkg", "apk", "pica"] {
      let tmp = tempfile::tempdir().expect("create temp dir");
      let root = tmp.path().to_path_buf();
      fs::create_dir_all(root.join("scripts")).expect("mkdir scripts");
      fs::write(root.join("scripts/install"), "#!/bin/sh\nexit 0\n").expect("write script");

      build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = {backend}\n")).expect("build");

      let built = artifact(&root, backend);
      assert!(built.is_file(), "{}", built.display());
      assert!(artifact_entries(&built).contains("scripts/install"), "{backend}");
    }
  }

  #[test]
  fn builds_a_root_only_package() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("root/etc")).expect("mkdir root");
    fs::write(root.join("root/etc/demo.conf"), "x\n").expect("write config");

    build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = pica\n")).expect("build");

    let built = artifact(&root, "pica");
    assert!(built.is_file(), "{}", built.display());
    assert!(artifact_entries(&built).contains("root/etc/demo.conf"));
  }

  #[test]
  fn a_pica_target_expands_its_containers_into_root() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    let staging = root.join("payload/etc/demo");
    fs::create_dir_all(&staging).expect("mkdir payload");
    fs::write(staging.join("from-container"), "container\n").expect("write payload");
    let container = make_ipk(&root, "demo-lib", &[("etc/demo/from-container", "container\n")]);
    fs::create_dir_all(root.join("pkgs/pica")).expect("mkdir pkgs");
    fs::copy(&container, root.join("pkgs/pica/demo-lib.ipk")).expect("stage container");

    build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = pica\n")).expect("build");

    let entries = artifact_entries(&artifact(&root, "pica"));
    assert!(
      entries.contains("root/etc/demo/from-container"),
      "the container's payload belongs in root/ so the device only copies files: {entries}"
    );
    assert!(
      !entries.lines().any(|entry| entry.starts_with("pkgs/")),
      "a pica artifact must not ship containers: {entries}"
    );
  }

  #[test]
  fn a_pica_target_merges_containers_with_the_overlay() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    let container = make_ipk(&root, "demo-lib", &[("etc/demo/shared", "container\n")]);
    fs::create_dir_all(root.join("pkgs/pica")).expect("mkdir pkgs");
    fs::copy(&container, root.join("pkgs/pica/demo-lib.ipk")).expect("stage container");
    fs::create_dir_all(root.join("root/etc/demo")).expect("mkdir root");
    fs::write(root.join("root/etc/demo/shared"), "overlay\n").expect("write config");
    fs::write(root.join("root/etc/demo/only-root"), "root\n").expect("write config");

    build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = pica\n")).expect("build");

    let built = artifact(&root, "pica");
    let entries = artifact_entries(&built);
    assert!(entries.contains("root/etc/demo/only-root"), "{entries}");
    assert!(
      entries.contains("root/etc/demo/shared"),
      "root/ wins a path the container also provides, so the merge is not a conflict: {entries}"
    );
    assert_eq!(
      artifact_file(&built, "root/etc/demo/shared"),
      "overlay\n",
      "root/ winning has to mean its contents, not just a file with that name"
    );
  }

  #[test]
  fn two_containers_providing_one_file_fail_the_build() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    let a = make_ipk(&root, "demo-a", &[("etc/demo.conf", "from-a\n")]);
    let b = make_ipk(&root, "demo-b", &[("etc/demo.conf", "from-b\n")]);
    fs::create_dir_all(root.join("pkgs/pica")).expect("mkdir pkgs");
    fs::copy(&a, root.join("pkgs/pica/demo-a.ipk")).expect("stage a");
    fs::copy(&b, root.join("pkgs/pica/demo-b.ipk")).expect("stage b");

    let err = build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = pica\n"))
      .expect_err("two containers may not claim one path");
    assert!(err.contains("demo-a") && err.contains("demo-b"), "{err}");
    assert!(err.contains("etc/demo.conf"), "{err}");

    // The refusal happens while the payload is assembled, so nothing is written.
    let written: Vec<String> = fs::read_dir(root.join("out/pica"))
      .map(|entries| {
        entries.flatten().map(|entry| entry.file_name().to_string_lossy().into()).collect()
      })
      .unwrap_or_default();
    assert!(written.is_empty(), "a refused build must leave no artifact: {written:?}");
  }

  #[test]
  fn the_reported_size_is_what_the_containers_brought() {
    // The package ships only `pkgs/pica/`: everything it installs comes out of
    // the container, so a size taken before the merge would report zero.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    let container = make_ipk(&root, "demo-lib", &[("usr/lib/libdemo.so", "0123456789")]);
    fs::create_dir_all(root.join("pkgs/pica")).expect("mkdir pkgs");
    fs::copy(&container, root.join("pkgs/pica/demo-lib.ipk")).expect("stage container");

    build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = pica\n")).expect("build");

    let built = artifact(&root, "pica");
    assert_eq!(artifact_file(&built, "root/usr/lib/libdemo.so"), "0123456789");
    assert!(
      artifact_file(&built, "manifest").contains("size = 10"),
      "the artifact manifest must describe the payload the device will deploy:\n{}",
      artifact_file(&built, "manifest")
    );

    let index = fs::read_to_string(root.join("out/pica/repo.json")).expect("read index");
    let parsed = parse_repo_json(&index).expect("valid index");
    assert_eq!(parsed.packages[0].size, Some(10), "the index advertises the same number");
  }

  #[test]
  fn an_opkg_artifact_counts_what_its_containers_install() {
    // The package ships only `pkgs/opkg/`, and opkg installs it on the device:
    // a size taken over `root/` alone would advertise a package that lays ten
    // bytes down as one that installs nothing.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    let container = make_ipk(&root, "demo-lib", &[("usr/lib/libdemo.so", "0123456789")]);
    fs::create_dir_all(root.join("pkgs/opkg")).expect("mkdir pkgs");
    fs::copy(&container, root.join("pkgs/opkg/demo-lib.ipk")).expect("stage container");
    // `root/` is deployed too, so the number is the two of them together: the
    // count is of files a device ends up with, whichever side brought them.
    fs::create_dir_all(root.join("root/etc")).expect("mkdir root");
    fs::write(root.join("root/etc/demo.conf"), "abc\n").expect("write config");

    build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = opkg\n")).expect("build");

    let built = artifact(&root, "opkg");
    let manifest = artifact_file(&built, "manifest");
    assert!(
      manifest.contains("size = 14"),
      "10 bytes of container plus a 4 byte root/ file:\n{manifest}"
    );

    let index = fs::read_to_string(root.join("out/opkg/repo.json")).expect("read index");
    let parsed = parse_repo_json(&index).expect("valid index");
    assert_eq!(parsed.packages[0].size, Some(14), "the index advertises the same number");
  }

  #[test]
  fn a_container_payload_is_measured_after_unpacking_it() {
    // The bytes only exist once the container has been expanded — an `.ipk` is
    // an archive of an archive — so this is what says the count is not taken
    // from the size of the file that carries it.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    let pkgs = root.join("pkgs");
    fs::create_dir_all(&pkgs).expect("mkdir pkgs");
    let container = make_ipk(&root, "demo-lib", &[("usr/lib/libdemo.so", "0123456789")]);
    fs::copy(&container, pkgs.join("demo-lib.ipk")).expect("stage container");
    // A file that is not a container is reported as well, and does not stop the
    // container beside it from being measured.
    fs::write(pkgs.join("junk.ipk"), "not a container").expect("write junk");

    let measured = super::measure_containers(&pkgs).expect("measuring may not fail a build");

    assert_eq!(measured.bytes, 10, "the payload, not the 4 kB container around it");
    assert_eq!(measured.counted, ["demo-lib.ipk"]);
    assert_eq!(measured.unmeasured.len(), 1, "{:?}", measured.unmeasured);
    assert!(measured.unmeasured[0].contains("junk.ipk"), "{:?}", measured.unmeasured);
  }

  #[test]
  fn an_adb_container_is_reported_rather_than_counted_as_zero() {
    // apk-tools 3.x' ADB format is what an `apk` target normally ships, and tar
    // cannot open it: the build has to finish, and the number it prints has to
    // be known to be short of the truth rather than passed off as the size.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pkgs = tmp.path().join("pkgs");
    fs::create_dir_all(&pkgs).expect("mkdir pkgs");
    fs::write(pkgs.join("demo.apk"), b"ADBd\0\0\0\0payload").expect("write adb container");

    let measured = super::measure_containers(&pkgs).expect("an ADB container is not a build error");

    assert_eq!(measured.bytes, 0);
    assert!(measured.counted.is_empty(), "{:?}", measured.counted);
    assert_eq!(measured.unmeasured.len(), 1, "{:?}", measured.unmeasured);
    assert!(measured.unmeasured[0].contains("demo.apk"), "{:?}", measured.unmeasured);
    assert!(measured.unmeasured[0].contains("ADB"), "{:?}", measured.unmeasured);
  }

  #[test]
  fn an_apk_artifact_ships_a_container_it_cannot_read() {
    // The refusal to unpack has to stay out of the artifact's way: the device's
    // apk is the one that installs an ADB container, and it is the one that can
    // say what it holds.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("pkgs/apk")).expect("mkdir pkgs");
    fs::write(root.join("pkgs/apk/demo.apk"), b"ADBd\0\0\0\0payload").expect("write container");

    build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = apk\n"))
      .expect("an ADB container is not a build error");

    let built = artifact(&root, "apk");
    assert!(
      artifact_entries(&built).contains("pkgs/demo.apk"),
      "the container is what the artifact is for:\n{}",
      artifact_entries(&built)
    );
  }

  #[test]
  fn pica_target_with_declared_dependencies_fails() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("root/etc")).expect("mkdir root");
    fs::write(root.join("root/etc/demo.conf"), "x\n").expect("write config");

    let err =
      build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = pica\nbase = luci-base\n"))
        .expect_err("pica cannot resolve dependencies");
    assert!(err.contains("cannot resolve dependencies"), "{err}");
    assert!(err.contains("targets = opkg"), "the message must suggest opkg/apk: {err}");
    assert!(err.contains("base.opkg"), "the message must offer the scoped form: {err}");
  }

  #[test]
  fn scoped_dependencies_do_not_block_a_pica_build() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("root/etc")).expect("mkdir root");
    fs::write(root.join("root/etc/demo.conf"), "x\n").expect("write config");

    fs::create_dir_all(root.join("pkgs/opkg")).expect("mkdir pkgs");
    fs::write(root.join("pkgs/opkg/demo.ipk"), b"ipk").expect("write container");

    build_with_manifest(
      &root,
      &format!("{BASE_MANIFEST}targets = opkg, pica\nbase.opkg = luci-base\n"),
    )
    .expect("a dependency that only opkg installs must not block the pica target");

    assert!(artifact(&root, "pica").is_file());
  }

  #[test]
  fn pkgs_dir_for_an_unselected_backend_fails() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("root/etc")).expect("mkdir root");
    fs::write(root.join("root/etc/demo.conf"), "x\n").expect("write config");
    fs::create_dir_all(root.join("pkgs/opkg")).expect("mkdir pkgs");
    fs::write(root.join("pkgs/opkg/demo.ipk"), b"ipk").expect("write container");

    let err = build_with_manifest(&root, &format!("{BASE_MANIFEST}targets = apk\n"))
      .expect_err("containers for an unbuilt backend must not be dropped silently");
    assert!(err.contains("pkgs/opkg/ present but opkg is not in targets"), "{err}");
  }
}
