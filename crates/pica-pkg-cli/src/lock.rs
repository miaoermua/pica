use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::Duration;

use crate::app::{CliError, CliResult, E_IO, E_LOCK_BUSY, E_RUNTIME};
use crate::i18n::{Key, Lang};

/// How long a lock directory may exist before it counts as abandoned.
///
/// The window it covers is the one between `create_dir` and the write of the pid
/// file, which is a handful of syscalls: a directory without a pid that is this
/// old is one whose owner died in between, not one being taken right now.
const ACQUIRE_GRACE: Duration = Duration::from_secs(5);

/// What can be said about a pid.
///
/// The three answers are not two: "still running" and "gone" both come from
/// looking, while "cannot tell" comes from not being able to look at all — no
/// `/proc` on this host, a pid hidden by `hidepid`, an unreadable entry. A lock
/// that cannot be checked is not a stale lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Liveness {
  Running,
  Gone,
  Unknown,
}

pub struct LockGuard {
  lock_dir: PathBuf,
}

impl LockGuard {
  pub fn acquire(lock_file: &Path, lang: Lang) -> CliResult<Self> {
    Self::acquire_with(lock_file, lang, process_liveness)
  }

  /// The same acquisition, with the process check handed in.
  ///
  /// It is a parameter so that "this host cannot answer" can be tested on a host
  /// that can: the behaviour that matters here is what happens when the answer is
  /// `Unknown`, and no test machine can be relied on to be such a host.
  fn acquire_with(
    lock_file: &Path,
    lang: Lang,
    liveness: impl Fn(u32) -> Liveness,
  ) -> CliResult<Self> {
    if let Some(parent) = lock_file.parent() {
      ensure_dir(parent)?;
    }

    let mut lock_name = OsString::from(lock_file.as_os_str());
    lock_name.push(".d");
    let lock_dir = PathBuf::from(lock_name);

    match fs::create_dir(&lock_dir) {
      Ok(()) => {}
      Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
        match inspect(&lock_dir, &liveness) {
          Verdict::Held(pid) => {
            let path = lock_dir.display().to_string();
            let pid = pid.to_string();
            return Err(CliError::new(E_LOCK_BUSY, lang.args(Key::LockBusy, &[&path, &pid])));
          }
          Verdict::Unverified(holder) => {
            // Deleting the lock here is the one action that cannot be undone: two
            // transactions would then write the same database, and the loser's work
            // is silently gone. Leaving a lock nobody holds costs an operator one
            // `rm`; taking a lock somebody holds costs the other install.
            let path = lock_dir.display().to_string();
            return Err(CliError::new(
              E_LOCK_BUSY,
              lang.args(Key::LockUnverified, &[&path, &holder]),
            ));
          }
          Verdict::Stale => {
            fs::remove_dir_all(&lock_dir).map_err(|remove_err| {
              let path = lock_dir.display().to_string();
              CliError::new(
                E_RUNTIME,
                lang.args(Key::LockCleanupFailed, &[&path, &remove_err.to_string()]),
              )
            })?;

            fs::create_dir(&lock_dir).map_err(|retry_err| {
              CliError::new(E_RUNTIME, lang.args(Key::LockFailed, &[&retry_err.to_string()]))
            })?;
          }
        }
      }
      Err(err) => {
        return Err(CliError::new(E_RUNTIME, lang.args(Key::LockFailed, &[&err.to_string()])));
      }
    }

    let pid_file = lock_dir.join("pid");
    let _ = fs::write(pid_file, process::id().to_string());

    Ok(Self { lock_dir })
  }
}

/// What to do with a lock directory that already exists.
enum Verdict {
  /// Its holder is still there.
  Held(u32),
  /// It cannot be established whether it is held, so it is left alone.
  Unverified(String),
  /// Nobody holds it and the directory can be replaced.
  Stale,
}

