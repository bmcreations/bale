//! Record fields only people and adapters set: a tree's origin, its pin,
//! and the activity a tool reports.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};

use crate::lock::WorkspaceLock;
use crate::ops::up::resolve;
use crate::registry::{workspace_id, Origin, Record, Registry};
use crate::BaleHome;

/// Records which tool session made the tree. `id_or_path` resolves as for
/// `bale up`, so an unregistered checkout is registered first.
pub fn set_origin(home: &BaleHome, id_or_path: &str, origin: Origin) -> Result<()> {
    update(home, id_or_path, |r| r.origin = Some(origin))
}

/// Pins or unpins the tree; `sweep` never ups or colds a pinned one.
/// `id_or_path` resolves as for `bale up`.
pub fn set_pinned(home: &BaleHome, id_or_path: &str, pinned: bool) -> Result<()> {
    update(home, id_or_path, |r| r.pinned = pinned)
}

/// Rewrites the record under its `WorkspaceLock`, like every record write.
fn update(home: &BaleHome, id_or_path: &str, change: impl FnOnce(&mut Record)) -> Result<()> {
    let id = resolve(home, id_or_path)?;
    let Some(_lock) = WorkspaceLock::try_acquire(home, &id)? else {
        bail!("{id} is locked by another bale process; try again");
    };
    let registry = Registry::open(home.clone());
    let mut record = registry
        .get(&id)?
        .with_context(|| format!("record for {id} vanished"))?;
    change(&mut record);
    registry.put(&record)
}

/// What [`touch`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Touch {
    /// The record's `last_activity` is now.
    Touched,
    /// No record has this path; nothing was written or traced.
    Untracked,
    /// Another bale process holds the tree's lock (an `up` or `cold` in
    /// progress); the record was left as it was.
    Locked,
}

impl crate::trace::Traced for Touch {
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Touch::Touched | Touch::Untracked => ("ok", None),
            Touch::Locked => ("skipped", Some("locked")),
        }
    }
}

/// Records that a tool finished a unit of work in the tree at `path`: sets
/// the record's `last_activity` to now, so sweep counts the tree's idle time
/// from here, and writes a `touch` op line. It never ups or colds the tree
/// and runs no git, so a tool can call it after every turn. A path with no
/// record gives [`Touch::Untracked`]; a tree whose lock is held is left
/// alone rather than waited for.
pub fn touch(home: &BaleHome, path: &Path) -> Result<Touch> {
    touch_with(home, path, false)
}

/// [`touch`] for a hook that restores stashed files: also sets the record's
/// `restore_hook`, which lets `sweep` stash the tree. Set in the same record
/// write as `last_activity`, under the same lock.
pub fn touch_restoring(home: &BaleHome, path: &Path) -> Result<Touch> {
    touch_with(home, path, true)
}

fn touch_with(home: &BaleHome, path: &Path, restore_hook: bool) -> Result<Touch> {
    // Records are keyed by the canonical path `register` stores, so a tree
    // reached through a symlink still finds its record.
    let canonical = match crate::paths::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?
        }
    };
    let id = workspace_id(&canonical);
    let registry = Registry::open(home.clone());
    if registry.get(&id)?.is_none() {
        return Ok(Touch::Untracked);
    }
    crate::trace::op(home, "touch", Some(&id), || {
        let Some(_lock) = WorkspaceLock::try_acquire(home, &id)? else {
            return Ok(Touch::Locked);
        };
        let Some(mut record) = registry.get(&id)? else {
            return Ok(Touch::Untracked);
        };
        record.last_activity = SystemTime::now();
        record.restore_hook |= restore_hook;
        registry.put(&record)?;
        Ok(Touch::Touched)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Level;
    use std::path::PathBuf;
    use std::time::Duration;

    fn record(path: &Path, at: SystemTime) -> Record {
        Record {
            id: workspace_id(path),
            repo_common_dir: PathBuf::from("/r/.git"),
            path: path.to_path_buf(),
            admin_dir: PathBuf::from("/r/.git/worktrees/t"),
            branch: Some("b".into()),
            head: "0".repeat(40),
            level: Level::Hot,
            last_activity: at,
            note: None,
            trash: Vec::new(),
            origin: None,
            pinned: false,
            scope: None,
            stash: None,
            restore_hook: false,
            spm: Vec::new(),
            shared_head: None,
        }
    }

    #[test]
    fn touch_bumps_last_activity_and_traces_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = BaleHome(tmp.path().join("bale"));
        let tree = tmp.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let tree = crate::paths::canonicalize(&tree).unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let registry = Registry::open(home.clone());
        registry.put(&record(&tree, old)).unwrap();

        assert_eq!(touch(&home, &tree).unwrap(), Touch::Touched);
        let after = registry.get(&workspace_id(&tree)).unwrap().unwrap();
        assert!(after.last_activity > old);
        assert_eq!(after.level, Level::Hot);
        let trace = std::fs::read_dir(home.trace_dir())
            .unwrap()
            .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
            .collect::<String>();
        assert!(trace.contains("\"op\":\"touch\""), "{trace}");
        assert!(trace.contains("\"outcome\":\"ok\""), "{trace}");
    }

    #[cfg(unix)]
    #[test]
    fn touch_finds_a_tree_reached_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let home = BaleHome(tmp.path().join("bale"));
        let tree = tmp.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let canonical = crate::paths::canonicalize(&tree).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&tree, &link).unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let registry = Registry::open(home.clone());
        registry.put(&record(&canonical, old)).unwrap();

        assert_eq!(touch(&home, &link).unwrap(), Touch::Touched);
        let after = registry.get(&workspace_id(&canonical)).unwrap().unwrap();
        assert!(after.last_activity > old);
    }

    #[test]
    fn touch_leaves_an_untracked_path_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let home = BaleHome(tmp.path().join("bale"));
        assert_eq!(touch(&home, tmp.path()).unwrap(), Touch::Untracked);
        assert!(!home.records_dir().exists());
        assert!(!home.trace_dir().exists());
    }

    #[test]
    fn touch_skips_a_locked_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let home = BaleHome(tmp.path().join("bale"));
        let tree = tmp.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let tree = crate::paths::canonicalize(&tree).unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let registry = Registry::open(home.clone());
        registry.put(&record(&tree, old)).unwrap();
        let id = workspace_id(&tree);
        let _held = WorkspaceLock::try_acquire(&home, &id).unwrap().unwrap();
        assert_eq!(touch(&home, &tree).unwrap(), Touch::Locked);
        assert_eq!(registry.get(&id).unwrap().unwrap().last_activity, old);
    }
}
