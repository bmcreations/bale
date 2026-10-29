//! Moves a tree's large build-output files into the store and leaves each as
//! an empty stub at the same inode.
//!
//! The stub keeps its inode because Xcode relinks anything whose inode
//! changed (about 45 s on a large app), so a stashed path is truncated in
//! place and never deleted, renamed over or cloned into. Nothing is truncated
//! until its blob and the manifest are durable and the record points at the
//! manifest; a crash before that leaves full files, and a crash after it is
//! finished by the next `stash`.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use walkdir::WalkDir;

use crate::busy::busy_holders_many;
use crate::config::{load_config, Config};
use crate::detect::{detect, stash_dirs};
use crate::lock::WorkspaceLock;
use crate::par::{par_map, ByteBudget};
use crate::registry::{Level, Record, Registry, StashState};
use crate::store::index::{stat_of, Index, IndexEntry};
use crate::store::manifest::{Entry, StashManifest};
use crate::store::{sha256_file, Hash, Store};
use crate::BaleHome;

/// Memory the parallel `put`s may hold for patch bases at once.
const BASE_BUDGET: u64 = 2 << 30;

#[derive(Debug, Clone, PartialEq)]
pub struct StashReport {
    /// Files truncated to stubs by this run.
    pub files: usize,
    /// Their bytes before truncation.
    pub bytes: u64,
    /// Compressed bytes this run added to the store.
    pub stored_bytes: u64,
    /// Files whose blob was already in the store.
    pub reused: usize,
    pub secs: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StashOutcome {
    Stashed(StashReport),
    Skipped(String),
}

/// Test hooks for [`stash_with`].
#[doc(hidden)]
#[derive(Default)]
pub struct StashOpts<'a> {
    /// Fail once the record points at the new manifest, before any file is
    /// truncated (a crash at that point).
    pub fail_before_truncate: bool,
    /// Called at the same point, just before truncation starts.
    pub before_truncate: Option<&'a dyn Fn()>,
}

/// Stashes the large files in the tree's build-output dirs (see
/// [`stash_dirs`]). Processes in `ignore_pids` do not count as holding the
/// tree. A stash left half done by a crash is finished instead.
pub fn stash(home: &BaleHome, id: &str, ignore_pids: &[u32]) -> Result<StashOutcome> {
    stash_with(home, id, ignore_pids, &StashOpts::default())
}

#[doc(hidden)]
pub fn stash_with(
    home: &BaleHome,
    id: &str,
    ignore_pids: &[u32],
    opts: &StashOpts,
) -> Result<StashOutcome> {
    let start = Instant::now();
    let skip = |s: &str| Ok(StashOutcome::Skipped(s.to_string()));
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return skip("locked");
    };
    let registry = Registry::open(home.clone());
    let Some(record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    if !matches!(record.level, Level::Hot | Level::Baled) {
        return skip(record.level.name());
    }
    // Rerunning over an interrupted unstash would truncate files it already
    // wrote back; unstash must finish first.
    if record.stash.as_ref().is_some_and(|s| s.restoring) {
        return skip("unstash in progress");
    }
    let root = crate::paths::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    if let Err(e) = super::disk::ensure_checkout(&root, &record.admin_dir) {
        return skip(&format!(
            "{} is not the worktree's checkout: {e:#}",
            root.display()
        ));
    }
    let cfg = load_config(home, Some(&root))?;
    let dirs = stash_dirs(&root, &detect(&root), &cfg);
    if let Some(busy) = busy(&dirs, ignore_pids)? {
        return skip(&busy);
    }

    let store = Store::open(home)?;
    let mut run = Run {
        store: &store,
        registry: &registry,
        record,
        root,
        dirs,
        ignore_pids,
        start,
    };
    match run.record.stash.clone() {
        Some(s) if !s.restored => run.resume(&s.commitment),
        prev => run.fresh(id, &cfg, prev, opts),
    }
}

/// `"busy: cmd (pid), ..."` when a process other than those in `ignore`
/// has its cwd or an open file under `dirs`.
fn busy(dirs: &[PathBuf], ignore: &[u32]) -> Result<Option<String>> {
    let holders: Vec<String> = busy_holders_many(dirs, ignore)?
        .into_iter()
        .flatten()
        .map(|h| format!("{} ({})", h.command, h.pid))
        .collect();
    Ok((!holders.is_empty()).then(|| format!("busy: {}", holders.join(", "))))
}

