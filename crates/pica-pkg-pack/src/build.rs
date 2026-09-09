use crate::archive::{compute_sha256, run_tar_create, write_sha256sum_entry};
use crate::backend::{parse_targets, BuildBackend};
use crate::platform::{collect_archs, collect_platforms, has_platform_arch_matrix};
use crate::rewrite::{append_manifest_kv, rewrite_manifest_for_build};
use crate::{msg, msg2};
use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::{
  copy_dir_recursive, ensure_dir, make_temp_dir, resolve_script_dir_from_exe,
};
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::repo::{expected_filename, parse_repo_json, Package, PackageIndex};
use std::fs;
use std::path::{Path, PathBuf};
use serde_json::Value;

pub(crate) struct BuildRequest<'a> {
  pub staging_dir: &'a Path,
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

pub(crate) fn main_build(staging_dir: &Path, outdir: Option<PathBuf>, pkgmgr: Option<&str>) -> PicaResult<()> {
  if !staging_dir.is_dir() {
    return Err(PicaError::msg(format!("not a directory: {}", staging_dir.display())));
  }

  let manifest_file = staging_dir.join("manifest");
  let cmd_dir = staging_dir.join("cmd");
  if !manifest_file.is_file() {
    return Err(PicaError::msg(format!("missing manifest in {}", staging_dir.display())));
  }
  if !cmd_dir.is_dir() {
    return Err(PicaError::msg(format!("missing cmd/ in {}", staging_dir.display())));
  }

  let manifest = Manifest::from_file(&manifest_file)?;

  let pkgname = manifest.require_non_empty("pkgname")?;
  let _appname = manifest.require_non_empty("appname")?;
  let pkgver = manifest.require_non_empty("pkgver")?;
  let _pkg_os = manifest.require_non_empty("os")?;
  let mut pkgrel = manifest.get_first("pkgrel");
  let mut platform = manifest.get_first("platform");
  let mut arch = manifest.get_first("arch");
  let pica = manifest.get_first("pica");
  let backends = parse_targets(pkgmgr, &manifest.get_first("pkgmgr"))?;

  let pkgver = if pkgrel.is_empty() {
    if let Some((legacy_pkgver, legacy_pkgrel)) = pkgver.split_once('-') {
      pkgrel = legacy_pkgrel.to_string();
      legacy_pkgver.to_string()
    } else {
      pkgrel = "1".to_string();
      pkgver
    }
  } else {
    pkgver
  };

  if platform.is_empty() {
    platform = "all".to_string();
  }
  if arch.is_empty() {
    arch = "all".to_string();
  }

  if pkgrel.is_empty() {
    return Err(PicaError::msg("manifest missing pkgrel"));
  }

  if pica.is_empty() {
    msg2("Pica requires: (not specified)");
  } else {
    msg2(format!("Pica requires >= {pica}"));
  }

  let output_dir = if let Some(path) = outdir {
    path
  } else {
    let script_dir = resolve_script_dir_from_exe()?;
    script_dir.join("bin").join(&pkgname)
  };
  ensure_dir(&output_dir)?;
  let binary_dir = staging_dir.join("binary");
  let depend_dir = staging_dir.join("depend");
  let has_matrix = has_platform_arch_matrix(&binary_dir) || has_platform_arch_matrix(&depend_dir);
  let mut roots = Vec::new();
  if binary_dir.is_dir() { roots.push(binary_dir.as_path()); }
  if depend_dir.is_dir() { roots.push(depend_dir.as_path()); }

  for backend in backends {
    let backend_output = if backend == BuildBackend::Opkg { output_dir.clone() } else { output_dir.join(backend.as_str()) };
    ensure_dir(&backend_output)?;
    if has_matrix {
      if roots.is_empty() {
        return Err(PicaError::msg("matrix layout detected but neither binary/ nor depend/ exists"));
      }
      let platforms = collect_platforms(&roots)?;
      for build_platform in platforms {
        let archs = collect_archs(&roots, &build_platform)?;
        for build_arch in archs {
          build_one(&BuildRequest { staging_dir, output_dir: &backend_output, pkgname: &pkgname, pkgver: &pkgver, pkgrel: &pkgrel, build_platform: &build_platform, build_arch: &build_arch, pica: &pica, has_matrix, backend })?;
        }
      }
    } else {
      build_one(&BuildRequest { staging_dir, output_dir: &backend_output, pkgname: &pkgname, pkgver: &pkgver, pkgrel: &pkgrel, build_platform: &platform, build_arch: &arch, pica: &pica, has_matrix, backend })?;
    }
  }

  Ok(())
}

