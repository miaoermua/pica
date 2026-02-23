mod lock;
mod commands;
mod types;

use crate::types::{
    ensure_dirs, parse_options, require_arg, App, CliError, CliResult, FeedPolicy, JsonMode,
    Options, Paths, DEFAULT_ERROR_CODE,
};
use pica_core::manifest::{get_first as manifest_get_first, Manifest};
use pica_core::repo::is_supported_url;
use pica_core::selector::Selector;
use pica_core::version::{pkgver_cmp_key, pkgver_ge, ver_ge};
use pica_core::PICA_VERSION;
use pica_core::io::{copy_dir_recursive as core_copy_dir_recursive, make_temp_dir as core_make_temp_dir};
use serde_json::{json, Map, Value};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use crate::lock::LockGuard;
use crate::commands::query::{query_info, query_installed, query_license};
use crate::commands::remove::remove_pkg;
use crate::commands::sync::sync_repos;
use crate::commands::upgrade::upgrade_all;
use crate::commands::install::{
    install_app_auto, install_app_via_opkg, install_pica_from_repo, install_pkg_source,
};

fn usage() {
    println!(
        "Usage:\n  pica-rs -S                 Sync (download repo.json and update index)\n  pica-rs -Su                Upgrade all installed pica packages\n  pica-rs -Syu               Sync, then upgrade all installed pica packages\n  pica-rs -Si <selector>     Install by selector (auto: opkg if available, else pica)\n  pica-rs -So <selector>     Install by selector (force opkg)\n  pica-rs -Sp <selector>     Install by selector (force pica repo)\n  pica-rs -U <pkgfile|url>   Install/Update from local file or URL\n  pica-rs -R <pkgname>       Remove package (no dependency handling)\n  pica-rs -Q                 List installed pica packages\n  pica-rs -Qi <pkgname>      Show installed package info\n  pica-rs -Ql <pkgname>      Show installed package license\n  pica-rs --json ...         Emit JSON on success and error (explicit only)\n  pica-rs --json-errors ...  Emit JSON only on error\n  pica-rs --non-interactive ...\n                            Disable prompts (for backend/automation)\n  pica-rs --feed-policy <mode>\n                            ask|feed-first|packaged-first|feed-only|packaged-only\n  pica-rs --version\n\nNotes:\n  - Requires: opkg, tar, and one fetcher (uclient-fetch/wget/curl) for URL install/sync.\n  - Config: /etc/pica/pica.json\n  - State:  /var/lib/pica/db.json, /var/lib/pica/index.json\n  - Lock:   /var/lib/pica/db.lck\n  - Selector example: app@author:version(branch)"
    );
}


fn main() {
    let paths = Paths::from_env();

    let (options, args) = match parse_options(env::args().skip(1).collect()) {
        Ok(value) => value,
        Err(err) => {
            let app = App::new(
                paths,
                Options {
                    json_mode: JsonMode::None,
                    non_interactive: false,
                    feed_policy: FeedPolicy::Ask,
                },
            );
            app.emit_error(&err);
            process::exit(1);
        }
    };

    let mut app = App::new(paths, options);

    if args.is_empty() {
        usage();
        process::exit(2);
    }

    let command = args[0].as_str();
    if matches!(command, "-h" | "--help" | "help") {
        usage();
        app.emit_success(command, "usage");
        return;
    }
    if command == "--version" {
        println!("{PICA_VERSION}");
        app.emit_success("--version", PICA_VERSION);
        return;
    }

    if app.options.json_mode != JsonMode::None && !has_command("jq") {
        let err = CliError::new(
            "E_MISSING_COMMAND",
            "--json/--json-errors requires command: jq",
        );
        app.emit_error(&err);
        process::exit(1);
    }

    let lock_guard = match LockGuard::acquire(&app.paths.lock_file) {
        Ok(guard) => guard,
        Err(err) => {
            app.emit_error(&CliError::new(err.code, err.message));
            process::exit(1);
        }
    };

    let result = run_command(&mut app, &args);
    drop(lock_guard);

    match result {
        Ok((cmd, target)) => app.emit_success(cmd, &target),
        Err(err) => {
            app.emit_error(&err);
            process::exit(1);
        }
    }
}

