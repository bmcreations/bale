use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use walkdir::WalkDir;

use crate::busy::{busy_holders, Holder};
use crate::config::load_config;
use crate::detect::{detect, regenerable_dirs};
use crate::git;
use crate::idle::last_source_change;
use crate::lock::WorkspaceLock;
use crate::registry::{workspace_id, Level, Record, Registry};
use crate::BaleHome;

const TRASH_PREFIX: &str = ".bale-trash-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(Level),
    Busy(Vec<Holder>),
    Skipped(String),
}

/// Creates a `Hot` record for a linked worktree, or refreshes an existing
/// record's path, branch, head and admin dir (keeping its level). Errors for
/// the main checkout and for paths that are not a worktree of their repo.
pub fn register(home: &BaleHome, path: &Path) -> Result<Record> {
    let root = fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let common = git::common_dir(&root)?;
    let info = git::worktrees(&root)?
        .into_iter()
        .find(|w| fs::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone()) == root)
        .with_context(|| {
            format!(
                "{} is not a worktree of {}",
                root.display(),
                common.display()
            )
        })?;
    if info.is_main {
        bail!(
            "{} is the main checkout; bale only manages linked worktrees",
            root.display()
        );
    }
    let admin = git::admin_dir(&root)?;
    let id = workspace_id(&root);
    let registry = Registry::open(home.clone());

    let Some(_lock) = WorkspaceLock::try_acquire(home, &id)? else {
        // Another bale process is working on it; use what it recorded.
        return registry
            .get(&id)?
            .with_context(|| format!("{} is locked by another bale process", root.display()));
    };

    let record = match registry.get(&id)? {
        Some(existing) => Record {
            repo_common_dir: common,
            path: root,
            admin_dir: admin,
            branch: info.branch,
            head: info.head,
            ..existing
        },
        None => {
            let last_activity = regenerable(home, &root)
                .and_then(|dirs| last_source_change(&root, &dirs))
                .unwrap_or_else(|_| SystemTime::now());
            Record {
                id,
                repo_common_dir: common,
                path: root,
                admin_dir: admin,
                branch: info.branch,
                head: info.head,
                level: Level::Hot,
                last_activity,
                note: None,
            }
        }
    };
    registry.put(&record)?;
    Ok(record)
}

/// Removes the worktree's regenerable output in place and marks it `Baled`.
/// A record left `Baling` by a crash resumes at trash removal.
pub fn up(home: &BaleHome, id: &str) -> Result<Outcome> {
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return Ok(Outcome::Skipped("locked".into()));
    };
    let registry = Registry::open(home.clone());
    let Some(mut record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    let root = fs::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;

    match record.level {
        Level::Cooling | Level::Cold => return Ok(Outcome::Skipped("cold".into())),
        Level::Baling => {}
        Level::Hot | Level::Baled => {
            let holders = busy_holders(&root)?;
            if !holders.is_empty() {
                return Ok(Outcome::Busy(holders));
            }
            git::lock_worktree(&root, &root)?;
            record.level = Level::Baling;
            registry.put(&record)?;

            if let Err(e) = move_to_trash(home, &root) {
                let unlocked = git::unlock_worktree_if_ours(&root, &root);
                record.level = Level::Hot;
                registry.put(&record)?;
                unlocked?;
                if is_busy_error(&e) {
                    return Ok(Outcome::Busy(Vec::new()));
                }
                return Err(e).context("moving build output aside");
            }
        }
    }

    remove_trash(&root)?;
    #[cfg(target_os = "macos")]
    remove_derived_data(&root)?;

    record.level = Level::Baled;
    registry.put(&record)?;
    Ok(Outcome::Done(Level::Baled))
}

fn regenerable(home: &BaleHome, root: &Path) -> Result<Vec<PathBuf>> {
    let cfg = load_config(home, Some(root))?;
    Ok(regenerable_dirs(root, &detect(root), &cfg))
}

fn is_busy_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || (cfg!(windows) && e.raw_os_error() == Some(32))
}

/// Renames each regenerable dir to `<parent>/.bale-trash-<name>-<nanos>`. On
/// failure every dir already moved is renamed back before the error returns.
fn move_to_trash(home: &BaleHome, root: &Path) -> io::Result<()> {
    let mut dirs = regenerable(home, root).map_err(io::Error::other)?;
    dirs.retain(|d| !in_trash(root, d));
    dirs.sort();
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for dir in dirs {
        let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
            continue;
        };
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let trash = parent.join(format!("{TRASH_PREFIX}{}-{nanos}", name.to_string_lossy()));
        if let Err(e) = fs::rename(&dir, &trash) {
            for (from, to) in moved.iter().rev() {
                // Best effort: a failed restore leaves a trash dir that the
                // next `up` removes, which is no worse than baling it.
                let _ = fs::rename(to, from);
            }
            return Err(e);
        }
        moved.push((dir, trash));
    }
    Ok(())
}

fn in_trash(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with(TRASH_PREFIX))
    })
}

/// Removes every `.bale-trash-*` entry under `root`. Symlinks are never
/// followed: a trash entry that is a link has only the link removed.
fn remove_trash(root: &Path) -> Result<()> {
    let mut found = Vec::new();
    let mut it = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = it.next() {
        let entry = entry.with_context(|| format!("walking {}", root.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let is_dir = entry.file_type().is_dir();
        if is_dir && name == ".git" {
            it.skip_current_dir();
            continue;
        }
        if name.starts_with(TRASH_PREFIX) {
            found.push(entry.path().to_path_buf());
            if is_dir {
                it.skip_current_dir();
            }
        }
    }
    for path in found {
        remove_entry(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

fn remove_entry(path: &Path) -> io::Result<()> {
    let ft = fs::symlink_metadata(path)?.file_type();
    if ft.is_symlink() {
        // A Windows directory symlink or junction needs remove_dir; neither
        // call follows the link.
        return fs::remove_file(path).or_else(|e| {
            if cfg!(windows) {
                fs::remove_dir(path)
            } else {
                Err(e)
            }
        });
    }
    if ft.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Removes global DerivedData folders built from this worktree. A folder with
/// a process holding it is left alone.
#[cfg(target_os = "macos")]
fn remove_derived_data(root: &Path) -> Result<()> {
    let Some(home) = dirs::home_dir() else {
        return Ok(());
    };
    let dd_root = home.join("Library/Developer/Xcode/DerivedData");
    if !dd_root.is_dir() {
        return Ok(());
    }
    for dir in crate::derived_data::derived_data_for(root, &dd_root) {
        if !busy_holders(&dir)?.is_empty() {
            continue;
        }
        remove_entry(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}
