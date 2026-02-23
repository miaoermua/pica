use crate::{
    db_del_installed, ensure_dirs, manifest_get_array, manifest_get_scalar, opkg_remove_pkg,
    read_json_file, run_command_capture_output, App, CliError, CliResult, DEFAULT_ERROR_CODE,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub fn remove_pkg(app: &mut App, pkgname: &str) -> CliResult<()> {
    ensure_dirs(&app.paths)?;

    let db = read_json_file(&app.paths.db_file)?;
    let manifest = db
        .get("installed")
        .and_then(Value::as_object)
        .and_then(|installed| installed.get(pkgname))
        .and_then(|entry| entry.get("manifest"))
        .cloned()
        .ok_or_else(|| CliError::new(DEFAULT_ERROR_CODE, format!("not installed: {pkgname}")))?;

    let cmd_remove = manifest_get_scalar(&manifest, "cmd_remove");

    app.log_info(format!("Removing {pkgname}..."));
    run_cmd_install_tree(app, &format!("{pkgname}/{cmd_remove}"), "remove")?;

    for cmdpath in manifest_get_array(&manifest, "cmd") {
        if cmdpath.is_empty() {
            continue;
        }
        let file = Path::new("/usr/bin").join(&cmdpath);
        app.log_info(format!("removing file: {}", file.display()));
        let _ = fs::remove_file(file);
    }

    for opkg_name in manifest_get_array(&manifest, "opkg") {
        if opkg_name.is_empty() {
            continue;
        }
        app.log_info(format!("removing opkg package: {opkg_name}"));
        opkg_remove_pkg(&opkg_name)?;
    }

    let env_file = Path::new("/etc/pica/env.d").join(format!("{pkgname}.env"));
    if env_file.is_file() {
        app.log_info(format!("removing env: {}", env_file.display()));
        let _ = fs::remove_file(env_file);
    }

    db_del_installed(&app.paths.db_file, pkgname)?;
    app.log_info("Transaction completed");
    Ok(())
}

fn run_cmd_install_tree(app: &mut App, cmd_rel: &str, label: &str) -> CliResult<()> {
    if cmd_rel.is_empty() {
        return Ok(());
    }

    let cmd_path = if cmd_rel.starts_with('/') {
        PathBuf::from(cmd_rel)
    } else {
        Path::new("/usr/lib/pica/cmd").join(cmd_rel)
    };

    if !cmd_path.is_file() {
        return Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!(
                "{label} cmd not found: {cmd_rel} (expected at {})",
                cmd_path.display()
            ),
        ));
    }

    app.log_info(format!("Running {label} cmd: {cmd_rel}"));
    run_command_capture_output("bash", &[cmd_path.to_string_lossy().as_ref()]).map(|_| ())
}