fn run_command(app: &mut App, args: &[String]) -> CliResult<(&'static str, String)> {
    let command = args[0].as_str();
    match command {
        "-S" => {
            app.set_phase("sync");
            sync_repos(app)?;
            Ok(("-S", "repos".to_string()))
        }
        "-Su" => {
            app.set_phase("upgrade");
            need_cmd("opkg")?;
            upgrade_all(app)?;
            Ok(("-Su", "all".to_string()))
        }
        "-Syu" => {
            app.set_phase("sync");
            need_cmd("opkg")?;
            sync_repos(app)?;
            app.set_phase("upgrade");
            upgrade_all(app)?;
            Ok(("-Syu", "all".to_string()))
        }
        "-Q" => {
            app.set_phase("query");
            query_installed(app)?;
            Ok(("-Q", "installed".to_string()))
        }
        "-Qi" => {
            app.set_phase("query");
            let pkgname = require_arg(args, 1, "-Qi requires <pkgname>")?;
            query_info(app, pkgname)?;
            Ok(("-Qi", pkgname.to_string()))
        }
        "-Ql" => {
            app.set_phase("query");
            let pkgname = require_arg(args, 1, "-Ql requires <pkgname>")?;
            query_license(app, pkgname)?;
            Ok(("-Ql", pkgname.to_string()))
        }
        "-So" => {
            app.set_phase("install");
            need_cmd("opkg")?;
            let selector = require_arg(args, 1, "-So requires <selector>")?;
            install_app_via_opkg(app, selector)?;
            Ok(("-So", selector.to_string()))
        }
        "-Si" => {
            app.set_phase("install");
            need_cmd("opkg")?;
            let selector = require_arg(args, 1, "-Si requires <selector>")?;
            install_app_auto(app, selector)?;
            Ok(("-Si", selector.to_string()))
        }
        "-Sp" => {
            app.set_phase("install");
            need_cmd("opkg")?;
            need_cmd("tar")?;
            let selector = require_arg(args, 1, "-Sp requires <selector>")?;
            install_pica_from_repo(app, selector)?;
            Ok(("-Sp", selector.to_string()))
        }
        "-U" => {
            app.set_phase("install");
            need_cmd("opkg")?;
            need_cmd("tar")?;
            let source = require_arg(args, 1, "-U requires <pkgfile|url>")?;
            install_pkg_source(app, source, None)?;
            Ok(("-U", source.to_string()))
        }
        "-R" => {
            app.set_phase("remove");
            need_cmd("opkg")?;
            let pkgname = require_arg(args, 1, "-R requires <pkgname>")?;
            remove_pkg(app, pkgname)?;
            Ok(("-R", pkgname.to_string()))
        }
        other => Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("unknown arg: {other}"),
        )),
    }
}

