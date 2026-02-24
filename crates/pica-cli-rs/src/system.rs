use crate::{
    CliError, CliResult, E_IO, E_MISSING_COMMAND, E_NO_SPACE, E_OPKG_INSTALL, E_OPKG_REMOVE,
    E_RUNTIME,
};
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn fetch_url(url: &str, is_supported_url: fn(&str) -> bool) -> CliResult<Vec<u8>> {
    if !is_supported_url(url) {
        return Err(CliError::new(
            E_RUNTIME,
            format!("unsupported URL: {url}"),
        ));
    }

    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path).map_err(|err| {
            CliError::new(E_IO, format!("read file url failed: {err}"))
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
        E_MISSING_COMMAND,
        "no fetch tool found (need uclient-fetch, wget, or curl)",
    ))
}

pub fn need_cmd(name: &str) -> CliResult<()> {
    if has_command(name) {
        Ok(())
    } else {
        Err(CliError::new(
            E_MISSING_COMMAND,
            format!("missing required command: {name}"),
        ))
    }
}

pub fn has_command(name: &str) -> bool {
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

pub fn opkg_update_ignore() {
    if !has_command("opkg") {
        return;
    }
    let _ = Command::new("opkg").arg("update").output();
}

pub fn opkg_has_package(name: &str) -> bool {
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

pub fn opkg_installed_version(name: &str) -> Option<String> {
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

pub fn opkg_install_pkg(label: &str, target: &str) -> CliResult<()> {
    let output = Command::new("opkg")
        .arg("install")
        .arg(target)
        .output()
        .map_err(|err| CliError::new(E_OPKG_INSTALL, format!("opkg install failed: {err}")))?;

    if output.status.success() {
        return Ok(());
    }

    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    if detail.to_ascii_lowercase().contains("no space left on device") {
        return Err(CliError::new(
            E_NO_SPACE,
            format!("{label} install failed: {target} (storage-full). detail=[{detail}]"),
        ));
    }

    Err(CliError::new(
        E_OPKG_INSTALL,
        format!("{label} install failed: {target} detail=[{detail}]"),
    ))
}

pub fn opkg_remove_pkg(target: &str) -> CliResult<()> {
    let output = Command::new("opkg")
        .arg("remove")
        .arg(target)
        .output()
        .map_err(|err| CliError::new(E_OPKG_REMOVE, format!("opkg remove failed: {err}")))?;

    if output.status.success() {
        Ok(())
    } else {
        let detail = stderr_or_stdout(&output.stdout, &output.stderr);
        Err(CliError::new(
            E_OPKG_REMOVE,
            format!("opkg remove failed: {target} detail=[{detail}]"),
        ))
    }
}

pub fn opkg_is_installed(name: &str) -> bool {
    let Ok(output) = Command::new("opkg").arg("status").arg(name).output() else {
        return false;
    };

    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.contains("status:") && text.contains("installed")
}

pub fn opkg_snapshot_installed() -> Vec<String> {
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

pub fn run_command_text(program: &str, args: &[&str]) -> CliResult<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| CliError::new(E_IO, format!("{program} failed: {err}")))?;

    if !output.status.success() {
        return Err(CliError::new(
            E_RUNTIME,
            format!("{program} exited with status {}", output.status),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn run_command_capture_output(program: &str, args: &[&str]) -> CliResult<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| CliError::new(E_IO, format!("{program} failed: {err}")))?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        let detail = stderr_or_stdout(&output.stdout, &output.stderr);
        Err(CliError::new(
            E_RUNTIME,
            format!("{program} failed: {detail}"),
        ))
    }
}

pub fn run_tar_extract(pkgfile: &Path, target_dir: &Path) -> CliResult<()> {
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(pkgfile)
        .arg("-C")
        .arg(target_dir)
        .output()
        .map_err(|err| CliError::new(E_IO, format!("tar extract failed: {err}")))?;

    if output.status.success() {
        Ok(())
    } else {
        let detail = stderr_or_stdout(&output.stdout, &output.stderr);
        Err(CliError::new(
            E_RUNTIME,
            format!("tar extract failed: {detail}"),
        ))
    }
}

fn run_fetch(command: &str, args: &[&str]) -> CliResult<Vec<u8>> {
    let output = Command::new(command)
        .args(args)
        .output()
        .map_err(|err| CliError::new(E_IO, format!("{command} failed: {err}")))?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        let detail = stderr_or_stdout(&output.stdout, &output.stderr);
        Err(CliError::new(
            E_RUNTIME,
            format!("{command} download failed: {detail}"),
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
