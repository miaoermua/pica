use crate::{
    build_precheck_report, canonicalize_display, conf_get_i18n, copy_dir_recursive,
    detect_luci_variant, detect_opkg_arches, detect_platform, ensure_dir, ensure_dirs, install_via_feeds_or_ipk,
    make_temp_dir, manifest_get_first, normalize_uname, pkg_list_diff_added, pkgver_cmp_key,
    pkgver_ge, precheck_assert_no_missing, reorder_app_list, required_manifest_field,
    resolve_lang, run_hook, ver_ge, write_file_atomic, detect_os, App, CliError, CliResult, Manifest, Selector,
    E_CONFIG_INVALID, E_IO, E_MANIFEST_INVALID, E_PACKAGE_INVALID, E_PLATFORM_UNSUPPORTED,
    E_REPO_INVALID, E_VERSION_INCOMPATIBLE, E_INTEGRITY_INVALID, PICA_VERSION,
};
use crate::state::{
    db_find_installed_pkgname_by_selector, db_has_installed, db_set_installed, read_json_file,
    report_set_install_result,
};
use crate::system::{
    fetch_url, need_cmd, opkg_has_package, opkg_install_pkg, opkg_installed_version,
    opkg_is_installed, opkg_snapshot_installed, opkg_update_ignore, run_command_text,
    run_tar_extract,
};
use pica_core::io::now_unix_secs;
use pica_core::repo::is_supported_url;
use serde_json::{json, Value};
use std::process::Command;
use std::fs;
use std::path::Path;

struct TempDirGuard {
    path: std::path::PathBuf,
}

impl TempDirGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn install_app_auto(app: &mut App, selector: &str) -> CliResult<()> {
    ensure_dirs(&app.paths)?;

    let parsed = Selector::parse(selector)
        .map_err(|err| CliError::new(E_CONFIG_INVALID, err))?;
    let installed_pkg = db_find_installed_pkgname_by_selector(&app.paths.db_file, &parsed)?;
    if let Some(pkgname) = installed_pkg {
        let db = read_json_file(&app.paths.db_file)?;
        let source = db
            .get("installed")
            .and_then(Value::as_object)
            .and_then(|installed| installed.get(&pkgname))
            .and_then(|entry| entry.get("manifest"))
            .map(|manifest| manifest_get_first(manifest, "source"))
            .unwrap_or_default();

        match source.as_str() {
            "opkg" => {
                install_app_via_opkg(app, selector)?;
                return Ok(());
            }
            "pica" => {
                install_pica_from_repo(app, selector)?;
                return Ok(());
            }
            _ => {}
        }
    }

    opkg_update_ignore();

    if !Selector::is_structured(selector) && opkg_has_package(&parsed.appname) {
        let should_install_opkg = if app.options.non_interactive {
            matches!(
                app.options.feed_policy,
                crate::FeedPolicy::Ask
                    | crate::FeedPolicy::FeedFirst
                    | crate::FeedPolicy::FeedOnly
            )
        } else {
            !matches!(app.options.feed_policy, crate::FeedPolicy::PackagedOnly)
        };

        if should_install_opkg {
            install_app_via_opkg(app, selector)?;
            return Ok(());
        }
    }

    install_pica_from_repo(app, selector)
}

pub fn install_app_via_opkg(app: &mut App, selector: &str) -> CliResult<()> {
    ensure_dirs(&app.paths)?;
    need_cmd("opkg")?;

    let parsed = Selector::parse(selector)
        .map_err(|err| CliError::new(E_CONFIG_INVALID, err))?;
    let appname = parsed.appname.clone();

    opkg_update_ignore();
    let lang = conf_get_i18n(&app.paths.conf_file).unwrap_or_else(|| "zh-cn".to_string());

    let mut candidates = vec![appname.clone(), format!("luci-app-{appname}")];
    if lang == "zh-cn" {
        candidates.push(format!("luci-i18n-{appname}-{lang}"));
    }

    let mut to_install = Vec::new();
    for pkg in candidates {
        if opkg_has_package(&pkg) {
            to_install.push(pkg);
        }
    }

    if to_install.is_empty() {
        return Err(CliError::new(
            E_CONFIG_INVALID,
            format!("opkg: package not found: {appname}"),
        ));
    }

    app.log_info(format!("Installing (opkg): {}", to_install.join(" ")));

    for pkg in &to_install {
        opkg_install_pkg("opkg", pkg)?;
    }

    let mut base_ver = opkg_installed_version(&appname).unwrap_or_default();
    if base_ver.is_empty() {
        if let Some(first_pkg) = to_install.first() {
            base_ver = opkg_installed_version(first_pkg).unwrap_or_default();
        }
    }

    let manifest = json!({
        "pkgname": appname,
        "appname": appname,
        "branch": parsed.branch,
        "pkgver": base_ver,
        "os": detect_os(),
        "platform": detect_platform(),
        "arch": "all",
        "source": "opkg",
        "pkgmgr": "opkg",
        "opkg": to_install,
    });

    db_set_installed(&app.paths.db_file, &appname, manifest, "opkg", &to_install)?;
    app.log_info("Transaction completed");
    Ok(())
}

