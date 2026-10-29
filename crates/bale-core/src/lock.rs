use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;

use crate::BaleHome;

/// Exclusive per-workspace lock on `locks/<id>.lock`, released on drop.
#[derive(Debug)]
pub struct WorkspaceLock {
    file: File,
    /// The lock file's mtime when this holder took it; see [`Self::requested`].
    mtime_at_acquire: Option<SystemTime>,
}

impl Drop for WorkspaceLock {
    // Unlock explicitly instead of relying on close: a child process forked
    // by another thread can hold a copy of the fd until it execs, and the
    // lock lives as long as any copy does.
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl WorkspaceLock {
    /// Returns `None` when another holder has the lock.
    pub fn try_acquire(home: &BaleHome, id: &str) -> Result<Option<WorkspaceLock>> {
        let dir = home.locks_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{id}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(true) => {
                let mtime_at_acquire = file.metadata().and_then(|m| m.modified()).ok();
                Ok(Some(WorkspaceLock {
                    file,
                    mtime_at_acquire,
                }))
            }
            Ok(false) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }

    /// Whether another holder has the lock, without creating the lock file
    /// or its dir: no file means nobody holds it. Any lock taken to check is
    /// released before returning.
    pub fn is_held(home: &BaleHome, id: &str) -> Result<bool> {
        let path = home.locks_dir().join(format!("{id}.lock"));
        let file = match OpenOptions::new().write(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        match file.try_lock_exclusive() {
            Ok(true) => {
                let _ = FileExt::unlock(&file);
                Ok(false)
            }
            Ok(false) => Ok(true),
            Err(e) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }

    /// Asks the current holder to give the lock up soon, without taking it:
    /// sets the lock file's mtime to now. Long holders (stash) poll
    /// [`Self::requested`] and stop. Needs no lock, so a waiting prompt can
    /// call it. A missing lock file means nobody holds the lock.
    pub fn request(home: &BaleHome, id: &str) -> Result<()> {
        let path = lock_path(home, id);
        match filetime::set_file_mtime(&path, filetime::FileTime::now()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("touching {}", path.display())),
        }
    }

    /// Whether someone called [`Self::request`] since this holder took the lock.
    pub fn requested(&self) -> bool {
        let now = self.file.metadata().and_then(|m| m.modified()).ok();
        now != self.mtime_at_acquire
    }
}

fn lock_path(home: &BaleHome, id: &str) -> PathBuf {
    home.locks_dir().join(format!("{id}.lock"))
}
