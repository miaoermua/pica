use crate::state::read_json_file;
use crate::system;
use crate::{
    ensure_dirs, App, CliError, CliResult, FeedPolicy, Manifest, Selector, DEFAULT_ERROR_CODE,
};
use pica_core::io::{
    copy_dir_recursive as core_copy_dir_recursive, make_temp_dir as core_make_temp_dir,
};
use pica_core::version::pkgver_cmp_key;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn find_pica_candidates_in_index(
    app: &App,
    selector: &str,
) -> CliResult<Vec<RepoCandidate>> {
    ensure_dirs(&app.paths)?;

    let index = read_json_file(&app.paths.index_file)?;
    let parsed = Selector::parse(selector);
    let host_platform = detect_platform();

    let mut out = Vec::new();

    let repos = index
        .get("repos")
        .and_then(Value::as_object)
        .ok_or_else(|| CliError::new(DEFAULT_ERROR_CODE, "missing index: run 'pica -S' first"))?;

    for (repo_name, repo_entry) in repos {
        let repo_url = repo_entry
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let repo_platform = repo_entry
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let Some(packages) = repo_entry
            .get("data")
            .and_then(|data| data.get("packages"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        for pkg in packages {
            let pkgname = pkg
                .get("pkgname")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let appname = pkg
                .get("appname")
                .and_then(Value::as_str)
                .unwrap_or(&pkgname)
                .to_string();
            let version_tag = pkg
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let branch = pkg
                .get("branch")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let pkgver = pkg
                .get("pkgver")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let pkgrel = pkg
                .get("pkgrel")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let filename = pkg
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let platform = pkg
                .get("platform")
                .and_then(Value::as_str)
                .unwrap_or("all")
                .to_string();
            let download_url = pkg
                .get("download_url")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let min_pica = pkg
                .get("pica")
                .and_then(Value::as_str)
                .map(ToString::to_string);

            if appname != parsed.appname {
                continue;
            }
            if !parsed.version.is_empty() {
                let cmpver = pkgver_cmp_key(&pkgver, &pkgrel);
                let version_match = version_tag == parsed.version
                    || branch == parsed.version
                    || pkgver == parsed.version
                    || cmpver == parsed.version;
                if !version_match {
                    continue;
                }
            }

            if !parsed.branch.is_empty() && branch != parsed.branch {
                continue;
            }

            let platform_match = platform == "all"
                || platform == host_platform
                || (!repo_platform.is_empty() && platform == repo_platform);
            if !platform_match {
                continue;
            }

            out.push(RepoCandidate {
                cmpver: pkgver_cmp_key(&pkgver, &pkgrel),
                repo: repo_name.to_string(),
                url: repo_url.clone(),
                filename,
                download_url,
                min_pica,
                pkgname,
            });
        }
    }

    Ok(out)
}

#[derive(Debug, Clone)]
pub(crate) struct RepoCandidate {
    pub(crate) cmpver: String,
    pub(crate) repo: String,
    pub(crate) url: String,
    pub(crate) filename: String,
    pub(crate) download_url: Option<String>,
    pub(crate) min_pica: Option<String>,
    pub(crate) pkgname: String,
}

pub(crate) fn required_manifest_field(manifest: &Manifest, key: &str) -> CliResult<String> {
    let value = manifest.get_first(key);
    if value.is_empty() {
        Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("manifest missing {key}"),
        ))
    } else {
        Ok(value)
    }
}

pub(crate) fn canonicalize_display(path: &Path) -> String {
    fs::canonicalize(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

pub(crate) fn normalize_uname(value: &str) -> String {
    match value {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn detect_opkg_arches() -> Vec<String> {
    let Ok(output) = Command::new("opkg").arg("print-architecture").output() else {
        return Vec::new();
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let _ = parts.next();
        if let Some(arch) = parts.next() {
            out.push(arch.to_string());
        }
    }
    out
}

pub(crate) fn detect_luci_variant() -> String {
    if opkg_is_installed("luci") {
        return "lua1".to_string();
    }
    if opkg_is_installed("luci2") {
        return "js2".to_string();
    }
    "unknown".to_string()
}

pub(crate) fn opkg_is_installed(name: &str) -> bool {
    system::opkg_is_installed(name)
}

pub(crate) fn pkg_list_diff_added(before: &[String], after: &[String]) -> Vec<String> {
    let before_set: std::collections::HashSet<&str> = before.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for item in after {
        if !before_set.contains(item.as_str()) {
            out.push(item.clone());
        }
    }
    out
}

pub(crate) fn ipk_dir_has_pkg(dir: &Path, pkg: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_string();
        if name.starts_with(&format!("{pkg}_")) || name.starts_with(&format!("{pkg}-")) {
            return true;
        }
    }

    false
}

pub(crate) fn precheck_dep_source(dep: &str, ipk_dir: &Path) -> String {
    if opkg_is_installed(dep) {
        return "installed".to_string();
    }
    if opkg_has_package(dep) {
        return "feed".to_string();
    }
    if ipk_dir_has_pkg(ipk_dir, dep) {
        return "packaged".to_string();
    }
    "missing".to_string()
}

pub(crate) fn build_precheck_report(
    manifest: &Manifest,
    depend_dir: &Path,
    binary_dir: &Path,
    app_list: &[String],
) -> Value {
    let kmod = manifest
        .get_array("kmod")
        .into_iter()
        .filter(|dep| !dep.is_empty())
        .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(&dep, Path::new(""))}))
        .collect::<Vec<Value>>();

    let base = manifest
        .get_array("base")
        .into_iter()
        .filter(|dep| !dep.is_empty())
        .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(&dep, depend_dir)}))
        .collect::<Vec<Value>>();

    let app = app_list
        .iter()
        .filter(|dep| !dep.is_empty())
        .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(dep, binary_dir)}))
        .collect::<Vec<Value>>();

    json!({
        "kmod": kmod,
        "base": base,
        "app": app,
    })
}

