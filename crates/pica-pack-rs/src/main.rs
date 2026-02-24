use pica_core::error::{PicaError, PicaResult};
use pica_core::io::{
    copy_dir_recursive, ensure_dir, make_temp_dir, now_unix_secs, resolve_script_dir_from_exe,
};
use pica_core::manifest::Manifest;
use pica_core::repo::expected_filename;
use pica_core::PICA_VERSION;
use std::collections::{BTreeSet, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

fn usage() {
    println!(
        "Usage:\n  pica-pack-rs build <staging_dir> [--outdir DIR]\n\
\nstaging_dir must contain:\n  - manifest\n  - cmd/\n  - binary/ is optional\n  - depend/ is optional\n  - LICENSE is optional\n\
\nIf binary/ exists, the recommended layout is:\n  binary/<platform>/<arch>/*.ipk\n\
\nIf depend/ exists, the recommended layout is:\n  depend/<platform>/<arch>/*.ipk\n\
\nWhen such layout is present, pica-pack-rs will build one package per\n<platform>/<arch> combination.\n\
\nPackage filename:\n  <pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz"
    );
}

fn msg(text: impl AsRef<str>) {
    println!("==> {}", text.as_ref());
}

fn msg2(text: impl AsRef<str>) {
    println!("  -> {}", text.as_ref());
}

fn main() {
    if let Err(err) = run() {
        eprintln!("pica-pack: {err}");
        process::exit(1);
    }
}

fn run() -> PicaResult<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        usage();
        return Err(PicaError::msg("missing command"));
    };

    match command.as_str() {
        "-h" | "--help" | "help" => {
            usage();
            Ok(())
        }
        "--version" => {
            println!("{PICA_VERSION}");
            Ok(())
        }
        "build" => {
            let Some(staging_dir_arg) = args.next() else {
                return Err(PicaError::msg("build requires <staging_dir>"));
            };

            let mut outdir: Option<PathBuf> = None;
            let rest: Vec<String> = args.collect();
            let mut index = 0;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--outdir" => {
                        let Some(value) = rest.get(index + 1) else {
                            return Err(PicaError::msg("--outdir requires DIR"));
                        };
                        outdir = Some(PathBuf::from(value));
                        index += 2;
                    }
                    "-h" | "--help" => {
                        usage();
                        return Ok(());
                    }
                    other => {
                        return Err(PicaError::msg(format!("unknown arg: {other}")));
                    }
                }
            }

            main_build(Path::new(&staging_dir_arg), outdir)
        }
        other => Err(PicaError::msg(format!("unknown command: {other}"))),
    }
}

