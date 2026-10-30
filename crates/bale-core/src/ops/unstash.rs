//! Writes a stashed tree's files back from the store.
//!
//! Each stub is refilled in place, at its own inode, and gets its original
//! mode and nanosecond mtime back, so Xcode and Gradle see the file they
//! built and do not relink it. A file that a build rewrote while the tree was
//! stashed is kept. The record says `restoring` from before the first write
//! until the last one is done, so a run that stops part way is finished by
//! the next one.
//!
//! On macOS a copy under a DerivedData `Build/Products/` dir whose twin
//! elsewhere in the manifest was just restored is cloned from that twin
//! (APFS `clonefile`) instead of decoded and written again. The clone is a new
//! inode, which Xcode accepts for a final product; everything else keeps its
//! inode.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use filetime::{set_file_mtime, FileTime};

use super::stash::{mode_of, owned_roots, owner_writable, set_mode};
use crate::lock::WorkspaceLock;
use crate::par::{par_map, ByteBudget};
use crate::registry::{Level, Record, Registry};
use crate::store::index::stat_of;
use crate::store::manifest::{Entry, StashManifest};
use crate::store::{sha256_file, Store};
use crate::BaleHome;

/// Memory the parallel restores may hold for patch bases at once.
const BASE_BUDGET: u64 = 2 << 30;
/// How long to wait for a stash or sweep holding the tree's lock.
const LOCK_WAIT: Duration = Duration::from_secs(60);
const LOCK_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq)]
pub struct UnstashReport {
    /// Files this run wrote back (recreated ones included).
    pub files: usize,
    /// Their bytes.
    pub bytes: u64,
    /// Files left alone because a build replaced or rewrote them while the
    /// tree was stashed.
    pub kept_newer: usize,
    /// Files that were gone and were created again; they have new inodes,
    /// so the next build relinks them.
    pub recreated: usize,
    /// Of `files`, those restored as clones of an identical restored file
    /// rather than written from the store; `cloned_bytes` is their size.
    pub cloned_files: usize,
    pub cloned_bytes: u64,
    pub secs: f64,
}

/// Test hooks for [`unstash_with`].
#[doc(hidden)]
#[derive(Default)]
pub struct UnstashOpts {
    /// Stop with an error while writing the manifest entry at this index,
    /// leaving that file half written and later entries untouched (a crash
    /// mid-restore). Earlier entries are restored normally.
    pub fail_after: Option<usize>,
    /// How long to wait for the tree's lock; defaults to 60 s.
    pub lock_wait: Option<Duration>,
}

/// Restores the tree's stashed files. `None` when the tree is not stashed
/// (or already restored), which costs one lock attempt and one record read; a
/// held lock is waited for (up to 60 s), since a stash may be under way.
pub fn unstash(home: &BaleHome, id: &str) -> Result<Option<UnstashReport>> {
    unstash_with(home, id, &UnstashOpts::default())
}

/// [`unstash`] for the tree that contains `cwd` (the registered tree with
/// the longest path prefix). `None` when `cwd` is in no registered tree.
pub fn unstash_path(home: &BaleHome, cwd: &Path) -> Result<Option<UnstashReport>> {
    match tree_containing(home, cwd)? {
        Some((_, id)) => unstash(home, &id),
        None => Ok(None),
    }
}

/// The registered tree that contains `cwd` (the longest path prefix), as its
/// canonical path and id. `None` when `cwd` is in no registered tree.
pub fn tree_containing(home: &BaleHome, cwd: &Path) -> Result<Option<(PathBuf, String)>> {
    let cwd =
        crate::paths::canonicalize(cwd).with_context(|| format!("resolving {}", cwd.display()))?;
    let registry = Registry::open(home.clone());
    let mut best: Option<(PathBuf, String)> = None;
    // One unreadable record must not stop every prompt from restoring.
    for (_, rec) in registry.all_lenient()? {
        let Ok(rec) = rec else { continue };
        let Ok(path) = crate::paths::canonicalize(&rec.path) else {
            continue;
        };
        let longer = best
            .as_ref()
            .is_none_or(|(p, _)| path.components().count() > p.components().count());
        if cwd.starts_with(&path) && longer {
            best = Some((path, rec.id));
        }
    }
    Ok(best)
}

