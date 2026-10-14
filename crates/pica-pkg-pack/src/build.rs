use crate::archive::{compute_sha256, run_tar_create, unpacked_size, write_sha256sum_entry};
use crate::backend::{parse_targets, BuildBackend};
use crate::matrix::{collect_archs, collect_platforms, has_platform_arch_matrix};
use crate::rewrite::{rewrite_manifest_for_build, BuiltFields};
use crate::{msg, msg2};
use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::{copy_dir_recursive, ensure_dir, make_temp_dir, resolve_script_dir_from_exe, write_atomic};
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::repo::{package_filename, parse_repo_json, Package, PackageIndex, REPO_SCHEMA};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Directory names inside a pkgdir. See docs/standard.md.
const MANIFEST: &str = "manifest";
const SCRIPTS: &str = "scripts";
const ROOT: &str = "root";
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
  pub install_size: u64,
}

pub(crate) fn main_build(pkgdir: &Path, outdir: Option<PathBuf>, pkgmgr: Option<&str>) -> PicaResult<()> {
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
  warn_on_unbuilt_backends(&pkgs_dir, &backends);

  if !has_payload(pkgdir, &pkgs_dir, &backends) {
    return Err(PicaError::msg(format!(
      "package would be empty: provide at least one of {SCRIPTS}/, {ROOT}/ or {PKGS}/<backend>/"
    )));
  }

  let install_size = unpacked_size(&pkgdir.join(ROOT));

  if pica.is_empty() {
    msg2("Pica requires: (not specified)");
  } else {
    msg2(format!("Pica requires >= {pica}"));
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
    let has_matrix = backend.is_native() && has_platform_arch_matrix(&backend_root);

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
            install_size,
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
        install_size,
      })?;
    }
  }

  Ok(())
}

fn has_payload(pkgdir: &Path, pkgs_dir: &Path, backends: &[BuildBackend]) -> bool {
  if pkgdir.join(SCRIPTS).is_dir() || pkgdir.join(ROOT).is_dir() {
    return true;
  }
  backends.iter().any(|backend| backend.is_native() && pkgs_dir.join(backend.as_str()).is_dir())
}

/// A pkgdir that ships `pkgs/apk/` but declares only `targets = opkg` is almost
/// always a forgotten declaration, and used to fail silently.
fn warn_on_unbuilt_backends(pkgs_dir: &Path, backends: &[BuildBackend]) {
  let Ok(entries) = fs::read_dir(pkgs_dir) else { return };

  for entry in entries.flatten() {
    let path = entry.path();
    if !path.is_dir() {
      continue;
    }
    let name = entry.file_name().to_string_lossy().to_string();
    let known = BuildBackend::parse(&name).is_ok();
    let selected = backends.iter().any(|backend| backend.as_str() == name);
    if known && !selected {
      msg2(format!("warning: {PKGS}/{name}/ present but not in targets, skipping"));
    }
  }
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

  msg(format!("Making package: {} {}-{} [{}]", req.pkgname, req.pkgver, req.pkgrel, req.backend.as_str()));
  msg2(format!("Platform: {}", req.build_platform));
  msg2(format!("Arch: {}", req.build_arch));
  msg2(format!("Install size: {} bytes", req.install_size));
  if req.pica.is_empty() {
    msg2("Pica requires: (not specified)");
  } else {
    msg2(format!("Pica requires >= {}", req.pica));
  }
  msg2("Creating archive...");

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
  rewrite_manifest_for_build(
    &req.pkgdir.join(MANIFEST),
    &tmpdir.join(MANIFEST),
    &BuiltFields {
      pkgver: req.pkgver,
      pkgrel: req.pkgrel,
      platform: req.build_platform,
      arch: req.build_arch,
      pkgmgr: req.backend.as_str(),
      size: req.install_size,
    },
  )?;

  let scripts_src = req.pkgdir.join(SCRIPTS);
  if scripts_src.is_dir() {
    copy_dir_recursive(&scripts_src, &tmpdir.join(SCRIPTS))?;
  }

  let root_src = req.pkgdir.join(ROOT);
  if root_src.is_dir() {
    copy_dir_recursive(&root_src, &tmpdir.join(ROOT))?;
  }

  if req.backend.is_native() {
    stage_packages(req, &tmpdir.join(PKGS))?;
  }

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
  write_repo_index(req, &built_manifest, pkgfile, &sha256, csize)?;

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

  let extension = req.backend.package_extension().expect("native backend has an extension");
  let copied = copy_packages_flat(&source, dst, extension)?;
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
      "no {extension} files found for the {} backend in {} (found: {})",
      req.backend.as_str(),
      source.display(),
      found.join(" ")
    )))
  }
}