pub fn install_pica_from_repo(app: &mut App, selector: &str) -> CliResult<()> {
    ensure_dirs(&app.paths)?;
    need_cmd("tar")?;

    let candidates = crate::find_pica_candidates_in_index(app, selector)?;
    if candidates.is_empty() {
        return Err(CliError::new(
            E_CONFIG_INVALID,
            format!("package not found in pica repos: {selector}"),
        ));
    }

    let mut best_index = 0usize;
    let mut best_ver = String::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if pkgver_ge(&candidate.cmpver, &best_ver) {
            best_index = index;
            best_ver = candidate.cmpver.clone();
        }
    }

    let best = &candidates[best_index];

    if let Some(min_pica) = &best.min_pica {
        if !min_pica.is_empty() && !ver_ge(PICA_VERSION, min_pica) {
            return Err(CliError::new(
                E_VERSION_INCOMPATIBLE,
                format!("pica too old: pkg requires >= {min_pica}, cli is {PICA_VERSION}"),
            ));
        }
    }

    if best.filename.contains('/') || !best.filename.ends_with(".pkg.tar.gz") {
        return Err(CliError::new(
            E_REPO_INVALID,
            format!("{}: invalid filename: {}", best.repo, best.filename),
        ));
    }

    if best.sha256.len() != 64 || !best.sha256.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(CliError::new(
            E_REPO_INVALID,
            format!("{}: invalid sha256 for {}", best.repo, best.pkgname),
        ));
    }

    let download_url = if let Some(url) = &best.download_url {
        if !is_supported_url(url) {
            return Err(CliError::new(
                E_REPO_INVALID,
                format!(
                    "{}: invalid download_url for {}: {url}",
                    best.repo, best.pkgname
                ),
            ));
        }
        url.to_string()
    } else {
        format!(
            "{}/packages/{}",
            best.url.trim_end_matches('/'),
            best.filename
        )
    };

    let cache_pkgs = app.paths.cache_dir.join("pkgs");
    ensure_dir(&cache_pkgs)?;
    let cached = cache_pkgs.join(&best.filename);

    app.log_info(format!(
        "Downloading {} ({}) from {}...",
        best.pkgname, best.cmpver, best.repo
    ));
    let raw = fetch_url(&download_url, is_supported_url)?;

    let actual_sha256 = sha256_hex(&raw)?;
    if !actual_sha256.eq_ignore_ascii_case(&best.sha256) {
        return Err(CliError::new(
            E_INTEGRITY_INVALID,
            format!(
                "sha256 mismatch for {}: expected {}, got {}",
                best.filename, best.sha256, actual_sha256
            ),
        ));
    }

    write_file_atomic(&cached, &raw)?;

    install_pkgfile(app, &cached, Some(selector.to_string()))
}

pub fn install_pkg_source(app: &mut App, source: &str, selector: Option<String>) -> CliResult<()> {
    if Path::new(source).is_file() {
        return install_pkgfile(app, Path::new(source), selector);
    }

    if is_supported_url(source) {
        ensure_dirs(&app.paths)?;
        need_cmd("tar")?;

        let cache_pkgs = app.paths.cache_dir.join("pkgs");
        ensure_dir(&cache_pkgs)?;

        let guessed = sanitize_cache_filename(
            Path::new(source)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("remote"),
        );
        let cached = cache_pkgs.join(guessed);

        app.log_info(format!("Downloading pkg from URL: {source}"));
        let raw = fetch_url(source, is_supported_url)?;
        write_file_atomic(&cached, &raw)?;
        return install_pkgfile(app, &cached, selector);
    }

    Err(CliError::new(
        E_CONFIG_INVALID,
        format!("pkg source not found or unsupported URL: {source}"),
    ))
}