fn find_pica_candidates_in_index(app: &App, selector: &str) -> CliResult<Vec<RepoCandidate>> {
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
            let author = pkg
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or("")
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
            if !parsed.author.is_empty() && author != parsed.author {
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
struct RepoCandidate {
    cmpver: String,
    repo: String,
    url: String,
    filename: String,
    download_url: Option<String>,
    min_pica: Option<String>,
    pkgname: String,
}

fn required_manifest_field(manifest: &Manifest, key: &str) -> CliResult<String> {
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

fn canonicalize_display(path: &Path) -> String {
    fs::canonicalize(path)
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn normalize_uname(value: &str) -> String {
    match value {
        "x86_64" => "amd64".to_string(),
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    }
}

fn detect_opkg_arches() -> Vec<String> {
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

fn detect_luci_variant() -> String {
    if opkg_is_installed("luci") {
        return "lua1".to_string();
    }
    if opkg_is_installed("luci2") {
        return "js2".to_string();
    }
    "unknown".to_string()
}

fn opkg_is_installed(name: &str) -> bool {
    let Ok(output) = Command::new("opkg").arg("status").arg(name).output() else {
        return false;
    };

    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.contains("status:") && text.contains("installed")
}

fn opkg_snapshot_installed() -> Vec<String> {
    let Ok(output) = Command::new("opkg").arg("list-installed").output() else {
        return Vec::new();
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((name, _)) = line.split_once(" - ") else {
            continue;
        };
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }

    out.sort();
    out.dedup();
    out
}

fn pkg_list_diff_added(before: &[String], after: &[String]) -> Vec<String> {
    let before_set: std::collections::HashSet<&str> = before.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for item in after {
        if !before_set.contains(item.as_str()) {
            out.push(item.clone());
        }
    }
    out
}

fn ipk_dir_has_pkg(dir: &Path, pkg: &str) -> bool {
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

fn precheck_dep_source(dep: &str, ipk_dir: &Path) -> String {
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

fn build_precheck_report(
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

fn precheck_assert_no_missing(precheck: &Value) -> CliResult<()> {
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

fn resolve_lang(conf_file: &Path) -> String {
    let conf = read_json_file(conf_file).ok();
    conf.and_then(|value| {
        value
            .get("lang")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
    .unwrap_or_else(|| "zh-cn".to_string())
}

fn reorder_app_list(list: Vec<String>) -> Vec<String> {
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

fn should_use_feeds(
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

fn install_via_feeds_or_ipk(
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

fn install_ipk_dir(label: &str, dir: &Path) -> CliResult<()> {
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

fn prompt_yn(question: &str, default_yes: bool) -> bool {
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

fn run_hook(app: &mut App, tmpdir: &Path, hook_rel: &str, label: &str) -> CliResult<()> {
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

fn report_set_install_result(
    app: &mut App,
    pkgname: &str,
    selector: &str,
    manifest: &Value,
    precheck: &Value,
    tx_added: &[String],
    app_added: &[String],
) -> CliResult<()> {
    ensure_dirs(&app.paths)?;

    let mut report = read_json_file(&app.paths.report_file).unwrap_or_else(|_| {
        json!({
            "schema": 1,
            "reports": {},
        })
    });

    report["schema"] = json!(1);
    ensure_json_object_field(&mut report, "reports")?;

    let appname = manifest_get_first(manifest, "appname");
    let appname = if appname.is_empty() {
        manifest_get_first(manifest, "pkgname")
    } else {
        appname
    };

    report["reports"][pkgname] = json!({
        "updated_at": now_unix_secs(),
        "selector": selector,
        "package": {
            "pkgname": manifest_get_first(manifest, "pkgname"),
            "appname": appname,
            "author": manifest_get_first(manifest, "author"),
            "version": manifest_get_first(manifest, "version"),
            "branch": manifest_get_first(manifest, "branch"),
            "protocol": manifest_get_first(manifest, "protocol"),
            "pkgver": manifest_get_first(manifest, "pkgver"),
            "pkgrel": manifest_get_first(manifest, "pkgrel"),
        },
        "precheck": precheck,
        "dependency_diff": {
            "transaction_added": tx_added,
            "app_stage_added": app_added,
        }
    });

    write_json_atomic_pretty(&app.paths.report_file, &report)
}

fn db_set_installed(
    db_file: &Path,
    pkgname: &str,
    manifest: Value,
    pkgfile: &str,
) -> CliResult<()> {
    let mut db = read_json_file(db_file)?;
    ensure_json_object_field(&mut db, "installed")?;

    let installed = db
        .get_mut("installed")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| CliError::new("E_DB_INVALID", "db installed is not object"))?;

    installed.insert(
        pkgname.to_string(),
        json!({
            "manifest": manifest,
            "pkgfile": pkgfile,
            "installed_at": now_unix_secs(),
        }),
    );

    write_json_atomic_pretty(db_file, &db)
}

fn db_del_installed(db_file: &Path, pkgname: &str) -> CliResult<()> {
    let mut db = read_json_file(db_file)?;
    ensure_json_object_field(&mut db, "installed")?;

    let installed = db
        .get_mut("installed")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| CliError::new("E_DB_INVALID", "db installed is not object"))?;
    installed.remove(pkgname);

    write_json_atomic_pretty(db_file, &db)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn db_has_installed(db_file: &Path, pkgname: &str) -> CliResult<bool> {
    let db = read_json_file(db_file)?;
    let has = db
        .get("installed")
        .and_then(Value::as_object)
        .map(|installed| installed.contains_key(pkgname))
        .unwrap_or(false);
    Ok(has)
}

fn db_find_installed_pkgname_by_selector(
    db_file: &Path,
    selector: &Selector,
) -> CliResult<Option<String>> {
    let db = read_json_file(db_file)?;
    let Some(installed) = db.get("installed").and_then(Value::as_object) else {
        return Ok(None);
    };

    for (pkgname, entry) in installed {
        let manifest = entry.get("manifest").unwrap_or(&Value::Null);

        let key_matches = pkgname == &selector.appname
            || manifest_get_first(manifest, "appname") == selector.appname
            || manifest_get_first(manifest, "pkgname") == selector.appname;
        if !key_matches {
            continue;
        }

        if !selector.author.is_empty() && manifest_get_first(manifest, "author") != selector.author
        {
            continue;
        }

        if !selector.version.is_empty() {
            let manifest_version = manifest_get_first(manifest, "version");
            let manifest_branch = manifest_get_first(manifest, "branch");
            let manifest_pkgver = manifest_get_first(manifest, "pkgver");
            let manifest_pkgrel = manifest_get_first(manifest, "pkgrel");
            let manifest_pkgver_rel = pkgver_cmp_key(&manifest_pkgver, &manifest_pkgrel);

            let version_matches = manifest_version == selector.version
                || manifest_branch == selector.version
                || manifest_pkgver == selector.version
                || manifest_pkgver_rel == selector.version;
            if !version_matches {
                continue;
            }
        }

        if !selector.branch.is_empty() && manifest_get_first(manifest, "branch") != selector.branch
        {
            continue;
        }

        return Ok(Some(pkgname.clone()));
    }

    Ok(None)
}

fn conf_get_i18n(conf_file: &Path) -> Option<String> {
    let conf = read_json_file(conf_file).ok()?;
    let value = conf.get("i18n").and_then(Value::as_str).unwrap_or("zh-cn");
    Some(value.to_string())
}

fn detect_platform() -> String {
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

fn unquote_shell_value(value: &str) -> String {
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

fn fetch_url(url: &str) -> CliResult<Vec<u8>> {
    if !is_supported_url(url) {
        return Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("unsupported URL: {url}"),
        ));
    }

    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path).map_err(|err| {
            CliError::new(DEFAULT_ERROR_CODE, format!("read file url failed: {err}"))
        });
    }

    if has_command("uclient-fetch") {
        return run_fetch("uclient-fetch", &["-O", "-", url]);
    }
    if has_command("wget") {
        return run_fetch("wget", &["-qO-", url]);
    }
    if has_command("curl") {
        return run_fetch("curl", &["-fsSL", url]);
    }

    Err(CliError::new(
        "E_MISSING_COMMAND",
        "no fetch tool found (need uclient-fetch, wget, or curl)",
    ))
}

fn run_fetch(command: &str, args: &[&str]) -> CliResult<Vec<u8>> {
    let output = Command::new(command)
        .args(args)
        .output()
        .map_err(|err| CliError::new(DEFAULT_ERROR_CODE, format!("{command} failed: {err}")))?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        let detail = stderr_or_stdout(&output.stdout, &output.stderr);
        Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("{command} download failed: {detail}"),
        ))
    }
}

fn need_cmd(name: &str) -> CliResult<()> {
    if has_command(name) {
        Ok(())
    } else {
        Err(CliError::new(
            "E_MISSING_COMMAND",
            format!("missing required command: {name}"),
        ))
    }
}

fn has_command(name: &str) -> bool {
    if name.contains('/') {
        return Path::new(name).is_file();
    }

    let Some(path_env) = env::var_os("PATH") else {
        return false;
    };

    env::split_paths(&path_env).any(|dir| {
        let full = dir.join(name);
        full.is_file()
    })
}

fn opkg_update_ignore() {
    if !has_command("opkg") {
        return;
    }
    let _ = Command::new("opkg").arg("update").output();
}

fn opkg_has_package(name: &str) -> bool {
    let Ok(output) = Command::new("opkg").arg("info").arg(name).output() else {
        return false;
    };

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    if text.trim().is_empty() {
        return false;
    }

    !text.to_ascii_lowercase().contains("unknown package")
}

fn opkg_installed_version(name: &str) -> Option<String> {
    let output = Command::new("opkg").arg("status").arg(name).output().ok()?;

    let content = String::from_utf8_lossy(&output.stdout);
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Version: ") {
            return Some(value.trim().to_string());
        }
    }

    None
}

fn opkg_install_pkg(label: &str, target: &str) -> CliResult<()> {
    let output = Command::new("opkg")
        .arg("install")
        .arg(target)
        .output()
        .map_err(|err| CliError::new("E_OPKG_INSTALL", format!("opkg install failed: {err}")))?;

    if output.status.success() {
        return Ok(());
    }

    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    if detail
        .to_ascii_lowercase()
        .contains("no space left on device")
    {
        return Err(CliError::new(
            "E_NO_SPACE",
            format!("{label} install failed: {target} (storage-full). detail=[{detail}]"),
        ));
    }

    Err(CliError::new(
        "E_OPKG_INSTALL",
        format!("{label} install failed: {target} detail=[{detail}]"),
    ))
}

fn opkg_remove_pkg(target: &str) -> CliResult<()> {
    let output = Command::new("opkg")
        .arg("remove")
        .arg(target)
        .output()
        .map_err(|err| CliError::new("E_OPKG_REMOVE", format!("opkg remove failed: {err}")))?;

    if output.status.success() {
        Ok(())
    } else {
        let detail = stderr_or_stdout(&output.stdout, &output.stderr);
        Err(CliError::new(
            "E_OPKG_REMOVE",
            format!("opkg remove failed: {target} detail=[{detail}]"),
        ))
    }
}

fn stderr_or_stdout(stdout: &[u8], stderr: &[u8]) -> String {
    let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();
    if !stderr_text.is_empty() {
        return stderr_text;
    }

    let stdout_text = String::from_utf8_lossy(stdout).trim().to_string();
    if !stdout_text.is_empty() {
        stdout_text
    } else {
        "unknown error".to_string()
    }
}

fn run_command_text(program: &str, args: &[&str]) -> CliResult<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| CliError::new(DEFAULT_ERROR_CODE, format!("{program} failed: {err}")))?;

    if !output.status.success() {
        return Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("{program} exited with status {}", output.status),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_command_capture_output(program: &str, args: &[&str]) -> CliResult<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| CliError::new(DEFAULT_ERROR_CODE, format!("{program} failed: {err}")))?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        let detail = stderr_or_stdout(&output.stdout, &output.stderr);
        Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("{program} failed: {detail}"),
        ))
    }
}

fn ensure_json_object_field(value: &mut Value, key: &str) -> CliResult<()> {
    let Some(obj) = value.as_object_mut() else {
        return Err(CliError::new(DEFAULT_ERROR_CODE, "json root is not object"));
    };

    if !obj.contains_key(key) {
        obj.insert(key.to_string(), Value::Object(Map::new()));
    }

    if !obj.get(key).is_some_and(Value::is_object) {
        return Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("json field '{key}' is not object"),
        ));
    }

    Ok(())
}

fn read_json_file(path: &Path) -> CliResult<Value> {
    let content = fs::read_to_string(path).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("read {} failed: {err}", path.display()),
        )
    })?;

    serde_json::from_str(&content).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("parse {} failed: {err}", path.display()),
        )
    })
}

