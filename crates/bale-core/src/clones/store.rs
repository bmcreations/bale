//! The clone store: one read-only copy of each large tracked file, shared by every worktree.

use super::reflink::clone_file;
use super::Key;
use crate::BaleHome;
use std::fs::FileTimes;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct CloneStore {
    root: PathBuf,
}

/// Removes the file on drop unless disarmed.
struct TempGuard<'a>(&'a Path, bool);

impl Drop for TempGuard<'_> {
    fn drop(&mut self) {
        if self.1 {
            let _ = std::fs::remove_file(self.0);
        }
    }
}

fn temp_name(path: &Path) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".tmp-{}-{n}", std::process::id()));
    PathBuf::from(s)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

impl CloneStore {
    /// Opens the store under `home`, creating `clones/`.
    pub fn open(home: &BaleHome) -> io::Result<CloneStore> {
        let root = home.clones_dir();
        std::fs::create_dir_all(&root)?;
        Ok(CloneStore { root })
    }

    /// The `clones/` directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `clones/git/<oid[..2]>/<oid>` or `clones/lfs/<sha256[..2]>/<sha256>`.
    pub fn path(&self, key: &Key) -> PathBuf {
        let (kind, id) = match key {
            Key::Git(id) => ("git", id),
            Key::Lfs(id) => ("lfs", id),
        };
        let prefix = id.get(..2).unwrap_or(id);
        self.root.join(kind).join(prefix).join(id)
    }

    pub fn has(&self, key: &Key) -> bool {
        self.path(key).is_file()
    }

    /// Clones `from`, which the caller has verified matches `key`, into the store.
    /// `Ok(false)` when the key is already stored.
    pub fn adopt(&self, key: &Key, from: &Path) -> io::Result<bool> {
        let dest = self.path(key);
        if dest.is_file() {
            return Ok(false);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = temp_name(&dest);
        clone_file(from, &tmp)?;
        let _guard = TempGuard(&tmp, true);
        set_mode(&tmp, 0o444)?;
        // A hard link never replaces an existing name, unlike rename.
        match std::fs::hard_link(&tmp, &dest) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Replaces `dst` with a clone of the stored file, with `mode` and, when given, `mtime`.
    pub fn place(
        &self,
        key: &Key,
        dst: &Path,
        mode: u32,
        mtime: Option<SystemTime>,
    ) -> io::Result<()> {
        let mut tmp = dst.as_os_str().to_owned();
        tmp.push(".bale-share");
        let tmp = PathBuf::from(tmp);
        clone_file(&self.path(key), &tmp)?;
        let guard = TempGuard(&tmp, true);
        if let Some(t) = mtime {
            let f = std::fs::File::open(&tmp)?;
            f.set_times(FileTimes::new().set_modified(t).set_accessed(t))?;
        }
        set_mode(&tmp, mode)?;
        std::fs::rename(&tmp, dst)?;
        std::mem::forget(guard);
        Ok(())
    }
}