pub(crate) fn build_one(req: &BuildRequest<'_>) -> PicaResult<()> {
  let base_pkgfile =
    expected_filename(req.pkgname, req.pkgver, req.pkgrel, req.build_platform, req.build_arch);
  let pkgfile = if req.backend == BuildBackend::Apk {
    base_pkgfile.replace(".pkg.tar.gz", "-apk.pkg.tar.gz")
  } else {
    base_pkgfile
  };

  msg(format!("Making package: {} {}-{}", req.pkgname, req.pkgver, req.pkgrel));
  msg2(format!("Platform: {}", req.build_platform));
  msg2(format!("Arch: {}", req.build_arch));
  if req.pica.is_empty() {
    msg2("Pica requires: (not specified)");
  } else {
    msg2(format!("Pica requires >= {}", req.pica));
  }
  msg2("Creating archive...");

  let tmpdir = make_temp_dir("pica-pack")?;

  let manifest_src = req.staging_dir.join("manifest");
  let manifest_dst = tmpdir.join("manifest");
  rewrite_manifest_for_build(
    &manifest_src,
    &manifest_dst,
    req.pkgver,
    req.pkgrel,
    req.build_platform,
    req.build_arch,
    req.backend.as_str(),
  )?;

  let cmd_src = req.staging_dir.join("cmd");
  let cmd_dst = tmpdir.join("cmd");
  copy_dir_recursive(&cmd_src, &cmd_dst)?;

  let binary_src = req.staging_dir.join("binary");
  if binary_src.is_dir() {
    let selected = if req.has_matrix {
      let path = binary_src.join(req.build_platform).join(req.build_arch);
      if !path.is_dir() {
        return Err(PicaError::msg(format!("missing binary/{}/{}", req.build_platform, req.build_arch)));
      }
      path
    } else {
      binary_src
    };
    copy_backend_files(&selected, &tmpdir.join("binary"), req.backend)?;
  }

  let depend_src = req.staging_dir.join("depend");
  if depend_src.is_dir() {
    if req.has_matrix {
      let selected = depend_src.join(req.build_platform).join(req.build_arch);
      if selected.is_dir() {
        copy_backend_files(&selected, &tmpdir.join("depend"), req.backend)?;
      }
    } else {
      copy_backend_files(&depend_src, &tmpdir.join("depend"), req.backend)?;
    }
  }

  let src_src = req.staging_dir.join("src");
  if src_src.is_dir() {
    copy_dir_recursive(&src_src, &tmpdir.join("src"))?;
  }

  let license_src = req.staging_dir.join("LICENSE");
  if license_src.is_file() {
    fs::copy(&license_src, tmpdir.join("LICENSE"))?;
  }

  let mut tar_items = vec!["manifest".to_string(), "cmd".to_string()];
  if tmpdir.join("binary").is_dir() {
    tar_items.push("binary".to_string());
  }
  if tmpdir.join("depend").is_dir() {
    tar_items.push("depend".to_string());
  }
  if tmpdir.join("src").is_dir() {
    tar_items.push("src".to_string());
  }
  if tmpdir.join("LICENSE").is_file() {
    tar_items.push("LICENSE".to_string());
  }

  let pkg_tmp = tmpdir.join(&pkgfile);
  run_tar_create(&tmpdir, &pkg_tmp, &tar_items)?;

  let pkg_size = fs::metadata(&pkg_tmp)?.len();
  append_manifest_kv(&manifest_dst, "size", &pkg_size.to_string())?;

  let final_pkg = req.output_dir.join(&pkgfile);
  let final_tmp = req.output_dir.join(format!(".{pkgfile}.tmp-{}", std::process::id()));
  run_tar_create(&tmpdir, &final_tmp, &tar_items)?;
  fs::rename(&final_tmp, &final_pkg)?;

  let sha256 = compute_sha256(&final_pkg)?;
  let sums_file = req.output_dir.join("SHA256SUMS");
  write_sha256sum_entry(&sums_file, &pkgfile, &sha256)?;
  write_backend_repo_index(req.output_dir, req, &pkgfile, &sha256)?;

  msg(format!("Finished: {}", final_pkg.display()));
  println!("{}", final_pkg.display());

  fs::remove_dir_all(&tmpdir)?;
  Ok(())
}

