use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use crate::{CliError, CliResult, E_IO, E_LOCK_BUSY, E_RUNTIME};

pub struct LockGuard {
    lock_dir: PathBuf,
}

impl LockGuard {
    pub fn acquire(lock_file: &Path) -> CliResult<Self> {
        if let Some(parent) = lock_file.parent() {
            ensure_dir(parent)?;
        }

        let mut lock_name = OsString::from(lock_file.as_os_str());
        lock_name.push(".d");
        let lock_dir = PathBuf::from(lock_name);

        match fs::create_dir(&lock_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(lock_holder) = read_lock_pid(&lock_dir) {
                    if pid_is_running(lock_holder) {
                        return Err(CliError::new(
                            E_LOCK_BUSY,
                            format!(
                                "cannot lock pica database: {} exists (held by pid {lock_holder})",
                                lock_dir.display()
                            ),
                        ));
                    }
                }

                fs::remove_dir_all(&lock_dir).map_err(|remove_err| {
                    CliError::new(
                        E_RUNTIME,
                        format!(
                            "cannot lock pica database: stale lock cleanup failed for {}: {remove_err}",
                            lock_dir.display()
                        ),
                    )
                })?;

                fs::create_dir(&lock_dir).map_err(|retry_err| {
                    CliError::new(
                        E_RUNTIME,
                        format!("cannot lock pica database: {retry_err}"),
                    )
                })?;
            }
            Err(err) => {
                return Err(CliError::new(
                    E_RUNTIME,
                    format!("cannot lock pica database: {err}"),
                ));
            }
        }

        let pid_file = lock_dir.join("pid");
        let _ = fs::write(pid_file, process::id().to_string());

        Ok(Self { lock_dir })
    }
}

fn read_lock_pid(lock_dir: &Path) -> Option<u32> {
    let pid_file = lock_dir.join("pid");
    let text = fs::read_to_string(pid_file).ok()?;
    text.trim().parse::<u32>().ok()
}

fn pid_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    let proc_entry = Path::new("/proc").join(pid.to_string());
    proc_entry.exists()
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.lock_dir);
    }
}

fn ensure_dir(path: &Path) -> CliResult<()> {
    fs::create_dir_all(path).map_err(|err| {
        CliError::new(
            E_IO,
            format!("mkdir {} failed: {err}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::LockGuard;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_lock_file() -> PathBuf {
        let mut path = std::env::temp_dir();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time must move forward")
            .as_nanos();
        path.push(format!(
            "pica-lock-test-{}-{now}.lck",
            std::process::id()
        ));
        path
    }

    #[test]
    fn acquire_replaces_stale_lock_dir() {
        let lock_file = unique_lock_file();
        let mut lock_name = lock_file.as_os_str().to_os_string();
        lock_name.push(".d");
        let lock_dir = PathBuf::from(lock_name);

        fs::create_dir_all(&lock_dir).expect("create stale lock dir");
        fs::write(lock_dir.join("pid"), "999999").expect("write stale pid");

        {
            let _guard = LockGuard::acquire(&lock_file).expect("must acquire after stale cleanup");
            assert!(lock_dir.exists());
        }

        assert!(!lock_dir.exists());
    }

    #[test]
    fn acquire_reports_busy_when_pid_alive() {
        let lock_file = unique_lock_file();
        let mut lock_name = lock_file.as_os_str().to_os_string();
        lock_name.push(".d");
        let lock_dir = PathBuf::from(lock_name);

        fs::create_dir_all(&lock_dir).expect("create lock dir");
        fs::write(lock_dir.join("pid"), std::process::id().to_string()).expect("write live pid");

        match LockGuard::acquire(&lock_file) {
            Ok(_) => panic!("must fail when holder is alive"),
            Err(err) => assert!(err.message.contains("held by pid")),
        }

        let _ = fs::remove_dir_all(lock_dir);
    }
}
