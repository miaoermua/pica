//! The dependency index a system package manager answers questions from.
//!
//! `opkg` and `apk` know whether a feed has a package because they fetched that
//! feed's index (`opkg update`, `apk update`). Three answers are possible and
//! they are different questions:
//!
//! * the feed has the package,
//! * the feed does not have the package,
//! * nobody can tell, because a configured feed has no index on this host.
//!
//! Only the first two let an install pick a source. The third is an error: a
//! backend reading a half-fetched index reports *every* package as missing, and
//! a caller that believes it installs a packaged container for a dependency the
//! feeds would have satisfied — or hands the feeds a package they never had.
//!
//! So a query refuses to answer unless [`index_ready`] holds, and
//! [`ensure_index`] refreshes only when it does not: an index that is already
//! on disk is not fetched again, because `opkg update` is the expensive step on
//! the routers this runs on (`docs/developer.md`).

use crate::app::{
  CliError, CliResult, E_INDEX_UNAVAILABLE, E_LOCK_BUSY, E_MISSING_COMMAND, E_RUNTIME,
};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::system::stderr_or_stdout;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Command;

/// Everything about one backend's dependency index that is not the same for
/// both of them: where its repositories are written down, which index files
/// those repositories need, and how it refreshes them.
pub(crate) trait IndexSource {
  /// The backend's name, as the messages that report a missing index say it.
  fn backend(&self) -> &'static str;

  /// The files that name this host's repositories.
  ///
  /// Only for the message: an operator reading "index unavailable" needs to
  /// know where the repositories that want an index came from.
  fn conf_files(&self) -> Vec<PathBuf>;

  /// What this host's configured repositories need and do not have, phrased
  /// for the error message — `opkg`'s missing list file per feed, and `apk`'s
  /// missing index per repository, which it knows exactly because apk names a
  /// cached index after the URL it came from.
  ///
  /// Empty when every configured repository has a readable index, and also
  /// empty when no repository is configured, since there is then nothing that
  /// could need one. `Err` when a configuration file exists but cannot be read:
  /// a host whose feeds are unknown is not a host with no feeds, and only the
  /// second one may be answered "not available" from.
  fn missing_indexes(&self) -> CliResult<Vec<String>>;

  /// Runs the package manager's own index refresh.
  ///
  /// `Err` says why it failed, and [`ensure_index`] — which looks at what the
  /// refresh left on disk — decides whether that failure is fatal.
  fn refresh(&self) -> CliResult<()>;
}

/// Whether the index can be read right now, without fetching it.
///
/// Callers that are about to *ask* the backend something use this, so that a
/// missing index is an error instead of an answer.
pub(crate) fn index_ready(source: &dyn IndexSource) -> CliResult<()> {
  let missing = source.missing_indexes()?;
  if missing.is_empty() { Ok(()) } else { Err(unavailable(source, &missing, None)) }
}

/// Makes the index usable, refreshing it only when it is not.
///
/// A refresh that fails is fatal exactly when a configured repository still has
/// no index: one unreachable feed out of several fails the refresh command but
/// leaves the others readable, and refusing then would fail an install over a
/// feed nothing in it needs.
pub(crate) fn ensure_index(source: &dyn IndexSource) -> CliResult<()> {
  if source.missing_indexes()?.is_empty() {
    return Ok(());
  }

  let failure = source.refresh().err();

  // A lock held by a live process is not a missing index that `ensure_index`
  // can paper over by building an index: opkg refused to run at all, so the
  // more specific lock error is what the caller must see instead of
  // `E_INDEX_UNAVAILABLE`.
  let lock_busy = failure.as_ref().is_some_and(|err| err.code == E_LOCK_BUSY);
  if lock_busy {
    return Err(failure.unwrap());
  }

  let missing = source.missing_indexes()?;
  if missing.is_empty() {
    return Ok(());
  }
  Err(unavailable(source, &missing, failure.as_ref()))
}

fn unavailable(
  source: &dyn IndexSource,
  missing: &[String],
  failure: Option<&CliError>,
) -> CliError {
  let conf = source.conf_files();
  let origin = if conf.is_empty() {
    String::new()
  } else {
    let conf: Vec<String> = conf.iter().map(|path| path.display().to_string()).collect();
    tr(Key::IndexReposFrom, &[&conf.join(", ")])
  };

  // The refresh's own words, which already name the command that failed: the
  // reader has to be able to tell a down link from a feed that answered with
  // nothing.
  let refresh = match failure {
    Some(err) => format!("; {}", err.message),
    None => String::new(),
  };

  CliError::new(
    E_INDEX_UNAVAILABLE,
    tr(Key::IndexUnavailableDetail, &[&source.backend(), &missing.join("; "), &origin, &refresh]),
  )
}

