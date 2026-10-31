use super::index::{self, IndexSource, run_refresh};
use super::{FeedCandidate, PackageManager, PackageManagerKind, describe_targets};
use crate::app::{
  CliError, CliResult, E_INDEX_UNAVAILABLE, E_IO, E_LOCK_BUSY, E_MISSING_COMMAND, E_NO_SPACE,
  E_OPKG_INSTALL, E_OPKG_REMOVE, E_RUNTIME,
};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::lock::{Liveness, process_liveness};
use crate::system::{run_command_capture_output, stderr_or_stdout};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where `OpenWrt`'s opkg keeps its feed lists when `lists_dir` is not set.
const OPKG_LISTS_DIRS: [&str; 2] = ["/var/opkg-lists", "/tmp/opkg-lists"];
/// The lock `opkg update` takes; its own error message names the real one.
const OPKG_LOCK_FILES: [&str; 2] = ["/var/lock/opkg.lock", "/tmp/lock/opkg.lock"];
const OPKG_INFO_DIR: &str = "/usr/lib/opkg/info";
/// opkg reads feeds from `/etc/opkg/*.conf` plus, on `OpenWrt`, `/etc/opkg.conf`.
const OPKG_CONF_FILE: &str = "/etc/opkg.conf";
const OPKG_CONF_DIR: &str = "/etc/opkg";

pub struct OpkgBackend;

impl OpkgBackend {
  /// The index of this host, as its configuration describes it.
  fn index() -> OpkgIndex {
    OpkgIndex::system()
  }
}

/// The feed index `opkg` answers "is this package available" from.
pub(crate) struct OpkgIndex {
  conf_files: Vec<PathBuf>,
  /// Where the feed lists live when the configuration does not say.
  lists_dirs: Vec<PathBuf>,
  /// Where `opkg update` takes its lock when its error does not say.
  lock_files: Vec<PathBuf>,
  /// The refresh command, program first.
  update: Vec<String>,
}

impl OpkgIndex {
  fn system() -> Self {
    Self {
      conf_files: opkg_conf_files(),
      lists_dirs: OPKG_LISTS_DIRS.iter().map(PathBuf::from).collect(),
      lock_files: OPKG_LOCK_FILES.iter().map(PathBuf::from).collect(),
      update: vec!["opkg".to_string(), "update".to_string()],
    }
  }

  /// The directory the feed lists belong in: what the configuration says, else
  /// the first default that already holds lists, else the first default.
  fn lists_dir(&self, conf: &OpkgConf) -> PathBuf {
    if let Some(dir) = &conf.lists_dir {
      return dir.clone();
    }
    self
      .lists_dirs
      .iter()
      .find(|dir| opkg_list_dir_has_files(dir))
      .or_else(|| self.lists_dirs.first())
      .cloned()
      .unwrap_or_else(|| PathBuf::from(OPKG_LISTS_DIRS[0]))
  }

  /// Every version of `name` the feed lists hold.
  ///
  /// The list files are read rather than asking `opkg info`, because they are
  /// the only place that holds feed data alone: `info` answers from the merged
  /// table, where a package that is installed as well as offered has already
  /// stopped distinguishing "what is on this device" from "what could replace
  /// it" — and it is the second answer a version bound needs.
  fn feed_candidates(&self, name: &str) -> CliResult<Vec<FeedCandidate>> {
    let conf = read_opkg_conf(&self.conf_files)?;
    let dir = self.lists_dir(&conf);
    let mut candidates = Vec::new();
    for feed in &conf.feeds {
      let path = dir.join(feed);
      let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        // `index_ready` has already refused a host with a missing list, so this
        // is a file that vanished in between: it contributes nothing, and the
        // feeds that are there still answer.
        Err(err) if err.kind() == ErrorKind::NotFound => continue,
        Err(err) => {
          return Err(CliError::new(E_IO, tr(Key::ReadFailed, &[&path.display(), &err])));
        }
      };
      let Ok(text) = std::str::from_utf8(&bytes) else {
        // Decoding garbage would invent entries, and saying "no candidates" from
        // a file nobody could read is the one answer this may not give.
        return Err(CliError::new(E_INDEX_UNAVAILABLE, tr(Key::NotTextIndex, &[&path.display()])));
      };
      candidates.extend(parse_opkg_lists(text, name));
    }
    Ok(candidates)
  }

  /// The lock files worth looking at: the one `opkg` named itself, which is
  /// authoritative because it comes from the binary, then the paths the
  /// `OpenWrt` builds are known to use.
  fn lock_paths(&self, detail: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = detail
      .split_whitespace()
      .map(|token| token.trim_matches(|ch: char| matches!(ch, ':' | ',' | '\'' | '"')))
      .filter(|token| token.ends_with("opkg.lock"))
      .map(PathBuf::from)
      .collect();
    for candidate in &self.lock_files {
      if !paths.contains(candidate) {
        paths.push(candidate.clone());
      }
    }
    paths
  }

  /// Removes the lock files `opkg` could not take, unless one of them belongs
  /// to a process that is still running.
  fn clear_stale_locks(&self, detail: &str) -> LockCleanup {
    let mut cleared = false;
    for path in self.lock_paths(detail) {
      if !path.exists() {
        continue;
      }
      // Anything but a pid this host can see is gone is treated as held: opkg's
      // lock is removed only when the process behind it is known to be gone, and a
      // host that cannot answer (no `/proc`) must not break a lock in use.
      if let Some(pid) =
        read_opkg_lock_pid(&path).filter(|pid| process_liveness(*pid) != Liveness::Gone)
      {
        // Breaking this lock would leave the opkg that holds it writing into a
        // database another instance is rebuilding.
        return LockCleanup::Held(path, pid);
      }
      if fs::remove_file(&path).is_ok() {
        cleared = true;
      }
    }
    if cleared { LockCleanup::Cleared } else { LockCleanup::None }
  }
}