fn write_json_atomic_pretty(path: &Path, value: &Value) -> CliResult<()> {
    let mut tmp_name = OsString::from(path.as_os_str());
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }

    let content = serde_json::to_string_pretty(value)
        .map_err(|err| CliError::new(DEFAULT_ERROR_CODE, err.to_string()))?;
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

fn write_file_atomic(path: &Path, content: &[u8]) -> CliResult<()> {
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

fn ensure_dir(path: &Path) -> CliResult<()> {
    fs::create_dir_all(path).map_err(|err| {
        CliError::new(
            DEFAULT_ERROR_CODE,
            format!("mkdir {} failed: {err}", path.display()),
        )
    })
}

fn make_temp_dir(prefix: &str) -> CliResult<PathBuf> {
    core_make_temp_dir(prefix).map_err(map_core_error)
}

fn run_tar_extract(pkgfile: &Path, target_dir: &Path) -> CliResult<()> {
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(pkgfile)
        .arg("-C")
        .arg(target_dir)
        .output()
        .map_err(|err| CliError::new(DEFAULT_ERROR_CODE, format!("tar extract failed: {err}")))?;

    if output.status.success() {
        Ok(())
    } else {
        let detail = stderr_or_stdout(&output.stdout, &output.stderr);
        Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("tar extract failed: {detail}"),
        ))
    }
}

fn copy_dir_recursive(source: &Path, target: &Path) -> CliResult<()> {
    core_copy_dir_recursive(source, target).map_err(map_core_error)
}

fn map_core_error(err: pica_core::error::PicaError) -> CliError {
    CliError::new(DEFAULT_ERROR_CODE, err.to_string())
}

fn manifest_get_array(value: &Value, key: &str) -> Vec<String> {
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

fn manifest_get_scalar(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_default()
}