/// A refresh command's exit status and its own words.
#[derive(Debug)]
pub(crate) struct RefreshOutcome {
  pub(crate) success: bool,
  pub(crate) code: Option<i32>,
  pub(crate) detail: String,
}

impl RefreshOutcome {
  /// The failure in one line. `opkg` exits with the number of feeds it could
  /// not fetch, so the code carries information of its own.
  pub(crate) fn failure(&self, program: &str) -> String {
    let status = match self.code {
      Some(code) => tr(Key::ExitCode, &[&code]),
      None => tr(Key::KilledBySignal, &[]),
    };
    if self.detail.is_empty() {
      tr(Key::UpdateFailedSilent, &[&program, &status])
    } else {
      tr(Key::UpdateFailedStatus, &[&program, &status, &self.detail])
    }
  }
}

/// Runs `program args…`, program first, and reports how it went.
///
/// The output is kept because the reason a refresh failed is the whole point of
/// reporting it: "wlan0 is down" and "the feed's signature is stale" need
/// different fixes.
pub(crate) fn run_refresh(command: &[String]) -> CliResult<RefreshOutcome> {
  let Some((program, args)) = command.split_first() else {
    return Err(CliError::new(E_RUNTIME, tr(Key::NoRefreshCommand, &[])));
  };

  let output = Command::new(program).args(args).output().map_err(|err| {
    // A backend binary that is not there is a different problem from a backend
    // binary that ran and failed, and the two are worth telling apart.
    let code = if err.kind() == ErrorKind::NotFound { E_MISSING_COMMAND } else { E_RUNTIME };
    if err.kind() == ErrorKind::NotFound {
      CliError::new(code, tr(Key::MissingCommand, &[&program]))
    } else {
      CliError::new(code, tr(Key::UpdateFailed, &[&program, &err]))
    }
  })?;

  Ok(RefreshOutcome {
    success: output.status.success(),
    code: output.status.code(),
    detail: stderr_or_stdout(&output.stdout, &output.stderr),
  })
}

#[cfg(test)]
mod tests {
  use super::{IndexSource, ensure_index, index_ready, run_refresh};
  use crate::app::{CliError, CliResult, E_INDEX_UNAVAILABLE, E_MISSING_COMMAND};
  use pretty_assertions::assert_eq;
  use std::cell::Cell;
  use std::fs;
  use std::path::{Path, PathBuf};

  /// A backend whose configuration and refresh behaviour the test decides.
  struct Fake {
    dir: PathBuf,
    conf: PathBuf,
    repos: Vec<String>,
    /// Whether a refresh writes the index the configured feeds need.
    produces_index: bool,
    /// The failure a refresh reports, when it reports one.
    refresh_failure: Option<&'static str>,
    refreshes: Cell<u32>,
  }

  impl Fake {
    fn new(root: &Path, repos: &[&str]) -> Self {
      Self {
        dir: root.join("lists"),
        conf: root.join("opkg.conf"),
        repos: repos.iter().map(|repo| (*repo).to_string()).collect(),
        produces_index: false,
        refresh_failure: None,
        refreshes: Cell::new(0),
      }
    }

    fn making(mut self) -> Self {
      self.produces_index = true;
      self
    }

    fn failing(mut self, detail: &'static str) -> Self {
      self.refresh_failure = Some(detail);
      self
    }

    /// Writes the list file a feed's index lives in, as a fetch would.
    fn write_index(&self, feed: &str) {
      fs::create_dir_all(&self.dir).expect("create lists dir");
      fs::write(self.dir.join(feed), "Package: demo\n").expect("write list file");
    }
  }

  impl IndexSource for Fake {
    fn backend(&self) -> &'static str {
      "opkg"
    }

    fn conf_files(&self) -> Vec<PathBuf> {
      vec![self.conf.clone()]
    }

    fn missing_indexes(&self) -> CliResult<Vec<String>> {
      Ok(
        self
          .repos
          .iter()
          .filter(|feed| !self.dir.join(feed.as_str()).is_file())
          .map(|feed| format!("no index for feed {feed}"))
          .collect(),
      )
    }