/// What came of looking at the lock files `opkg update` could not take.
enum LockCleanup {
  /// A stale lock was removed, so the refresh is worth retrying.
  Cleared,
  /// Nothing was there to remove.
  None,
  /// A live process owns the lock; it is not pica's to break.
  Held(PathBuf, u32),
}

impl IndexSource for OpkgIndex {
  fn backend(&self) -> &'static str {
    "opkg"
  }

  fn conf_files(&self) -> Vec<PathBuf> {
    self.conf_files.clone()
  }

  fn missing_indexes(&self) -> CliResult<Vec<String>> {
    let conf = read_opkg_conf(&self.conf_files)?;
    if conf.feeds.is_empty() {
      return Ok(Vec::new());
    }
    let dir = self.lists_dir(&conf);
    Ok(
      conf
        .feeds
        .iter()
        .map(|feed| (feed, dir.join(feed.as_str())))
        .filter(|(_, path)| !path.is_file())
        .map(|(feed, path)| tr(Key::NoFeedIndex, &[&feed, &path.display()]))
        .collect(),
    )
  }

  fn refresh(&self) -> CliResult<()> {
    let program = self.update.first().cloned().unwrap_or_else(|| "opkg".to_string());
    let first = run_refresh(&self.update)?;
    if first.success {
      return Ok(());
    }

    let detail = first.detail.clone();
    if !is_opkg_lock_error(&detail) {
      return Err(CliError::new(E_RUNTIME, first.failure(&program)));
    }

    match self.clear_stale_locks(&detail) {
      LockCleanup::Cleared => {}
      LockCleanup::Held(path, pid) => {
        return Err(CliError::new(
          E_LOCK_BUSY,
          tr(Key::UpdateBlocked, &[&program, &path.display(), &pid]),
        ));
      }
      LockCleanup::None => return Err(CliError::new(E_RUNTIME, first.failure(&program))),
    }

    let retry = run_refresh(&self.update)?;
    if retry.success { Ok(()) } else { Err(CliError::new(E_RUNTIME, retry.failure(&program))) }
  }
}

/// The configuration files `opkg` reads: `/etc/opkg.conf`, then every
/// `/etc/opkg/*.conf` in name order, which is where `OpenWrt` puts its feeds.
fn opkg_conf_files() -> Vec<PathBuf> {
  let mut files = vec![PathBuf::from(OPKG_CONF_FILE)];
  let mut extra: Vec<PathBuf> = fs::read_dir(OPKG_CONF_DIR)
    .into_iter()
    .flatten()
    .flatten()
    .map(|entry| entry.path())
    .filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
    .collect();
  extra.sort();
  files.extend(extra);
  files
}

/// The `src` and `lists_dir` lines of an opkg configuration.
#[derive(Default)]
struct OpkgConf {
  /// Feed names, which are also the list file names `opkg` writes them as.
  feeds: Vec<String>,
  lists_dir: Option<PathBuf>,
}

