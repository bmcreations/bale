//! Writes a stashed tree's files back from the store.
//!
//! Each stub is refilled in place, at its own inode, and gets its original
//! mode and nanosecond mtime back, so Xcode and Gradle see the file they
//! built and do not relink it. A file that a build rewrote while the tree was
//! stashed is kept. The record says `restoring` from before the first write
//! until the last one is done, so a run that stops part way is finished by
//! the next one.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use filetime::{set_file_mtime, FileTime};

use super::stash::{mode_of, owner_writable, set_mode};
use crate::lock::WorkspaceLock;
use crate::par::{par_map, ByteBudget};
use crate::registry::{Record, Registry};
use crate::store::index::stat_of;
use crate::store::manifest::Entry;
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
/// (or already restored), which costs one record read.
pub fn unstash(home: &BaleHome, id: &str) -> Result<Option<UnstashReport>> {
    unstash_with(home, id, &UnstashOpts::default())
}

/// [`unstash`] for the tree that contains `cwd` (the registered tree with
/// the longest path prefix). `None` when `cwd` is in no registered tree.
pub fn unstash_path(home: &BaleHome, cwd: &Path) -> Result<Option<UnstashReport>> {
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
    match best {
        Some((_, id)) => unstash(home, &id),
        None => Ok(None),
    }
}

fn has_work(rec: &Record) -> bool {
    rec.stash.as_ref().is_some_and(|s| !s.restored)
}

#[doc(hidden)]
pub fn unstash_with(
    home: &BaleHome,
    id: &str,
    opts: &UnstashOpts,
) -> Result<Option<UnstashReport>> {
    let start = Instant::now();
    let registry = Registry::open(home.clone());
    let Some(record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    if !has_work(&record) {
        return Ok(None);
    }

    // The prompt is waiting, so wait out a stash or sweep instead of
    // skipping.
    let _lock = wait_for_lock(home, id, opts.lock_wait.unwrap_or(LOCK_WAIT))?;
    // A restore may have finished, or a stash moved the record, meanwhile.
    let Some(mut record) = registry.get(id)? else {
        bail!("record for {id} disappeared");
    };
    if !has_work(&record) {
        return Ok(None);
    }
    let Some(state) = record.stash.clone() else {
        return Ok(None);
    };
    let was_restoring = state.restoring;

    let root = crate::paths::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    super::disk::ensure_checkout(&root, &record.admin_dir)
        .with_context(|| format!("{} is not the worktree's checkout", root.display()))?;
    let store = Store::open(home)?;
    let mut manifest = store.read_manifest(&state.commitment)?;

    let mut pruned = 0;
    if !was_restoring {
        // Once `restoring` is set, a non-empty file at an entry's inode reads
        // as half written and gets overwritten. A file a build rewrote in
        // place while the tree was stashed looks the same, so it leaves the
        // manifest first, in the same record write that sets `restoring`.
        let before = manifest.entries.len();
        manifest
            .entries
            .retain(|e| !rewritten_while_stashed(&root.join(&e.rel), e));
        pruned = before - manifest.entries.len();
        let old = state.commitment.clone();
        let commitment = if pruned > 0 {
            store.write_manifest(&manifest)?
        } else {
            old.clone()
        };
        if let Some(s) = record.stash.as_mut() {
            s.commitment = commitment.clone();
            s.restoring = true;
        }
        registry.put(&record)?;
        if commitment != old {
            store.remove_manifest(&old)?;
        }
    }

    let budget = ByteBudget::new(BASE_BUDGET);
    let indexed: Vec<(usize, &Entry)> = manifest.entries.iter().enumerate().collect();
    let results = par_map(&indexed, |&(i, e)| {
        let fail = match opts.fail_after {
            Some(n) if i > n => return Ok(Outcome::Done),
            Some(n) => i == n,
            None => false,
        };
        let job = Job {
            store: &store,
            root: &root,
            budget: &budget,
            was_restoring,
            fail,
        };
        job.restore(e)
            .with_context(|| format!("restoring {}", root.join(&e.rel).display()))
    });

    let mut report = UnstashReport {
        files: 0,
        bytes: 0,
        kept_newer: pruned,
        recreated: 0,
        secs: 0.0,
    };
    let mut errors = Vec::new();
    for (e, r) in manifest.entries.iter().zip(results) {
        match r {
            Ok(Outcome::Written { recreated }) => {
                report.files += 1;
                report.bytes += e.size;
                report.recreated += usize::from(recreated);
            }
            Ok(Outcome::KeptNewer) => report.kept_newer += 1,
            Ok(Outcome::Done) => {}
            Err(err) => errors.push(err),
        }
    }
    if let Some(first) = errors.into_iter().next() {
        // `restoring` stays set; the next run finishes the job.
        return Err(first);
    }

    if let Some(s) = record.stash.as_mut() {
        s.restored = true;
        s.restoring = false;
    }
    registry.put(&record)?;
    report.secs = start.elapsed().as_secs_f64();
    Ok(Some(report))
}

fn wait_for_lock(home: &BaleHome, id: &str, wait: Duration) -> Result<WorkspaceLock> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(lock) = WorkspaceLock::try_acquire(home, id)? {
            return Ok(lock);
        }
        if Instant::now() >= deadline {
            bail!(
                "workspace {id} is still locked after {}s by another bale process \
                 (a stash or sweep); lock file {}",
                wait.as_secs(),
                home.locks_dir().join(format!("{id}.lock")).display()
            );
        }
        std::thread::sleep(LOCK_POLL);
    }
}