pub(crate) fn precheck_assert_no_missing(precheck: &Value) -> CliResult<()> {
    let mut missing = Vec::new();

    for (group, key) in [
        (precheck.get("kmod"), "kmod"),
        (precheck.get("base"), "base"),
        (precheck.get("app"), "app"),
    ] {
        let Some(entries) = group.and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
            if status == "missing" {
                let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
                missing.push(format!("{key}:{name}"));
            }
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("dependency precheck failed, missing: {}", missing.join(" ")),
        ))
    }
}

pub(crate) fn resolve_lang(conf_file: &Path) -> String {
    let conf = read_json_file(conf_file).ok();
    conf.and_then(|value| {
        value
            .get("i18n")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
    .unwrap_or_else(|| "zh-cn".to_string())
}

pub(crate) fn reorder_app_list(list: Vec<String>) -> Vec<String> {
    let mut core = Vec::new();
    let mut luci = Vec::new();
    let mut i18n = Vec::new();

    for item in list {
        if item.is_empty() {
            continue;
        }
        if item.starts_with("luci-i18n-") {
            i18n.push(item);
        } else if item.starts_with("luci-app-") {
            luci.push(item);
        } else {
            core.push(item);
        }
    }

    core.extend(luci);
    core.extend(i18n);
    core
}

pub(crate) fn should_use_feeds(
    app: &App,
    label: &str,
    pkg_list: &[String],
    have_ipk_dir: bool,
) -> CliResult<i8> {
    match app.options.feed_policy {
        FeedPolicy::FeedOnly => {
            if pkg_list.is_empty() {
                Ok(-1)
            } else {
                Ok(1)
            }
        }
        FeedPolicy::PackagedOnly => Ok(0),
        FeedPolicy::FeedFirst => {
            if pkg_list.is_empty() {
                Ok(0)
            } else {
                Ok(1)
            }
        }
        FeedPolicy::PackagedFirst => {
            if have_ipk_dir {
                Ok(0)
            } else {
                Ok(1)
            }
        }
        FeedPolicy::Ask => {
            if !have_ipk_dir {
                return Ok(1);
            }
            if pkg_list.is_empty() {
                return Ok(0);
            }

            opkg_update_ignore();
            let mut total = 0usize;
            let mut available = 0usize;
            for dep in pkg_list {
                if dep.is_empty() {
                    continue;
                }
                total += 1;
                if opkg_has_package(dep) {
                    available += 1;
                }
            }

            if available == 0 {
                return Ok(0);
            }

            if app.options.non_interactive {
                return Ok(1);
            }

            if prompt_yn(
                &format!(
                    "Found {available}/{total} {label} packages in opkg feeds. Use feeds instead of packaged ipks?"
                ),
                true,
            ) {
                Ok(1)
            } else {
                Ok(0)
            }
        }
    }
}

pub(crate) fn install_via_feeds_or_ipk(
    app: &App,
    label: &str,
    pkg_list: &[String],
    ipk_dir: &Path,
    have_ipk_dir: bool,
) -> CliResult<()> {
    let use_feeds = should_use_feeds(app, label, pkg_list, have_ipk_dir)?;
    if use_feeds == -1 {
        return Err(CliError::new(
            "E_POLICY_INVALID",
            format!("{label} requires feed packages under feed-only policy"),
        ));
    }

    if use_feeds == 1 {
        if pkg_list.is_empty() {
            return Err(CliError::new(
                DEFAULT_ERROR_CODE,
                format!("{label} packages not defined in manifest"),
            ));
        }
        for dep in pkg_list {
            if dep.is_empty() {
                continue;
            }
            opkg_install_pkg(label, dep)?;
        }
        return Ok(());
    }

    if have_ipk_dir {
        install_ipk_dir(label, ipk_dir)?;
        return Ok(());
    }

    Err(CliError::new(
        DEFAULT_ERROR_CODE,
        format!("{label} not available in feeds and no packaged ipks provided"),
    ))
}

pub(crate) fn install_ipk_dir(label: &str, dir: &Path) -> CliResult<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    let mut installed_any = false;
    let entries = fs::read_dir(dir).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("read {} failed: {err}", dir.display()),
        )
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("ipk") {
            continue;
        }
        opkg_install_pkg(label, &path.display().to_string())?;
        installed_any = true;
    }

    if !installed_any {
        return Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("no ipk files found in {label} dir: {}", dir.display()),
        ));
    }

    Ok(())
}

