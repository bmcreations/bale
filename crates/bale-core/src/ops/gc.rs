//! `bale gc`: forget workspaces whose worktree is gone, and clear what an
//! interrupted `bale open` left on a hot one.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::clones::{gc as clones_gc, store::CloneStore, tracked as clones_tracked};
use crate::config::load_config;
use crate::git::{self, Git, WorktreeInfo, LOCK_REASON};
use crate::lock::WorkspaceLock;
use crate::ops::disk;
use crate::ops::open::{delete_saved, saved_refs};
use crate::registry::{canonical_or_ancestor, is_workspace_id, Level, Record, Registry};
use crate::store::{gc as store_gc, Store};
use crate::BaleHome;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Records deleted, with their refs and saved state.
    pub removed: Vec<String>,
    /// Hot workspaces whose stale bale worktree lock was released.
    pub unlocked: Vec<String>,
    /// Hot workspaces whose leftover refs or saved state were deleted.
    pub cleaned: Vec<String>,
    /// `(id, path)` for each check dir a crashed cold left, removed.
    pub checks: Vec<(String, PathBuf)>,
    /// `(id, error)` for each record gc could not check or act on.
    pub errors: Vec<(String, String)>,
    /// Bytes of store blobs, manifests and stat indexes removed (or that a
    /// dry run would remove).
    pub store_bytes_freed: u64,
    /// Bytes of unreferenced clone-store files removed (or that a dry run
    /// would remove).
    pub clones_bytes_freed: u64,
}

/// Removes a `Hot` or `Baled` record when its dir is gone and git no longer
/// lists the worktree, deleting `refs/bale/<id>/*` and `$BALE_HOME/<id>`.
/// For a `Hot` workspace whose dir is still its own checkout, releases a
/// worktree lock carrying bale's reason and deletes leftover saved state.
/// Other levels, workspaces
/// another bale process holds, and records whose repo cannot be probed are
/// left alone. With `dry_run`, reports the same lists and changes nothing.
pub fn gc(home: &BaleHome, dry_run: bool) -> Result<GcReport> {
    if dry_run {
        return gc_untraced(home, true);
    }
    crate::trace::op(home, "gc", None, || gc_untraced(home, false))
}

fn gc_untraced(home: &BaleHome, dry_run: bool) -> Result<GcReport> {
    let registry = Registry::open(home.clone());
    let mut report = GcReport::default();
    for (stem, read) in registry.all_lenient()? {
        let result = read.and_then(|r| {
            if r.id != stem {
                bail!("record file {stem}.json holds id {:?}", r.id);
            }
            collect(home, &registry, &r.id, dry_run, &mut report)
        });
        if let Err(e) = result {
            report.errors.push((stem, format!("{e:#}")));
        }
    }
    collect_store(home, &registry, dry_run, &mut report);
    collect_clones(home, &registry, dry_run, &mut report);
    Ok(report)
}

/// Frees store blobs and manifests no record reaches. A record gc cannot
/// read might hold a stash, so then it removes nothing; the per-record pass
/// has already reported that record as an error.
fn collect_store(home: &BaleHome, registry: &Registry, dry_run: bool, report: &mut GcReport) {
    let result = (|| -> Result<Option<u64>> {
        let mut commitments = Vec::new();
        let mut ids = Vec::new();
        for (stem, read) in registry.all_lenient()? {
            let Ok(record) = read else {
                return Ok(None);
            };
            commitments.extend(record.stash.map(|s| s.commitment));
            ids.push(stem);
        }
        let store = Store::open(home)?;
        let blobs = store_gc::collect(&store, &commitments, dry_run)?;
        Ok(Some(
            blobs + store_gc::collect_index(&store, &ids, dry_run)?,
        ))
    })();
    match result {
        Ok(n) => report.store_bytes_freed = n.unwrap_or(0),
        Err(e) => report.errors.push(("store".to_string(), format!("{e:#}"))),
    }
}

/// Frees clone-store files no existing tree lists as a candidate. A record
/// gc cannot read, or a tree whose candidates cannot be listed, might still
/// use a file, so then it removes nothing (tmp files included).
fn collect_clones(home: &BaleHome, registry: &Registry, dry_run: bool, report: &mut GcReport) {
    let result = (|| -> Result<u64> {
        if !home.clones_dir().exists() {
            return Ok(0);
        }
        let mut live = HashSet::new();
        for (stem, read) in registry.all_lenient()? {
            let record = read.with_context(|| format!("record {stem} is unreadable"))?;
            if fs::symlink_metadata(&record.path).is_err() {
                continue;
            }
            let cfg = load_config(home, Some(&record.path))?;
            let found = clones_tracked::candidates(&record.path, cfg.clone_min_bytes)
                .with_context(|| format!("listing {}", record.path.display()))?;
            live.extend(found.into_iter().map(|c| c.key));
        }
        let store = CloneStore::open(home)?;
        clones_gc::collect(&store, &live, store_gc::GC_GRACE, dry_run)
    })();
    match result {
        Ok(n) => report.clones_bytes_freed = n,
        Err(e) => report.errors.push(("clones".to_string(), format!("{e:#}"))),
    }
}