/// Reads configuration files that need not exist.
///
/// A file that is not there contributes nothing, which is how an `OpenWrt` box
/// without `customfeeds.conf` is read. A file that is there and cannot be read
/// is an error instead: the feeds it might name are then unknown, not absent.
fn read_opkg_conf(paths: &[PathBuf]) -> CliResult<OpkgConf> {
  let mut conf = OpkgConf::default();
  for path in paths {
    match fs::read_to_string(path) {
      Ok(text) => {
        let parsed = parse_opkg_conf(&text);
        conf.feeds.extend(parsed.feeds);
        conf.lists_dir = parsed.lists_dir.or(conf.lists_dir);
      }
      Err(err) if err.kind() == ErrorKind::NotFound => {}
      Err(err) => {
        return Err(CliError::new(E_IO, tr(Key::ReadFailed, &[&path.display(), &err])));
      }
    }
  }
  Ok(conf)
}

/// `src/gz <name> <url>` names a feed, `lists_dir [ext] <path>` moves the
/// directory the feed lists are written to. Unknown lines are opkg's business,
/// not pica's, and are skipped.
fn parse_opkg_conf(text: &str) -> OpkgConf {
  let mut conf = OpkgConf::default();
  for line in text.lines() {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
      continue;
    }
    let mut fields = line.split_whitespace();
    let Some(keyword) = fields.next() else { continue };
    if keyword.starts_with("src") {
      if let Some(name) = fields.next() {
        conf.feeds.push(name.to_string());
      }
    } else if keyword == "lists_dir" {
      // `lists_dir ext /var/opkg-lists` and `lists_dir /var/opkg-lists` both
      // name the path last.
      if let Some(path) = line.split_whitespace().last() {
        conf.lists_dir = Some(PathBuf::from(path));
      }
    }
  }
  conf
}

/// Whether `opkg info`'s output holds this package.
///
/// `opkg info` accepts a pattern and prints every match, and it exits 0 whether
/// or not it matched anything, so neither "it printed something" nor the exit
/// status answers the question: the `Package:` field does, compared exactly
/// (`info 'foo*'` printing `foobar` is not a `foo` in the feed).
fn opkg_lists_package(stdout: &str, name: &str) -> bool {
  stdout.lines().any(|line| line.strip_prefix("Package:").map(str::trim) == Some(name))
}

/// One `Packages`-style paragraph, kept until the blank line ends it.
#[derive(Default)]
struct OpkgEntry {
  package: Option<String>,
  version: Option<String>,
  download: Option<u64>,
  installed: Option<u64>,
}

impl OpkgEntry {
  /// The candidate this paragraph describes, when it is `name`'s.
  ///
  /// A zero size is dropped rather than reported: a feed that writes `0` is
  /// saying it does not know, and "0 bytes" would read as a measurement.
  fn candidate(&self, name: &str) -> Option<FeedCandidate> {
    if self.package.as_deref() != Some(name) {
      return None;
    }
    Some(FeedCandidate {
      version: self.version.clone()?,
      download: self.download.filter(|bytes| *bytes > 0),
      installed: self.installed.filter(|bytes| *bytes > 0),
    })
  }
}

/// The entries in one feed list that describe `name`.
///
/// The format is one paragraph per package version: `Field: value` lines, ended
/// by a blank line, with continuation lines (indented) belonging to the field
/// above. `Size` is the package file to fetch, `Installed-Size` is what the
/// unpacked package takes — the two figures a user is deciding between, and the
/// reason they are read separately instead of inferring one from the other.
fn parse_opkg_lists(text: &str, name: &str) -> Vec<FeedCandidate> {
  let mut candidates = Vec::new();
  let mut entry = OpkgEntry::default();
  // The closing blank line is not guaranteed, so the loop adds one.
  for line in text.lines().chain(std::iter::once("")) {
    let line = line.trim_end();
    if line.trim().is_empty() {
      candidates.extend(entry.candidate(name));
      entry = OpkgEntry::default();
      continue;
    }
    let Some((key, value)) = line.split_once(':') else {
      continue;
    };
    let value = value.trim();
    match key {
      "Package" => entry.package = Some(value.to_string()),
      "Version" => entry.version = Some(value.to_string()),
      "Size" => entry.download = value.parse().ok(),
      "Installed-Size" => entry.installed = value.parse().ok(),
      _ => {}
    }
  }
  candidates
}

impl PackageManager for OpkgBackend {
  fn kind(&self) -> PackageManagerKind {
    PackageManagerKind::Opkg
  }

  fn update_index(&self) -> CliResult<()> {
    index::ensure_index(&Self::index())
  }