fn inspect(lock_dir: &Path, liveness: &impl Fn(u32) -> Liveness) -> Verdict {
  if let Some(pid) = read_lock_pid(lock_dir) {
    return match liveness(pid) {
      Liveness::Running => Verdict::Held(pid),
      Liveness::Gone => Verdict::Stale,
      Liveness::Unknown => Verdict::Unverified(format!("pid {pid}")),
    };
  }

  // No readable pid. Either the holder is between creating the directory and
  // writing its pid — the two happen within microseconds of each other — or it
  // died there, and only its age tells them apart.
  match lock_dir_age(lock_dir) {
    Some(age) if age >= ACQUIRE_GRACE => Verdict::Stale,
    Some(_) => Verdict::Unverified("a lock being taken".to_string()),
    None => Verdict::Unverified("a lock without a pid".to_string()),
  }
}

fn read_lock_pid(lock_dir: &Path) -> Option<u32> {
  let pid_file = lock_dir.join("pid");
  let text = fs::read_to_string(pid_file).ok()?;
  text.trim().parse::<u32>().ok()
}

fn lock_dir_age(lock_dir: &Path) -> Option<Duration> {
  fs::metadata(lock_dir).ok()?.modified().ok()?.elapsed().ok()
}

/// Whether the process `pid` is still there.
///
/// `/proc` is how a router answers this at all. Where it is missing there is no
/// portable answer — a development machine and a container with a trimmed `/proc`
/// both look like "no such pid" if all you ask is whether a directory exists —
/// and saying so is the point of [`Liveness::Unknown`]: the caller must not read
/// it as "gone" and reclaim a lock a running process holds.
pub(crate) fn process_liveness(pid: u32) -> Liveness {
  if pid == 0 {
    return Liveness::Gone;
  }

  let proc_dir = Path::new("/proc");
  match fs::metadata(proc_dir) {
    Ok(metadata) if metadata.is_dir() => {
      match fs::symlink_metadata(proc_dir.join(pid.to_string())) {
        Ok(_) => Liveness::Running,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Liveness::Gone,
        // Hidden by `hidepid`, or some other refusal: the pid may well be alive.
        Err(_) => Liveness::Unknown,
      }
    }
    _ => Liveness::Unknown,
  }
}

impl Drop for LockGuard {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.lock_dir);
  }
}

fn ensure_dir(path: &Path) -> CliResult<()> {
  fs::create_dir_all(path)
    .map_err(|err| CliError::new(E_IO, format!("mkdir {} failed: {err}", path.display())))
}

#[cfg(test)]
mod tests {
  use super::{ACQUIRE_GRACE, Liveness, LockGuard};
  use crate::i18n::Lang;
  use std::fs;
  use std::path::{Path, PathBuf};
  use std::time::{Duration, SystemTime, UNIX_EPOCH};

  fn unique_lock_file() -> PathBuf {
    let mut path = std::env::temp_dir();
    let now =
      SystemTime::now().duration_since(UNIX_EPOCH).expect("time must move forward").as_nanos();
    path.push(format!("pica-lock-test-{}-{now}.lck", std::process::id()));
    path
  }

  fn lock_dir_of(lock_file: &Path) -> PathBuf {
    let mut lock_name = lock_file.as_os_str().to_os_string();
    lock_name.push(".d");
    PathBuf::from(lock_name)
  }

  #[test]
  fn acquire_replaces_stale_lock_dir() {
    let lock_file = unique_lock_file();
    let lock_dir = lock_dir_of(&lock_file);

    fs::create_dir_all(&lock_dir).expect("create stale lock dir");
    fs::write(lock_dir.join("pid"), "999999").expect("write stale pid");

    {
      let _guard = LockGuard::acquire_with(&lock_file, Lang::EnUs, |_| Liveness::Gone)
        .expect("must acquire after stale cleanup");
      assert!(lock_dir.exists());
      assert_eq!(
        fs::read_to_string(lock_dir.join("pid")).expect("pid"),
        std::process::id().to_string(),
        "the lock names this process now"
      );
    }

    assert!(!lock_dir.exists());
  }

  #[test]
  fn acquire_reports_busy_when_the_holder_is_alive() {
    let lock_file = unique_lock_file();
    let lock_dir = lock_dir_of(&lock_file);

    fs::create_dir_all(&lock_dir).expect("create lock dir");
    fs::write(lock_dir.join("pid"), std::process::id().to_string()).expect("write live pid");

    match LockGuard::acquire_with(&lock_file, Lang::EnUs, |_| Liveness::Running) {
      Ok(_) => panic!("must fail when holder is alive"),
      Err(err) => {
        assert!(err.message.contains("held by pid"), "{}", err.message);
        assert_eq!(err.code, crate::app::E_LOCK_BUSY);
      }
    }

    let _ = fs::remove_dir_all(lock_dir);
  }

