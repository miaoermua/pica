use super::{PackageManager, PackageManagerKind};
use crate::app::{CliError, CliResult, E_NO_SPACE, E_RUNTIME};
use crate::system::{has_command, stderr_or_stdout};
use std::path::Path;
use std::process::Command;

pub struct ApkBackend {
  /// The `apk_force_non_repository` setting, which decides whether a local
  /// `.apk` file is given `--force-non-repository` on an `apk` that takes it.
  force_non_repository: bool,
}

impl ApkBackend {
  /// `force_non_repository` is [`crate::app::conf_get_apk_force_non_repository`]:
  /// whether a local file that no repository vouches for may be installed.
  #[must_use]
  pub fn new(force_non_repository: bool) -> Self {
    Self { force_non_repository }
  }
}

impl Default for ApkBackend {
  fn default() -> Self {
    // Matches the setting's own default: a local file is installed the way
    // current OpenWrt needs, and the version check below keeps that from
    // reaching an `apk` that has never heard of the flag.
    Self::new(true)
  }
}

impl PackageManager for ApkBackend {
  fn kind(&self) -> PackageManagerKind {
    PackageManagerKind::Apk
  }

  fn update_index(&self) {
    if !has_command("apk") {
      return;
    }
    let _ = Command::new("apk").arg("update").output();
  }

  fn package_exists(&self, name: &str) -> bool {
    let Ok(output) = Command::new("apk").args(["search", "--exact", name]).output() else {
      return false;
    };
    output.status.success()
      && String::from_utf8_lossy(&output.stdout).lines().any(|line| !line.trim().is_empty())
  }

  fn is_installed(&self, name: &str) -> bool {
    let Ok(output) = Command::new("apk").args(["info", "--installed", name]).output() else {
      return false;
    };
    output.status.success()
  }

  fn installed_version(&self, name: &str) -> Option<String> {
    let output = Command::new("apk").args(["list", "--installed", "--", name]).output().ok()?;
    if !output.status.success() {
      return None;
    }
    parse_package_version(&String::from_utf8_lossy(&output.stdout), name)
  }

  fn install(&self, label: &str, target: &str) -> CliResult<()> {
    let local = is_local_apk_path(target);
    let forced = local
      && self.force_non_repository
      && accepts_force_non_repository(installed_apk_version().as_deref());

    let output = install_command(target, forced)?
      .output()
      .map_err(|err| CliError::new("E_APK_INSTALL", format!("apk add failed: {err}")))?;
    if output.status.success() {
      return Ok(());
    }
    let mut detail = stderr_or_stdout(&output.stdout, &output.stderr);
    // apk's own wording for the two ways a local file is turned down is not
    // something to guess at, so the message says what pica did instead: which
    // knob to move, and which way it is currently set.
    if local && !forced {
      detail.push_str(
        "; this local apk was installed without --force-non-repository, which apk 3.x requires for a \
         package that no repository provides (set \"apk_force_non_repository\": true in pica.json)",
      );
    }
    if forced && detail.to_ascii_lowercase().contains("force-non-repository") {
      detail.push_str(
        "; this apk does not accept --force-non-repository (set \"apk_force_non_repository\": false \
         in pica.json)",
      );
    }
    if detail.to_ascii_lowercase().contains("no space left on device") {
      return Err(CliError::new(
        E_NO_SPACE,
        format!("{label} install failed: {target} (storage-full). detail=[{detail}]"),
      ));
    }
    Err(CliError::new(E_RUNTIME, format!("{label} install failed: {target} detail=[{detail}]")))
  }

  fn remove(&self, target: &str) -> CliResult<()> {
    let output = Command::new("apk")
      .args(["del", target])
      .output()
      .map_err(|err| CliError::new(E_RUNTIME, format!("apk del failed: {err}")))?;
    if output.status.success() {
      Ok(())
    } else {
      Err(CliError::new(
        E_RUNTIME,
        format!(
          "apk del failed: {target} detail=[{}]",
          stderr_or_stdout(&output.stdout, &output.stderr)
        ),
      ))
    }
  }

  fn installed(&self) -> Vec<(String, String)> {
    let Ok(output) = Command::new("apk").args(["list", "--installed", "-q"]).output() else {
      return Vec::new();
    };
    let mut packages: Vec<(String, String)> =
      String::from_utf8_lossy(&output.stdout).lines().filter_map(split_installed_line).collect();
    packages.sort();
    packages.dedup();
    packages
  }

  fn depends(&self, name: &str) -> Vec<String> {
    let Ok(output) = Command::new("apk").args(["info", "--depends", name]).output() else {
      return Vec::new();
    };
    let mut packages: Vec<String> = String::from_utf8_lossy(&output.stdout)
      .lines()
      .filter_map(|line| split_installed_line(line).map(|(name, _)| name))
      .collect();
    packages.sort();
    packages.dedup();
    packages
  }

