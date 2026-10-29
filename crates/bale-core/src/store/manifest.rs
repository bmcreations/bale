//! The list of files one stash moved into the store.
//!
//! A manifest is written once as `manifests/<commitment>.json`. The tree
//! record holds only the commitment, so unstash can tell a manifest that was
//! edited or swapped from the one the stash wrote.

use std::fs::{self, File};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::blob::{is_hash, Hash, Store};
use super::hex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Path relative to the tree root.
    pub rel: PathBuf,
    pub hash: Hash,
    pub size: u64,
    /// Full `st_mode` bits.
    pub mode: u32,
    pub mtime_ns: i128,
    /// The inode the stub keeps, to tell bale's stub from a file a build
    /// recreated. Not part of the commitment: it identifies the stub, not the
    /// content.
    pub ino: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashManifest {
    pub tree_id: String,
    pub created_unix: i64,
    pub entries: Vec<Entry>,
}

impl StashManifest {
    /// SHA-256 over the entries sorted by `rel`, each as
    /// `rel\0hash\0size\0mode\0mtime_ns\n`.
    pub fn commitment(&self) -> Hash {
        let mut entries: Vec<&Entry> = self.entries.iter().collect();
        entries.sort_by(|a, b| a.rel.cmp(&b.rel));
        let mut hasher = Sha256::new();
        for e in entries {
            hasher.update(e.rel.to_string_lossy().as_bytes());
            hasher.update(format!(
                "\0{}\0{}\0{}\0{}\n",
                e.hash, e.size, e.mode, e.mtime_ns
            ));
        }
        hex(&hasher.finalize())
    }
}

impl Store {
    fn manifest_path(&self, c: &str) -> Result<PathBuf> {
        if !is_hash(c) {
            bail!("not a sha256 hex digest: {c:?}");
        }
        Ok(self.root.join("manifests").join(format!("{c}.json")))
    }

    /// Writes `m` under its commitment (temp file, fsync, rename, fsync of
    /// the directory) and returns the commitment.
    pub fn write_manifest(&self, m: &StashManifest) -> Result<Hash> {
        let c = m.commitment();
        let path = self.manifest_path(&c)?;
        let dir = self.root.join("manifests");
        let mut tmp = NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(&mut tmp, m)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path)
            .map_err(|e| anyhow!("persist manifest {c}: {}", e.error))?;
        File::open(&dir).and_then(|d| d.sync_all()).ok();
        Ok(c)
    }

    /// The manifest committed to by `c`; errors if its content no longer
    /// hashes to `c`.
    pub fn read_manifest(&self, c: &Hash) -> Result<StashManifest> {
        let path = self.manifest_path(c)?;
        let text = fs::read_to_string(&path).with_context(|| format!("read manifest {c}"))?;
        let m: StashManifest =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if m.commitment() != *c {
            bail!("manifest {c} is corrupt: commitment mismatch");
        }
        Ok(m)
    }

    /// Commitments of every manifest in the store, sorted.
    pub fn list_manifests(&self) -> Result<Vec<Hash>> {
        let dir = self.root.join("manifests");
        let mut out = Vec::new();
        for ent in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let name = ent?.file_name();
            if let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) {
                if is_hash(stem) {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Removes the manifest; one already gone is not an error.
    pub fn remove_manifest(&self, c: &Hash) -> Result<()> {
        let path = self.manifest_path(c)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("remove manifest {c}")),
        }
    }
}
