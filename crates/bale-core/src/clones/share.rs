//! Replaces a tree's large tracked files with clones of one stored copy, so
//! every worktree of a repo shares their disk blocks.
//!
//! A file is replaced only when its bytes match its key (the index's blob oid,
//! or an LFS object's SHA-256) and it has not changed since it was checked;
//! anything else is left alone and listed in `skipped`.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};

use super::reflink::same_volume;
use super::store::{Adopted, CloneStore};
use super::tracked::candidates;
use super::{Candidate, Key};
use crate::busy::busy_holders;
use crate::config::{load_config, Config};
use crate::git::Git;
use crate::lock::WorkspaceLock;
use crate::registry::Registry;
use crate::store::sha256_file;
use crate::BaleHome;

/// What one `share` did.
#[derive(Debug, Default)]
pub struct ShareReport {
    /// Files replaced by a clone of the stored copy.
    pub files: u64,
    /// Their bytes.
    pub bytes: u64,
    /// Files this run added to the store.
    pub stored: u64,
    /// Files left alone, with why: "modified", "missing", or an io error.
    pub skipped: Vec<(PathBuf, String)>,
    /// Set when the whole tree was skipped: "locked", "busy", "other volume",
    /// "clones unsupported" or "disabled". Files placed before clones turned
    /// out unsupported stay placed.
    pub reason: Option<String>,
}

impl ShareReport {
    /// Some file was left alone for an io error rather than for being
    /// modified or missing.
    pub fn had_errors(&self) -> bool {
        self.skipped
            .iter()
            .any(|(_, why)| !matches!(why.as_str(), MODIFIED | MISSING | ODD_PATH))
    }

    /// The one line `bale share` prints and the sweep reports.
    pub fn summary(&self) -> String {
        match &self.reason {
            Some(reason) => format!("skipped: {reason}"),
            None => format!(
                "shared {} files ({:.1} MiB), stored {}",
                self.files,
                self.bytes as f64 / (1u64 << 20) as f64,
                self.stored
            ),
        }
    }

    fn skipped(reason: &str) -> ShareReport {
        ShareReport {
            reason: Some(reason.to_string()),
            ..ShareReport::default()
        }
    }
}

const MODIFIED: &str = "modified";
const MISSING: &str = "missing";
const ODD_PATH: &str = "unsupported path";

/// Shares the large tracked files of the registered tree `id` through the
/// clone store. Takes the tree's lock without waiting and skips a busy tree.
/// Sets the record's `shared_head` when every file was shared or left alone
/// for being modified or missing.
pub fn share(home: &BaleHome, id: &str) -> Result<ShareReport> {
    crate::trace::op(home, "share", Some(id), || share_untraced(home, id))
}

fn share_untraced(home: &BaleHome, id: &str) -> Result<ShareReport> {
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return Ok(ShareReport::skipped("locked"));
    };
    let registry = Registry::open(home.clone());
    let Some(record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    let root = crate::paths::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    let cfg = load_config(home, Some(&root))?;
    if !cfg.share_checkouts {
        return Ok(ShareReport::skipped("disabled"));
    }
    if !busy_holders(&root, &[])?.is_empty() {
        return Ok(ShareReport::skipped("busy"));
    }
    let store = CloneStore::open(home)
        .with_context(|| format!("opening {}", home.clones_dir().display()))?;
    if !same_volume(&home.clones_dir(), &root)? {
        return Ok(ShareReport::skipped("other volume"));
    }

    let mut report = ShareReport::default();
    let matched = verify(&root, candidates(&root, cfg.clone_min_bytes)?, &mut report)?;
    for (c, seen) in matched {
        let dst = root.join(&c.path);
        match place_one(&store, &c, &dst, &seen) {
            Ok(Placed::Done { stored }) => {
                report.files += 1;
                report.bytes += c.size;
                report.stored += u64::from(stored);
            }
            Ok(Placed::Changed) => report.skipped.push((c.path, MODIFIED.into())),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                report.reason = Some("clones unsupported".into());
                break;
            }
            Err(e) => report.skipped.push((c.path, e.to_string())),
        }
    }

    let git = Git::at(&root);
    if report.files > 0 {
        // Placed files have a new inode and ctime; refresh the index's stat
        // data so `git status` doesn't rehash them. Their bytes are right
        // either way, so a failure here changes nothing.
        let _ = git.run(&["update-index", "-q", "--refresh"]);
    }
    if report.reason.is_none() && !report.had_errors() {
        if let Some(mut now) = registry.get(id)? {
            now.shared_head = Some(now.head.clone());
            registry.put(&now)?;
        }
    }
    Ok(report)
}