  fn owns_file(&self, path: &str) -> Option<String> {
    let output = Command::new("apk").args(["info", "--who-owns", path]).output().ok()?;
    if !output.status.success() {
      return None;
    }
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .find_map(|line| split_installed_line(line).map(|(name, _)| name))
  }

  fn architectures(&self) -> Vec<String> {
    let Ok(output) = Command::new("apk").arg("--print-arch").output() else {
      return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .map(str::trim)
      .filter(|line| !line.is_empty())
      .map(ToString::to_string)
      .collect()
  }
}

/// The `apk add` for `target`, with `force_non_repository` saying whether a
/// local file may be installed without a repository vouching for it.
///
/// A local file and a package name are two different requests and are built
/// apart here: the file flags apply to a path pica extracted from its own
/// archive, while a name is left to apk's own repositories and their signatures.
fn install_command(target: &str, force_non_repository: bool) -> CliResult<Command> {
  let mut command = Command::new("apk");
  command.arg("add");
  if is_local_apk_path(target) {
    let path = Path::new(target)
      .canonicalize()
      .map_err(|err| CliError::new("E_PACKAGE_INVALID", format!("invalid local apk: {err}")))?;
    // `--allow-untrusted` waives apk's signature check, which is what an apk
    // extracted from a Pica archive needs: it was signed by whoever built it,
    // not by a key this router holds. What stands in its place is Pica's own
    // check on the archive it came out of — a repository install has its sha256
    // compared against the index before anything is unpacked — so this flag is
    // for files that arrived inside a verified archive, plus any file the
    // operator named directly on the command line, which is their own decision
    // to trust it. A repository package never reaches this branch.
    command.arg("--allow-untrusted");
    // apk-tools 3.x (OpenWrt 25.12 and later) also refuses a package that is
    // not from a configured repository, unless told otherwise; 2.x has no such
    // flag and takes the file as it comes.
    if force_non_repository {
      command.arg("--force-non-repository");
    }
    command.arg("--").arg(path);
  } else {
    if !valid_package_name(target)
      || Path::new(target).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("apk"))
    {
      return Err(CliError::new(
        "E_PACKAGE_INVALID",
        "expected a package name or an existing local apk file",
      ));
    }
    command.arg("--").arg(target);
  }
  Ok(command)
}

/// The version line `apk` prints, or `None` when it cannot be read.
fn installed_apk_version() -> Option<String> {
  let output = Command::new("apk").arg("--version").output().ok()?;
  // Either stream: apk-tools has printed its version on both across releases.
  let text = stderr_or_stdout(&output.stdout, &output.stderr);
  (!text.is_empty()).then_some(text)
}

/// Whether `apk --version` output describes an apk that knows
/// `--force-non-repository`.
///
/// apk-tools 2.x, which `OpenWrt` shipped before 25.12, has no such flag and
/// installs a local file without it. Passing an option the binary does not know
/// aborts the command before it does anything, so a version that reads as 2.x
/// drops the flag. A version that cannot be read keeps it: the case this
/// protects is the refusal on 3.x, whereas the opposite mistake surfaces as
/// `unrecognized option` and names the setting to change.
fn accepts_force_non_repository(version: Option<&str>) -> bool {
  !matches!(version.and_then(apk_major_version), Some(major) if major < 3)
}

/// The major version in an `apk --version` line such as `apk-tools 3.0.0-r0`,
/// `apk-tools 2.14.4, compiled for x86_64` or `apk 3.0.0_rc1`.
fn apk_major_version(version: &str) -> Option<u32> {
  version.split_whitespace().find_map(|token| {
    token
      .split(['.', '-', '_', ':'])
      .next()
      .filter(|major| major.bytes().all(|ch| ch.is_ascii_digit()))
      .and_then(|major| major.parse().ok())
  })
}

fn valid_package_name(name: &str) -> bool {
  name.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
    && name.bytes().all(|ch| ch.is_ascii_alphanumeric() || b"+_.-".contains(&ch))
}

fn is_local_apk_path(target: &str) -> bool {
  let path = Path::new(target);
  path.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("apk"))
}

fn parse_package_version(output: &str, name: &str) -> Option<String> {
  output.lines().map(str::trim).filter(|line| !line.is_empty()).find_map(|line| {
    let suffix = line.strip_prefix(name)?.strip_prefix('-')?;
    (!suffix.is_empty()).then(|| suffix.to_string())
  })
}

/// `apk list` prints `name-version`. Package names may contain `-`, so the split
/// is the last `-` that starts a version (a version always starts with a digit).
fn split_installed_line(line: &str) -> Option<(String, String)> {
  let value = line.trim();
  if value.is_empty() {
    return None;
  }
  let value = value.strip_suffix(" [installed]").unwrap_or(value);
  value.rmatch_indices('-').find_map(|(index, _)| {
    let version = &value[index + 1..];
    (version.as_bytes().first().is_some_and(u8::is_ascii_digit) && index > 0)
      .then(|| (value[..index].to_string(), version.to_string()))
  })
}