#[doc(hidden)]
pub fn unstash_with(
    home: &BaleHome,
    id: &str,
    opts: &UnstashOpts,
) -> Result<Option<UnstashReport>> {
    let start = Instant::now();
    let registry = Registry::open(home.clone());
    // The lock comes before the record read: a stash holds it while hashing,
    // before it moves the record, so a record read without it can say
    // "nothing to do" just before the tree is stubbed.
    let _lock = match WorkspaceLock::try_acquire(home, id)? {
        Some(lock) => lock,
        // Ask the holder to stop and wait for it, since the prompt is about
        // to use the tree.
        None => wait_for_lock(home, id, opts.lock_wait.unwrap_or(LOCK_WAIT))?,
    };
    let Some(mut record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    // A tree that went cold or is going there has no build dirs to refill.
    if !matches!(record.level, Level::Hot | Level::Baled) {
        return Ok(None);
    }
    let Some(state) = record.stash.clone().filter(|s| !s.restored) else {
        return Ok(None);
    };
    let was_restoring = state.restoring;

    let root = crate::paths::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    super::disk::ensure_checkout(&root, &record.admin_dir)
        .with_context(|| format!("{} is not the worktree's checkout", root.display()))?;
    let store = Store::open(home)?;
    let mut manifest = store.read_manifest(&state.commitment)?;

    // A folder outside the tree is written to only while it is still a
    // DerivedData folder built from this tree; the rest of its entries are
    // lost (the build regenerates them).
    let owned = owned_roots(&manifest, &root);
    // The roots alone, to resolve paths while `manifest.entries` changes.
    let layout = StashManifest {
        tree_id: String::new(),
        created_unix: 0,
        roots: manifest.roots.clone(),
        entries: Vec::new(),
    };
    let mut pruned = 0;
    if !was_restoring {
        // Once `restoring` is set, a non-empty file at an entry's inode reads
        // as half written and gets overwritten. A file a build rewrote in
        // place while the tree was stashed looks the same, so it leaves the
        // manifest first, in the same record write that sets `restoring`.
        // So does a file a stash committed but never cut (it stopped before
        // truncating); that one is not counted as newer.
        manifest
            .entries
            .retain(|e| match look(&layout.path_of(e, &root), e) {
                // Left for `restore` to report if its folder moved.
                _ if !owned[usize::from(e.root)] => true,
                Look::Stub | Look::Missing => true,
                Look::NeverStubbed => false,
                Look::Rewritten => {
                    pruned += 1;
                    false
                }
            });
        if let Some(s) = record.stash.as_mut() {
            s.restoring = true;
        }
        commit(&store, &registry, &mut record, &manifest)?;
    }

    let budget = ByteBudget::new(BASE_BUDGET);
    let twins = clone_twins(&manifest.entries, &owned);
    let run = |&i: &usize| {
        let e = &manifest.entries[i];
        let fail = match opts.fail_after {
            Some(n) if i > n => return Ok(Outcome::Done),
            Some(n) => i == n,
            None => false,
        };
        let job = Job {
            store: &store,
            root: &root,
            layout: &layout,
            owned: &owned,
            budget: &budget,
            was_restoring,
            fail,
        };
        let sources: Vec<&Entry> = twins[i].iter().map(|&t| &manifest.entries[t]).collect();
        job.restore(e, &sources)
            .with_context(|| format!("restoring {}", layout.path_of(e, &root).display()))
    };
    // Sources first: a clone needs its twin on disk.
    let (clones, plain): (Vec<usize>, Vec<usize>) =
        (0..manifest.entries.len()).partition(|&i| !twins[i].is_empty());
    let mut results: Vec<Option<Result<Outcome>>> =
        (0..manifest.entries.len()).map(|_| None).collect();
    for batch in [plain, clones] {
        let done = par_map(&batch, run);
        for (i, r) in batch.into_iter().zip(done) {
            results[i] = Some(r);
        }
    }

    let mut report = UnstashReport {
        files: 0,
        bytes: 0,
        kept_newer: pruned,
        recreated: 0,
        cloned_files: 0,
        cloned_bytes: 0,
        secs: 0.0,
    };
    let mut errors = Vec::new();
    let mut lost = Vec::new();
    for (i, r) in results.into_iter().enumerate() {
        let e = &manifest.entries[i];
        match r.expect("every entry was restored") {
            Ok(Outcome::Written { recreated, cloned }) => {
                report.files += 1;
                report.bytes += e.size;
                report.recreated += usize::from(recreated);
                if cloned {
                    report.cloned_files += 1;
                    report.cloned_bytes += e.size;
                }
            }
            Ok(Outcome::KeptNewer) => report.kept_newer += 1,
            Ok(Outcome::Done) => {}
            Ok(Outcome::Lost(err)) => lost.push((i, err)),
            Err(err) => errors.push(err),
        }
    }

    // A file whose blob is gone or corrupt can never be restored; retrying
    // it on every prompt would keep the tree `restoring` and overwrite what
    // the build writes meanwhile. It stays a stub (the build regenerates it)
    // and leaves the manifest.
    let lost_paths: Vec<String> = lost
        .iter()
        .map(|(i, err)| {
            let e = &manifest.entries[*i];
            let shown = if e.root == 0 {
                e.rel.clone()
            } else {
                layout.path_of(e, &root)
            };
            format!("{}: {err:#}", shown.display())
        })
        .collect();
    if !lost.is_empty() {
        let gone: BTreeSet<usize> = lost.iter().map(|(i, _)| *i).collect();
        let mut i = 0;
        manifest.entries.retain(|_| {
            i += 1;
            !gone.contains(&(i - 1))
        });
    }
    if let Some(first) = errors.into_iter().next() {
        // `restoring` stays set; the next run finishes the job.
        if !lost.is_empty() {
            commit(&store, &registry, &mut record, &manifest)?;
        }
        return Err(first);
    }

    if let Some(s) = record.stash.as_mut() {
        s.restored = true;
        s.restoring = false;
    }
    commit(&store, &registry, &mut record, &manifest)?;
    if !lost_paths.is_empty() {
        bail!(
            "{} stashed file(s) could not be restored and were left empty for the \
             build to regenerate: {}",
            lost_paths.len(),
            lost_paths.join("; ")
        );
    }
    report.secs = start.elapsed().as_secs_f64();
    let mut f = serde_json::Map::new();
    f.insert("id".into(), id.into());
    f.insert("files".into(), report.files.into());
    f.insert("bytes".into(), report.bytes.into());
    f.insert("cloned_files".into(), report.cloned_files.into());
    f.insert("cloned_bytes".into(), report.cloned_bytes.into());
    f.insert("kept_newer".into(), report.kept_newer.into());
    f.insert("secs".into(), report.secs.into());
    crate::trace::emit(home, "unstash", f);
    Ok(Some(report))
}

/// Points the record at `manifest` (written first if it differs from the
/// record's), saves the record, then removes the manifest it pointed at
/// before, so the record never points at a missing manifest.
fn commit(
    store: &Store,
    registry: &Registry,
    record: &mut Record,
    manifest: &StashManifest,
) -> Result<()> {
    let Some(s) = record.stash.as_mut() else {
        return registry.put(record);
    };
    let old = s.commitment.clone();
    let new = manifest.commitment();
    if new != old {
        store.write_manifest(manifest)?;
        s.commitment = new.clone();
    }
    registry.put(record)?;
    if new != old {
        store.remove_manifest(&old)?;
    }
    Ok(())
}

/// Where a failed background restore of tree `id` leaves its error text:
/// next to the registry, not in the tree.
pub fn failure_marker_path(home: &BaleHome, id: &str) -> PathBuf {
    home.0.join("restore-failed").join(format!("{id}.txt"))
}

/// Records why a background restore of `id` failed, for the next prompt or
/// tool hook to show (see [`take_failure_marker`]). An earlier marker the
/// user has not seen yet is kept above the new text. Written to a temp file
/// and renamed, so a reader never sees half of it.
pub fn write_failure_marker(home: &BaleHome, id: &str, text: &str) -> Result<()> {
    let path = failure_marker_path(home, id);
    let dir = path.parent().expect("marker path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut body = fs::read_to_string(&path).unwrap_or_default();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(text);
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let tmp = dir.join(format!("{id}.txt.tmp{}", std::process::id()));
    fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))
}

