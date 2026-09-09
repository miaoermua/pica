use super::{PackageManager, PackageManagerKind};
use crate::app::{CliError, CliResult, E_NO_SPACE, E_RUNTIME};
use crate::system::{has_command, stderr_or_stdout};
use std::path::Path;
use std::process::Command;

pub struct ApkBackend;

impl PackageManager for ApkBackend {
  fn kind(&self) -> PackageManagerKind { PackageManagerKind::Apk }

  fn update_index(&self) {
    if !has_command("apk") { return; }
    let _ = Command::new("apk").arg("update").output();
  }

  fn package_exists(&self, name: &str) -> bool {
    let Ok(output) = Command::new("apk").args(["search", "--exact", name]).output() else { return false; };
    output.status.success() && String::from_utf8_lossy(&output.stdout).lines().any(|line| !line.trim().is_empty())
  }

  fn is_installed(&self, name: &str) -> bool {
    let Ok(output) = Command::new("apk").args(["info", "--installed", name]).output() else { return false; };
    output.status.success()
  }

  fn installed_version(&self, name: &str) -> Option<String> {
    let output = Command::new("apk").args(["list", "--installed", "--", name]).output().ok()?;
    if !output.status.success() { return None; }
    parse_package_version(&String::from_utf8_lossy(&output.stdout), name)
  }

  fn install(&self, label: &str, target: &str) -> CliResult<()> {
    let output = install_command(target)?.output().map_err(|err| CliError::new("E_APK_INSTALL", format!("apk add failed: {err}")))?;
    if output.status.success() { return Ok(()); }
    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    if detail.to_ascii_lowercase().contains("no space left on device") {
      return Err(CliError::new(E_NO_SPACE, format!("{label} install failed: {target} (storage-full). detail=[{detail}]")));
    }
    Err(CliError::new(E_RUNTIME, format!("{label} install failed: {target} detail=[{detail}]")))
  }

  fn remove(&self, target: &str) -> CliResult<()> {
    let output = Command::new("apk").args(["del", target]).output().map_err(|err| CliError::new(E_RUNTIME, format!("apk del failed: {err}")))?;
    if output.status.success() { Ok(()) } else { Err(CliError::new(E_RUNTIME, format!("apk del failed: {target} detail=[{}]", stderr_or_stdout(&output.stdout, &output.stderr)))) }
  }

  fn snapshot_installed(&self) -> Vec<String> {
    let Ok(output) = Command::new("apk").args(["list", "--installed", "-q"]).output() else { return Vec::new(); };
    let mut packages: Vec<String> = String::from_utf8_lossy(&output.stdout).lines().filter_map(package_name_from_installed_line).collect();
    packages.sort(); packages.dedup(); packages
  }

  fn architectures(&self) -> Vec<String> {
    let Ok(output) = Command::new("apk").arg("--print-arch").output() else { return Vec::new(); };
    String::from_utf8_lossy(&output.stdout).lines().map(str::trim).filter(|line| !line.is_empty()).map(ToString::to_string).collect()
  }
}

fn install_command(target: &str) -> CliResult<Command> {
  let mut command = Command::new("apk");
  command.arg("add");
  if is_local_apk_path(target) {
    let path = Path::new(target).canonicalize()
      .map_err(|err| CliError::new("E_PACKAGE_INVALID", format!("invalid local apk: {err}")))?;
    command.args(["--allow-untrusted", "--"]).arg(path);
  } else {
    if !valid_package_name(target) || Path::new(target).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("apk")) {
      return Err(CliError::new("E_PACKAGE_INVALID", "expected a package name or an existing local apk file"));
    }
    command.arg("--").arg(target);
  }
  Ok(command)
}

fn valid_package_name(name: &str) -> bool {
  name.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
    && name.bytes().all(|ch| ch.is_ascii_alphanumeric() || b"+_.-".contains(&ch))
}

fn is_local_apk_path(target: &str) -> bool {
  let path = Path::new(target);
  path.is_file()
    && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("apk"))
}

fn parse_package_version(output: &str, name: &str) -> Option<String> {
  output.lines().map(str::trim).filter(|line| !line.is_empty()).find_map(|line| {
    let suffix = line.strip_prefix(name)?.strip_prefix('-')?;
    (!suffix.is_empty()).then(|| suffix.to_string())
  })
}

fn package_name_from_installed_line(line: &str) -> Option<String> {
  let value = line.trim();
  if value.is_empty() { return None; }
  let value = value.strip_suffix(" [installed]").unwrap_or(value);
  value.rmatch_indices('-').find_map(|(index, _)| {
    let version = &value[index + 1..];
    (version.as_bytes().first().is_some_and(u8::is_ascii_digit) && index > 0)
      .then(|| value[..index].to_string())
  })
}

#[cfg(test)]
mod tests {
  use super::{install_command, is_local_apk_path, package_name_from_installed_line, parse_package_version};
  #[test]
  fn parses_apk_name_version() {
    assert_eq!(parse_package_version("luci-1.2.3\n", "luci"), Some("1.2.3".into()));
    assert_eq!(parse_package_version("luci-2:1.2.3-r0\n", "luci"), Some("2:1.2.3-r0".into()));
  }
  #[test]
  fn parses_apk_name_with_hyphens() {
    assert_eq!(package_name_from_installed_line("luci-app-test-1.2.3"), Some("luci-app-test".into()));
    assert_eq!(package_name_from_installed_line("luci-app-test-1.2.3-r0 [installed]"), Some("luci-app-test".into()));
  }
  #[test]
  fn rejects_empty_apk_name_version() { assert_eq!(parse_package_version("luci-\n", "luci"), None); assert_eq!(package_name_from_installed_line(""), None); }

  #[test]
  fn only_existing_apk_files_are_local_packages() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let apk = dir.path().join("demo.APK");
    std::fs::write(&apk, b"apk").expect("write apk");
    assert!(is_local_apk_path(&apk.to_string_lossy()));
    assert!(!is_local_apk_path("https://example.test/demo.apk"));
    assert!(!is_local_apk_path("missing.apk"));
  }

  #[test]
  fn apk_install_command_separates_local_files_and_package_names() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let apk = dir.path().join("demo.apk");
    std::fs::write(&apk, b"apk").expect("write apk");
    let local = install_command(&apk.to_string_lossy()).expect("local command").get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();
    let canonical_apk = apk.canonicalize().expect("canonical apk").to_string_lossy().into_owned();
    assert_eq!(local, vec!["add".to_string(), "--allow-untrusted".to_string(), "--".to_string(), canonical_apk]);
    let remote = install_command("luci-app-demo").expect("package command").get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();
    assert_eq!(remote, vec!["add", "--", "luci-app-demo"]);
    assert!(install_command("missing.apk").is_err());
  }
}