/// A file's identity when it was verified; it must be unchanged when placed.
#[derive(Debug, PartialEq)]
struct Seen {
    len: u64,
    mtime: SystemTime,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: (i64, i64),
    mode: u32,
}

fn seen(path: &Path) -> io::Result<Option<Seen>> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !md.file_type().is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    let (mode, ino, ctime) = {
        use std::os::unix::fs::MetadataExt;
        (md.mode() & 0o7777, md.ino(), (md.ctime(), md.ctime_nsec()))
    };
    #[cfg(not(unix))]
    let mode = 0o644;
    Ok(Some(Seen {
        len: md.len(),
        mtime: md.modified()?,
        #[cfg(unix)]
        ino,
        #[cfg(unix)]
        ctime,
        mode,
    }))
}

/// Keeps the candidates whose bytes match their key, each with its stat as
/// verified; records the rest in `report.skipped`.
fn verify(
    root: &Path,
    all: Vec<Candidate>,
    report: &mut ShareReport,
) -> Result<Vec<(Candidate, Seen)>> {
    let mut present = Vec::new();
    for c in all {
        let path = root.join(&c.path);
        match seen(&path) {
            Ok(Some(s)) => present.push((c, s)),
            Ok(None) => {
                let gone = matches!(std::fs::symlink_metadata(&path), Err(e) if e.kind() == io::ErrorKind::NotFound);
                report
                    .skipped
                    .push((c.path, if gone { MISSING } else { MODIFIED }.into()));
            }
            Err(e) => report.skipped.push((c.path, e.to_string())),
        }
    }

    // `--stdin-paths` reads one path per line, so a path with a newline
    // can't be named; leave such files alone.
    let (git_paths, lfs): (Vec<_>, Vec<_>) = present
        .into_iter()
        .partition(|(c, _)| matches!(c.key, Key::Git(_)));
    let mut batch = Vec::new();
    for (c, s) in git_paths {
        match c.path.to_str() {
            Some(p) if !p.contains('\n') => batch.push((c, s)),
            _ => report.skipped.push((c.path, ODD_PATH.into())),
        }
    }

    let mut matched = Vec::new();
    if !batch.is_empty() {
        let mut input = String::new();
        for (c, _) in &batch {
            input.push_str(c.path.to_str().unwrap_or_default());
            input.push('\n');
        }
        let out = Git::at(root).run_stdin(
            &["hash-object", "--no-filters", "--stdin-paths"],
            input.as_bytes(),
        )?;
        let oids: Vec<&str> = out.lines().collect();
        if oids.len() != batch.len() {
            bail!(
                "git hash-object returned {} oids for {} paths",
                oids.len(),
                batch.len()
            );
        }
        for ((c, s), oid) in batch.into_iter().zip(oids) {
            if matches!(&c.key, Key::Git(k) if k == oid.trim()) {
                matched.push((c, s));
            } else {
                report.skipped.push((c.path, MODIFIED.into()));
            }
        }
    }
    for (c, s) in lfs {
        match sha256_file(&root.join(&c.path)) {
            Ok(sha) if matches!(&c.key, Key::Lfs(k) if *k == sha) => matched.push((c, s)),
            Ok(_) => report.skipped.push((c.path, MODIFIED.into())),
            Err(e) => report.skipped.push((c.path, format!("{e:#}"))),
        }
    }
    Ok(matched)
}

enum Placed {
    /// Replaced; `stored` when this call added the key to the store.
    Done { stored: bool },
    /// The file changed after it was verified, so it was left alone.
    Changed,
}