/// The failure text a background restore of `id` left, removed so it is shown
/// once. `None` when there is none (one `stat`, so cheap on every hook).
pub fn take_failure_marker(home: &BaleHome, id: &str) -> Option<String> {
    let path = failure_marker_path(home, id);
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    Some(text)
}

/// Whether `id` has a stash still to restore, or another process holds its
/// lock (a stash may be under way and about to move the record). Reads the
/// record only: no lock is taken and the store is not opened.
pub fn needs_restore(home: &BaleHome, id: &str) -> Result<bool> {
    if let Some(record) = Registry::open(home.clone()).get(id)? {
        let stashed = record.stash.as_ref().is_some_and(|s| !s.restored);
        if stashed && matches!(record.level, Level::Hot | Level::Baled) {
            return Ok(true);
        }
    }
    WorkspaceLock::is_held(home, id)
}

/// The tree's lock stayed held (by a stash or sweep) for the whole wait.
#[derive(Debug)]
pub struct LockTimeout {
    pub id: String,
    pub waited: Duration,
    pub lock_file: PathBuf,
}

impl std::fmt::Display for LockTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "workspace {} is still locked after {}s by another bale process \
             (a stash or sweep); lock file {}",
            self.id,
            self.waited.as_secs(),
            self.lock_file.display()
        )
    }
}

