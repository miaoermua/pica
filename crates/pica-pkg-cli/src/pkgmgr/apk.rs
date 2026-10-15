use super::index::{self, IndexSource, run_refresh};
use super::{PackageManager, PackageManagerKind, describe_targets};
use crate::app::{CliError, CliResult, E_IO, E_MISSING_COMMAND, E_NO_SPACE, E_RUNTIME};
use crate::system::stderr_or_stdout;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Where apk keeps the indexes it fetched, in the order apk looks for them: the
/// configured cache directory, else `var/cache/apk` under the root.
const APK_CACHE_DIRS: [&str; 2] = ["/etc/apk/cache", "/var/cache/apk"];
/// The file that names apk's repositories.
const APK_REPOSITORIES: &str = "/etc/apk/repositories";

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

  /// The index of this host, as its configuration describes it.
  fn index() -> ApkIndex {
    ApkIndex::system()
  }
}

/// The feed index `apk` answers "is this package available" from.
pub(crate) struct ApkIndex {
  conf_files: Vec<PathBuf>,
  cache_dirs: Vec<PathBuf>,
  /// The refresh command, program first.
  update: Vec<String>,
}

impl ApkIndex {
  fn system() -> Self {
    Self {
      conf_files: vec![PathBuf::from(APK_REPOSITORIES)],
      cache_dirs: APK_CACHE_DIRS.iter().map(PathBuf::from).collect(),
      update: vec!["apk".to_string(), "update".to_string()],
    }
  }

  /// The directory apk caches its indexes in, when there is one.
  fn cache_dir(&self) -> Option<&Path> {
    self.cache_dirs.iter().find(|dir| dir.is_dir()).map(PathBuf::as_path)
  }

  /// The indexes apk has cached.
  ///
  /// apk names each one after a digest of the repository URL it came from
  /// (`APKINDEX.<digest>.tar.gz`, and a `.adb` on apk-tools 3.x), and that
  /// digest changed between releases, so pica cannot work out which file
  /// belongs to which feed: it counts them instead, and leaves it to the
  /// refresh to make the count match the configuration.
  fn cached_indexes(&self) -> Vec<PathBuf> {
    let Some(dir) = self.cache_dir() else {
      return Vec::new();
    };
    fs::read_dir(dir)
      .into_iter()
      .flatten()
      .flatten()
      .map(|entry| entry.path())
      .filter(|path| {
        path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("APKINDEX"))
      })
      .collect()
  }

  /// When the newest configured repository list was written.
  fn newest_conf(&self) -> Option<SystemTime> {
    self
      .conf_files
      .iter()
      .filter_map(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok())
      .max()
  }
}

impl IndexSource for ApkIndex {
  fn backend(&self) -> &'static str {
    "apk"
  }

  fn conf_files(&self) -> Vec<PathBuf> {
    self.conf_files.clone()
  }

  fn missing_indexes(&self) -> CliResult<Vec<String>> {
    let repos = read_apk_repositories(&self.conf_files)?;
    if repos.is_empty() {
      return Ok(Vec::new());
    }

    let dirs: Vec<String> = self.cache_dirs.iter().map(|dir| dir.display().to_string()).collect();
    let Some(dir) = self.cache_dir() else {
      return Ok(vec![format!("no index cache directory ({})", dirs.join(", "))]);
    };

    let indexes = self.cached_indexes();
    // apk fetches one index per repository, so fewer files than repositories
    // means at least one of them was never fetched — which one cannot be told
    // (the file names carry a digest, not the URL), and does not have to be.
    if indexes.len() < repos.len() {
      return Ok(vec![format!(
        "too few indexes cached in {}: {} for {} repositories",
        dir.display(),
        indexes.len(),
        repos.len()
      )]);
    }

    // A repository list edited after the last update is not what the cached
    // indexes were fetched for, so apk would answer about feeds that are no
    // longer the ones this host names.
    let newest = indexes
      .iter()
      .filter_map(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok())
      .max();
    match (self.newest_conf(), newest) {
      (Some(conf), Some(newest)) if newest < conf => {
        let file = self
          .conf_files
          .iter()
          .map(|path| path.display().to_string())
          .collect::<Vec<_>>()
          .join(", ");
        Ok(vec![format!("the APKINDEX in {} is older than {file}", dir.display())])
      }
      _ => Ok(Vec::new()),
    }
  }

  fn refresh(&self) -> CliResult<()> {
    let program = self.update.first().cloned().unwrap_or_else(|| "apk".to_string());
    let outcome = run_refresh(&self.update)?;
    if outcome.success { Ok(()) } else { Err(CliError::new(E_RUNTIME, outcome.failure(&program))) }
  }
}