pub(crate) fn prompt_yn(question: &str, default_yes: bool) -> bool {
    use std::io::{self, Read, Write};

    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    eprint!("{question} {hint} ");
    let _ = io::stderr().flush();

    let mut input = String::new();
    let mut stdin = io::stdin();
    if stdin.read_to_string(&mut input).is_ok() {
        let answer = input
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match answer.as_str() {
            "" => default_yes,
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default_yes,
        }
    } else {
        default_yes
    }
}

pub(crate) fn run_hook(app: &mut App, tmpdir: &Path, hook_rel: &str, label: &str) -> CliResult<()> {
    if hook_rel.is_empty() {
        return Ok(());
    }

    let hook_path = tmpdir.join(hook_rel);
    if !hook_path.is_file() {
        return Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("{label} hook not found: {hook_rel}"),
        ));
    }

    app.log_info(format!("Running {label} hook: {hook_rel}"));
    run_command_capture_output("bash", &[hook_path.to_string_lossy().as_ref()]).map(|_| ())
}


pub(crate) fn conf_get_i18n(conf_file: &Path) -> Option<String> {
    let conf = read_json_file(conf_file).ok()?;
    let value = conf.get("i18n").and_then(Value::as_str).unwrap_or("zh-cn");
    Some(value.to_string())
}

pub(crate) fn detect_platform() -> String {
    if let Ok(openwrt_release) = fs::read_to_string("/etc/openwrt_release") {
        for line in openwrt_release.lines() {
            let trimmed = line.trim();
            if let Some(value) = trimmed.strip_prefix("DISTRIB_TARGET=") {
                return unquote_shell_value(value);
            }
        }
    }

    run_command_text("uname", &["-m"]).unwrap_or_else(|_| "unknown".to_string())
}

pub(crate) fn unquote_shell_value(value: &str) -> String {
    let trimmed = value.trim();
    if ((trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
        && trimmed.len() >= 2
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}



pub(crate) fn write_file_atomic(path: &Path, content: &[u8]) -> CliResult<()> {
    let mut tmp_name = OsString::from(path.as_os_str());
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }

    fs::write(&tmp_path, content).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("write {} failed: {err}", tmp_path.display()),
        )
    })?;
    fs::rename(&tmp_path, path).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("rename {} failed: {err}", path.display()),
        )
    })?;

    Ok(())
}

pub(crate) fn ensure_dir(path: &Path) -> CliResult<()> {
    fs::create_dir_all(path).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("mkdir {} failed: {err}", path.display()),
        )
    })
}

pub(crate) fn make_temp_dir(prefix: &str) -> CliResult<PathBuf> {
    core_make_temp_dir(prefix).map_err(map_core_error)
}

pub(crate) fn need_cmd(name: &str) -> CliResult<()> {
    system::need_cmd(name)
}

pub(crate) fn has_command(name: &str) -> bool {
    system::has_command(name)
}

pub(crate) fn opkg_update_ignore() {
    system::opkg_update_ignore()
}

pub(crate) fn opkg_has_package(name: &str) -> bool {
    system::opkg_has_package(name)
}

pub(crate) fn opkg_install_pkg(label: &str, target: &str) -> CliResult<()> {
    system::opkg_install_pkg(label, target)
}

pub(crate) fn run_command_text(program: &str, args: &[&str]) -> CliResult<String> {
    system::run_command_text(program, args)
}

pub(crate) fn run_command_capture_output(program: &str, args: &[&str]) -> CliResult<Vec<u8>> {
    system::run_command_capture_output(program, args)
}

pub(crate) fn copy_dir_recursive(source: &Path, target: &Path) -> CliResult<()> {
    core_copy_dir_recursive(source, target).map_err(map_core_error)
}

fn map_core_error(err: pica_core::error::PicaError) -> CliError {
    CliError::new(DEFAULT_ERROR_CODE, err.to_string())
}

pub(crate) fn manifest_get_array(value: &Value, key: &str) -> Vec<String> {
    let Some(entry) = value.get(key) else {
        return Vec::new();
    };

    match entry {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        Value::String(text) => vec![text.to_string()],
        _ => Vec::new(),
    }
}

pub(crate) fn manifest_get_scalar(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_default()
}