impl std::error::Error for LockTimeout {}

fn wait_for_lock(home: &BaleHome, id: &str, wait: Duration) -> Result<WorkspaceLock> {
    let deadline = Instant::now() + wait;
    loop {
        // Asked again each time: a stash that took the lock after an earlier
        // ask would not see it.
        WorkspaceLock::request(home, id)?;
        if let Some(lock) = WorkspaceLock::try_acquire(home, id)? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            return Err(LockTimeout {
                id: id.to_string(),
                waited: wait,
                lock_file: home.locks_dir().join(format!("{id}.lock")),
            }
            .into());
        }
        std::thread::sleep(LOCK_POLL);
    }
}

enum Outcome {
    Written {
        recreated: bool,
        /// Cloned from a restored twin instead of written from the store.
        cloned: bool,
    },
    KeptNewer,
    /// Already restored (by an earlier run), or not reached.
    Done,
    /// The blob is missing or corrupt; the file was left empty (or still
    /// missing) for the build to regenerate.
    Lost(anyhow::Error),
}

/// A failed [`Job::stream`]: `blob` when the store could not produce the
/// bytes, as opposed to the file not taking them.
struct StreamError {
    err: anyhow::Error,
    blob: bool,
}

struct Job<'a> {
    store: &'a Store,
    root: &'a Path,
    /// The manifest's roots, to find each entry's own.
    layout: &'a StashManifest,
    /// Which of those roots may be written to; see [`owned_roots`].
    owned: &'a [bool],
    budget: &'a ByteBudget,
    /// An earlier run stopped mid-restore: a non-empty file at the entry's
    /// inode may be half written.
    was_restoring: bool,
    /// Test hook: write half the entry, then fail.
    fail: bool,
}