fn main_build(staging_dir: &Path, outdir: Option<PathBuf>) -> PicaResult<()> {
    if !staging_dir.is_dir() {
        return Err(PicaError::msg(format!(
            "not a directory: {}",
            staging_dir.display()
        )));
    }

    let manifest_file = staging_dir.join("manifest");
    let cmd_dir = staging_dir.join("cmd");
    if !manifest_file.is_file() {
        return Err(PicaError::msg(format!(
            "missing manifest in {}",
            staging_dir.display()
        )));
    }
    if !cmd_dir.is_dir() {
        return Err(PicaError::msg(format!(
            "missing cmd/ in {}",
            staging_dir.display()
        )));
    }

    let manifest = Manifest::from_file(&manifest_file)?;

    let pkgname = manifest.require_non_empty("pkgname")?;
    let pkgver = manifest.require_non_empty("pkgver")?;
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
            return Err(PicaError::msg(
                "matrix layout detected but neither binary/ nor depend/ exists",
            ));
        }

        let platforms = collect_platforms(&roots)?;
        for build_platform in platforms {
            let archs = collect_archs(&roots, &build_platform)?;
            for build_arch in archs {
                build_one(BuildRequest {
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
        build_one(BuildRequest {
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

struct BuildRequest<'a> {
    staging_dir: &'a Path,
    output_dir: &'a Path,
    pkgname: &'a str,
    pkgver: &'a str,
    pkgrel: &'a str,
    build_platform: &'a str,
    build_arch: &'a str,
    pica: &'a str,
    has_matrix: bool,
}

fn build_one(req: BuildRequest<'_>) -> PicaResult<()> {
    let pkgfile = expected_filename(
        req.pkgname,
        req.pkgver,
        req.pkgrel,
        req.build_platform,
        req.build_arch,
    );

    msg(format!(
        "Making package: {} {}-{}",
        req.pkgname, req.pkgver, req.pkgrel
    ));
    msg2(format!("Platform: {}", req.build_platform));
    msg2(format!("Arch: {}", req.build_arch));
    if req.pica.is_empty() {
        msg2("Pica requires: (not specified)");
    } else {
        msg2(format!("Pica requires >= {}", req.pica));
    }
    msg2("Creating archive...");

    let tmpdir = make_temp_dir("pica-pack-rs")?;

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
    if tmpdir.join("LICENSE").is_file() {
        tar_items.push("LICENSE".to_string());
    }

    let pkg_tmp = tmpdir.join(&pkgfile);
    run_tar_create(&tmpdir, &pkg_tmp, &tar_items)?;

    let pkg_size = fs::metadata(&pkg_tmp)?.len();
    append_manifest_kv(&manifest_dst, "size", &pkg_size.to_string())?;

    let final_pkg = req.output_dir.join(&pkgfile);
    run_tar_create(&tmpdir, &final_pkg, &tar_items)?;

    let sha256 = file_sha256_hex(&final_pkg)?;
    let sums_file = req.output_dir.join("SHA256SUMS");
    write_sha256sum_entry(&sums_file, &pkgfile, &sha256)?;

    msg(format!("Finished: {}", final_pkg.display()));
    println!("{}", final_pkg.display());

    fs::remove_dir_all(&tmpdir)?;
    Ok(())
}

fn file_sha256_hex(path: &Path) -> PicaResult<String> {
    if let Ok(output) = Command::new("sha256sum").arg(path).output() {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(sum) = text.split_whitespace().next() {
                if !sum.is_empty() {
                    return Ok(sum.to_string());
                }
            }
        }
    }

    if let Ok(output) = Command::new("openssl")
        .arg("dgst")
        .arg("-sha256")
        .arg(path)
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some((_, sum)) = text.rsplit_once("= ") {
                let trimmed = sum.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
            }
        }
    }

    Err(PicaError::msg(
        "cannot compute sha256: need command 'sha256sum' or 'openssl'",
    ))
}

fn write_sha256sum_entry(path: &Path, filename: &str, sha256: &str) -> PicaResult<()> {
    let mut lines: Vec<String> = if path.is_file() {
        fs::read_to_string(path)?
            .lines()
            .map(ToString::to_string)
            .collect()
    } else {
        Vec::new()
    };

    lines.retain(|line| {
        let trimmed = line.trim_end();
        !trimmed.ends_with(&format!("  {filename}"))
    });
    lines.push(format!("{sha256}  {filename}"));
    lines.sort();

    let mut content = lines.join("\n");
    content.push('\n');
    fs::write(path, content)?;
    Ok(())
}

fn rewrite_manifest_for_build(
    source: &Path,
    target: &Path,
    pkgver: &str,
    pkgrel: &str,
    platform: &str,
    arch: &str,
) -> PicaResult<()> {
    let content = fs::read_to_string(source)?;
    let mut lines = Vec::new();
    let remove_keys: HashSet<&str> = [
        "builddate",
        "size",
        "platform",
        "arch",
        "pkgver",
        "pkgrel",
        "uname",
    ]
    .into_iter()
    .collect();

    for raw_line in content.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with('#') {
            lines.push(raw_line.to_string());
            continue;
        }

        let Some((raw_key, _)) = raw_line.split_once('=') else {
            lines.push(raw_line.to_string());
            continue;
        };

        let key = raw_key.trim();
        if remove_keys.contains(key) {
            continue;
        }
        lines.push(raw_line.to_string());
    }

    lines.push(format!("builddate = {}", now_unix_secs()));
    lines.push(format!("pkgver = {pkgver}"));
    lines.push(format!("pkgrel = {pkgrel}"));
    lines.push(format!("platform = {platform}"));
    lines.push(format!("arch = {arch}"));

    let mut output = lines.join("\n");
    output.push('\n');
    fs::write(target, output)?;
    Ok(())
}

fn append_manifest_kv(path: &Path, key: &str, value: &str) -> PicaResult<()> {
    let mut content = fs::read_to_string(path)?;
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(key);
    content.push_str(" = ");
    content.push_str(value);
    content.push('\n');
    fs::write(path, content)?;
    Ok(())
}

fn run_tar_create(base_dir: &Path, output: &Path, items: &[String]) -> PicaResult<()> {
    let mut command = Command::new("tar");
    command.arg("-C");
    command.arg(base_dir);
    command.arg("-czf");
    command.arg(output);
    for item in items {
        command.arg(item);
    }

    let result = command.output()?;
    if result.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
        Err(PicaError::msg(format!(
            "tar create failed: {}",
            if stderr.is_empty() {
                "unknown error"
            } else {
                &stderr
            }
        )))
    }
}