#[cfg(test)]
mod tests {
  use super::{
    accepts_force_non_repository, apk_major_version, install_command, is_local_apk_path,
    parse_package_version, split_installed_line,
  };

  #[test]
  fn parses_apk_name_version() {
    assert_eq!(parse_package_version("luci-1.2.3\n", "luci"), Some("1.2.3".into()));
    assert_eq!(parse_package_version("luci-2:1.2.3-r0\n", "luci"), Some("2:1.2.3-r0".into()));
  }
  #[test]
  fn parses_apk_name_with_hyphens() {
    assert_eq!(
      split_installed_line("luci-app-test-1.2.3"),
      Some(("luci-app-test".into(), "1.2.3".into()))
    );
    assert_eq!(
      split_installed_line("luci-app-test-1.2.3-r0 [installed]"),
      Some(("luci-app-test".into(), "1.2.3-r0".into()))
    );
  }
  #[test]
  fn rejects_empty_apk_name_version() {
    assert_eq!(parse_package_version("luci-\n", "luci"), None);
    assert_eq!(split_installed_line(""), None);
  }

  #[test]
  fn splits_installed_lines_with_revision_and_epoch() {
    assert_eq!(
      split_installed_line("luci-app-test-1.2.3-r0"),
      Some(("luci-app-test".into(), "1.2.3-r0".into()))
    );
    assert_eq!(
      split_installed_line("luci-2:1.2.3-r0 [installed]"),
      Some(("luci".into(), "2:1.2.3-r0".into()))
    );
    assert_eq!(split_installed_line("no-version-here"), None);
  }

  #[test]
  fn only_existing_apk_files_are_local_packages() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let apk = dir.path().join("demo.APK");
    std::fs::write(&apk, b"apk").expect("write apk");
    assert!(is_local_apk_path(&apk.to_string_lossy()));
    assert!(!is_local_apk_path("https://example.test/demo.apk"));
    assert!(!is_local_apk_path("missing.apk"));
  }

  /// The argument list `install_command` would run, as text.
  fn args(target: &str, force_non_repository: bool) -> Vec<String> {
    install_command(target, force_non_repository)
      .expect("command")
      .get_args()
      .map(|arg| arg.to_string_lossy().into_owned())
      .collect()
  }

  #[test]
  fn apk_install_command_separates_local_files_and_package_names() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let apk = dir.path().join("demo.apk");
    std::fs::write(&apk, b"apk").expect("write apk");
    let canonical_apk = apk.canonicalize().expect("canonical apk").to_string_lossy().into_owned();

    assert_eq!(
      args(&apk.to_string_lossy(), true),
      vec![
        "add".to_string(),
        "--allow-untrusted".to_string(),
        "--force-non-repository".to_string(),
        "--".to_string(),
        canonical_apk.clone()
      ],
      "a local file carries the flags that make apk 3.x accept one"
    );
    assert_eq!(
      args(&apk.to_string_lossy(), false),
      vec!["add".to_string(), "--allow-untrusted".to_string(), "--".to_string(), canonical_apk]
    );
    assert_eq!(
      args("luci-app-demo", true),
      vec!["add", "--", "luci-app-demo"],
      "a repository package is left to apk and its signatures: neither local flag applies to a name"
    );
    assert!(install_command("missing.apk", true).is_err());
  }

  #[test]
  fn the_apk_version_line_is_read_from_the_real_spellings() {
    assert_eq!(apk_major_version("apk-tools 3.0.0"), Some(3));
    assert_eq!(apk_major_version("apk-tools 2.14.4, compiled for x86_64"), Some(2));
    assert_eq!(apk_major_version("apk 3.0.0-r0"), Some(3));
    assert_eq!(apk_major_version("apk-tools 3.0.0_rc1"), Some(3));
    assert_eq!(apk_major_version("unknown error"), None);
    assert_eq!(apk_major_version(""), None);
  }

  #[test]
  fn only_apk_3_is_told_about_the_non_repository_flag() {
    for version in ["apk-tools 3.0.0", "apk-tools 4.1.0", "apk-tools 3.0.0_rc1"] {
      assert!(accepts_force_non_repository(Some(version)), "{version}");
    }
    assert!(!accepts_force_non_repository(Some("apk-tools 2.14.4, compiled for x86_64")));
    // A version that cannot be read keeps the flag: the alternative is dropping
    // it on the release this exists for, and the opposite mistake names the way
    // out in its own error message.
    assert!(accepts_force_non_repository(None));
    assert!(accepts_force_non_repository(Some("unknown error")));
  }
}
