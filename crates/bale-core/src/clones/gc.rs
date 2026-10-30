//! Garbage collection for the clone store: files no registered tree still
//! uses, and temp files an interrupted `adopt` left behind.
//!
//! Anything younger than the grace period is kept, so a file adopted just
//! before the live set was computed is not lost.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::store::CloneStore;
use super::Key;

/// Removes store files whose key is not in `live` and tmp files, when their
/// mtime is older than `grace`; returns the bytes freed (or that would be,
/// with `dry_run`). A file whose name is not a key is left alone. A missing
/// store frees nothing and is not created.
pub fn collect(
    store: &CloneStore,
    live: &HashSet<Key>,
    grace: Duration,
    dry_run: bool,
) -> Result<u64> {
    let now = SystemTime::now();
    let mut freed = 0;
    for kind in ["git", "lfs"] {
        let kind_dir = store.root().join(kind);
        for prefix in read_dir_or_empty(&kind_dir)? {
            let prefix = prefix?;
            if !prefix.file_type()?.is_dir() {
                continue;
            }
            let prefix_name = prefix.file_name().to_string_lossy().into_owned();
            for entry in read_dir_or_empty(&prefix.path())? {
                let entry = entry?;
                let meta = entry.metadata()?;
                if !meta.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let removable = if name.contains(".tmp-") {
                    true
                } else {
                    match key_for(kind, &prefix_name, &name) {
                        Some(key) => !live.contains(&key),
                        None => false,
                    }
                };
                let old = meta
                    .modified()
                    .map(|m| now.duration_since(m).is_ok_and(|age| age > grace))
                    .unwrap_or(false);
                if !removable || !old {
                    continue;
                }
                if !dry_run {
                    match fs::remove_file(entry.path()) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                        Err(e) => {
                            return Err(e)
                                .with_context(|| format!("removing {}", entry.path().display()))
                        }
                    }
                }
                freed += meta.len();
            }
        }
    }
    Ok(freed)
}

/// The key a store file at `<kind>/<prefix>/<name>` holds, if the name is a
/// hex id filed under its own two-character prefix.
fn key_for(kind: &str, prefix: &str, name: &str) -> Option<Key> {
    let hex = name.len() >= 2
        && name.bytes().all(|b| b.is_ascii_hexdigit())
        && name.get(..2) == Some(prefix);
    if !hex {
        return None;
    }
    match kind {
        "git" => Some(Key::Git(name.to_string())),
        "lfs" => Some(Key::Lfs(name.to_string())),
        _ => None,
    }
}

fn read_dir_or_empty(dir: &Path) -> Result<Vec<io::Result<fs::DirEntry>>> {
    match fs::read_dir(dir) {
        Ok(rd) => Ok(rd.collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}