impl Job<'_> {
    /// `twins` are identical entries that may be cloned from once restored;
    /// empty for everything that is written in place.
    fn restore(&self, e: &Entry, twins: &[&Entry]) -> Result<Outcome> {
        let path = self.layout.path_of(e, self.root);
        if !self.owned[usize::from(e.root)] {
            return Ok(Outcome::Lost(anyhow::anyhow!(
                "{} is no longer a DerivedData folder built from this tree",
                self.layout.root_of(e, self.root).display()
            )));
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return self.recreate(&path, e),
            Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
        };
        if !meta.file_type().is_file() {
            return Ok(Outcome::KeptNewer);
        }
        let (size, mtime_ns, ino) = stat_of(&meta);
        let finished = size == e.size && mtime_ns == e.mtime_ns;
        if ino != e.ino {
            // An earlier run recreates a missing file at a new inode and
            // gives it the entry's mtime only once its bytes are verified.
            if self.was_restoring && finished {
                fix_mode(&path, &meta, e)?;
                return Ok(Outcome::Done);
            }
            return Ok(Outcome::KeptNewer);
        }
        if size != 0 {
            if !self.was_restoring {
                // Written while stashed: a build's, not ours.
                return Ok(Outcome::KeptNewer);
            }
            // Half written, or written in full by a run that stopped
            // before setting mtime and mode (it sets mtime only after the
            // bytes are verified).
            if finished {
                fix_mode(&path, &meta, e)?;
                return Ok(Outcome::Done);
            }
            if size == e.size && sha256_file(&path)? == e.hash {
                set_mtime(&path, e.mtime_ns)?;
                fix_mode(&path, &meta, e)?;
                return Ok(Outcome::Done);
            }
        }
        if !self.fail && !twins.is_empty() && self.clone_from(&path, e, twins) {
            return Ok(Outcome::Written {
                recreated: false,
                cloned: true,
            });
        }
        self.refill(&path, &meta, e)
    }

    /// Replaces the stub at `path` with a clone of the first twin that is on
    /// disk as its entry says. A twin that failed to restore, or a build
    /// rewrote, does not match and is passed over. Returns whether the stub
    /// was replaced; on any failure the stub is untouched and the caller
    /// decodes the blob instead.
    fn clone_from(&self, path: &Path, e: &Entry, twins: &[&Entry]) -> bool {
        // Restored (or finished by an earlier run) means size and mtime are
        // the entry's: both are set only after the bytes were verified.
        let Some(twin) = twins.iter().find_map(|t| {
            if !self.owned[usize::from(t.root)] {
                return None;
            }
            let p = self.layout.path_of(t, self.root);
            let m = fs::symlink_metadata(&p).ok()?;
            let (size, mtime_ns, _) = stat_of(&m);
            (m.file_type().is_file() && size == t.size && mtime_ns == t.mtime_ns).then_some(p)
        }) else {
            return false;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        let tmp = parent.join(format!(".bale-clone-{}-{}", std::process::id(), e.ino));
        let _ = fs::remove_file(&tmp);
        let placed = crate::clones::reflink::clone_file(&twin, &tmp)
            .and_then(|()| set_mtime(&tmp, e.mtime_ns).map_err(io::Error::other))
            .and_then(|()| set_mode(&tmp, e.mode, false))
            .and_then(|()| fs::rename(&tmp, path));
        if placed.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        placed.is_ok()
    }

    /// Writes the blob into the existing file at the entry's inode.
    fn refill(&self, path: &Path, meta: &fs::Metadata, e: &Entry) -> Result<Outcome> {
        let read_only = !owner_writable(meta);
        if read_only {
            set_mode(path, mode_of(meta), true)
                .with_context(|| format!("making {} writable", path.display()))?;
        }
        let undo_mode = || {
            if read_only {
                let _ = set_mode(path, mode_of(meta), false);
            }
        };
        let file = match OpenOptions::new().write(true).open(path) {
            Ok(f) => f,
            Err(err) => {
                undo_mode();
                return Err(err).with_context(|| format!("open {}", path.display()));
            }
        };
        // Same handle, same inode: check once more before cutting. A stub a
        // build wrote into since the first look is the build's now.
        let (now_size, _, now_ino) = stat_of(&file.metadata()?);
        if now_ino != e.ino || (now_size != 0 && !self.was_restoring) {
            drop(file);
            undo_mode();
            return Ok(Outcome::KeptNewer);
        }
        let written = file
            .set_len(0)
            .map_err(|err| StreamError {
                err: err.into(),
                blob: false,
            })
            .and_then(|()| self.stream(&file, e));
        if let Err(StreamError { err, blob }) = written {
            if !self.fail {
                // Keep the inode; a stub is what the next run expects, and a
                // stub with a new mtime is what the build regenerates.
                let _ = file.set_len(0);
            }
            drop(file);
            let _ = set_mode(path, e.mode, false);
            return if blob {
                Ok(Outcome::Lost(err))
            } else {
                Err(err)
            };
        }
        drop(file);
        set_mtime(path, e.mtime_ns)?;
        set_mode(path, e.mode, false).with_context(|| format!("chmod {}", path.display()))?;
        Ok(Outcome::Written {
            recreated: false,
            cloned: false,
        })
    }

    /// Creates a missing file next to its path, fills and stamps it, then
    /// links it into place without replacing anything that appeared there
    /// meanwhile. A crash leaves only a temp file, so the path is still
    /// missing for the next run.
    fn recreate(&self, path: &Path, e: &Entry) -> Result<Outcome> {
        let parent = path
            .parent()
            .with_context(|| format!("{} has no parent", path.display()))?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        // A dir swapped for a symlink must not lead the write out of the tree.
        let real = crate::paths::canonicalize(parent)?;
        let own_root = self.layout.root_of(e, self.root);
        if !real.starts_with(own_root) {
            bail!("{} resolves outside the tree", parent.display());
        }
        let tmp = tempfile::Builder::new()
            .prefix(".bale-unstash-")
            .tempfile_in(parent)
            .with_context(|| format!("creating a temp file in {}", parent.display()))?;
        match self.stream(tmp.as_file(), e) {
            Ok(()) => {}
            // The temp file goes on drop; the path stays missing.
            Err(StreamError { err, blob: true }) => return Ok(Outcome::Lost(err)),
            Err(StreamError { err, blob: false }) => return Err(err),
        }
        set_mtime(tmp.path(), e.mtime_ns)?;
        set_mode(tmp.path(), e.mode, false)?;
        match tmp.persist_noclobber(path) {
            Ok(_) => Ok(Outcome::Written {
                recreated: true,
                cloned: false,
            }),
            Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => Ok(Outcome::KeptNewer),
            Err(err) => Err(err.error).with_context(|| format!("placing {}", path.display())),
        }
    }

    /// Streams the entry's blob into `file`, holding its patch base's share
    /// of the budget. `Store::read_to` verifies size and hash after writing.
    fn stream(&self, file: &File, e: &Entry) -> std::result::Result<(), StreamError> {
        let blob = |err: anyhow::Error| StreamError { err, blob: true };
        let meta = self.store.meta(&e.hash).map_err(blob)?;
        // Resolving a base below depth 0 holds about twice its size.
        let hold = match &meta.base {
            Some(b) => 2 * self.store.meta(b).map_err(blob)?.size,
            None => 0,
        };
        let _held = self.budget.acquire(hold);
        let mut out = BufWriter::with_capacity(
            1 << 20,
            Limit {
                inner: file,
                left: self.fail.then_some(e.size / 2),
                failed: false,
            },
        );
        let res = self
            .store
            .read_to(&e.hash, &mut out)
            .and_then(|_| out.flush().map_err(anyhow::Error::from));
        // Any error the file gave is the file's; any other is the blob's.
        let file_failed = out.get_ref().failed;
        res.map_err(|err| StreamError {
            err,
            blob: !file_failed,
        })
    }
}