struct Run<'a> {
    store: &'a Store,
    registry: &'a Registry,
    record: Record,
    root: PathBuf,
    dirs: Vec<PathBuf>,
    ignore_pids: &'a [u32],
    start: Instant,
}

/// A file picked for stashing, with the stat it was hashed at.
struct Candidate {
    rel: PathBuf,
    hash: Hash,
    size: u64,
    mtime_ns: i128,
    ino: u64,
    mode: u32,
}

impl Candidate {
    fn stat(&self) -> (u64, i128, u64) {
        (self.size, self.mtime_ns, self.ino)
    }
}

/// What truncation did with the manifest's entries.
struct Truncated {
    files: usize,
    bytes: u64,
    /// Entries left as they were because the file changed since it was
    /// hashed; they come out of the manifest.
    changed: Vec<usize>,
}

impl Run<'_> {
    /// A stash from full files: hash, store, commit, then truncate.
    fn fresh(
        &mut self,
        id: &str,
        cfg: &Config,
        prev: Option<StashState>,
        opts: &StashOpts,
    ) -> Result<StashOutcome> {
        let rels = candidates(&self.root, &self.dirs, cfg.stash_min_bytes);
        if rels.is_empty() {
            return Ok(StashOutcome::Skipped("nothing to stash".into()));
        }

        // The restored manifest's hash for a path is the patch base for it.
        let prev = prev.filter(|s| s.restored);
        let bases: HashMap<PathBuf, Hash> = prev
            .as_ref()
            .and_then(|s| self.store.read_manifest(&s.commitment).ok())
            .map(|m| m.entries.into_iter().map(|e| (e.rel, e.hash)).collect())
            .unwrap_or_default();

        let index = Mutex::new(Index::load(self.store, id)?);
        let mut found: Vec<Candidate> = par_map(&rels, |rel| hash_one(&self.root, rel, &index))
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let mut index = index.into_inner().unwrap_or_else(|e| e.into_inner());
        let keep: BTreeSet<PathBuf> = rels.iter().cloned().collect();
        index.forget_missing(&keep);
        index.save(self.store, id)?;

        // One put per distinct content, the first file holding it as source.
        let mut sources: Vec<(Hash, PathBuf)> = Vec::new();
        let mut seen = BTreeSet::new();
        for c in &found {
            if seen.insert(c.hash.clone()) {
                sources.push((c.hash.clone(), c.rel.clone()));
            }
        }
        let present: BTreeSet<Hash> = sources
            .iter()
            .filter(|(h, _)| self.store.has(h))
            .map(|(h, _)| h.clone())
            .collect();
        let stats: HashMap<&PathBuf, (u64, i128, u64)> =
            found.iter().map(|c| (&c.rel, c.stat())).collect();
        let budget = ByteBudget::new(BASE_BUDGET);
        let puts = par_map(&sources, |(hash, rel)| -> Result<Option<u64>> {
            if present.contains(hash) {
                return Ok(Some(0));
            }
            let base = bases.get(rel).filter(|b| *b != hash);
            let hold = base
                .filter(|b| self.store.has(b))
                .and_then(|b| self.store.meta(b).ok())
                .map_or(0, |m| 2 * m.size);
            let full = self.root.join(rel);
            let meta = {
                let _held = budget.acquire(hold);
                self.store.put(&full, hash, base, cfg.patch_max_depth)?
            };
            // `put` trusts the hash it is given; a file that changed while
            // it was read left a blob whose bytes may not match its name.
            let now = fs::symlink_metadata(&full).map(|m| stat_of(&m)).ok();
            if now != Some(stats[rel]) {
                self.store.remove(hash)?;
                return Ok(None);
            }
            Ok(Some(meta.stored))
        });
        let mut stored_bytes = 0;
        let mut dropped = BTreeSet::new();
        for ((hash, _), put) in sources.iter().zip(puts) {
            match put? {
                Some(n) => stored_bytes += n,
                None => {
                    dropped.insert(hash.clone());
                }
            }
        }
        found.retain(|c| !dropped.contains(&c.hash));
        if found.is_empty() {
            return Ok(StashOutcome::Skipped("nothing to stash".into()));
        }
        let reused = found.iter().filter(|c| present.contains(&c.hash)).count();

        let manifest = StashManifest {
            tree_id: id.to_string(),
            created_unix: now_unix(),
            entries: found
                .into_iter()
                .map(|c| Entry {
                    rel: c.rel,
                    hash: c.hash,
                    size: c.size,
                    mode: c.mode,
                    mtime_ns: c.mtime_ns,
                    ino: c.ino,
                })
                .collect(),
        };
        let commitment = self.store.write_manifest(&manifest)?;
        self.record.stash = Some(StashState {
            commitment: commitment.clone(),
            at_unix: now_unix(),
            restored: false,
            restoring: false,
        });
        self.registry.put(&self.record)?;
        // Nothing points at the restored manifest any more; its blobs stay
        // until GC decides.
        if let Some(p) = prev.filter(|p| p.commitment != commitment) {
            self.store.remove_manifest(&p.commitment)?;
        }

        if opts.fail_before_truncate {
            bail!("stash: injected failure before truncation");
        }
        if let Some(f) = opts.before_truncate {
            f();
        }
        // Hashing and storing can take minutes; a build may have started
        // since the first check. The record already points at the manifest,
        // so the next `stash` finishes the job.
        if let Some(busy) = busy(&self.dirs, self.ignore_pids)? {
            return Ok(StashOutcome::Skipped(busy));
        }
        let done = self.truncate(manifest, commitment)?;
        Ok(StashOutcome::Stashed(StashReport {
            files: done.files,
            bytes: done.bytes,
            stored_bytes,
            reused,
            secs: self.start.elapsed().as_secs_f64(),
        }))
    }

    /// Finishes a stash the record already points at: truncates the entries
    /// still full size and unchanged. Files added to the tree since are left
    /// for the next stash.
    fn resume(&mut self, commitment: &Hash) -> Result<StashOutcome> {
        let manifest = self.store.read_manifest(commitment)?;
        // Checked again right before cutting, as in `fresh`.
        if let Some(busy) = busy(&self.dirs, self.ignore_pids)? {
            return Ok(StashOutcome::Skipped(busy));
        }
        let done = self.truncate(manifest, commitment.clone())?;
        if done.files == 0 {
            return Ok(StashOutcome::Skipped("already stashed".into()));
        }
        Ok(StashOutcome::Stashed(StashReport {
            files: done.files,
            bytes: done.bytes,
            stored_bytes: 0,
            reused: 0,
            secs: self.start.elapsed().as_secs_f64(),
        }))
    }

    /// Truncates each entry's file that is still as it was hashed, then
    /// drops the changed ones from the manifest: new manifest, record moved
    /// to it, old manifest removed, in that order, so the record never
    /// points at a missing manifest.
    fn truncate(&mut self, manifest: StashManifest, commitment: Hash) -> Result<Truncated> {
        let mut done = Truncated {
            files: 0,
            bytes: 0,
            changed: Vec::new(),
        };
        for (i, e) in manifest.entries.iter().enumerate() {
            match stub(&self.root.join(&e.rel), e) {
                Stub::Truncated => {
                    done.files += 1;
                    done.bytes += e.size;
                }
                Stub::Already => {}
                Stub::Changed => done.changed.push(i),
            }
        }
        if done.changed.is_empty() {
            return Ok(done);
        }

        let mut kept = manifest;
        let mut i = 0;
        kept.entries.retain(|_| {
            let keep = !done.changed.contains(&i);
            i += 1;
            keep
        });
        if kept.entries.is_empty() {
            self.record.stash = None;
            self.registry.put(&self.record)?;
        } else {
            let c = self.store.write_manifest(&kept)?;
            if let Some(s) = self.record.stash.as_mut() {
                s.commitment = c.clone();
            }
            self.registry.put(&self.record)?;
            if c == commitment {
                return Ok(done);
            }
        }
        self.store.remove_manifest(&commitment)?;
        Ok(done)
    }
}

