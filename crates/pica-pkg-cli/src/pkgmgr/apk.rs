use super::apk_repositories::{
  self, APK_ARCH, APK_REPOSITORIES, APK_REPOSITORIES_DIRS, Configuration, IndexFile,
};
use super::index::{self, IndexSource, run_refresh};
use super::{FeedCandidate, PackageManager, PackageManagerKind, describe_targets};
use crate::app::{
  CliError, CliResult, E_MISSING_COMMAND, E_NO_SPACE, E_PACKAGE_INVALID, E_PKGMGR_INSTALL,
  E_PKGMGR_REMOVE, E_RUNTIME,
};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::system::stderr_or_stdout;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where apk keeps the indexes it fetched, in the order apk looks for them: the
/// configured cache directory, else `var/cache/apk` under the root.
const APK_CACHE_DIRS: [&str; 2] = ["/etc/apk/cache", "/var/cache/apk"];

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
  /// The repository files apk reads, in order, and the directories it then
  /// scans for `*.list` files: on `OpenWrt` the feeds are only in the latter.
  conf_files: Vec<PathBuf>,
  conf_dirs: Vec<PathBuf>,
  /// The file naming the machine type a repository URL that does not name its
  /// own index file needs.
  arch_file: PathBuf,
  /// The directories apk caches the indexes it fetched in, the first one that
  /// exists winning.
  cache_dirs: Vec<PathBuf>,
  /// The refresh command, program first.
  update: Vec<String>,
}

impl ApkIndex {
  fn system() -> Self {
    Self {
      conf_files: vec![PathBuf::from(APK_REPOSITORIES)],
      conf_dirs: APK_REPOSITORIES_DIRS.iter().map(PathBuf::from).collect(),
      arch_file: PathBuf::from(APK_ARCH),
      cache_dirs: APK_CACHE_DIRS.iter().map(PathBuf::from).collect(),
      update: vec!["apk".to_string(), "update".to_string()],
    }
  }

  /// The repositories apk would load on this host.
  fn configuration(&self) -> CliResult<Configuration> {
    apk_repositories::read(&self.conf_files, &self.conf_dirs, &self.arch_file)
  }

  /// The directory apk caches its indexes in, when there is one.
  fn cache_dir(&self) -> Option<&Path> {
    self.cache_dirs.iter().find(|dir| dir.is_dir()).map(PathBuf::as_path)
  }
}

