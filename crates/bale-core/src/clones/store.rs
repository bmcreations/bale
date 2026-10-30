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

/// What [`CloneStore::adopt`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adopted {
    /// This call added the file to the store.
    Stored,
    /// The key was already stored.
    Present,
    /// The source changed during the clone; nothing was stored.
    Changed,
}

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

    /// Whether files in the store can be cloned: clones a tiny file inside
    /// `clones/` and removes both. `Ok(false)` on a filesystem or platform
    /// without clones.
    pub fn supports_clones(&self) -> io::Result<bool> {
        let src = temp_name(&self.root.join(".probe"));
        let dst = temp_name(&self.root.join(".probe"));
        // A block-sized file: btrfs stores tiny files inline, and some
        // kernels refuse to clone inline extents.
        std::fs::write(&src, [0u8; 4096])?;
        let _src = TempGuard(&src, true);
        match clone_file(&src, &dst) {
            Ok(()) => {
                let _ = std::fs::remove_file(&dst);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::Unsupported => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Clones `from`, which the caller has verified matches `key`, into the
    /// store. `unchanged` runs after the clone and before the copy is
    /// published; it returns whether `from` is still the file that was
    /// verified, and when it isn't the copy is dropped unseen.
    pub fn adopt(
        &self,
        key: &Key,
        from: &Path,
        unchanged: impl FnOnce() -> io::Result<bool>,
    ) -> io::Result<Adopted> {
        let dest = self.path(key);
        if dest.is_file() {
            return Ok(Adopted::Present);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = temp_name(&dest);
        clone_file(from, &tmp)?;
        let _guard = TempGuard(&tmp, true);
        // A clone keeps the source's mtime; gc's grace counts from now.
        std::fs::File::open(&tmp)?.set_modified(SystemTime::now())?;
        if !unchanged()? {
            return Ok(Adopted::Changed);
        }
        set_mode(&tmp, 0o444)?;
        // A hard link never replaces an existing name, unlike rename.
        match std::fs::hard_link(&tmp, &dest) {
            Ok(()) => Ok(Adopted::Stored),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(Adopted::Present),
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