fn write_backend_repo_index(
  output_dir: &Path,
  req: &BuildRequest<'_>,
  filename: &str,
  sha256: &str,
) -> PicaResult<()> {
  let index_path = output_dir.join("repo.json");
  let mut index = if index_path.is_file() {
    let content = fs::read_to_string(&index_path)?;
    parse_repo_json(&content).unwrap_or(PackageIndex { schema: 1, packages: Vec::new() })
  } else {
    PackageIndex { schema: 1, packages: Vec::new() }
  };
  index.packages.retain(|package| package.filename != filename);
  let manifest = fs::read_to_string(req.staging_dir.join("manifest"))?;
  let mut manifest_value = serde_json::Map::new();
  for line in manifest.lines() {
    let Some((key, value)) = line.split_once('=') else { continue };
    let key = key.trim();
    let value = value.trim();
    if !key.is_empty() && !value.is_empty() {
      manifest_value.insert(key.to_string(), Value::String(value.to_string()));
    }
  }
  manifest_value.insert("pkgmgr".to_string(), Value::String(req.backend.as_str().to_string()));
  index.packages.push(Package {
    pkgname: req.pkgname.to_string(),
    pkgver: req.pkgver.to_string(),
    pkgrel: req.pkgrel.to_string(),
    platform: req.build_platform.to_string(),
    arch: req.build_arch.to_string(),
    filename: filename.to_string(),
    sha256: sha256.to_string(),
    appname: Some(req.pkgname.to_string()),
    branch: None,
    protocol: None,
    url: None,
    origin: None,
    luci_url: None,
    luci_desc: None,
    pica: (!req.pica.is_empty()).then(|| req.pica.to_string()),
    download_url: None,
    manifest: Some(Value::Object(manifest_value)),
  });
  let content = serde_json::to_vec_pretty(&index)?;
  pica_pkg_core::io::write_atomic(&index_path, &content)?;
  Ok(())
}

fn copy_backend_files(source: &Path, target: &Path, backend: BuildBackend) -> PicaResult<()> {
  if backend == BuildBackend::None {
    return copy_dir_recursive(source, target);
  }
  ensure_dir(target)?;
  let extension = backend.input_extension().expect("non-none backend has an extension");
  let mut copied = 0usize;
  for entry in fs::read_dir(source)? {
    let entry = entry?;
    let from = entry.path();
    let to = target.join(entry.file_name());
    let metadata = fs::symlink_metadata(&from)?;
    if metadata.is_dir() {
      copied += copy_backend_files_count(&from, &to, extension)?;
    } else if metadata.is_file() {
      if is_native_package(&from) {
        if has_extension(&from, extension) {
          fs::copy(&from, &to)?;
          copied += 1;
        }
      } else {
        fs::copy(&from, &to)?;
      }
    }
  }
  if copied == 0 && source.read_dir()?.next().is_some() {
    return Err(PicaError::msg(format!("no {extension} files found for {} backend in {}", backend.as_str(), source.display())));
  }
  Ok(())
}