/// Regular files with a single link, at least `min` bytes, under `dirs`,
/// as paths relative to `root`. Paths that are not valid UTF-8 are left out:
/// the manifest can't store them.
fn candidates(root: &Path, dirs: &[PathBuf], min: u64) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        for entry in WalkDir::new(dir).follow_links(false).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() < min || nlink(&meta) != 1 {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            if rel.to_str().is_some() {
                out.push(rel.to_path_buf());
            }
        }
    }
    out.sort();
    out
}

/// Hashes `root/rel` through the shared index, hashing outside the lock. A
/// file that is gone, unreadable, no longer a single-link regular file, or
/// changed while it was read is left out.
fn hash_one(root: &Path, rel: &Path, index: &Mutex<Index>) -> Result<Option<Candidate>> {
    let full = root.join(rel);
    let Ok(before) = fs::symlink_metadata(&full) else {
        return Ok(None);
    };
    if !before.file_type().is_file() || nlink(&before) != 1 {
        return Ok(None);
    }
    let (size, mtime_ns, ino) = stat_of(&before);
    let lock = || index.lock().unwrap_or_else(|e| e.into_inner());
    let cached = lock()
        .get(rel)
        .filter(|e| e.size == size && e.mtime_ns == mtime_ns && e.ino == ino)
        .map(|e| e.hash.clone());
    let hash = match cached {
        Some(h) => h,
        None => {
            let Ok(h) = sha256_file(&full) else {
                return Ok(None);
            };
            match fs::symlink_metadata(&full) {
                Ok(after) if stat_of(&after) == (size, mtime_ns, ino) => {}
                _ => return Ok(None),
            }
            lock().insert(
                rel.to_path_buf(),
                IndexEntry {
                    size,
                    mtime_ns,
                    ino,
                    hash: h.clone(),
                },
            );
            h
        }
    };
    Ok(Some(Candidate {
        rel: rel.to_path_buf(),
        hash,
        size,
        mtime_ns,
        ino,
        mode: mode_of(&before),
    }))
}