  fn package_exists(&self, name: &str) -> CliResult<bool> {
    let output = Command::new("opkg").arg("info").arg(name).output().map_err(|err| {
      let (code, message) = if err.kind() == ErrorKind::NotFound {
        (E_MISSING_COMMAND, tr(Key::MissingCommand, &[&"opkg"]))
      } else {
        (E_RUNTIME, tr(Key::OpkgInfoFailed, &[&name, &err]))
      };
      CliError::new(code, message)
    })?;

    if opkg_lists_package(&String::from_utf8_lossy(&output.stdout), name) {
      return Ok(true);
    }
    if !output.status.success() {
      let detail = stderr_or_stdout(&output.stdout, &output.stderr);
      return Err(CliError::new(
        E_RUNTIME,
        tr(Key::OpkgInfoExit, &[&name, &exit_status_text(output.status), &detail]),
      ));
    }

    // `opkg info` found nothing, but it reads an index that may never have been
    // fetched: without lists it finds nothing for *every* name, which is not an
    // answer to give about one of them.
    index::index_ready(&Self::index())?;
    Ok(false)
  }

  fn feed_candidates(&self, name: &str) -> CliResult<Vec<FeedCandidate>> {
    index::index_ready(&Self::index())?;
    Self::index().feed_candidates(name)
  }

  fn is_installed(&self, name: &str) -> bool {
    let Ok(output) = Command::new("opkg").arg("status").arg(name).output() else {
      return false;
    };
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    text.contains("status:") && text.contains("installed")
  }

  fn installed_version(&self, name: &str) -> Option<String> {
    let output = Command::new("opkg").arg("status").arg(name).output().ok()?;
    String::from_utf8_lossy(&output.stdout)
      .lines()
      .find_map(|line| line.trim().strip_prefix("Version: ").map(str::trim).map(str::to_string))
  }

  fn install(&self, label: &str, targets: &[String]) -> CliResult<()> {
    // One `opkg install` for the whole request: opkg reads the local files in a
    // single run as one dependency graph, so a container that depends on a
    // sibling in the same request is satisfied by that sibling. Installing a
    // local file on its own leaves opkg with nothing but the repositories to
    // resolve against.
    if targets.is_empty() {
      return Ok(());
    }
    // Best effort, for callers that reach an install without having asked for
    // the index first. It is not fatal here because `opkg install` is the
    // authority on its own resolution and names the dependency it could not
    // find; the questions pica answers *itself* are the ones that must not
    // guess, and they go through `update_index` and `package_exists`.
    let _ = self.update_index();
    let output = Command::new("opkg")
      .arg("install")
      .args(targets)
      .output()
      .map_err(|err| CliError::new(E_OPKG_INSTALL, tr(Key::OpkgInstallFailed, &[&err])))?;
    if output.status.success() {
      return Ok(());
    }
    let target = describe_targets(targets);
    let detail = stderr_or_stdout(&output.stdout, &output.stderr);
    if detail.to_ascii_lowercase().contains("no space left on device") {
      return Err(CliError::new(
        E_NO_SPACE,
        tr(Key::InstallStorageFull, &[&label, &target, &detail]),
      ));
    }
    Err(CliError::new(E_OPKG_INSTALL, tr(Key::BackendInstallFailed, &[&label, &target, &detail])))
  }

  fn remove(&self, target: &str) -> CliResult<()> {
    let output = Command::new("opkg")
      .arg("remove")
      .arg(target)
      .output()
      .map_err(|err| CliError::new(E_OPKG_REMOVE, tr(Key::OpkgRemoveFailed, &[&err])))?;
    if output.status.success() {
      Ok(())
    } else {
      let detail = stderr_or_stdout(&output.stdout, &output.stderr);
      Err(CliError::new(E_OPKG_REMOVE, tr(Key::OpkgRemoveTargetFailed, &[&target, &detail])))
    }
  }

  fn installed(&self) -> CliResult<Vec<(String, String)>> {
    let output = Command::new("opkg").arg("list-installed").output().map_err(|err| {
      let (code, message) = if err.kind() == ErrorKind::NotFound {
        (E_MISSING_COMMAND, tr(Key::MissingCommand, &[&"opkg"]))
      } else {
        (E_RUNTIME, tr(Key::OpkgListFailed, &[&err]))
      };
      CliError::new(code, message)
    })?;
    if !output.status.success() {
      let detail = stderr_or_stdout(&output.stdout, &output.stderr);
      return Err(CliError::new(
        E_RUNTIME,
        tr(Key::OpkgListExit, &[&exit_status_text(output.status), &detail]),
      ));
    }
    let mut packages = parse_installed_list(&String::from_utf8_lossy(&output.stdout));
    packages.sort();
    packages.dedup();
    Ok(packages)
  }

  fn depends(&self, name: &str) -> Vec<String> {
    let Ok(text) = fs::read_to_string(Path::new(OPKG_INFO_DIR).join(format!("{name}.control")))
    else {
      return Vec::new();
    };
    parse_depends_field(&text)
  }