fn copy_backend_files_count(source: &Path, target: &Path, extension: &str) -> PicaResult<usize> {
  ensure_dir(target)?;
  let mut copied = 0usize;
  for entry in fs::read_dir(source)? {
    let entry = entry?;
    let from = entry.path();
    let to = target.join(entry.file_name());
    let metadata = fs::symlink_metadata(&from)?;
    if metadata.is_dir() {
      copied += copy_backend_files_count(&from, &to, extension)?;
    } else if metadata.is_file() {
      if is_native_package(&from) {
        if has_extension(&from, extension) {
          fs::copy(&from, &to)?;
          copied += 1;
        }
      } else {
        fs::copy(&from, &to)?;
      }
    }
  }
  Ok(copied)
}

fn is_native_package(path: &Path) -> bool {
  has_extension(path, ".ipk") || has_extension(path, ".apk")
}

fn has_extension(path: &Path, extension: &str) -> bool {
  path.extension()
    .and_then(|value| value.to_str())
    .is_some_and(|value| format!(".{value}").eq_ignore_ascii_case(extension))
}

#[cfg(test)]
mod tests {
  use super::{copy_backend_files, main_build, write_backend_repo_index};
  use pica_pkg_core::repo::parse_repo_json;
  use crate::backend::BuildBackend;
  use std::fs;

  #[test]
  fn backend_filter_keeps_shared_files_and_target_packages() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("source");
    let target = tmp.path().join("target");
    fs::create_dir_all(&source).expect("mkdir source");
    fs::write(source.join("shared.txt"), "shared").expect("write shared");
    fs::write(source.join("one.ipk"), "ipk").expect("write ipk");
    fs::write(source.join("one.apk"), "apk").expect("write apk");

    copy_backend_files(&source, &target, BuildBackend::Apk).expect("filter apk files");
    assert!(target.join("shared.txt").is_file());
    assert!(target.join("one.apk").is_file());
    assert!(!target.join("one.ipk").exists());
  }

  #[test]
  fn backend_repo_index_replaces_same_filename_and_validates() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let staging = tmp.path().join("staging");
    let output = tmp.path().join("output");
    fs::create_dir_all(&staging).expect("mkdir staging");
    fs::create_dir_all(&output).expect("mkdir output");
    fs::write(
      staging.join("manifest"),
      "pkgname = hello\nappname = hello\npkgver = 1.0.0\npkgrel = 1\nos = linux\n",
    )
    .expect("write manifest");
    let request = super::BuildRequest {
      staging_dir: &staging,
      output_dir: &output,
      pkgname: "hello",
      pkgver: "1.0.0",
      pkgrel: "1",
      build_platform: "all",
      build_arch: "all",
      pica: "",
      has_matrix: false,
      backend: BuildBackend::Apk,
    };
    write_backend_repo_index(&output, &request, "hello-1.0.0-1-all-apk.pkg.tar.gz", &"1".repeat(64))
      .expect("write index");
    let first = fs::read_to_string(output.join("repo.json")).expect("read index");
    write_backend_repo_index(&output, &request, "hello-1.0.0-1-all-apk.pkg.tar.gz", &"2".repeat(64))
      .expect("replace index");
    let parsed = parse_repo_json(&fs::read_to_string(output.join("repo.json")).expect("read replaced index"))
      .expect("valid index");
    assert_eq!(parsed.packages.len(), 1);
    assert_eq!(parsed.packages[0].sha256, "2".repeat(64));
    assert_ne!(first, fs::read_to_string(output.join("repo.json")).expect("read final index"));
  }

  #[test]
  fn main_build_requires_os_field() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("cmd")).expect("mkdir cmd");
    fs::write(root.join("cmd").join("install"), "#!/bin/sh\nexit 0\n").expect("write cmd");
    fs::write(
      root.join("manifest"),
      "pkgname = hello\nappname = hello\npkgver = 1.0.0\npkgrel = 1\nplatform = amd64\narch = x86_64\n",
    )
    .expect("write manifest");

    let err = main_build(&root, Some(root.join("out")), None).expect_err("missing os must fail");
    assert!(err.to_string().contains("manifest missing os"));
  }
}