  #[test]
  fn a_lock_that_cannot_be_checked_is_not_reclaimed() {
    // The host cannot say whether the holder is alive — no `/proc` at all, a pid
    // hidden by `hidepid`, a container. The lock stays where it is: reclaiming it
    // on a guess would let a second `pica` write the database while the first one
    // still has it open, and the guess would be wrong on exactly the host that
    // could not answer.
    let lock_file = unique_lock_file();
    let lock_dir = lock_dir_of(&lock_file);

    fs::create_dir_all(&lock_dir).expect("create lock dir");
    fs::write(lock_dir.join("pid"), "4242").expect("write pid");

    match LockGuard::acquire_with(&lock_file, Lang::EnUs, |_| Liveness::Unknown) {
      Ok(_) => panic!("must not take a lock it cannot verify"),
      Err(err) => {
        assert_eq!(err.code, crate::app::E_LOCK_BUSY);
        assert!(err.message.contains("4242"), "{}", err.message);
        assert!(
          err.message.contains("not") || err.message.contains("cannot"),
          "the message has to say the check failed, not that the holder is gone: {}",
          err.message
        );
      }
    }

    assert!(lock_dir.exists(), "the directory was left alone");
    assert_eq!(
      fs::read_to_string(lock_dir.join("pid")).expect("pid"),
      "4242",
      "and so was the pid it names"
    );
    let _ = fs::remove_dir_all(lock_dir);
  }

  #[test]
  fn a_lock_directory_without_a_pid_is_left_alone_while_it_is_fresh() {
    // Writing the pid is the step right after `create_dir`, so a directory with no
    // pid is usually one being taken *right now*. Who holds it cannot be answered
    // from the directory alone, so it is left alone rather than deleted under the
    // acquirer's feet.
    let lock_file = unique_lock_file();
    let lock_dir = lock_dir_of(&lock_file);

    fs::create_dir_all(&lock_dir).expect("create lock dir");
    fs::create_dir_all(lock_dir.join("someone")).expect("create a directory to look alive");

    match LockGuard::acquire_with(&lock_file, Lang::EnUs, |_| Liveness::Gone) {
      Ok(_) => panic!("a lock being taken must not be reclaimed"),
      Err(err) => assert_eq!(err.code, crate::app::E_LOCK_BUSY),
    }

    assert!(lock_dir.join("someone").exists(), "the directory was not removed");

    let _ = fs::remove_dir_all(lock_dir);
  }

  #[test]
  fn a_lock_directory_without_a_pid_is_reclaimed_once_it_is_old() {
    // The other side of the same window: an owner that died between `create_dir`
    // and the write leaves a directory that is never going to get a pid.
    let lock_file = unique_lock_file();
    let lock_dir = lock_dir_of(&lock_file);

    fs::create_dir_all(&lock_dir).expect("create lock dir");
    let stale = SystemTime::now() - ACQUIRE_GRACE - Duration::from_secs(60);
    fs::File::open(&lock_dir)
      .expect("open for setting the time")
      .set_modified(stale)
      .expect("age the directory");

    let _guard = LockGuard::acquire_with(&lock_file, Lang::EnUs, |_| Liveness::Gone)
      .expect("an abandoned directory is reclaimed");
    assert!(lock_dir.join("pid").exists());

    let _ = fs::remove_dir_all(lock_dir);
  }

  #[test]
  fn the_lock_a_live_process_holds_is_reported_by_the_real_check() {
    // The injected probe above proves the decision; this one proves the probe, on
    // whichever host is running it: this pid has `/proc` when the host has `/proc`
    // at all, and a pid that cannot exist is gone where the host can answer.
    if cfg!(target_os = "linux") {
      assert_eq!(super::process_liveness(std::process::id()), Liveness::Running);
      assert_eq!(super::process_liveness(0), Liveness::Gone);
      assert_eq!(super::process_liveness(u32::MAX - 1), Liveness::Gone);
    }
  }
}
