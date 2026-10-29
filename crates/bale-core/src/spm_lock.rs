//! The lock on `<bale home>/spm/.lock` that keeps the sweep's removal of a
//! shared packages folder from racing a hook that links to it.
//!
//! Anything that makes a symlink to `spm/<key>` or seeds a folder into `spm/`
//! holds the lock shared until the link exists. The sweep takes it exclusive
//! to rename an unreferenced `spm/<key>` out of the way. Neither side waits:
//! a hook that can't get it links nothing (the tree builds unshared), and a
//! sweep that can't get it removes nothing.

use std::fs::{self, File};

use crate::BaleHome;

/// How a lock is wanted.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Shared,
    Exclusive,
}

/// A held lock; released when dropped.
#[derive(Debug)]
pub(crate) struct SpmLock {
    _file: File,
}

/// Takes the lock without waiting, creating `spm/` and `spm/.lock` if
/// missing. `None` when someone holds it in a conflicting mode, or on any
/// failure to take it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn try_lock(home: &BaleHome, mode: Mode) -> Option<SpmLock> {
    use std::os::fd::AsRawFd;
    let dir = home.spm_dir();
    fs::create_dir_all(&dir).ok()?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))
        .ok()?;
    let op = match mode {
        Mode::Shared => libc::LOCK_SH,
        Mode::Exclusive => libc::LOCK_EX,
    } | libc::LOCK_NB;
    // SAFETY: flock on a descriptor we own.
    (unsafe { libc::flock(file.as_raw_fd(), op) } == 0).then_some(SpmLock { _file: file })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn try_lock(_home: &BaleHome, _mode: Mode) -> Option<SpmLock> {
    None
}
