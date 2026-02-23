use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

#[derive(Debug)]
pub struct LockError {
    pub code: &'static str,
    pub message: String,
}

pub type LockResult<T> = Result<T, LockError>;

pub struct LockGuard {
    lock_dir: PathBuf,
}

impl LockGuard {
    pub fn acquire(lock_file: &Path) -> LockResult<Self> {
        if let Some(parent) = lock_file.parent() {
            ensure_dir(parent)?;
        }

        let mut lock_name = OsString::from(lock_file.as_os_str());
        lock_name.push(".d");
        let lock_dir = PathBuf::from(lock_name);

        match fs::create_dir(&lock_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(LockError {
                    code: "E_LOCK_BUSY",
                    message: format!("cannot lock pica database: {} exists", lock_dir.display()),
                });
            }
            Err(err) => {
                return Err(LockError {
                    code: "E_RUNTIME",
                    message: format!("cannot lock pica database: {err}"),
                });
            }
        }

        let pid_file = lock_dir.join("pid");
        let _ = fs::write(pid_file, process::id().to_string());

        Ok(Self { lock_dir })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.lock_dir);
    }
}

fn ensure_dir(path: &Path) -> LockResult<()> {
    fs::create_dir_all(path).map_err(|err| LockError {
        code: "E_RUNTIME",
        message: format!("mkdir {} failed: {err}", path.display()),
    })
}