/// For each entry, the entries it may be cloned from: those with its hash
/// that are not themselves cloned. Empty unless the entry is a build product
/// (see [`is_product`]) in a root bale may write to, on macOS, and has such a
/// twin; empty means the entry is written in place.
fn clone_twins(entries: &[Entry], owned: &[bool]) -> Vec<Vec<usize>> {
    let mut twins = vec![Vec::new(); entries.len()];
    if !cfg!(target_os = "macos") {
        return twins;
    }
    let is_clone = |e: &Entry| owned[usize::from(e.root)] && is_product(&e.rel);
    let mut by_hash: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.size > 0 {
            by_hash.entry(e.hash.as_str()).or_default().push(i);
        }
    }
    for group in by_hash.values() {
        let sources: Vec<usize> = group
            .iter()
            .copied()
            .filter(|&i| !is_clone(&entries[i]))
            .collect();
        for &i in group {
            if is_clone(&entries[i]) {
                twins[i] = sources.clone();
            }
        }
    }
    twins
}

/// Whether `rel` lies under a DerivedData `Build/Products` dir, where Xcode
/// accepts a final product at a new inode (it copies products from linked
/// intermediates instead of linking them).
fn is_product(rel: &Path) -> bool {
    let parts: Vec<_> = rel.components().collect();
    parts
        .windows(3)
        .any(|w| w[0].as_os_str() == "Build" && w[1].as_os_str() == "Products")
}