enum Outcome {
    Written {
        recreated: bool,
    },
    KeptNewer,
    /// Already restored (by an earlier run), or not reached.
    Done,
}

struct Job<'a> {
    store: &'a Store,
    root: &'a Path,
    budget: &'a ByteBudget,
    /// An earlier run stopped mid-restore: a non-empty file at the entry's
    /// inode may be half written.
    was_restoring: bool,
    /// Test hook: write half the entry, then fail.
    fail: bool,
}

impl Job<'_> {
    fn restore(&self, e: &Entry) -> Result<Outcome> {
        let path = self.root.join(&e.rel);
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
        self.refill(&path, &meta, e)
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
            .map_err(anyhow::Error::from)
            .and_then(|()| self.stream(&file, e));
        if let Err(err) = written {
            if !self.fail {
                // Keep the inode; a stub is what the next run expects.
                let _ = file.set_len(0);
            }
            drop(file);
            let _ = set_mode(path, e.mode, false);
            return Err(err);
        }
        drop(file);
        set_mtime(path, e.mtime_ns)?;
        set_mode(path, e.mode, false).with_context(|| format!("chmod {}", path.display()))?;
        Ok(Outcome::Written { recreated: false })
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
        if !real.starts_with(self.root) {
            bail!("{} resolves outside the tree", parent.display());
        }
        let tmp = tempfile::Builder::new()
            .prefix(".bale-unstash-")
            .tempfile_in(parent)
            .with_context(|| format!("creating a temp file in {}", parent.display()))?;
        self.stream(tmp.as_file(), e)?;
        set_mtime(tmp.path(), e.mtime_ns)?;
        set_mode(tmp.path(), e.mode, false)?;
        match tmp.persist_noclobber(path) {
            Ok(_) => Ok(Outcome::Written { recreated: true }),
            Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => Ok(Outcome::KeptNewer),
            Err(err) => Err(err.error).with_context(|| format!("placing {}", path.display())),
        }
    }

    /// Streams the entry's blob into `file`, holding its patch base's share
    /// of the budget. `Store::read_to` verifies size and hash after writing.
    fn stream(&self, file: &File, e: &Entry) -> Result<()> {
        let meta = self.store.meta(&e.hash)?;
        // Resolving a base below depth 0 holds about twice its size.
        let hold = match &meta.base {
            Some(b) => 2 * self.store.meta(b)?.size,
            None => 0,
        };
        let _held = self.budget.acquire(hold);
        let mut out = BufWriter::with_capacity(
            1 << 20,
            Limit {
                inner: file,
                left: self.fail.then_some(e.size / 2),
            },
        );
        self.store.read_to(&e.hash, &mut out)?;
        out.flush()?;
        Ok(())
    }
}

/// A writer that fails once `left` bytes have gone through (test hook).
struct Limit<W> {
    inner: W,
    left: Option<u64>,
}

impl<W: Write> Write for Limit<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(left) = self.left.as_mut() else {
            return self.inner.write(buf);
        };
        if *left == 0 {
            return Err(io::Error::other("unstash: injected failure mid-file"));
        }
        let n = buf.len().min(*left as usize);
        let n = self.inner.write(&buf[..n])?;
        *left -= n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
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

/// Whether the file at an entry's path is no longer its stub: replaced (a
/// new inode or not a regular file) or written into. A missing file is not:
/// it is recreated.
fn rewritten_while_stashed(path: &Path, e: &Entry) -> bool {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            let (size, _, ino) = stat_of(&m);
            !m.file_type().is_file() || ino != e.ino || size != 0
        }
        Err(_) => false,
    }
}