enum Stub {
    Truncated,
    /// Already a stub at the entry's inode.
    Already,
    /// Not as it was hashed (or gone, or could not be truncated); left alone.
    Changed,
}

/// Truncates `path` to 0 bytes in place if it still has the entry's size,
/// mtime and inode. Never creates, removes or renames: the inode stays.
fn stub(path: &Path, e: &Entry) -> Stub {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Stub::Changed;
    };
    if !meta.file_type().is_file() {
        return Stub::Changed;
    }
    let (size, mtime_ns, ino) = stat_of(&meta);
    if size == 0 && ino == e.ino {
        return Stub::Already;
    }
    if (size, mtime_ns, ino) != (e.size, e.mtime_ns, e.ino) {
        return Stub::Changed;
    }
    let mode = mode_of(&meta);
    let read_only = !owner_writable(&meta);
    if read_only && set_mode(path, mode, true).is_err() {
        return Stub::Changed;
    }
    let cut = OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| {
            // Same handle, same inode: check once more, then cut.
            let now = stat_of(&f.metadata()?);
            if now != (e.size, e.mtime_ns, e.ino) {
                return Ok(false);
            }
            f.set_len(0)?;
            Ok(true)
        })
        .unwrap_or(false);
    if cut {
        // The entry's mode, which also undoes a write bit added by a run
        // that crashed before it could restore it.
        if read_only || (mode & 0o7777) != (e.mode & 0o7777) {
            let _ = set_mode(path, e.mode, false);
        }
    } else if read_only {
        let _ = set_mode(path, mode, false);
    }
    if cut {
        Stub::Truncated
    } else {
        Stub::Changed
    }
}

/// Sets `mode`'s permission bits, plus the owner write bit if `writable`.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32, writable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let bits = mode & 0o7777;
    let bits = if writable { bits | 0o200 } else { bits };
    fs::set_permissions(path, fs::Permissions::from_mode(bits))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32, writable: bool) -> std::io::Result<()> {
    let mut p = fs::metadata(path)?.permissions();
    p.set_readonly(!writable && mode & 0o200 == 0);
    fs::set_permissions(path, p)
}

#[cfg(unix)]
fn owner_writable(meta: &fs::Metadata) -> bool {
    mode_of(meta) & 0o200 != 0
}

#[cfg(not(unix))]
fn owner_writable(meta: &fs::Metadata) -> bool {
    !meta.permissions().readonly()
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn mode_of(meta: &fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
fn nlink(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(not(unix))]
fn nlink(_meta: &fs::Metadata) -> u64 {
    1
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
