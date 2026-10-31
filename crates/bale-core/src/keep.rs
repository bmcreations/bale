//! A per-tree keep-list: small files a tool writes inside a worktree that
//! git ignores, copied into `$BALE_HOME/keep/<id>/` so they survive the
//! tree being deleted and made again at the same path.
//!
//! The caller names the paths (relative to the tree root; a trailing `/`
//! is allowed). Core only copies. [`save`] copies what is there, [`restore`]
//! puts back what is absent and never overwrites. Both are capped so the
//! keep-list cannot turn into a build-output copier, take the tree's lock
//! without waiting, and write temp-file, fsync, rename.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde_json::Map;
use walkdir::WalkDir;

use crate::lock::WorkspaceLock;
use crate::BaleHome;

/// What to keep and how much.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepSpec {
    /// Paths relative to the tree root. Absolute paths and `..` are ignored.
    pub paths: Vec<String>,
    /// Files larger than this are skipped.
    pub max_file_bytes: u64,
    /// Total bytes the tree's keep store may hold; a file that would pass it
    /// is skipped.
    pub max_total_bytes: u64,
}

/// What a [`save`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SaveReport {
    /// Files written or refreshed in the store.
    pub saved: usize,
    /// Files skipped for size (file or total cap).
    pub skipped: usize,
}

/// Where a tree's kept files live.
pub fn keep_dir(home: &BaleHome, id: &str) -> PathBuf {
    home.0.join("keep").join(id)
}

/// A configured path as a clean relative path, or `None` if it could leave the tree.
fn clean(p: &str) -> Option<PathBuf> {
    let path = Path::new(p.trim_end_matches(['/', '\\']));
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// True when `root/rel` has a symlink in any component below `root`: the
/// copy must not follow a link out of the tree (or out of the store).
fn through_symlink(root: &Path, rel: &Path) -> bool {
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// Regular files under `root/rel` (or that file itself), as paths relative to
/// `root`, with their sizes. Symlinks are skipped.
fn files_under(root: &Path, rel: &Path) -> Vec<(PathBuf, u64)> {
    if through_symlink(root, rel) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for entry in WalkDir::new(root.join(rel))
        .follow_links(false)
        .sort_by_file_name()
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if let Ok(r) = entry.path().strip_prefix(root) {
            out.push((r.to_path_buf(), meta.len()));
        }
    }
    out
}

fn store_total(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Writes `src`'s bytes to `dest` through a temp file in `dest`'s directory,
/// fsynced, then renamed (`noclobber`: fail rather than replace).
fn write_atomic(src: &Path, dest: &Path, noclobber: bool) -> Result<bool> {
    let dir = dest.parent().context("destination has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut input = File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    io::copy(&mut input, tmp.as_file_mut())?;
    tmp.as_file().sync_all()?;
    if let Ok(meta) = input.metadata() {
        let _ = fs::set_permissions(tmp.path(), meta.permissions());
    }
    if noclobber {
        match tmp.persist_noclobber(dest) {
            Ok(_) => Ok(true),
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.error).with_context(|| format!("writing {}", dest.display())),
        }
    } else {
        tmp.persist(dest)
            .map_err(|e| e.error)
            .with_context(|| format!("writing {}", dest.display()))?;
        Ok(true)
    }
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    match (fs::read(a), fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Copies the kept paths of the live tree at `tree` into the store.
/// Returns `None` when another bale process holds the tree's lock. Files
/// removed from the tree stay in the store: the point is to outlive the tree.
pub fn save(home: &BaleHome, id: &str, tree: &Path, spec: &KeepSpec) -> Result<Option<SaveReport>> {
    let mut report = SaveReport::default();
    let paths: Vec<PathBuf> = spec.paths.iter().filter_map(|p| clean(p)).collect();
    // Look before locking: most turns have nothing new to say.
    let mut wanted: BTreeMap<PathBuf, u64> = BTreeMap::new();
    for rel in &paths {
        wanted.extend(files_under(tree, rel));
    }
    if wanted.is_empty() {
        return Ok(Some(report));
    }
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return Ok(None);
    };
    let dest_root = keep_dir(home, id);
    let mut total = store_total(&dest_root);
    for (rel, size) in wanted {
        let src = tree.join(&rel);
        let dest = dest_root.join(&rel);
        if size > spec.max_file_bytes {
            report.skipped += 1;
            continue;
        }
        if through_symlink(&dest_root, &rel) {
            continue;
        }
        let old = fs::metadata(&dest).map(|m| m.len()).ok();
        if old == Some(size) && same_bytes(&src, &dest) {
            continue;
        }
        if total - old.unwrap_or(0).min(total) + size > spec.max_total_bytes {
            report.skipped += 1;
            continue;
        }
        write_atomic(&src, &dest, false)?;
        total = total - old.unwrap_or(0).min(total) + size;
        report.saved += 1;
    }
    Ok(Some(report))
}

/// Copies kept files that are absent from the live tree at `tree` back into
/// it, never replacing a file (or anything else) already there, and writes a
/// `restore-kept` op line with the count when it restored any. Returns the
/// count, or `None` when the tree is locked.
pub fn restore(home: &BaleHome, id: &str, tree: &Path, spec: &KeepSpec) -> Result<Option<usize>> {
    let src_root = keep_dir(home, id);
    if !src_root.is_dir() || !tree.is_dir() {
        return Ok(Some(0));
    }
    let mut missing: Vec<PathBuf> = Vec::new();
    for rel in spec.paths.iter().filter_map(|p| clean(p)) {
        for (file, _) in files_under(&src_root, &rel) {
            if fs::symlink_metadata(tree.join(&file)).is_err() && !through_symlink(tree, &file) {
                missing.push(file);
            }
        }
    }
    if missing.is_empty() {
        return Ok(Some(0));
    }
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return Ok(None);
    };
    let count = crate::trace::op(home, "restore-kept", Some(id), || {
        let mut n = 0usize;
        for file in &missing {
            if write_atomic(&src_root.join(file), &tree.join(file), true)? {
                n += 1;
            }
        }
        Ok(Restored(n))
    })?;
    Ok(Some(count.0))
}

/// The result of one `restore-kept` op.
pub(crate) struct Restored(pub usize);

impl crate::trace::Traced for Restored {
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>) {
        ("ok", None)
    }
    fn traced_fields(&self) -> Map<String, serde_json::Value> {
        let mut m = Map::new();
        m.insert("files".into(), self.0.into());
        m
    }
}

/// Removes keep stores of trees with no record that nothing wrote to for
/// `max_age`. A tree deleted and made again keeps its id, so a store is held
/// a while after its record goes. Best-effort.
pub fn prune_orphans(home: &BaleHome, live_ids: &[String], max_age: Duration) {
    let Ok(entries) = fs::read_dir(home.0.join("keep")) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if live_ids.contains(&name) {
            continue;
        }
        let newest = WalkDir::new(e.path())
            .into_iter()
            .flatten()
            .filter_map(|x| x.metadata().ok()?.modified().ok())
            .max();
        let old = newest.is_none_or(|t| now.duration_since(t).is_ok_and(|d| d > max_age));
        if old {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}