  fn owns_file(&self, path: &str) -> Option<String> {
    let entries = fs::read_dir(OPKG_INFO_DIR).ok()?;
    let mut owners: Vec<String> = entries
      .flatten()
      .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "list"))
      .filter_map(|entry| {
        let name = entry.path().file_stem()?.to_string_lossy().into_owned();
        let text = fs::read_to_string(entry.path()).ok()?;
        list_owns_path(&text, path).then_some(name)
      })
      .collect();
    owners.sort();
    owners.into_iter().next()
  }

  fn architectures(&self) -> Vec<String> {
    let Ok(output) = run_command_capture_output("opkg", &["print-architecture"]) else {
      return Vec::new();
    };
    String::from_utf8_lossy(&output)
      .lines()
      .filter_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("arch")).then(|| fields.next()).flatten()
      })
      .map(ToString::to_string)
      .collect()
  }
}

/// `opkg list-installed` prints `name - version`, one package per line.
fn parse_installed_list(text: &str) -> Vec<(String, String)> {
  text
    .lines()
    .filter_map(|line| {
      let (name, version) = line.split_once(" - ")?;
      let name = name.trim();
      let version = version.trim();
      (!name.is_empty() && !version.is_empty()).then(|| (name.to_string(), version.to_string()))
    })
    .collect()
}

/// `Depends: libc (>= 1.0), libubox` -> `["libc", "libubox"]`.
fn parse_depends_field(control: &str) -> Vec<String> {
  let mut values: Vec<String> = Vec::new();
  let mut collecting = false;
  for line in control.lines() {
    if collecting && line.starts_with([' ', '\t']) {
      values.extend(split_dependency_list(line));
      continue;
    }
    collecting = false;
    if line.starts_with("Depends:") {
      collecting = true;
      values.extend(split_dependency_list(line.trim_start_matches("Depends:")));
    }
  }
  values.sort();
  values.dedup();
  values
}

fn split_dependency_list(value: &str) -> Vec<String> {
  value
    .split(',')
    .filter_map(|item| {
      let name = item.trim().split(['(', ' ']).next()?.trim();
      (!name.is_empty()).then(|| name.to_string())
    })
    .collect()
}

/// Ownership lists hold one absolute path per line.
fn list_owns_path(list: &str, path: &str) -> bool {
  list.lines().any(|line| line.trim() == path)
}

fn opkg_list_dir_has_files(dir: &Path) -> bool {
  let Ok(entries) = fs::read_dir(dir) else {
    return false;
  };
  entries.flatten().any(|entry| entry.path().is_file())
}

/// How a command ended, for the messages that report it ended badly.
fn exit_status_text(status: std::process::ExitStatus) -> String {
  match status.code() {
    Some(code) => tr(Key::ExitCode, &[&code]),
    None => tr(Key::KilledBySignal, &[]),
  }
}

fn is_opkg_lock_error(detail: &str) -> bool {
  let text = detail.to_ascii_lowercase();
  if text.contains("opkg.lock") {
    return true;
  }
  let mentions_lock = text.contains(" lock") || text.starts_with("lock") || text.contains("locked");
  let lock_failure =
    ["resource temporarily unavailable", "could not lock", "failed to lock", "cannot lock"]
      .iter()
      .any(|pattern| text.contains(pattern));
  mentions_lock && lock_failure
}

fn read_opkg_lock_pid(path: &Path) -> Option<u32> {
  let text = fs::read_to_string(path).ok()?;
  text
    .split(|ch: char| !ch.is_ascii_digit())
    .filter(|token| !token.is_empty())
    .filter_map(|token| token.parse::<u32>().ok())
    .find(|pid| *pid > 0)
}

#[cfg(test)]
mod tests {
  use super::{
    OpkgIndex, is_opkg_lock_error, list_owns_path, opkg_list_dir_has_files, opkg_lists_package,
    parse_depends_field, parse_installed_list, parse_opkg_conf, parse_opkg_lists,
    read_opkg_lock_pid,
  };
  use crate::app::E_INDEX_UNAVAILABLE;
  use crate::pkgmgr::FeedCandidate;
  use crate::pkgmgr::index::{self, IndexSource};
  use pretty_assertions::assert_eq;
  use std::fs;
  use std::path::{Path, PathBuf};

  /// An index reading one configuration file and one list directory, refreshed
  /// by whatever command the test hands it.
  fn index_for(
    root: &Path,
    conf: &str,
    update: Vec<String>,
    lock_files: Vec<PathBuf>,
  ) -> OpkgIndex {
    let conf_file = root.join("opkg.conf");
    fs::write(&conf_file, conf).expect("write conf file");
    let lists = root.join("lists");
    fs::create_dir_all(&lists).expect("create lists dir");
    OpkgIndex { conf_files: vec![conf_file], lists_dirs: vec![lists], lock_files, update }
  }