fn place_one(store: &CloneStore, c: &Candidate, dst: &Path, verified: &Seen) -> io::Result<Placed> {
    // A crash between the clone and the rename leaves `<file>.bale-share`;
    // the name is bale's own.
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".bale-share");
    match std::fs::remove_file(PathBuf::from(tmp)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let stored = if store.has(&c.key) {
        false
    } else {
        match store.adopt(&c.key, dst, || unchanged(dst, verified))? {
            Adopted::Stored => true,
            Adopted::Present => false,
            Adopted::Changed => return Ok(Placed::Changed),
        }
    };
    if !unchanged(dst, verified)? {
        return Ok(Placed::Changed);
    }
    store.place(&c.key, dst, verified.mode, Some(verified.mtime))?;
    Ok(Placed::Done { stored })
}

/// Whether `path` is still the file that was verified.
fn unchanged(path: &Path, verified: &Seen) -> io::Result<bool> {
    Ok(seen(path)?.as_ref() == Some(verified))
}

/// Fills the index and files of `tree`, a worktree just added with
/// `--no-checkout`. When sharing is on and the store is on the tree's
/// volume, large tracked files already in the store are placed as clones of
/// it and git checks out only the rest; then the large files git wrote are
/// verified and added to the store for the next tree. Fails only when git
/// can't fill the index or check files out; sharing problems leave git to
/// write the file.
pub(crate) fn materialize(home: &BaleHome, tree: &Path, cfg: &Config) -> Result<()> {
    let git = Git::at(tree);
    git.run(&["read-tree", "HEAD"])?;

    let shared = if cfg.share_checkouts {
        sharing_store(home, tree).map(|store| {
            // A failure listing candidates only means nothing is shared.
            (
                store,
                candidates(tree, cfg.clone_min_bytes).unwrap_or_default(),
            )
        })
    } else {
        None
    };

    // Index paths as git prints them, NUL-separated and unconverted.
    let listing = git.run_stdin_bytes(&["ls-files", "-z"], b"")?;
    let paths: Vec<&[u8]> = listing
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .collect();

    let mut placed: HashSet<&[u8]> = HashSet::new();
    if let Some((store, cands)) = &shared {
        // Candidate paths were read lossily; place only those that name an
        // index path exactly.
        let exact: HashSet<&[u8]> = paths.iter().copied().collect();
        for c in cands {
            let Some(p) = c.path.to_str() else { continue };
            if !exact.contains(p.as_bytes()) || !store.has(&c.key) {
                continue;
            }
            let dst = tree.join(&c.path);
            let res = match dst.parent() {
                Some(dir) => std::fs::create_dir_all(dir),
                None => Ok(()),
            }
            .and_then(|()| store.place(&c.key, &dst, c.mode, None));
            match res {
                Ok(()) => {
                    placed.insert(p.as_bytes());
                }
                Err(e) if e.kind() == io::ErrorKind::Unsupported => break,
                // Git writes this file instead.
                Err(_) => {}
            }
        }
    }

    let mut input = Vec::new();
    for p in paths.iter().filter(|p| !placed.contains(*p)) {
        input.extend_from_slice(p);
        input.push(0);
    }
    if !input.is_empty() {
        git.run_stdin_bytes(&["checkout-index", "-u", "-q", "-z", "--stdin"], &input)?;
    }
    // Placed files have no stat data in the index yet; their bytes are right
    // either way, so a failure here changes nothing.
    let _ = git.run(&["update-index", "-q", "--refresh"]);

    if let Some((store, cands)) = shared {
        let fresh: Vec<Candidate> = cands.into_iter().filter(|c| !store.has(&c.key)).collect();
        if !fresh.is_empty() {
            let mut report = ShareReport::default();
            // Adopting is for later trees; this one is complete without it.
            if let Ok(matched) = verify(tree, fresh, &mut report) {
                for (c, s) in matched {
                    let path = tree.join(&c.path);
                    match store.adopt(&c.key, &path, || unchanged(&path, &s)) {
                        Err(e) if e.kind() == io::ErrorKind::Unsupported => break,
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

/// The clone store, when it can share files with `tree`.
fn sharing_store(home: &BaleHome, tree: &Path) -> Option<CloneStore> {
    let store = CloneStore::open(home).ok()?;
    same_volume(&home.clones_dir(), tree)
        .ok()
        .filter(|&same| same)
        .map(|_| store)
}