    fn refresh(&self) -> CliResult<()> {
      self.refreshes.set(self.refreshes.get() + 1);
      if self.produces_index {
        for feed in &self.repos {
          self.write_index(feed);
        }
      }
      match self.refresh_failure {
        Some(detail) => Err(CliError::new(crate::app::E_RUNTIME, detail)),
        None => Ok(()),
      }
    }
  }

  fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pica-index-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
  }

  #[test]
  fn a_host_without_repositories_needs_no_index() {
    let root = temp_root("no-repos");
    let source = Fake::new(&root, &[]).making();

    assert_eq!(ensure_index(&source).is_ok(), true);
    assert_eq!(source.refreshes.get(), 0, "nothing is configured, so nothing is fetched");
    assert_eq!(index_ready(&source).is_ok(), true, "and a query may be answered");

    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn an_index_that_is_already_there_is_not_fetched_again() {
    let root = temp_root("ready");
    let source = Fake::new(&root, &["openwrt_core"]).making();
    source.write_index("openwrt_core");

    assert_eq!(ensure_index(&source).is_ok(), true);
    assert_eq!(source.refreshes.get(), 0, "`opkg update` is the expensive step and is skipped");

    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn a_missing_index_is_fetched() {
    let root = temp_root("fetch");
    let source = Fake::new(&root, &["openwrt_core", "openwrt_base"]).making();

    assert_eq!(ensure_index(&source).is_ok(), true);
    assert_eq!(source.refreshes.get(), 1);
    assert_eq!(source.dir.join("openwrt_core").is_file(), true);
    assert_eq!(source.dir.join("openwrt_base").is_file(), true);

    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn a_failed_refresh_that_left_no_index_is_reported() {
    let root = temp_root("failed");
    let source = Fake::new(&root, &["openwrt_core"]).failing("wlan0: network unreachable");

    let err = ensure_index(&source).expect_err("a feed with no index cannot be queried");
    assert_eq!(err.code, E_INDEX_UNAVAILABLE);
    assert!(err.message.contains("no index for feed openwrt_core"), "{}", err.message);
    assert!(err.message.contains("repositories come from"), "{}", err.message);
    assert!(err.message.contains("opkg.conf"), "{}", err.message);
    assert!(err.message.contains("wlan0: network unreachable"), "{}", err.message);

    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn a_failed_refresh_that_left_a_usable_index_is_not_fatal() {
    let root = temp_root("partial");
    // One feed of several answers, the command reports the other one, and the
    // index this host needs to answer questions with is complete.
    let source = Fake::new(&root, &["openwrt_core"]).making().failing("2 feeds failed");

    assert_eq!(ensure_index(&source).is_ok(), true, "the index on disk is usable");
    assert_eq!(source.refreshes.get(), 1);

    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn a_refresh_that_wrote_nothing_is_reported_without_a_failure() {
    let root = temp_root("empty-refresh");
    let source = Fake::new(&root, &["openwrt_core"]);

    let err = ensure_index(&source).expect_err("the feed still has no index");
    assert_eq!(err.code, E_INDEX_UNAVAILABLE);
    assert!(err.message.contains("no index for feed openwrt_core"), "{}", err.message);
    assert_eq!(err.message.contains("update failed"), false, "{}", err.message);

    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn a_query_refuses_to_answer_from_an_index_that_is_not_there() {
    let root = temp_root("query");
    let source = Fake::new(&root, &["openwrt_core"]);

    let err = index_ready(&source).expect_err("nothing may be answered from an empty index");
    assert_eq!(err.code, E_INDEX_UNAVAILABLE);
    assert!(err.message.contains("no index for feed openwrt_core"), "{}", err.message);
    assert_eq!(source.refreshes.get(), 0, "asking is not fetching");

    source.write_index("openwrt_core");
    assert_eq!(index_ready(&source).is_ok(), true);

    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn a_binary_that_is_not_there_is_reported_as_missing() {
    let err = run_refresh(&["pica-no-such-refresh-command".to_string()])
      .expect_err("the command does not exist");
    assert_eq!(err.code, E_MISSING_COMMAND);
    assert!(err.message.contains("pica-no-such-refresh-command"), "{}", err.message);
  }

  #[test]
  fn a_refresh_reports_its_exit_code_and_output() {
    let failing = run_refresh(&[
      "sh".to_string(),
      "-c".to_string(),
      "echo 'Could not lock /tmp/opkg.lock' >&2; exit 3".to_string(),
    ])
    .expect("the command ran");
    assert_eq!(failing.success, false);
    assert_eq!(failing.code, Some(3));
    assert!(failing.detail.contains("Could not lock"), "{}", failing.detail);
    assert!(failing.failure("opkg").contains("exit 3"), "{}", failing.failure("opkg"));

    let ok = run_refresh(&["sh".to_string(), "-c".to_string(), "echo fetched".to_string()])
      .expect("the command ran");
    assert_eq!(ok.success, true);
    assert_eq!(ok.code, Some(0));
  }
}