impl IndexSource for ApkIndex {
  fn backend(&self) -> &'static str {
    "apk"
  }

  fn conf_files(&self) -> Vec<PathBuf> {
    // The files apk read, which on a host that keeps its feeds in
    // `/etc/apk/repositories.d` are not `/etc/apk/repositories` at all: the
    // message has to name what the operator edits. Only reachable through the
    // message of a missing index, whose own read already succeeded, so the
    // fallback is for a host whose configuration broke since.
    match self.configuration() {
      Ok(configuration) => configuration.files,
      Err(_) => self.conf_files.iter().chain(&self.conf_dirs).cloned().collect(),
    }
  }

  fn missing_indexes(&self) -> CliResult<Vec<String>> {
    let configuration = self.configuration()?;
    // No repository is a host with no feeds, which is not a host whose feeds
    // could not be read: there is nothing that wants an index.
    if configuration.is_empty() {
      return Ok(Vec::new());
    }

    let dir = self.cache_dir();
    let mut missing = Vec::new();

    // Without a cache directory no fetched index can be there at all, which is
    // one reason for all of them rather than the same one per feed.
    let fetched =
      configuration.repositories.iter().filter(|repository| repository.index.fetched()).count();
    if dir.is_none() && fetched > 0 {
      let dirs: Vec<String> =
        self.cache_dirs.iter().map(|path| path.display().to_string()).collect();
      missing.push(tr(Key::ApkNoCacheDir, &[&dirs.join(", "), &fetched]));
    }

    // apk names the index of a remote repository after a digest of its index
    // URL (`APKINDEX.<digest>.tar.gz`), so the file that repository needs is
    // known exactly and pica can say *which* feed has no index. A local
    // repository carries its index itself, where it lies.
    for repository in &configuration.repositories {
      let (path, complaint) = match &repository.index {
        // A local repository carries its index itself: apk reads it where it
        // lies, so that file is the one that has to be there.
        IndexFile::Local(path) => (path.clone(), tr(Key::NoIndexFile, &[&path.display()])),
        IndexFile::Fetched(name) => match dir {
          Some(dir) => (dir.join(name), tr(Key::NoCachedIndex, &[&repository.printable])),
          // said once above, for every repository that wants a cached index
          None => continue,
        },
      };
      if !path.is_file() {
        missing.push(complaint);
      }
    }

    Ok(missing)
  }

  fn refresh(&self) -> CliResult<()> {
    let program = self.update.first().cloned().unwrap_or_else(|| "apk".to_string());
    let outcome = run_refresh(&self.update)?;
    if outcome.success { Ok(()) } else { Err(CliError::new(E_RUNTIME, outcome.failure(&program))) }
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

  fn update_index(&self) -> CliResult<()> {
    index::ensure_index(&Self::index())
  }

  fn package_exists(&self, name: &str) -> CliResult<bool> {
    let output = Command::new("apk").args(["search", "--exact", name]).output().map_err(|err| {
      let (code, message) = if err.kind() == ErrorKind::NotFound {
        (E_MISSING_COMMAND, tr(Key::MissingCommand, &[&"apk"]))
      } else {
        (E_RUNTIME, tr(Key::ApkSearchFailed, &[&name, &err]))
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
      Some(code) => {
        Err(CliError::new(E_RUNTIME, tr(Key::ApkSearchExit, &[&name, &code, &detail()])))
      }
      None => Err(CliError::new(E_RUNTIME, tr(Key::ApkSearchKilled, &[&name, &detail()]))),
    }
  }

  fn feed_candidates(&self, name: &str) -> CliResult<Vec<FeedCandidate>> {
    index::index_ready(&Self::index())?;
    // `apk query` is the only command that reports both figures, and only apk 3
    // has it. An apk that cannot answer in that form still knows the versions,
    // so the fallback keeps the offer visible and leaves the sizes unknown
    // instead of borrowing one figure for the other.
    if let Some(candidates) = query_candidates(name) {
      return Ok(candidates);
    }
    search_candidates(name)
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
      .map_err(|err| CliError::new(E_PKGMGR_INSTALL, tr(Key::ApkAddFailed, &[&err])))?;
    if output.status.success() {
      return Ok(());
    }
    // apk's own wording for the two ways a local file is turned down is not
    // something to guess at, so the message says what pica did instead: which
    // knob to move, and which way it is currently set.
    let target = describe_targets(targets);
    let mut detail = stderr_or_stdout(&output.stdout, &output.stderr);
    if local && !forced {
      detail.push_str(&tr(Key::ApkNeedsForce, &[]));
    }
    if forced && detail.to_ascii_lowercase().contains("force-non-repository") {
      detail.push_str(&tr(Key::ApkRejectsForce, &[]));
    }
    if detail.to_ascii_lowercase().contains("no space left on device") {
      return Err(CliError::new(
        E_NO_SPACE,
        tr(Key::InstallStorageFull, &[&label, &target, &detail]),
      ));
    }
    Err(CliError::new(E_PKGMGR_INSTALL, tr(Key::BackendInstallFailed, &[&label, &target, &detail])))
  }

  fn remove(&self, target: &str) -> CliResult<()> {
    let output = Command::new("apk")
      .args(["del", target])
      .output()
      .map_err(|err| CliError::new(E_PKGMGR_REMOVE, tr(Key::ApkDelFailed, &[&err])))?;
    if output.status.success() {
      Ok(())
    } else {
      Err(CliError::new(
        E_PKGMGR_REMOVE,
        tr(Key::ApkDelTargetFailed, &[&target, &stderr_or_stdout(&output.stdout, &output.stderr)]),
      ))
    }
  }

  fn installed(&self) -> CliResult<Vec<(String, String)>> {
    let output =
      Command::new("apk").args(["list", "--installed", "-q"]).output().map_err(|err| {
        let (code, message) = if err.kind() == ErrorKind::NotFound {
          (E_MISSING_COMMAND, tr(Key::MissingCommand, &[&"apk"]))
        } else {
          (E_RUNTIME, tr(Key::ApkListFailed, &[&err]))
        };
        CliError::new(code, message)
      })?;
    if !output.status.success() {
      let detail = stderr_or_stdout(&output.stdout, &output.stderr);
      return Err(CliError::new(E_RUNTIME, tr(Key::ApkListExit, &[&output.status, &detail])));
    }
    let mut packages: Vec<(String, String)> =
      String::from_utf8_lossy(&output.stdout).lines().filter_map(split_installed_line).collect();
    packages.sort();
    packages.dedup();
    Ok(packages)
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
    return Err(CliError::new(E_PACKAGE_INVALID, tr(Key::ApkMixedRequest, &[])));
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
        .map_err(|err| CliError::new(E_PACKAGE_INVALID, tr(Key::InvalidLocalApk, &[&err])))?;
      command.arg(path);
    }
  } else {
    for target in targets {
      if !valid_package_name(target)
        || Path::new(target).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("apk"))
      {
        return Err(CliError::new(E_PACKAGE_INVALID, tr(Key::ApkTargetInvalid, &[])));
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

/// What `apk query` reports for `name`, or `None` when this `apk` cannot answer
/// in that form.
///
/// `--fields name,version,installed-size,file-size` with `--format json` gives
/// one row per matching package. `file-size` is the package file (`APKINDEX`'s
/// `S:`), `installed-size` the unpacked size (`I:`), both in bytes. apk omits a
/// field it does not know and counts a written `0` as unknown; both become
/// `None` here. Anything unexpected yields `None`: a shape pica does not
/// understand may not be turned into a number.
fn query_candidates(name: &str) -> Option<Vec<FeedCandidate>> {
  let output = Command::new("apk")
    .args([
      "query",
      "--available",
      "--format",
      "json",
      "--fields",
      "name,version,installed-size,file-size",
      "--",
      name,
    ])
    .output()
    .ok()?;
  if !output.status.success() {
    return None;
  }
  parse_apk_query(&String::from_utf8_lossy(&output.stdout), name)
}

/// The rows of an `apk query --format json` answer.
fn parse_apk_query(text: &str, name: &str) -> Option<Vec<FeedCandidate>> {
  let value: serde_json::Value = serde_json::from_str(text).ok()?;
  let rows = match value {
    serde_json::Value::Array(rows) => rows,
    single @ serde_json::Value::Object(_) => vec![single],
    _ => return None,
  };
  let mut candidates = Vec::new();
  for row in rows {
    let serde_json::Value::Object(fields) = row else {
      return None;
    };
    let (found, version) = match (
      fields.get("name").and_then(serde_json::Value::as_str),
      fields.get("version").and_then(serde_json::Value::as_str),
    ) {
      (Some(found), Some(version)) => (found.to_string(), version.to_string()),
      // A shape that keeps name and version in one string is what `apk search`
      // prints, and reading it here costs nothing.
      _ => split_installed_line(fields.get("package").and_then(serde_json::Value::as_str)?)?,
    };
    if found != name {
      continue;
    }
    candidates.push(FeedCandidate {
      version,
      download: apk_size(&fields, "file-size"),
      installed: apk_size(&fields, "installed-size"),
    });
  }
  Some(candidates)
}

/// One size from an `apk query` row, in bytes.
fn apk_size(fields: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u64> {
  let underscored = key.replace('-', "_");
  [key, underscored.as_str()].into_iter().find_map(|key| {
    let bytes = match fields.get(key) {
      Some(serde_json::Value::Number(number)) => number.as_u64(),
      Some(serde_json::Value::String(text)) => text.parse().ok(),
      _ => None,
    };
    bytes.filter(|bytes| *bytes > 0)
  })
}

/// The versions `apk search` reports, without sizes.
///
/// This is the fallback for an `apk` that has no `query` command, and the
/// version bounds are all it can serve: it prints `name-version` lines and says
/// nothing about bytes.
fn search_candidates(name: &str) -> CliResult<Vec<FeedCandidate>> {
  let output = Command::new("apk").args(["search", "--exact", name]).output().map_err(|err| {
    let (code, message) = if err.kind() == ErrorKind::NotFound {
      (E_MISSING_COMMAND, tr(Key::MissingCommand, &[&"apk"]))
    } else {
      (E_RUNTIME, tr(Key::ApkSearchFailed, &[&name, &err]))
    };
    CliError::new(code, message)
  })?;

  let stdout = String::from_utf8_lossy(&output.stdout);
  let candidates: Vec<FeedCandidate> = stdout
    .lines()
    .filter_map(split_installed_line)
    .filter(|(found, _)| found == name)
    .map(|(_, version)| FeedCandidate { version, download: None, installed: None })
    .collect();

  match output.status.code() {
    // apk 3 exits 1 for a query that matched nothing; the releases that exit 0
    // say the same with an empty list.
    Some(0 | 1) => Ok(candidates),
    Some(code) => Err(CliError::new(
      E_RUNTIME,
      tr(Key::ApkSearchExit, &[&name, &code, &stderr_or_stdout(&output.stdout, &output.stderr)]),
    )),
    None => Err(CliError::new(
      E_RUNTIME,
      tr(Key::ApkSearchKilled, &[&name, &stderr_or_stdout(&output.stdout, &output.stderr)]),
    )),
  }
}

#[cfg(test)]
mod tests {
  use super::{
    ApkIndex, IndexFile, accepts_force_non_repository, apk_major_version, apk_result_name,
    install_command, is_local_apk_path, parse_apk_query, parse_package_version,
    split_installed_line,
  };
  use crate::app::{E_INDEX_UNAVAILABLE, E_IO};
  use crate::pkgmgr::FeedCandidate;
  use crate::pkgmgr::index::{self, IndexSource};
  use pretty_assertions::assert_eq;
  use std::fs;
  use std::path::{Path, PathBuf};

  /// An index reading one repository file, one `repositories.d` directory and
  /// one cache directory, refreshed by whatever command the test hands it.
  fn index_for(root: &Path, repos: &str, update: Vec<String>) -> (ApkIndex, PathBuf) {
    let conf = root.join("repositories");
    fs::write(&conf, repos).expect("write repositories");
    let cache = root.join("cache");
    fs::create_dir_all(&cache).expect("create cache dir");
    let index = ApkIndex {
      conf_files: vec![conf],
      conf_dirs: Vec::new(),
      arch_file: arch_file(root),
      cache_dirs: vec![cache.clone()],
      update,
    };
    (index, cache)
  }

  /// The file naming the machine type, which a repository URL that does not
  /// name its own index file needs.
  fn arch_file(root: &Path) -> PathBuf {
    let arch = root.join("arch");
    fs::write(&arch, "x86_64\n").expect("write arch");
    arch
  }

  /// The name apk caches the index of `url_index` under, taken from the module
  /// that recognizes the repository: what a test has to write to say "this
  /// repository has an index".
  fn cached_file(index: &ApkIndex, url_index: &str) -> String {
    match &index
      .configuration()
      .expect("the repositories are readable")
      .repositories
      .iter()
      .find(|repository| repository.url_index == url_index)
      .unwrap_or_else(|| panic!("no configured repository has the index {url_index}"))
      .index
    {
      IndexFile::Fetched(name) => name.clone(),
      IndexFile::Local(path) => panic!("{url_index} is read from {}", path.display()),
    }
  }

  fn shell(script: String) -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string(), script]
  }

  #[test]
  fn apk_query_rows_carry_the_two_sizes_apart() {
    // `file-size` is the package apk downloads, `installed-size` what it takes
    // once unpacked: an offer that swapped them would overstate or understate
    // the bytes over the air.
    let rows = r#"[
      {"name":"luci-app-demo","version":"1.2.3-r1","installed-size":40960,"file-size":8192},
      {"name":"luci-app-demo","version":"1.0.0-r1","installed-size":20480}
    ]"#;
    assert_eq!(
      parse_apk_query(rows, "luci-app-demo"),
      Some(vec![
        FeedCandidate {
          version: "1.2.3-r1".to_string(),
          download: Some(8192),
          installed: Some(40960),
        },
        FeedCandidate { version: "1.0.0-r1".to_string(), download: None, installed: Some(20480) },
      ])
    );
  }

  #[test]
  fn apk_query_rows_without_sizes_report_none() {
    let rows = r#"[{"name":"demo","version":"1.0-r0"}]"#;
    assert_eq!(
      parse_apk_query(rows, "demo"),
      Some(vec![FeedCandidate { version: "1.0-r0".to_string(), download: None, installed: None }])
    );
    // A written 0 is apk saying it does not know, not a measurement.
    let zeros = r#"[{"name":"demo","version":"1.0-r0","installed-size":0,"file-size":0}]"#;
    assert_eq!(
      parse_apk_query(zeros, "demo"),
      Some(vec![FeedCandidate { version: "1.0-r0".to_string(), download: None, installed: None }])
    );
  }

  #[test]
  fn apk_query_answers_only_for_the_name_asked_about() {
    let rows = r#"[{"name":"provider","version":"2.0-r0","file-size":10}]"#;
    assert_eq!(parse_apk_query(rows, "demo"), Some(Vec::new()));
    // Strings are read the same way: apk may print numbers as strings.
    let text = r#"[{"name":"demo","version":"1.0-r0","file-size":"1024"}]"#;
    assert_eq!(
      parse_apk_query(text, "demo"),
      Some(vec![FeedCandidate {
        version: "1.0-r0".to_string(),
        download: Some(1024),
        installed: None,
      }])
    );
  }

  #[test]
  fn an_unreadable_apk_query_answer_is_not_a_number() {
    // Old releases print `name-version` lines instead of JSON, and a failure
    // prints its message: neither may become a size.
    assert_eq!(parse_apk_query("demo-1.0-r0\n", "demo"), None);
    assert_eq!(parse_apk_query("ERROR: unable to select packages:\n", "demo"), None);
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
  fn every_repository_needs_the_index_apk_would_cache_for_it() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (index, cache) = index_for(
      root.path(),
      "https://example.test/main\nhttps://example.test/community\n",
      Vec::new(),
    );

    // One message per repository, each naming the feed that has no index,
    // which the file name apk derives from the index URL makes possible.
    let missing = index.missing_indexes().expect("repositories are readable");
    assert_eq!(missing.len(), 2);
    assert!(
      missing[0].contains("no cached index for https://example.test/main/x86_64/APKINDEX.tar.gz"),
      "{}",
      missing[0]
    );
    assert!(missing[1].contains("https://example.test/community"), "{}", missing[1]);

    // The index of one repository says nothing about another one's.
    let main = cached_file(&index, "https://example.test/main/x86_64/APKINDEX.tar.gz");
    fs::write(cache.join(&main), b"index").expect("write index");
    let missing = index.missing_indexes().expect("repositories are readable");
    assert_eq!(missing.len(), 1);
    assert!(missing[0].contains("https://example.test/community"), "{}", missing[0]);

    let community = cached_file(&index, "https://example.test/community/x86_64/APKINDEX.tar.gz");
    fs::write(cache.join(community), b"index").expect("write second index");
    assert_eq!(index.missing_indexes().expect("repositories are readable"), Vec::<String>::new());
  }

  #[test]
  fn a_host_without_a_cache_directory_is_told_to_make_one() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (index, cache) = index_for(
      root.path(),
      "https://example.test/main\nhttps://example.test/community\n",
      Vec::new(),
    );
    fs::remove_dir_all(&cache).expect("remove the cache directory");
    let index = ApkIndex { cache_dirs: vec![cache.clone()], ..index };

    let missing = index.missing_indexes().expect("repositories are readable");

    // No cached index can be there at all: one reason for every feed, not the
    // same one per feed.
    assert_eq!(missing.len(), 1);
    assert!(missing[0].contains("no index cache directory"), "{}", missing[0]);
    assert!(missing[0].contains("2 repositories"), "{}", missing[0]);
    assert!(missing[0].contains(&cache.display().to_string()), "{}", missing[0]);
  }

  #[test]
  fn a_local_repository_without_its_index_is_reported() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (index, _) = index_for(root.path(), "/srv/packages\n", Vec::new());

    // A local repository carries its index itself: apk reads it where it lies
    // and caches nothing for it, so that file is the one that has to be there.
    let missing = index.missing_indexes().expect("repositories are readable");
    assert_eq!(missing.len(), 1);
    assert!(
      missing[0].contains("no index file at /srv/packages/x86_64/APKINDEX.tar.gz"),
      "{}",
      missing[0]
    );
  }

  #[test]
  fn feeds_in_the_configuration_directory_are_read() {
    // OpenWrt 25.12 keeps every feed in `/etc/apk/repositories.d/distfeeds.list`
    // and writes nothing to `/etc/apk/repositories`: a reader that knows only
    // that file sees a host with no feeds, and answers "not in the feeds" from
    // a host whose feeds have the package.
    let root = tempfile::tempdir().expect("create temp dir");
    let dir = root.path().join("repositories.d");
    fs::create_dir_all(&dir).expect("create repositories.d");
    let feeds = dir.join("distfeeds.list");
    fs::write(&feeds, "https://example.test/releases/25.12.0/base/packages.adb\n")
      .expect("write feeds");
    let cache = root.path().join("cache");
    fs::create_dir_all(&cache).expect("create cache dir");
    let index = ApkIndex {
      conf_files: vec![root.path().join("repositories")],
      conf_dirs: vec![dir],
      arch_file: arch_file(root.path()),
      cache_dirs: vec![cache],
      update: Vec::new(),
    };

    let missing = index.missing_indexes().expect("repositories are readable");

    assert_eq!(missing.len(), 1);
    assert!(
      missing[0]
        .contains("no cached index for https://example.test/releases/25.12.0/base/packages.adb"),
      "{}",
      missing[0]
    );
    // and the message says where those repositories come from, which is the
    // file the operator edits rather than the one apk finds empty
    assert_eq!(index.conf_files(), vec![feeds]);
  }

  #[test]
  fn a_repository_file_that_cannot_be_read_is_an_error() {
    let root = tempfile::tempdir().expect("create temp dir");
    let cache = root.path().join("cache");
    fs::create_dir_all(&cache).expect("create cache dir");
    let index = ApkIndex {
      // A directory where a file belongs: present, and impossible to read.
      conf_files: vec![root.path().to_path_buf()],
      conf_dirs: Vec::new(),
      arch_file: arch_file(root.path()),
      cache_dirs: vec![cache],
      update: Vec::new(),
    };
    let err = index.missing_indexes().expect_err("the repositories are unknown, not absent");
    assert_eq!(err.code, E_IO);
  }

  #[test]
  fn a_missing_index_is_fetched_and_a_present_one_is_not() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (mut index, cache) = index_for(root.path(), "https://example.test/main\n", Vec::new());
    // The refresh stands in for apk here, so it writes what apk would write:
    // the index of this repository, under the name apk caches it under.
    let name = cached_file(&index, "https://example.test/main/x86_64/APKINDEX.tar.gz");
    let fetched = cache.join(&name);
    index.update = shell(format!("echo index > {}", fetched.display()));

    index::ensure_index(&index).expect("the refresh wrote the index");
    assert!(fetched.is_file(), "the index apk reads is the one the refresh wrote");

    // The same repositories, this time with a refresh that would fail: an index
    // that is already cached is not fetched again, which is what keeps the
    // expensive step off the routers.
    let would_fail = ApkIndex { update: shell("exit 9".to_string()), ..index };
    index::ensure_index(&would_fail).expect("an index that is there is not refetched");
  }

  #[test]
  fn a_refresh_that_fetched_nothing_is_reported() {
    let root = tempfile::tempdir().expect("create temp dir");
    let (index, _) = index_for(
      root.path(),
      "https://example.test/main\n",
      shell("echo 'ERROR: https://example.test/main: temporary failure' >&2; exit 1".to_string()),
    );
    let err = index::ensure_index(&index).expect_err("no index was written");
    assert_eq!(err.code, E_INDEX_UNAVAILABLE);
    assert!(
      err.message.contains("no cached index for https://example.test/main/x86_64/APKINDEX.tar.gz"),
      "{}",
      err.message
    );
    // which repository file said so, and what the refresh had to say
    assert!(err.message.contains("repositories come from"), "{}", err.message);
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
