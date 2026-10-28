use std::fs::{self, File, OpenOptions};

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;

use crate::BaleHome;

/// Exclusive per-workspace lock on `locks/<id>.lock`, released on drop.
#[derive(Debug)]
pub struct WorkspaceLock {
    file: File,
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
            Ok(true) => Ok(Some(WorkspaceLock { file })),
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
                drop(WorkspaceLock { file });
                Ok(false)
            }
            Ok(false) => Ok(true),
            Err(e) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }
}