fn collect(
    home: &BaleHome,
    registry: &Registry,
    id: &str,
    dry_run: bool,
    report: &mut GcReport,
) -> Result<()> {
    if !is_workspace_id(id) {
        bail!("{id:?} is not a workspace id");
    }
    // A dry run only probes the lock, so it creates no lock files.
    let _lock = if dry_run {
        if WorkspaceLock::is_held(home, id)? {
            return Ok(());
        }
        None
    } else {
        match WorkspaceLock::try_acquire(home, id)? {
            Some(lock) => Some(lock),
            None => return Ok(()),
        }
    };
    // Re-read (under the lock, for a real run): it may have changed since.
    let Some(record) = registry.get(id)? else {
        return Ok(());
    };
    // Under the lock no check runs, so any check dir is a crashed cold's.
    let checks = if dry_run {
        disk::stale_checks(&record.path, id)?
    } else {
        disk::remove_stale_checks(&record.path, id)?
    };
    report
        .checks
        .extend(checks.into_iter().map(|p| (id.to_string(), p)));
    match record.level {
        Level::Hot | Level::Baled => {}
        // Their dir can be legitimately absent while cold or mid-operation.
        Level::Cold | Level::Opening | Level::Cooling | Level::Baling => return Ok(()),
    }
    let gone = match fs::symlink_metadata(&record.path) {
        Ok(_) => false,
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(e) => return Err(e).with_context(|| format!("checking {}", record.path.display())),
    };
    if !gone && record.level != Level::Hot {
        return Ok(());
    }

    let g = repo_git(&record)?;
    let listed = listing(&record)?;
    if gone {
        if listed.is_some() {
            return Ok(());
        }
        if !dry_run {
            delete_saved(home, &g, id)?;
            // Its pre-made DerivedData folders go with the record that names them.
            #[cfg(target_os = "macos")]
            crate::spm_link::drop_stubs(&mut record.clone());
            registry.remove(id)?;
        }
        report.removed.push(id.to_string());
        return Ok(());
    }

    // Hot and present: clear what an interrupted `bale open` left behind,
    // but only from the worktree's own checkout. A folder recreated at the
    // path (git in it climbs into an enclosing repo, or fails) does not hold
    // the work open restored, so the save may be its only copy.
    if disk::ensure_checkout(&record.path, &record.admin_dir).is_err() {
        return Ok(());
    }
    if listed.is_some_and(|w| w.locked.as_deref() == Some(LOCK_REASON)) {
        if !dry_run {
            git::unlock_worktree_if_ours(&record.repo_common_dir, &record.path)?;
        }
        report.unlocked.push(id.to_string());
    }
    let has_refs = !saved_refs(&g, id)?.is_empty();
    let has_dir = fs::symlink_metadata(home.saved_dir(id)).is_ok();
    if has_refs || has_dir {
        if !dry_run {
            delete_saved(home, &g, id)?;
        }
        report.cleaned.push(id.to_string());
    }
    Ok(())
}

/// Whether the record's worktree is gone without bale's doing: a hot or baled
/// tree whose path is absent. Cold and mid-operation trees can be absent on
/// purpose. `collect` forgets such a record (unless git still lists it).
pub(crate) fn is_gone(record: &Record) -> bool {
    matches!(record.level, Level::Hot | Level::Baled)
        && matches!(fs::symlink_metadata(&record.path),
            Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// A git runner for the record's repo. Errs unless git, run in the recorded
/// common dir, resolves to that same common dir: a deleted repo, or a dir
/// from which git would climb into an enclosing repo, fails the probe.
fn repo_git(record: &Record) -> Result<Git> {
    let dir = &record.repo_common_dir;
    let expected =
        crate::paths::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let actual = git::common_dir(dir)?;
    if crate::paths::canonicalize(&actual).unwrap_or(actual.clone()) != expected {
        bail!(
            "{} is no longer a git repository (git resolved {})",
            dir.display(),
            actual.display()
        );
    }
    Ok(Git::at(dir))
}

/// The repo's worktree entry for the record's path, if git lists one.
fn listing(record: &Record) -> Result<Option<WorktreeInfo>> {
    let want = canonical_or_ancestor(&record.path);
    Ok(git::worktrees(&record.repo_common_dir)?
        .into_iter()
        .find(|w| !w.is_main && canonical_or_ancestor(&w.path) == want))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::open::delete_saved_dir;

    #[test]
    fn workspace_ids_are_twelve_lowercase_hex() {
        assert!(is_workspace_id("0123456789ab"));
        for bad in [
            "",
            "registry",
            "locks",
            "0123456789AB",
            "0123456789a",
            "../012345678",
            "0123456789abc",
        ] {
            assert!(!is_workspace_id(bad), "{bad}");
        }
    }

    #[test]
    fn saved_dir_refuses_a_non_id() {
        let h = tempfile::tempdir().unwrap();
        let home = BaleHome(h.path().into());
        fs::create_dir_all(home.records_dir()).unwrap();
        assert!(delete_saved_dir(&home, "registry").is_err());
        assert!(home.records_dir().exists());
    }
}