pub fn install_pkgfile(app: &mut App, pkgfile: &Path, selector: Option<String>) -> CliResult<()> {
    if !pkgfile.is_file() {
        return Err(CliError::new(
            E_PACKAGE_INVALID,
            format!("pkgfile not found: {}", pkgfile.display()),
        ));
    }

    ensure_dirs(&app.paths)?;

    app.log_info("Loading package files...");

    let tmpdir = make_temp_dir("pica-install")?;
    let _tmpdir_guard = TempDirGuard::new(tmpdir.clone());
    run_tar_extract(pkgfile, &tmpdir)?;

    let manifest_file = tmpdir.join("manifest");
    if !manifest_file.is_file() {
        return Err(CliError::new(
            E_PACKAGE_INVALID,
            "package missing manifest",
        ));
    }
    if !tmpdir.join("cmd").is_dir() {
        return Err(CliError::new(E_PACKAGE_INVALID, "package missing cmd/"));
    }
    let manifest = Manifest::from_file(&manifest_file)
        .map_err(|err| CliError::new(E_MANIFEST_INVALID, format!("invalid manifest: {err}")))?;

    let pkgname = required_manifest_field(&manifest, "pkgname")?;
    let _appname = required_manifest_field(&manifest, "appname")?;
    let pica_required = manifest.get_first("pica");
    let pkgver = required_manifest_field(&manifest, "pkgver")?;
    let pkgrel = manifest.get_first("pkgrel");
    let pkgver_display = pkgver_cmp_key(&pkgver, &pkgrel);
    let pkg_platform = required_manifest_field(&manifest, "platform")?;
    let pkg_os = required_manifest_field(&manifest, "os")?;
    let pkg_arch = required_manifest_field(&manifest, "arch")?;
    let pkg_uname = manifest.get_first("uname");
    let pkg_luci = manifest.get_first("luci");
    let pkgmgr = manifest.get_first("pkgmgr");
    let visibility = manifest.get_first("visibility");

    let cmd_install = manifest.get_scalar("cmd_install");
    let cmd_update = manifest.get_scalar("cmd_update");
    let cmd_remove = manifest.get_scalar("cmd_remove");

    let canonical_selector = manifest.canonical_selector(&pkgname);
    let selector = selector.unwrap_or(canonical_selector);

    let mut installed_files = Vec::new();

    let is_upgrade = db_has_installed(&app.paths.db_file, &pkgname)?;

    if !pica_required.is_empty() && !ver_ge(PICA_VERSION, &pica_required) {
        return Err(CliError::new(
            E_VERSION_INCOMPATIBLE,
            format!("pica too old: pkg requires >= {pica_required}, cli is {PICA_VERSION}"),
        ));
    }

    let host_platform = detect_platform();
    let host_os = detect_os();
    let host_uname_raw =
        run_command_text("uname", &["-m"]).unwrap_or_else(|_| "unknown".to_string());
    let host_uname = normalize_uname(&host_uname_raw);
    let pkg_uname_norm = normalize_uname(&pkg_uname);

    if !pkg_uname.is_empty() && pkg_uname_norm != host_uname {
        return Err(CliError::new(
            E_PLATFORM_UNSUPPORTED,
            format!("unsupported uname: pkg={pkg_uname} host={host_uname_raw}"),
        ));
    }

    if pkg_os != "all" && pkg_os != host_os {
        return Err(CliError::new(
            E_PLATFORM_UNSUPPORTED,
            format!("unsupported os: pkg={pkg_os} host={host_os}"),
        ));
    }

    if pkg_arch != "all" {
        let host_arches = detect_opkg_arches();
        if !host_arches.iter().any(|arch| arch == &pkg_arch) {
            return Err(CliError::new(
                E_PLATFORM_UNSUPPORTED,
                format!(
                    "unsupported arch: pkg={pkg_arch} (opkg arches: {})",
                    host_arches.join(" ")
                ),
            ));
        }
    }

    let pkgmgr = if pkgmgr.is_empty() {
        "opkg".to_string()
    } else {
        pkgmgr
    };
    if pkgmgr != "opkg" && pkgmgr != "none" {
        return Err(CliError::new(
            E_CONFIG_INVALID,
            format!("invalid pkgmgr value: {pkgmgr} (supported: opkg, none)"),
        ));
    }

    if pkgmgr == "opkg" && !tmpdir.join("binary").is_dir() {
        return Err(CliError::new(E_PACKAGE_INVALID, "package missing binary/"));
    }

    if visibility != "open" && visibility != "mix" && visibility != "closed" {
        return Err(CliError::new(
            E_CONFIG_INVALID,
            format!("invalid visibility value: {visibility} (supported: open, mix, closed)"),
        ));
    }

    if manifest.has_type("luci") {
        if pkg_luci.is_empty() {
            return Err(CliError::new(
                E_CONFIG_INVALID,
                "type=luci requires luci=<lua1|js2>",
            ));
        }
        if pkg_luci != "lua1" && pkg_luci != "js2" {
            return Err(CliError::new(
                E_CONFIG_INVALID,
                format!("invalid luci value: {pkg_luci}"),
            ));
        }
        let host_luci = detect_luci_variant();
        if host_luci == "unknown" {
            return Err(CliError::new(
                E_PLATFORM_UNSUPPORTED,
                format!("luci variant required ({pkg_luci}) but cannot detect host"),
            ));
        }
        if host_luci != pkg_luci {
            return Err(CliError::new(
                E_PLATFORM_UNSUPPORTED,
                format!("unsupported luci variant: pkg={pkg_luci} host={host_luci}"),
            ));
        }
    }

    app.log_info(format!("Installing {pkgname}..."));
    app.log_info(format!("  version: {pkgver_display}"));
    app.log_info(format!("  os: {pkg_os} (host: {host_os})"));
    app.log_info(format!("  platform: {pkg_platform} (host: {host_platform})"));
    app.log_info(format!("  arch: {pkg_arch}"));
    app.log_info(format!("  pkgmgr: {pkgmgr}"));
    app.log_info(format!("  visibility: {visibility}"));
    if !pkg_uname.is_empty() {
        app.log_info(format!("  uname: {pkg_uname} (host: {host_uname_raw})"));
    }
    let pkg_type = manifest.get_first("type");
    if !pkg_type.is_empty() {
        app.log_info(format!("  type: {pkg_type}"));
    }
    if !pkg_luci.is_empty() {
        app.log_info(format!("  luci: {pkg_luci}"));
    }

    let depend_dir = tmpdir.join("depend");
    let binary_dir = tmpdir.join("binary");

    let mut app_list = manifest.get_array("app");
    let lang = resolve_lang(&app.paths.conf_file);
    let i18n_template = manifest.get_first("app_i18n");
    if !i18n_template.is_empty() && lang == "zh-cn" {
        app_list.push(i18n_template.replace("{lang}", &lang));
    }
    app_list = reorder_app_list(app_list);

    let mut precheck = json!({"kmod": [], "base": [], "app": []});
    let mut tx_added = Vec::new();
    let mut app_added = Vec::new();

    if pkgmgr == "opkg" {
        opkg_update_ignore();

        precheck = build_precheck_report(&manifest, &depend_dir, &binary_dir, &app_list);
        precheck_assert_no_missing(&precheck)?;

        let snap_before_tx = opkg_snapshot_installed();

        for dep in manifest.get_array("kmod") {
            if dep.is_empty() {
                continue;
            }
            if !opkg_is_installed(&dep) {
                opkg_install_pkg("kmod", &dep)?;
            }
        }

        let base_list = manifest.get_array("base");
        let has_depend_dir = depend_dir.is_dir();
        install_via_feeds_or_ipk(app, "base", &base_list, &depend_dir, has_depend_dir)?;

        let snap_before_app = opkg_snapshot_installed();

        install_via_feeds_or_ipk(app, "app", &app_list, &binary_dir, true)?;

        let snap_after_app = opkg_snapshot_installed();
        let snap_after_tx = opkg_snapshot_installed();

        tx_added = pkg_list_diff_added(&snap_before_tx, &snap_after_tx);
        app_added = pkg_list_diff_added(&snap_before_app, &snap_after_app);
    }

    if is_upgrade {
        run_hook(app, &tmpdir, &cmd_update, "update")?;
    } else {
        run_hook(app, &tmpdir, &cmd_install, "install")?;
    }

    ensure_dir(Path::new("/usr/lib/pica/cmd").join(&pkgname).as_path())?;
    for hook_rel in [&cmd_install, &cmd_update, &cmd_remove] {
        if hook_rel.is_empty() || hook_rel.starts_with('/') {
            continue;
        }
        let source = tmpdir.join(hook_rel);
        if source.is_file() {
            let target = Path::new("/usr/lib/pica/cmd").join(&pkgname).join(hook_rel);
            if let Some(parent) = target.parent() {
                ensure_dir(parent)?;
            }
            fs::copy(&source, &target).map_err(|err| {
                CliError::new(E_IO, format!("copy hook failed: {err}"))
            })?;
            installed_files.push(target.display().to_string());
        } else {
            return Err(CliError::new(
                E_PACKAGE_INVALID,
                format!("cmd script not found in package: {hook_rel}"),
            ));
        }
    }

    let cmd_dir = tmpdir.join("cmd");
    if cmd_dir.is_dir() {
        ensure_dir(Path::new("/usr/bin"))?;
        copy_dir_recursive(&cmd_dir, Path::new("/usr/bin"))?;
        if let Ok(entries) = fs::read_dir(&cmd_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                        if name != ".env" {
                            installed_files.push(Path::new("/usr/bin").join(name).display().to_string());
                        }
                    }
                }
            }
        }
    }

    let env_file = tmpdir.join("cmd/.env");
    if env_file.is_file() {
        ensure_dir(Path::new("/etc/pica/env.d"))?;
        let target = Path::new("/etc/pica/env.d").join(format!("{pkgname}.env"));
        fs::copy(env_file, target).map_err(|err| {
            CliError::new(E_IO, format!("copy env file failed: {err}"))
        })?;
        installed_files.push(Path::new("/etc/pica/env.d").join(format!("{pkgname}.env")).display().to_string());
    }

    report_set_install_result(
        &app.paths,
        &pkgname,
        &selector,
        &manifest.value,
        &precheck,
        &tx_added,
        &app_added,
    )?;

    let mut manifest_stored = manifest.value.clone();
    if manifest_stored.get("source").is_none() {
        manifest_stored["source"] = json!("pica");
    }
    if manifest_stored.get("url").is_none() {
        let origin = manifest_stored
            .get("origin")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        manifest_stored["url"] = json!(origin);
    }
    if manifest_stored.get("pkgmgr").is_none() {
        manifest_stored["pkgmgr"] = json!("opkg");
    }
    for key in ["branch", "protocol", "url", "luci_url", "luci_desc", "pkgmgr", "visibility"] {
        if manifest_stored.get(key).is_none() {
            manifest_stored[key] = json!("");
        }
    }

    db_set_installed(
        &app.paths.db_file,
        &pkgname,
        manifest_stored,
        &canonicalize_display(pkgfile),
        &installed_files,
    )?;

    app.log_info("Transaction completed");

    Ok(())
}