fn has_platform_arch_matrix(root: &Path) -> bool {
    if !root.is_dir() {
        return false;
    }

    let Ok(platform_entries) = fs::read_dir(root) else {
        return false;
    };
    for platform_entry in platform_entries.flatten() {
        let platform_path = platform_entry.path();
        if !platform_path.is_dir() {
            continue;
        }
        let Ok(arch_entries) = fs::read_dir(platform_path) else {
            continue;
        };
        for arch_entry in arch_entries.flatten() {
            if arch_entry.path().is_dir() {
                return true;
            }
        }
    }

    false
}

fn collect_platforms(roots: &[&Path]) -> PicaResult<Vec<String>> {
    let mut platforms = BTreeSet::new();
    for root in roots {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.path().is_dir() {
                platforms.insert(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    Ok(platforms.into_iter().collect())
}

fn collect_archs(roots: &[&Path], platform: &str) -> PicaResult<Vec<String>> {
    let mut archs = BTreeSet::new();
    for root in roots {
        let platform_path = root.join(platform);
        if !platform_path.is_dir() {
            continue;
        }
        for entry in fs::read_dir(platform_path)? {
            let entry = entry?;
            if entry.path().is_dir() {
                archs.insert(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    Ok(archs.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmp_dir(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("pica-pack-test-{name}-{now}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    #[test]
    fn matrix_detection_and_collection_work() {
        let root = unique_tmp_dir("matrix");
        let binary = root.join("binary");
        fs::create_dir_all(binary.join("mt7621").join("all")).expect("mkdir binary mt7621/all");
        fs::create_dir_all(binary.join("mt7621").join("arm")).expect("mkdir binary mt7621/arm");
        fs::create_dir_all(binary.join("x86").join("all")).expect("mkdir binary x86/all");

        assert!(has_platform_arch_matrix(&binary));

        let platforms = collect_platforms(&[binary.as_path()]).expect("collect platforms");
        assert_eq!(platforms, vec!["mt7621".to_string(), "x86".to_string()]);

        let archs = collect_archs(&[binary.as_path()], "mt7621").expect("collect archs");
        assert_eq!(archs, vec!["all".to_string(), "arm".to_string()]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_manifest_for_build_replaces_runtime_fields() {
        let root = unique_tmp_dir("manifest");
        let source = root.join("manifest.src");
        let target = root.join("manifest.dst");

        fs::write(
            &source,
            "pkgname = hello\nplatform = old\narch = old\npkgver = 0.0.1\npkgrel = 1\nsize = 1\nbuilddate = 1\n",
        )
        .expect("write source manifest");

        rewrite_manifest_for_build(&source, &target, "1.2.3", "4", "all", "arm64")
            .expect("rewrite manifest");

        let out = fs::read_to_string(&target).expect("read target manifest");
        assert!(out.contains("pkgname = hello"));
        assert!(out.contains("pkgver = 1.2.3"));
        assert!(out.contains("pkgrel = 4"));
        assert!(out.contains("platform = all"));
        assert!(out.contains("arch = arm64"));
        assert!(out.contains("builddate = "));
        assert!(!out.contains("size = 1"));
        assert!(!out.contains("platform = old"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_manifest_supports_legacy_pkgver_without_pkgrel() {
        let root = unique_tmp_dir("legacy-pkgver");
        let source = root.join("manifest.src");
        let target = root.join("manifest.dst");

        fs::write(
            &source,
            "pkgname = hello\npkgver = 0.1.0-7\nplatform = old\narch = old\n",
        )
        .expect("write source manifest");

        rewrite_manifest_for_build(&source, &target, "0.1.0", "7", "all", "all")
            .expect("rewrite manifest");

        let out = fs::read_to_string(&target).expect("read target manifest");
        assert!(out.contains("pkgver = 0.1.0"));
        assert!(out.contains("pkgrel = 7"));
        assert!(out.contains("platform = all"));
        assert!(out.contains("arch = all"));
        assert!(!out.contains("platform = old"));

        let _ = fs::remove_dir_all(&root);
    }
}
