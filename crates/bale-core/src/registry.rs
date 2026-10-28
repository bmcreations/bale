use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::BaleHome;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Hot,
    Baling,
    Baled,
    Cooling,
    Cold,
    /// `bale open` is restoring a cold workspace; a rerun resumes it.
    Opening,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub repo_common_dir: PathBuf,
    pub path: PathBuf,
    pub admin_dir: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub level: Level,
    pub last_activity: SystemTime,
    pub note: Option<String>,
}

/// First 12 hex characters of the sha256 of the canonicalized path, falling
/// back to the given path when it no longer exists.
pub fn workspace_id(path: &Path) -> String {
    let canonical = canonical_or_ancestor(path);
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Canonicalizes `path`; when it no longer exists, canonicalizes the nearest
/// existing ancestor and re-joins the remaining components, so the result
/// matches what canonicalize gave while the path existed.
pub(crate) fn canonical_or_ancestor(path: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(path) {
        return c;
    }
    let mut rest = Vec::new();
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name.to_os_string());
        }
        if let Ok(mut c) = fs::canonicalize(parent) {
            c.extend(rest.iter().rev());
            return c;
        }
        cur = parent;
    }
    path.to_path_buf()
}

pub struct Registry {
    home: BaleHome,
}

impl Registry {
    pub fn open(home: BaleHome) -> Registry {
        Registry { home }
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.home.records_dir().join(format!("{id}.json"))
    }

    fn read(path: &Path) -> Result<Record> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading record {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("corrupt record {}", path.display()))
    }

    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        let path = self.record_path(id);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .with_context(|| format!("corrupt record {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading record {}", path.display())),
        }
    }

    /// Callers hold the `WorkspaceLock` for this id (the shared tmp name relies on it).
    ///
    /// Writes `<id>.json.tmp`, fsyncs it, renames it over `<id>.json`, then
    /// fsyncs the directory (skipped on Windows).
    pub fn put(&self, record: &Record) -> Result<()> {
        let dir = self.home.records_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let final_path = self.record_path(&record.id);
        let tmp_path = dir.join(format!("{}.json.tmp", record.id));
        let bytes = serde_json::to_vec_pretty(record)?;
        let mut f =
            File::create(&tmp_path).with_context(|| format!("creating {}", tmp_path.display()))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp_path, &final_path)
            .with_context(|| format!("renaming into {}", final_path.display()))?;
        #[cfg(not(windows))]
        File::open(&dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("syncing {}", dir.display()))?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let path = self.record_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// Reads every record; a corrupt one is an error naming its file.
    pub fn all(&self) -> Result<Vec<Record>> {
        self.record_paths()?.iter().map(|p| Self::read(p)).collect()
    }

    /// Reads every record separately, pairing each result with its file
    /// stem, so one corrupt record does not hide the others. Errs only when
    /// the registry directory cannot be listed.
    pub fn all_lenient(&self) -> Result<Vec<(String, Result<Record>)>> {
        Ok(self
            .record_paths()?
            .into_iter()
            .map(|p| {
                let stem = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (stem, Self::read(&p))
            })
            .collect())
    }

    /// The `*.json` files in the records dir, sorted; none when it is absent.
    fn record_paths(&self) -> Result<Vec<PathBuf>> {
        let dir = self.home.records_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry
                .with_context(|| format!("reading {}", dir.display()))?
                .path();
            if path.extension().is_some_and(|e| e == "json") {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }
}