/// The repository URLs of an apk repository file: one per line, `#` comments
/// skipped, and a leading `@tag` dropped since it pins a priority rather than
/// naming a feed of its own.
fn read_apk_repositories(paths: &[PathBuf]) -> CliResult<Vec<String>> {
  let mut repos = Vec::new();
  for path in paths {
    match fs::read_to_string(path) {
      Ok(text) => repos.extend(parse_apk_repositories(&text)),
      Err(err) if err.kind() == ErrorKind::NotFound => {}
      Err(err) => {
        return Err(CliError::new(E_IO, format!("read {} failed: {err}", path.display())));
      }
    }
  }
  Ok(repos)
}

fn parse_apk_repositories(text: &str) -> Vec<String> {
  text
    .lines()
    .map(|line| line.split('#').next().unwrap_or_default().trim())
    .filter(|line| !line.is_empty())
    .map(|line| line.split_whitespace().last().unwrap_or_default())
    .filter(|url| !url.is_empty())
    .map(ToString::to_string)
    .collect()
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

  fn update_index(&self) -> CliResult<()> {
    index::ensure_index(&Self::index())
  }

  fn package_exists(&self, name: &str) -> CliResult<bool> {
    let output = Command::new("apk").args(["search", "--exact", name]).output().map_err(|err| {
      let (code, message) = if err.kind() == ErrorKind::NotFound {
        (E_MISSING_COMMAND, "missing required command: apk".to_string())
      } else {
        (E_RUNTIME, format!("apk search --exact {name} failed: {err}"))
      };
      CliError::new(code, message)
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let names: Vec<String> = stdout.lines().filter_map(apk_result_name).collect();
    if names.iter().any(|found| found == name) {
      return Ok(true);
    }

    let detail = || stderr_or_stdout(&output.stdout, &output.stderr);
    match output.status.code() {
      // apk 3.x exits 1 for a query that matched nothing; the releases that
      // exit 0 say the same thing with an empty list. Either way an answer that
      // names no package is the answer "not there".
      Some(0 | 1) if names.is_empty() => {
        // ...but only if apk had an index to answer from: with none cached it
        // matches nothing for *every* name, which is not an answer to give
        // about one of them.
        index::index_ready(&Self::index())?;
        Ok(false)
      }
      // A match that is not this package by name: a feed package that
      // `provides` it, which is what `apk add <name>` would resolve to.
      Some(0) => Ok(true),
      Some(code) => Err(CliError::new(
        E_RUNTIME,
        format!("apk search --exact {name} failed (exit {code}): {}", detail()),
      )),
      None => Err(CliError::new(
        E_RUNTIME,
        format!("apk search --exact {name} was killed by a signal: {}", detail()),
      )),
    }
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

  fn install(&self, label: &str, targets: &[String]) -> CliResult<()> {
    let Some(first) = targets.first() else {
      return Ok(());
    };
    let local = is_local_apk_path(first);
    let forced = local
      && self.force_non_repository
      && accepts_force_non_repository(installed_apk_version().as_deref());

    let output = install_command(targets, forced)?
      .output()
      .map_err(|err| CliError::new("E_APK_INSTALL", format!("apk add failed: {err}")))?;
    if output.status.success() {
      return Ok(());
    }
    // apk's own wording for the two ways a local file is turned down is not
    // something to guess at, so the message says what pica did instead: which
    // knob to move, and which way it is currently set.
    let target = describe_targets(targets);
    let mut detail = stderr_or_stdout(&output.stdout, &output.stderr);
    if local && !forced {
      detail.push_str(
        "; a local apk needs --force-non-repository on apk 3.x for a package no repository provides, \
         and this request ran without it (set \"apk_force_non_repository\": true in pica.json)",
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

/// The `apk add` for `targets`, with `force_non_repository` saying whether local
/// files may be installed without a repository vouching for them.
///
/// Local files and package names are two different requests and are built apart
/// here: the file flags apply to paths pica extracted from its own archive,
/// while a name is left to apk's own repositories and their signatures.
///
/// One command carries the whole request, which is what lets apk resolve a
/// dependency between two of the files it is given. apk sees every package of a
/// single `add` as one transaction; a file installed on its own can only be
/// satisfied from the repositories.
fn install_command(targets: &[String], force_non_repository: bool) -> CliResult<Command> {
  let mut command = Command::new("apk");
  command.arg("add");
  let Some(first) = targets.first() else {
    return Ok(command);
  };

  // Local files and repository names are two different requests. A file carries
  // flags a name must not be given, so a request holding both has no single
  // meaning and is refused rather than guessed at.
  let local = is_local_apk_path(first);
  if targets.iter().any(|target| is_local_apk_path(target) != local) {
    return Err(CliError::new(
      "E_PACKAGE_INVALID",
      "one apk request cannot mix local apk files with package names",
    ));
  }

  if local {
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
    command.arg("--");
    for target in targets {
      let path = Path::new(target)
        .canonicalize()
        .map_err(|err| CliError::new("E_PACKAGE_INVALID", format!("invalid local apk: {err}")))?;
      command.arg(path);
    }
  } else {
    for target in targets {
      if !valid_package_name(target)
        || Path::new(target).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("apk"))
      {
        return Err(CliError::new(
          "E_PACKAGE_INVALID",
          "expected a package name or an existing local apk file",
        ));
      }
    }
    command.arg("--");
    command.args(targets);
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

/// The package a line of `apk search --exact` output names.
///
/// The output is `name-version` (or `name-version [installed]`), and a release
/// that prints bare names is read as one too. Anything else — a warning, a
/// progress line — names no package, and is not a match for anything.
fn apk_result_name(line: &str) -> Option<String> {
  let value = line.trim();
  if let Some((name, _)) = split_installed_line(value) {
    return Some(name);
  }
  valid_package_name(value).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
  use super::{
    ApkIndex, accepts_force_non_repository, apk_major_version, apk_result_name, install_command,
    is_local_apk_path, parse_apk_repositories, parse_package_version, split_installed_line,
  };
  use crate::app::{E_INDEX_UNAVAILABLE, E_IO};
  use crate::pkgmgr::index::{self, IndexSource};
  use pretty_assertions::assert_eq;
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::time::{Duration, SystemTime};

  /// An index reading one repository file and one cache directory, refreshed by
  /// whatever command the test hands it.
  fn index_for(root: &Path, repos: &str, update: Vec<String>) -> (ApkIndex, PathBuf) {
    let conf = root.join("repositories");
    fs::write(&conf, repos).expect("write repositories");
    let cache = root.join("cache");
    fs::create_dir_all(&cache).expect("create cache dir");
    let index = ApkIndex { conf_files: vec![conf], cache_dirs: vec![cache.clone()], update };
    (index, cache)
  }

  fn shell(script: String) -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string(), script]
  }

  /// Backdates `path`, which is how a test says "this index was fetched before
  /// the repository list it is being asked about".
  fn age(path: &Path, seconds: u64) {
    let file = fs::File::options().write(true).open(path).expect("open file to age");
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
    file.set_times(fs::FileTimes::new().set_modified(when)).expect("set mtime");
  }

  #[test]
  fn parses_apk_repository_lines() {
    assert_eq!(
      parse_apk_repositories(
        "# a comment\n\
         \n\
         https://dl-cdn.alpinelinux.org/alpine/v3.20/main\n\
         \t https://example.test/edge  # trailing comment\n\
         @edge https://example.test/pinned\n"
      ),
      vec![
        "https://dl-cdn.alpinelinux.org/alpine/v3.20/main".to_string(),
        "https://example.test/edge".to_string(),
        "https://example.test/pinned".to_string(),
      ]
    );
  }

  #[test]
  fn apk_search_output_names_packages_or_nothing() {
    assert_eq!(apk_result_name("luci-app-demo-1.2.3-r0"), Some("luci-app-demo".to_string()));
    assert_eq!(
      apk_result_name("luci-app-demo-1.2.3-r0 [installed]"),
      Some("luci-app-demo".to_string())
    );
    assert_eq!(apk_result_name("luci-app-demo"), Some("luci-app-demo".to_string()));
    assert_eq!(apk_result_name("ERROR: unable to select packages"), None);
    assert_eq!(apk_result_name(""), None);
  }

  #[test]
  fn a_host_without_repositories_has_no_index_to_need() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (index, _) = index_for(root.path(), "# no repositories\n", Vec::new());
    assert_eq!(index.missing_indexes().expect("repositories are readable"), Vec::<String>::new());
  }

  #[test]
  fn every_repository_needs_an_index_file_of_its_own() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (index, cache) = index_for(
      root.path(),
      "https://example.test/main\nhttps://example.test/community\n",
      Vec::new(),
    );
    let missing = index.missing_indexes().expect("repositories are readable");
    assert_eq!(missing.len(), 1);
    assert!(missing[0].contains("too few indexes cached in"), "{}", missing[0]);
    assert!(missing[0].contains("0 for 2 repositories"), "{}", missing[0]);
    assert!(missing[0].contains(&cache.display().to_string()), "{}", missing[0]);

    // apk names the cached index after a digest of the repository URL, which
    // pica does not try to reproduce — so the files are counted, not matched.
    fs::write(cache.join("APKINDEX.abcdef12.tar.gz"), b"index").expect("write index");
    let missing = index.missing_indexes().expect("repositories are readable");
    assert_eq!(missing.len(), 1);
    assert!(missing[0].contains("1 for 2 repositories"), "{}", missing[0]);

    fs::write(cache.join("APKINDEX.34567890.tar.gz"), b"index").expect("write second index");
    assert_eq!(index.missing_indexes().expect("repositories are readable"), Vec::<String>::new());
  }

  #[test]
  fn an_index_older_than_the_repository_list_is_missing() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (index, cache) = index_for(root.path(), "https://example.test/main\n", Vec::new());
    let cached = cache.join("APKINDEX.abcdef12.tar.gz");
    fs::write(&cached, b"index").expect("write index");
    // Written long before the repository list that is being asked about: it
    // cannot say which feeds that list names.
    age(&cached, 0);

    let missing = index.missing_indexes().expect("repositories are readable");
    assert_eq!(missing.len(), 1);
    assert!(missing[0].contains("older than"), "{}", missing[0]);
  }

  #[test]
  fn a_repository_file_that_cannot_be_read_is_an_error() {
    let root = tempfile::tempdir().expect("create temp dir");
    let cache = root.path().join("cache");
    fs::create_dir_all(&cache).expect("create cache dir");
    let index = ApkIndex {
      // A directory where a file belongs: present, and impossible to read.
      conf_files: vec![root.path().to_path_buf()],
      cache_dirs: vec![cache],
      update: Vec::new(),
    };
    let err = index.missing_indexes().expect_err("the repositories are unknown, not absent");
    assert_eq!(err.code, E_IO);
  }

  #[test]
  fn a_missing_index_is_fetched_and_a_present_one_is_not() {
    let root = tempfile::tempdir().expect("create temp dir");
    let fetch = shell(format!(
      "mkdir -p {0} && echo index > {0}/APKINDEX.abcdef12.tar.gz",
      root.path().join("cache").display()
    ));
    let (index, cache) = index_for(root.path(), "https://example.test/main\n", fetch);
    index::ensure_index(&index).expect("the refresh wrote the index");
    assert!(cache.join("APKINDEX.abcdef12.tar.gz").is_file());

    // The same repositories, this time with a refresh that would fail: an index
    // that is already cached is not fetched again, which is what keeps the
    // expensive step off the routers.
    let would_fail = ApkIndex { update: shell("exit 9".to_string()), ..index };
    index::ensure_index(&would_fail).expect("an index that is there is not refetched");
  }

  #[test]
  fn a_refresh_that_fetched_nothing_is_reported() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (index, cache) = index_for(
      root.path(),
      "https://example.test/main\n",
      shell("echo 'ERROR: https://example.test/main: temporary failure' >&2; exit 1".to_string()),
    );
    let err = index::ensure_index(&index).expect_err("no index was written");
    assert_eq!(err.code, E_INDEX_UNAVAILABLE);
    assert!(err.message.contains("too few indexes cached in"), "{}", err.message);
    assert!(err.message.contains(&cache.display().to_string()), "{}", err.message);
    assert!(
      err.message.contains("update failed (exit 1): ERROR: https://example.test/main"),
      "{}",
      err.message
    );
  }

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
  fn args(targets: &[&str], force_non_repository: bool) -> Vec<String> {
    let targets: Vec<String> = targets.iter().map(ToString::to_string).collect();
    install_command(&targets, force_non_repository)
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
      args(&[&apk.to_string_lossy()], true),
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
      args(&[&apk.to_string_lossy()], false),
      vec!["add".to_string(), "--allow-untrusted".to_string(), "--".to_string(), canonical_apk]
    );
    assert_eq!(
      args(&["luci-app-demo"], true),
      vec!["add", "--", "luci-app-demo"],
      "a repository package is left to apk and its signatures: neither local flag applies to a name"
    );
    assert!(install_command(&["missing.apk".to_string()], true).is_err());
    assert!(install_command(&["a/b.apk".to_string()], true).is_err());
  }

  #[test]
  fn one_apk_add_carries_a_whole_set_of_local_files() {
    // The containers of one application are installed in one request, so a file
    // that depends on a sibling can be resolved: apk only sees the two as one
    // transaction when they arrive together.
    let dir = tempfile::tempdir().expect("create temp dir");
    let mut files = Vec::new();
    for name in ["aaa-app.apk", "zzz-core.apk"] {
      let path = dir.path().join(name);
      std::fs::write(&path, b"apk").expect("write apk");
      files.push(path.canonicalize().expect("canonical apk").to_string_lossy().into_owned());
    }
    let targets: Vec<&str> = files.iter().map(String::as_str).collect();

    assert_eq!(
      args(&targets, true),
      vec![
        "add".to_string(),
        "--allow-untrusted".to_string(),
        "--force-non-repository".to_string(),
        "--".to_string(),
        files[0].clone(),
        files[1].clone()
      ],
      "both files, in the order the caller gave, after the one `--`"
    );

    // A repository request keeps the same shape, and keeps its names.
    assert_eq!(args(&["aaa-app", "luci-base"], true), vec!["add", "--", "aaa-app", "luci-base"]);
  }

  #[test]
  fn a_local_file_and_a_package_name_are_not_one_request() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let apk = dir.path().join("demo.apk");
    std::fs::write(&apk, b"apk").expect("write apk");
    let mixed = vec![apk.to_string_lossy().into_owned(), "luci-app-demo".to_string()];

    let err = install_command(&mixed, true).expect_err("a mixed request has no single meaning");
    assert!(err.message.contains("package name"), "{}", err.message);

    // The same the other way round: a name first, a file after it.
    let reversed = vec!["luci-app-demo".to_string(), mixed[0].clone()];
    let err = install_command(&reversed, true).expect_err("a mixed request has no single meaning");
    assert!(err.message.contains("package name"), "{}", err.message);
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
