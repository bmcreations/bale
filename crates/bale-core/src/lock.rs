use std::fs::{self, File, OpenOptions};

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;

use crate::BaleHome;

/// Exclusive per-workspace lock on `locks/<id>.lock`. The OS releases it when
/// the file closes on drop.
#[derive(Debug)]
pub struct WorkspaceLock {
    _file: File,
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
            Ok(true) => Ok(Some(WorkspaceLock { _file: file })),
            Ok(false) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }
}