pub fn sanitize_cache_filename(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    if out.is_empty() {
        out = format!("remote-{}-{}", now_unix_secs(), std::process::id());
    }
    if !out.ends_with(".pkg.tar.gz") {
        out.push_str(".pkg.tar.gz");
    }

    out
}

fn sha256_hex(content: &[u8]) -> CliResult<String> {
    if let Ok(mut child) = Command::new("sha256sum")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        use std::io::Write;
        if let Some(stdin) = &mut child.stdin {
            let _ = stdin.write_all(content);
        }

        if let Ok(output) = child.wait_with_output() {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                if let Some(sum) = text.split_whitespace().next() {
                    if !sum.is_empty() {
                        return Ok(sum.to_string());
                    }
                }
            }
        }
    }

    if let Ok(mut child) = Command::new("openssl")
        .arg("dgst")
        .arg("-sha256")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        use std::io::Write;
        if let Some(stdin) = &mut child.stdin {
            let _ = stdin.write_all(content);
        }

        if let Ok(output) = child.wait_with_output() {
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
    }

    Err(CliError::new(
        E_IO,
        "cannot compute sha256: need command 'sha256sum' or 'openssl'",
    ))
}

#[cfg(test)]
mod tests {
    use super::{sanitize_cache_filename, TempDirGuard};
    use std::fs;

    #[test]
    fn sanitize_cache_filename_keeps_extension() {
        let value = sanitize_cache_filename("hello package");
        assert!(value.ends_with(".pkg.tar.gz"));
        assert!(!value.contains(' '));
    }

    #[test]
    fn temp_dir_guard_cleans_on_drop() {
        let path = std::env::temp_dir().join(format!(
            "pica-install-guard-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        fs::write(path.join("marker"), "x").expect("write marker");

        {
            let _guard = TempDirGuard::new(path.clone());
        }

        assert!(!path.exists());
    }
}
