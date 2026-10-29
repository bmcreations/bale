//! Garbage collection for the store: blobs and manifests no tree record
//! reaches, and the stat index of trees that no longer have a record.
//!
//! Why there is a grace period: a stash puts its blobs, then writes the
//! manifest, then updates the record. GC running between those steps sees
//! blobs and a manifest that no record points to yet. Anything touched in the
//! last [`GC_GRACE`] is kept, along with what it needs (the entries of a young
//! manifest, the bases of a young blob), and `Store::put` refreshes the mtime
//! of the blobs it reuses so an old blob a new stash depends on counts as young.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::blob::{Hash, Store};

/// Files younger than this are never collected.
pub const GC_GRACE: Duration = Duration::from_secs(60 * 60);

/// Test hooks for [`collect_with`].
#[doc(hidden)]
pub struct CollectOpts {
    pub grace: Duration,
}

impl Default for CollectOpts {
    fn default() -> Self {
        CollectOpts { grace: GC_GRACE }
    }
}

/// Removes blobs and manifests that no manifest in `live_manifests` reaches
/// and that are older than [`GC_GRACE`]; returns the bytes freed (or that
/// would be, with `dry_run`). Fails without removing anything when a live
/// manifest cannot be read.
pub fn collect(store: &Store, live_manifests: &[Hash], dry_run: bool) -> Result<u64> {
    collect_with(store, live_manifests, dry_run, &CollectOpts::default())
}

#[doc(hidden)]
pub fn collect_with(
    store: &Store,
    live_manifests: &[Hash],
    dry_run: bool,
    opts: &CollectOpts,
) -> Result<u64> {
    let now = SystemTime::now();
    let manifests = store.list_manifests()?;

    // Roots: live manifests (which must read), plus young manifests.
    let live: BTreeSet<&Hash> = live_manifests.iter().collect();
    let mut roots: BTreeSet<Hash> = BTreeSet::new();
    let mut doomed_manifests = Vec::new();
    let mut to_read: Vec<&Hash> = live_manifests.iter().collect();
    for c in &manifests {
        if live.contains(c) {
            continue;
        }
        if is_young(&store.manifest_path(c)?, now, opts.grace) {
            to_read.push(c);
        } else {
            doomed_manifests.push(c);
        }
    }
    for c in to_read {
        let m = store
            .read_manifest(c)
            .with_context(|| format!("cannot read manifest {c}; store left untouched"))?;
        roots.extend(m.entries.into_iter().map(|e| e.hash));
    }

    // Young blobs are roots too, so their bases survive with them.
    let blobs = store.list()?;
    for b in &blobs {
        if is_young(&store.json_path(&b.hash), now, opts.grace)
            || is_young(&store.zst_path(&b.hash), now, opts.grace)
        {
            roots.insert(b.hash.clone());
        }
    }
    let mut keep: BTreeSet<Hash> = BTreeSet::new();
    let mut stack: Vec<Hash> = roots.into_iter().collect();
    while let Some(h) = stack.pop() {
        if !keep.insert(h.clone()) {
            continue;
        }
        let meta = store
            .meta(&h)
            .with_context(|| format!("cannot read blob {h}; store left untouched"))?;
        stack.extend(meta.base);
    }

    let mut freed = 0;
    for b in blobs.iter().filter(|b| !keep.contains(&b.hash)) {
        // A stash may have reused this blob (or patched against it) since
        // the scan; `put` refreshes its mtime, so look again just before
        // removing. This narrows the race to the stat; a blob lost in it
        // restores as a stub the build regenerates.
        let now = SystemTime::now();
        if is_young(&store.json_path(&b.hash), now, opts.grace)
            || is_young(&store.zst_path(&b.hash), now, opts.grace)
        {
            continue;
        }
        freed += if dry_run {
            file_len(&store.json_path(&b.hash))? + file_len(&store.zst_path(&b.hash))?
        } else {
            store.remove(&b.hash)?
        };
    }
    for c in doomed_manifests {
        let path = store.manifest_path(c)?;
        freed += file_len(&path)?;
        if !dry_run {
            store.remove_manifest(c)?;
        }
    }
    Ok(freed)
}

/// Removes `index/<id>.json` for every id not in `live_ids`; returns the
/// bytes freed (or that would be, with `dry_run`).
pub fn collect_index(store: &Store, live_ids: &[String], dry_run: bool) -> Result<u64> {
    let dir = store.root.join("index");
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut freed = 0;
    for ent in entries {
        let path = ent?.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if live_ids.iter().any(|l| l == id) {
            continue;
        }
        freed += file_len(&path)?;
        if !dry_run {
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(freed)
}

fn file_len(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(m) => Ok(m.len()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

/// Modified within `grace` of `now`. A file whose mtime is in the future
/// counts as young; a file that cannot be stat-ed does not (it is gone).
fn is_young(path: &Path, now: SystemTime, grace: Duration) -> bool {
    if grace.is_zero() {
        return false;
    }
    match fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => now.duration_since(t).map_or(true, |age| age < grace),
        Err(_) => false,
    }
}