fn copy_packages_flat(source: &Path, dst: &Path, extension: &str) -> PicaResult<usize> {
  let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
  collect_packages(source, extension, &mut files)?;
  if files.is_empty() {
    return Ok(0);
  }

  ensure_dir(dst)?;
  for (name, from) in &files {
    fs::copy(from, dst.join(name))?;
  }
  Ok(files.len())
}

fn collect_packages(dir: &Path, extension: &str, out: &mut BTreeMap<String, PathBuf>) -> PicaResult<()> {
  for entry in fs::read_dir(dir)? {
    let entry = entry?;
    let path = entry.path();
    if path.is_dir() {
      collect_packages(&path, extension, out)?;
      continue;
    }
    if !has_extension(&path, extension) {
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

fn has_extension(path: &Path, extension: &str) -> bool {
  path
    .extension()
    .and_then(|value| value.to_str())
    .is_some_and(|value| format!(".{value}").eq_ignore_ascii_case(extension))
}

/// Writes the per-backend repo index. Top-level fields are filled from the
/// built manifest so the index is queryable without unpacking the artifact,
/// and the embedded manifest keeps full array semantics for repeated keys.
fn write_repo_index(
  req: &BuildRequest<'_>,
  manifest: &Manifest,
  filename: &str,
  sha256: &str,
  csize: u64,
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
    size: Some(req.install_size),
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
  use super::{copy_packages_flat, list_extensions, main_build, unpacked_size, write_repo_index, BuildRequest};
  use crate::backend::BuildBackend;
  use pica_pkg_core::repo::parse_repo_json;
  use std::fs;
  use std::path::Path;

  fn write_pkgdir(root: &Path, manifest: &str) {
    fs::create_dir_all(root).expect("mkdir pkgdir");
    fs::write(root.join("manifest"), manifest).expect("write manifest");
  }

  const BASE_MANIFEST: &str = "pkgname = hello\nappname = hello\npkgver = 1.0.0\npkgrel = 1\nos = linux\n";

  #[test]
  fn copies_only_matching_extension_and_flattens() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("apk").join("all").join("all");
    fs::create_dir_all(&source).expect("mkdir nested");
    fs::write(source.join("one.apk"), b"apk").expect("write apk");
    fs::write(source.join("one.ipk"), b"ipk").expect("write ipk");

    let dst = tmp.path().join("out");
    let copied = copy_packages_flat(tmp.path().join("apk").as_path(), &dst, ".apk").expect("copy");
    assert_eq!(copied, 1);
    assert!(dst.join("one.apk").is_file());
    assert!(!dst.join("one.ipk").exists());
    assert_eq!(list_extensions(&dst), vec![".apk".to_string()]);
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

    let err = copy_packages_flat(tmp.path(), &tmp.path().join("out"), ".apk").expect_err("must fail");
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

    let manifest = pica_pkg_core::manifest::Manifest::from_file(pkgdir.join("manifest")).expect("parse");
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
      install_size: 4096,
    };
    write_repo_index(&req, &manifest, "luci-app-demo-1.0.0-1-all-apk.pkg.tar.gz", &"1".repeat(64), 2048)
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
    write_repo_index(&req, &manifest, "luci-app-demo-1.0.0-1-all-apk.pkg.tar.gz", &"2".repeat(64), 4096)
      .expect("rewrite index");
    let replaced = parse_repo_json(&fs::read_to_string(output.join("repo.json")).expect("read")).expect("valid");
    assert_eq!(replaced.packages.len(), 1);
    assert_eq!(replaced.packages[0].sha256, "2".repeat(64));
  }

  #[test]
  fn build_requires_os_field() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("scripts")).expect("mkdir scripts");
    fs::write(root.join("scripts/install"), "#!/bin/sh\nexit 0\n").expect("write script");
    write_pkgdir(&root, "pkgname = hello\nappname = hello\npkgver = 1.0.0\npkgrel = 1\nplatform = amd64\narch = x86_64\n");

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
}
