//! PID-based lock files.
//!
//! Provides mutual exclusion between processes using lock files containing
//! the holder's PID. Stale locks (where the holding process has exited)
//! are automatically recovered.
//!
//! Linux-only: liveness is checked via `/proc/{pid}`.
//!
//! # Limitations
//!
//! PID recycling can cause a stale lock to appear live if the OS reassigns
//! the PID to an unrelated process before we check `/proc`. This is
//! defense-in-depth — systemd's `Type=oneshot` prevents overlapping
//! heartbeat runs, so the lock is a secondary safeguard.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// RAII guard that removes the lock file on drop.
pub struct Lock {
    path: PathBuf,
}

/// Why a lock could not be acquired.
#[derive(Debug)]
#[allow(dead_code)] // Variants are constructed and matched; fields read by callers in future PRs.
pub enum LockStatus {
    /// Another live process holds the lock.
    Held(u32),
    /// Lock file exists but the process is dead (recovered automatically
    /// on the next acquire attempt — this variant is returned when recovery
    /// itself races with another acquirer).
    Stale,
    /// Filesystem error.
    Io(io::Error),
}

impl Lock {
    /// Attempt to acquire the lock at `path`.
    ///
    /// If a lock file exists, checks whether the PID inside is still alive.
    /// Stale locks are removed before retrying. Uses `create_new` for atomic
    /// creation to avoid TOCTOU races.
    pub fn acquire(path: &Path) -> Result<Self, LockStatus> {
        // If a lock file already exists, inspect it.
        if let Ok(contents) = fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse::<u32>()
                && process_alive(pid)
            {
                return Err(LockStatus::Held(pid));
            }
            // Stale — remove and retry.
            let _ = fs::remove_file(path);
        }

        // Atomic create — fails if another process raced us.
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    LockStatus::Stale
                } else {
                    LockStatus::Io(e)
                }
            })?;

        // No sync_data() — if we crash before flush, the empty/partial file
        // will fail to parse on next acquire and be treated as stale. Correct.
        io::Write::write_all(&mut &file, std::process::id().to_string().as_bytes())
            .map_err(LockStatus::Io)?;

        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Check if a process is alive via `/proc/{pid}`.
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        {
            let _lock = Lock::acquire(&path).unwrap();
            assert!(path.exists());
            let contents = fs::read_to_string(&path).unwrap();
            assert_eq!(contents, std::process::id().to_string());
        }

        assert!(!path.exists());
    }

    #[test]
    fn double_acquire_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let _lock = Lock::acquire(&path).unwrap();
        let result = Lock::acquire(&path);

        assert!(matches!(result, Err(LockStatus::Held(_))));
    }

    #[test]
    fn stale_lock_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        // Write a lock file with a PID that doesn't exist.
        // PID 0 is the idle task (swapper) — never a user process, and
        // /proc/0 doesn't exist on Linux.
        fs::write(&path, "0").unwrap();

        let lock = Lock::acquire(&path).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, std::process::id().to_string());
        drop(lock);
    }
}
