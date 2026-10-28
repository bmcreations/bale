use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// Newest mtime of any file under `root`, ignoring `.git` entries and the
/// `regenerable` dirs (absolute paths). Symlinks are not followed. Returns
/// `UNIX_EPOCH` when no files remain.
pub fn last_source_change(root: &Path, regenerable: &[PathBuf]) -> Result<SystemTime> {
    let mut newest = UNIX_EPOCH;
    let walker = WalkDir::new(root).follow_links(false).into_iter();
    let walker = walker.filter_entry(|e| {
        e.path() == root
            || (e.file_name() != ".git" && !regenerable.iter().any(|r| e.path().starts_with(r)))
    });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                if err.io_error().map(|e| e.kind()) == Some(std::io::ErrorKind::NotFound) {
                    continue;
                }
                let path = err.path().unwrap_or(root).to_path_buf();
                return Err(err).with_context(|| format!("walking {}", path.display()));
            }
        };
        if entry.file_type().is_dir() {
            continue;
        }
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("stat {}", entry.path().display())),
        };
        let mtime = meta
            .modified()
            .with_context(|| format!("mtime of {}", entry.path().display()))?;
        if mtime > newest {
            newest = mtime;
        }
    }
    Ok(newest)
}