  fn shell(script: String) -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string(), script]
  }

  #[test]
  fn detects_opkg_lock_errors() {
    assert!(is_opkg_lock_error("Could not lock /var/lock/opkg.lock"));
    assert!(is_opkg_lock_error("Cannot lock package database"));
    assert!(!is_opkg_lock_error("wget: bad address"));
  }

  /// One feed list holding two versions of `luci-app-demo`, one of them without
  /// a download size, and one unrelated package.
  const FEED_LIST: &str = "\
Package: luci-app-demo
Version: 1.2.3-1
Depends: libc
Installed-Size: 40960
Size: 8192
 continuation: belongs to the description

Package: luci-app-demo
Version: 1.0.0-1
Installed-Size: 20480

Package: other
Version: 9.9.9-1
Size: 1
Installed-Size: 2
";

  #[test]
  fn feed_list_separates_download_from_installed_size() {
    let candidates = parse_opkg_lists(FEED_LIST, "luci-app-demo");
    assert_eq!(
      candidates,
      vec![
        FeedCandidate {
          version: "1.2.3-1".to_string(),
          download: Some(8192),
          installed: Some(40960),
        },
        FeedCandidate { version: "1.0.0-1".to_string(), download: None, installed: Some(20480) },
      ],
      "Size is the package file and Installed-Size the unpacked size: a feed that \
       omits one may not be given the other"
    );
  }

  #[test]
  fn feed_list_without_sizes_has_no_numbers_to_report() {
    let text = "Package: demo\nVersion: 1.0\n\n";
    assert_eq!(
      parse_opkg_lists(text, "demo"),
      vec![FeedCandidate { version: "1.0".to_string(), download: None, installed: None }]
    );
  }

  #[test]
  fn a_zero_size_is_not_a_measurement() {
    let text = "Package: demo\nVersion: 1.0\nSize: 0\nInstalled-Size: 0\n";
    assert_eq!(
      parse_opkg_lists(text, "demo"),
      vec![FeedCandidate { version: "1.0".to_string(), download: None, installed: None }],
      "a feed that writes 0 is saying it does not know"
    );
  }

  #[test]
  fn feed_list_keeps_versions_apart_and_ignores_other_packages() {
    let candidates = parse_opkg_lists(FEED_LIST, "other");
    assert_eq!(candidates.len(), 1, "only the paragraph named `other`: {candidates:?}");
    assert_eq!(candidates[0].version, "9.9.9-1");
    assert_eq!(parse_opkg_lists(FEED_LIST, "absent"), Vec::new());
  }

  #[test]
  fn list_dir_check_requires_regular_files() {
    let dir = tempfile::tempdir().expect("create temp dir");
    assert!(!opkg_list_dir_has_files(dir.path()));
    fs::create_dir_all(dir.path().join("subdir")).expect("create nested dir");
    assert!(!opkg_list_dir_has_files(dir.path()));
    fs::write(dir.path().join("generic"), b"Package: test\n").expect("write list file");
    assert!(opkg_list_dir_has_files(dir.path()));
  }

  #[test]
  fn read_opkg_lock_pid_extracts_numeric_token() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let lock_file = dir.path().join("opkg.lock");
    fs::write(&lock_file, "pid: 1234\n").expect("write lock file");
    assert_eq!(read_opkg_lock_pid(&lock_file), Some(1234));
    fs::write(&lock_file, "nonsense").expect("write non pid lock file");
    assert_eq!(read_opkg_lock_pid(&lock_file), None);
  }

  #[test]
  fn parses_feed_names_and_lists_dir_from_a_configuration() {
    let conf = parse_opkg_conf(
      "# OpenWrt feeds\n\
       src/gz openwrt_core https://downloads.example/core\n\
       src/gz openwrt_base https://downloads.example/base\n\
       option check_signature\n\
       lists_dir ext /var/opkg-lists\n",
    );
    assert_eq!(conf.feeds, vec!["openwrt_core".to_string(), "openwrt_base".to_string()]);
    assert_eq!(conf.lists_dir, Some(PathBuf::from("/var/opkg-lists")));

    let plain =
      parse_opkg_conf("src openwrt_luci https://downloads.example/luci\nlists_dir /tmp/lists\n");
    assert_eq!(plain.feeds, vec!["openwrt_luci".to_string()]);
    assert_eq!(plain.lists_dir, Some(PathBuf::from("/tmp/lists")));
  }

  #[test]
  fn only_the_feeds_without_a_list_file_are_missing() {
    let root = tempfile::tempdir().expect("create temp dir");
    let index = index_for(
      root.path(),
      "src/gz openwrt_core https://example/core\nsrc/gz openwrt_base https://example/base\n",
      Vec::new(),
      Vec::new(),
    );
    // opkg names each feed's list file after the feed, so one file is one feed.
    fs::write(root.path().join("lists/openwrt_core"), "Package: demo\n").expect("write list file");

    let missing = index.missing_indexes().expect("conf is readable");
    assert_eq!(missing.len(), 1);
    assert!(missing[0].contains("openwrt_base"), "{}", missing[0]);
    assert!(missing[0].contains("lists/openwrt_base"), "{}", missing[0]);
  }

  #[test]
  fn a_host_without_feeds_has_no_index_to_need() {
    let root = tempfile::tempdir().expect("create temp dir");
    let index = index_for(
      root.path(),
      "dest root /\nlists_dir ext /var/opkg-lists\n",
      Vec::new(),
      Vec::new(),
    );
    assert_eq!(index.missing_indexes().expect("conf is readable"), Vec::<String>::new());
  }

  #[test]
  fn the_configured_lists_dir_is_the_one_that_counts() {
    let root = tempfile::tempdir().expect("create temp dir");
    let lists = root.path().join("elsewhere");
    fs::create_dir_all(&lists).expect("create lists dir");
    let index = index_for(
      root.path(),
      &format!("src/gz openwrt_core https://example/core\nlists_dir ext {}\n", lists.display()),
      Vec::new(),
      Vec::new(),
    );
    assert_eq!(index.missing_indexes().expect("conf is readable").len(), 1);

    fs::write(lists.join("openwrt_core"), "Package: demo\n").expect("write list file");
    assert_eq!(index.missing_indexes().expect("conf is readable"), Vec::<String>::new());
  }

  #[test]
  fn the_first_known_lists_dir_is_used_when_nothing_says_otherwise() {
    let root = tempfile::tempdir().expect("create temp dir");
    let index =
      index_for(root.path(), "src/gz openwrt_core https://example/core\n", Vec::new(), Vec::new());
    let missing = index.missing_indexes().expect("conf is readable");
    // The host's own first candidate, which on OpenWrt is the one opkg writes.
    let expected = root.path().join("lists").join("openwrt_core").display().to_string();
    assert!(missing[0].contains(&expected), "{}", missing[0]);
  }

  #[test]
  fn a_configuration_that_cannot_be_read_is_an_error() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let index = OpkgIndex {
      // A directory where a file belongs: present, and impossible to read.
      conf_files: vec![dir.path().to_path_buf()],
      lists_dirs: vec![dir.path().join("lists")],
      lock_files: Vec::new(),
      update: Vec::new(),
    };
    let err = index.missing_indexes().expect_err("the feeds are unknown, not absent");
    assert_eq!(err.code, crate::app::E_IO);
  }

  #[test]
  fn a_missing_list_file_is_fetched_and_a_present_one_is_not() {
    let root = tempfile::tempdir().expect("create temp dir");
    let lists = root.path().join("lists");
    let fetch = shell(format!(
      "mkdir -p {} && echo 'Package: demo' > {}/openwrt_core",
      lists.display(),
      lists.display()
    ));
    let index =
      index_for(root.path(), "src/gz openwrt_core https://example/core\n", fetch, Vec::new());
    index::ensure_index(&index).expect("the refresh wrote the list file");
    assert!(lists.join("openwrt_core").is_file());

    // The same feed, this time with a refresh that would fail: an index that is
    // already on disk is not fetched again, which is what keeps the expensive
    // step off the routers.
    let would_fail = OpkgIndex { update: shell("exit 9".to_string()), ..index };
    index::ensure_index(&would_fail).expect("an index that is there is not refetched");
  }

  #[test]
  fn a_refresh_that_fetched_nothing_is_reported() {
    let root = tempfile::tempdir().expect("create temp dir");
    let index = index_for(
      root.path(),
      "src/gz openwrt_core https://example/core\n",
      shell("echo 'wget: bad address' >&2; exit 1".to_string()),
      Vec::new(),
    );
    let err = index::ensure_index(&index).expect_err("the feed still has no list file");
    assert_eq!(err.code, E_INDEX_UNAVAILABLE);
    assert!(err.message.contains("no index for feed openwrt_core"), "{}", err.message);
    assert!(err.message.contains("update failed (exit 1): wget: bad address"), "{}", err.message);
  }

  #[test]
  fn a_stale_lock_is_cleared_and_the_refresh_retried_once() {
    // Clearing opkg's lock rests on telling a dead pid from a live one, which only
    // `/proc` answers. A host that cannot answer must leave the lock alone, so the
    // test asserts whichever of the two the host it runs on can prove — and never
    // assumes the machine has `/proc`.
    let host_can_check = crate::lock::process_liveness(999_999) == crate::lock::Liveness::Gone;

    let root = tempfile::tempdir().expect("create temp dir");
    let lists = root.path().join("lists");
    let lock = root.path().join("opkg.lock");
    let retried = root.path().join("retried");
    // A pid nothing is running as: the lock is left over from an opkg that died.
    fs::write(&lock, "999999").expect("write lock file");
    let script = format!(
      "if [ -f {retried} ]; then mkdir -p {lists} && echo 'Package: demo' > {lists}/openwrt_core; exit 0; fi; \
       touch {retried}; echo 'Could not lock {lock}: Resource temporarily unavailable' >&2; exit 1",
      retried = retried.display(),
      lists = lists.display(),
      lock = lock.display()
    );
    let index = index_for(
      root.path(),
      "src/gz openwrt_core https://example/core\n",
      shell(script),
      vec![lock.clone()],
    );

    if host_can_check {
      index::ensure_index(&index).expect("the stale lock was cleared and the refresh retried");
      assert_eq!(lock.exists(), false, "the stale lock is gone");
      assert!(lists.join("openwrt_core").is_file(), "the retry wrote the list file");
    } else {
      let err = index::ensure_index(&index).expect_err("a lock that cannot be checked is kept");
      assert_eq!(err.code, crate::app::E_LOCK_BUSY);
      assert!(err.message.contains("999999"), "{}", err.message);
      assert!(lock.exists(), "the lock of an opkg that may still be running is untouched");
      assert!(
        !lists.join("openwrt_core").exists(),
        "and the refresh was not retried behind its back"
      );
    }
  }

  #[test]
  fn a_lock_held_by_a_running_process_is_left_alone() {
    // Only a host with /proc can tell a live pid from a dead one, and only such
    // a host can be asked not to break a lock.
    if crate::lock::process_liveness(std::process::id()) != crate::lock::Liveness::Running {
      return;
    }

    let root = tempfile::tempdir().expect("create temp dir");
    let lock = root.path().join("opkg.lock");
    fs::write(&lock, std::process::id().to_string()).expect("write lock file");
    let script = format!(
      "echo 'Could not lock {}: Resource temporarily unavailable' >&2; exit 1",
      lock.display()
    );
    let index = index_for(
      root.path(),
      "src/gz openwrt_core https://example/core\n",
      shell(script),
      vec![lock.clone()],
    );

    let err = index::ensure_index(&index).expect_err("the lock is not pica's to break");
    assert_eq!(err.code, crate::app::E_LOCK_BUSY);
    assert!(err.message.contains("held by pid"), "{}", err.message);
    assert!(lock.exists(), "the lock of a live opkg is untouched");
  }

  #[test]
  fn the_package_field_is_what_answers_the_question() {
    assert!(opkg_lists_package("Package: demo\nVersion: 1.0\n", "demo"));
    assert!(opkg_lists_package("Name: x\nPackage:   demo  \n", "demo"));
    // `opkg info` takes a pattern, so a match is not the package itself.
    assert!(!opkg_lists_package("Package: demo-extra\n", "demo"));
    // And a warning is not an answer to anything.
    assert!(!opkg_lists_package("Unknown package 'demo'.\n", "demo"));
    assert!(!opkg_lists_package("", "demo"));
  }

  #[test]
  fn parses_list_installed_output() {
    let text = "busybox - 1.36.1-1\nluci-app-demo - 2:1.0.0-r0\nbroken line\n";
    assert_eq!(
      parse_installed_list(text),
      vec![
        ("busybox".to_string(), "1.36.1-1".to_string()),
        ("luci-app-demo".to_string(), "2:1.0.0-r0".to_string()),
      ]
    );
  }

  #[test]
  fn parses_depends_field_with_versions_and_continuations() {
    let control = "Package: demo\nDepends: libc (>= 1.0), libubox\n  , luci-base\n";
    assert_eq!(
      parse_depends_field(control),
      vec!["libc".to_string(), "libubox".to_string(), "luci-base".to_string()]
    );
    assert_eq!(parse_depends_field("Package: demo\n"), Vec::<String>::new());
  }

  #[test]
  fn matches_file_ownership_by_exact_path() {
    let list = "/usr/bin/demo\n/usr/lib/demo/lib.so\n";
    assert!(list_owns_path(list, "/usr/bin/demo"));
    assert!(!list_owns_path(list, "/usr/bin/demo2"));
  }
}
