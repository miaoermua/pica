use crate::archive::{compute_sha256, run_tar_create, write_sha256sum_entry};
use crate::platform::{collect_archs, collect_platforms, has_platform_arch_matrix};
use crate::rewrite::{append_manifest_kv, rewrite_manifest_for_build};
use crate::{msg, msg2};
use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::io::{
  copy_dir_recursive, ensure_dir, make_temp_dir, resolve_script_dir_from_exe,
};
use pica_pkg_core::manifest::Manifest;
use pica_pkg_core::repo::expected_filename;
use std::fs;
use std::path::{Path, PathBuf};

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
}

pub(crate) fn main_build(staging_dir: &Path, outdir: Option<PathBuf>) -> PicaResult<()> {
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

  if has_matrix {
    let mut roots = Vec::new();
    if binary_dir.is_dir() {
      roots.push(binary_dir.as_path());
    }
    if depend_dir.is_dir() {
      roots.push(depend_dir.as_path());
    }

    if roots.is_empty() {
      return Err(PicaError::msg("matrix layout detected but neither binary/ nor depend/ exists"));
    }

    let platforms = collect_platforms(&roots)?;
    for build_platform in platforms {
      let archs = collect_archs(&roots, &build_platform)?;
      for build_arch in archs {
        build_one(&BuildRequest {
          staging_dir,
          output_dir: &output_dir,
          pkgname: &pkgname,
          pkgver: &pkgver,
          pkgrel: &pkgrel,
          build_platform: &build_platform,
          build_arch: &build_arch,
          pica: &pica,
          has_matrix,
        })?;
      }
    }
  } else {
    build_one(&BuildRequest {
      staging_dir,
      output_dir: &output_dir,
      pkgname: &pkgname,
      pkgver: &pkgver,
      pkgrel: &pkgrel,
      build_platform: &platform,
      build_arch: &arch,
      pica: &pica,
      has_matrix,
    })?;
  }

  Ok(())
}

pub(crate) fn build_one(req: &BuildRequest<'_>) -> PicaResult<()> {
  let pkgfile =
    expected_filename(req.pkgname, req.pkgver, req.pkgrel, req.build_platform, req.build_arch);

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
  )?;

  let cmd_src = req.staging_dir.join("cmd");
  let cmd_dst = tmpdir.join("cmd");
  copy_dir_recursive(&cmd_src, &cmd_dst)?;

  let binary_src = req.staging_dir.join("binary");
  if binary_src.is_dir() {
    if req.has_matrix {
      let selected = binary_src.join(req.build_platform).join(req.build_arch);
      if !selected.is_dir() {
        return Err(PicaError::msg(format!(
          "missing binary/{}/{}",
          req.build_platform, req.build_arch
        )));
      }
      copy_dir_recursive(&selected, &tmpdir.join("binary"))?;
    } else {
      copy_dir_recursive(&binary_src, &tmpdir.join("binary"))?;
    }
  }

  let depend_src = req.staging_dir.join("depend");
  if depend_src.is_dir() {
    if req.has_matrix {
      let selected = depend_src.join(req.build_platform).join(req.build_arch);
      if selected.is_dir() {
        copy_dir_recursive(&selected, &tmpdir.join("depend"))?;
      }
    } else {
      copy_dir_recursive(&depend_src, &tmpdir.join("depend"))?;
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
  run_tar_create(&tmpdir, &final_pkg, &tar_items)?;

  let sha256 = compute_sha256(&final_pkg)?;
  let sums_file = req.output_dir.join("SHA256SUMS");
  write_sha256sum_entry(&sums_file, &pkgfile, &sha256)?;

  msg(format!("Finished: {}", final_pkg.display()));
  println!("{}", final_pkg.display());

  fs::remove_dir_all(&tmpdir)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::main_build;
  use std::fs;

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

    let err = main_build(&root, Some(root.join("out"))).expect_err("missing os must fail");
    assert!(err.to_string().contains("manifest missing os"));
  }
}
