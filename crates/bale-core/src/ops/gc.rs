//! `bale gc`: forget workspaces whose worktree is gone, and clear what an
//! interrupted `bale open` left on a hot one.

use std::fs;
use std::io;

use anyhow::{bail, Context, Result};

use crate::git::{self, Git, WorktreeInfo, LOCK_REASON};
use crate::lock::WorkspaceLock;
use crate::ops::disk;
use crate::ops::open::{delete_saved, saved_refs};
use crate::registry::{canonical_or_ancestor, is_workspace_id, Level, Record, Registry};
use crate::BaleHome;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Records deleted, with their refs and saved state.
    pub removed: Vec<String>,
    /// Hot workspaces whose stale bale worktree lock was released.
    pub unlocked: Vec<String>,
    /// Hot workspaces whose leftover refs or saved state were deleted.
    pub cleaned: Vec<String>,
    /// `(id, error)` for each record gc could not check or act on.
    pub errors: Vec<(String, String)>,
}

/// Removes a `Hot` or `Baled` record when its dir is gone and git no longer
/// lists the worktree, deleting `refs/bale/<id>/*` and `$BALE_HOME/<id>`.
/// For a `Hot` workspace whose dir exists, releases a worktree lock carrying
/// bale's reason and deletes leftover saved state. Other levels, workspaces
/// another bale process holds, and records whose repo cannot be probed are
/// left alone. With `dry_run`, reports the same lists and changes nothing.
pub fn gc(home: &BaleHome, dry_run: bool) -> Result<GcReport> {
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
    Ok(report)
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
    if !dry_run {
        disk::remove_stale_checks(&record.path, id)?;
    }
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
            registry.remove(id)?;
        }
        report.removed.push(id.to_string());
        return Ok(());
    }

    // Hot and present: clear what an interrupted `bale open` left behind.
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

/// A git runner for the record's repo. Errs unless git, run in the recorded
/// common dir, resolves to that same common dir: a deleted repo, or a dir
/// from which git would climb into an enclosing repo, fails the probe.
fn repo_git(record: &Record) -> Result<Git> {
    let dir = &record.repo_common_dir;
    let expected = fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let actual = git::common_dir(dir)?;
    if fs::canonicalize(&actual).unwrap_or(actual.clone()) != expected {
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
