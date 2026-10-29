//! A stat index, like git's: remembers each file's hash along with the size,
//! mtime and inode it had, so a file whose stat is unchanged is not re-read.
//! Stat is trusted; a rewrite that restores size and mtime in place is not
//! noticed, which is what lets an unstashed file match its entry again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use super::blob::{is_hash, Hash, Store};
use super::sha256_file;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub size: u64,
    /// Nanoseconds since the epoch, see [`stat_of`].
    pub mtime_ns: i128,
    pub ino: u64,
    pub hash: Hash,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Index {
    entries: BTreeMap<PathBuf, IndexEntry>,
}

/// `(size, mtime_ns, ino)`. On unix `mtime_ns` is `mtime() * 1e9 +
/// mtime_nsec()`, the same seconds/nanoseconds pair `filetime` reads and
/// writes, so a restored mtime compares equal.
pub fn stat_of(meta: &fs::Metadata) -> (u64, i128, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let ns = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
        (meta.len(), ns, meta.ino())
    }
    #[cfg(not(unix))]
    {
        let ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i128);
        (meta.len(), ns, 0)
    }
}

impl Index {
    fn path(store: &Store, tree_id: &str) -> PathBuf {
        store.root.join("index").join(format!("{tree_id}.json"))
    }

    /// A missing file is an empty index.
    pub fn load(store: &Store, tree_id: &str) -> Result<Index> {
        let path = Self::path(store, tree_id);
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, store: &Store, tree_id: &str) -> Result<()> {
        let path = Self::path(store, tree_id);
        let dir = path.parent().expect("index path has a parent");
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let mut tmp = NamedTempFile::new_in(dir)?;
        serde_json::to_writer(tmp.as_file_mut(), self)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path)
            .map_err(|e| anyhow!("persist {}: {}", path.display(), e.error))?;
        File::open(dir).and_then(|d| d.sync_all()).ok();
        Ok(())
    }

    /// The hash of `root/rel`: cached when size, mtime and inode all match,
    /// otherwise computed and recorded.
    pub fn hash(&mut self, root: &Path, rel: &Path) -> Result<Hash> {
        let full = root.join(rel);
        let meta = fs::metadata(&full).with_context(|| format!("stat {}", full.display()))?;
        let (size, mtime_ns, ino) = stat_of(&meta);
        if let Some(e) = self.entries.get(rel) {
            if e.size == size && e.mtime_ns == mtime_ns && e.ino == ino && is_hash(&e.hash) {
                return Ok(e.hash.clone());
            }
        }
        let hash = sha256_file(&full)?;
        self.entries.insert(
            rel.to_path_buf(),
            IndexEntry {
                size,
                mtime_ns,
                ino,
                hash: hash.clone(),
            },
        );
        Ok(hash)
    }

    pub fn forget_missing(&mut self, keep: &BTreeSet<PathBuf>) {
        self.entries.retain(|p, _| keep.contains(p));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