/// Passes writes through, noting whether the destination failed. With
/// `left` set it fails once that many bytes have gone through (test hook).
struct Limit<W> {
    inner: W,
    left: Option<u64>,
    failed: bool,
}

impl<W: Write> Limit<W> {
    fn note<T>(&mut self, r: io::Result<T>) -> io::Result<T> {
        self.failed |= r.is_err();
        r
    }
}

impl<W: Write> Write for Limit<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(left) = self.left else {
            let r = self.inner.write(buf);
            return self.note(r);
        };
        if left == 0 {
            let r = Err(io::Error::other("unstash: injected failure mid-file"));
            return self.note(r);
        }
        let n = buf.len().min(left as usize);
        let r = self.inner.write(&buf[..n]);
        let n = self.note(r)?;
        self.left = Some(left - n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let r = self.inner.flush();
        self.note(r)
    }
}

/// Sets the mtime from nanoseconds since the epoch, keeping atime.
fn set_mtime(path: &Path, mtime_ns: i128) -> Result<()> {
    let secs = mtime_ns.div_euclid(1_000_000_000) as i64;
    let nanos = mtime_ns.rem_euclid(1_000_000_000) as u32;
    set_file_mtime(path, FileTime::from_unix_time(secs, nanos))
        .with_context(|| format!("setting mtime of {}", path.display()))
}

/// Puts the entry's permission bits back if they differ.
fn fix_mode(path: &Path, meta: &fs::Metadata, e: &Entry) -> Result<()> {
    if mode_of(meta) & 0o7777 != e.mode & 0o7777 {
        set_mode(path, e.mode, false).with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
}

/// What is at an entry's path before a restore starts.
enum Look {
    /// The empty file stash left, at the entry's inode.
    Stub,
    /// Gone; it is recreated.
    Missing,
    /// Exactly as the entry recorded it: a stash committed the manifest but
    /// stopped before cutting this file.
    NeverStubbed,
    /// Replaced (a new inode, or not a regular file) or written into.
    Rewritten,
}

fn look(path: &Path, e: &Entry) -> Look {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_file() => Look::Rewritten,
        Ok(m) => match stat_of(&m) {
            (0, _, ino) if ino == e.ino => Look::Stub,
            stat if stat == (e.size, e.mtime_ns, e.ino) => Look::NeverStubbed,
            _ => Look::Rewritten,
        },
        Err(_) => Look::Missing,
    }
}
