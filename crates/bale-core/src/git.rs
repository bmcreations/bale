//! Thin wrapper over the `git` binary: worktree listing, locking, status.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Lock reason bale writes; only locks with this reason are ever released.
pub const LOCK_REASON: &str = "baled by bale";

const BACKOFF_MS: [u64; 5] = [100, 200, 400, 800, 1600];

#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    envs: Vec<(String, std::ffi::OsString)>,
}

impl Git {
    pub fn at(dir: impl Into<PathBuf>) -> Git {
        Git {
            dir: dir.into(),
            envs: Vec::new(),
        }
    }

    /// Sets `key` for every command this runner starts. Applied after the
    /// repo-redirecting variables are scrubbed, so it can point git at a
    /// private `GIT_INDEX_FILE`.
    pub fn env(mut self, key: &str, value: impl Into<std::ffi::OsString>) -> Git {
        self.envs.push((key.to_string(), value.into()));
        self
    }

    /// Runs `git <args>` in the directory and returns stdout. Retries up to five
    /// times (100, 200, 400, 800, 1600 ms) when stderr reports a held
    /// `index.lock` or ref lock; any other failure returns immediately.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let mut attempt = 0;
        loop {
            let out = Command::new("git")
                .args(args)
                .current_dir(&self.dir)
                .env("LC_ALL", "C")
                .env("GIT_TERMINAL_PROMPT", "0")
                // A git hook sets these; they would redirect us to another repo.
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .envs(self.envs.iter().map(|(k, v)| (k, v)))
                .output()
                .with_context(|| format!("failed to run git {}", args.join(" ")))?;
            if out.status.success() {
                return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            let locked = stderr.contains("index.lock") || stderr.contains("cannot lock ref");
            if locked && attempt < BACKOFF_MS.len() {
                std::thread::sleep(Duration::from_millis(BACKOFF_MS[attempt]));
                attempt += 1;
                continue;
            }
            bail!(
                "git {} failed in {}: {}",
                args.join(" "),
                self.dir.display(),
                stderr.trim()
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub is_main: bool,
    /// `Some(reason)` when locked (reason may be empty).
    pub locked: Option<String>,
    pub prunable: bool,
}

impl WorktreeInfo {
    /// The skip reason for a lock someone other than bale holds, if any.
    pub fn foreign_lock(&self) -> Option<String> {
        match self.locked.as_deref() {
            None | Some(LOCK_REASON) => None,
            Some("") => Some("locked: no reason given".into()),
            Some(reason) => Some(format!("locked: {reason}")),
        }
    }
}

/// Parses `git worktree list --porcelain`; the first entry is the main worktree.
pub fn worktrees(repo: &Path) -> Result<Vec<WorktreeInfo>> {
    let out = Git::at(repo).run(&["worktree", "list", "--porcelain"])?;
    let mut list: Vec<WorktreeInfo> = Vec::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            list.push(WorktreeInfo {
                path: PathBuf::from(p),
                head: String::new(),
                branch: None,
                is_main: list.is_empty(),
                locked: None,
                prunable: false,
            });
            continue;
        }
        let Some(cur) = list.last_mut() else { continue };
        if let Some(h) = line.strip_prefix("HEAD ") {
            cur.head = h.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "locked" {
            cur.locked = Some(String::new());
        } else if let Some(r) = line.strip_prefix("locked ") {
            cur.locked = Some(r.to_string());
        } else if line == "prunable" || line.starts_with("prunable ") {
            cur.prunable = true;
        }
    }
    Ok(list)
}

fn path_query(dir: &Path, flag: &str) -> Result<PathBuf> {
    let out = Git::at(dir).run(&["rev-parse", "--path-format=absolute", flag])?;
    Ok(PathBuf::from(out.trim_end_matches(['\n', '\r'])))
}

/// The repository's shared `.git` directory.
pub fn common_dir(dir: &Path) -> Result<PathBuf> {
    path_query(dir, "--git-common-dir")
}

/// This worktree's private admin dir (`.git` itself for the main worktree).
pub fn admin_dir(dir: &Path) -> Result<PathBuf> {
    path_query(dir, "--git-dir")
}

/// True when `status --porcelain` shows anything, untracked files included.
pub fn is_dirty(dir: &Path) -> Result<bool> {
    Ok(!Git::at(dir)
        .run(&[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all",
        ])?
        .trim()
        .is_empty())
}

/// The entry `git worktree list` in `repo` gives for `path`.
pub fn find_worktree(repo: &Path, path: &Path) -> Result<WorktreeInfo> {
    let want = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    worktrees(repo)?
        .into_iter()
        .find(|w| std::fs::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone()) == want)
        .with_context(|| format!("{} is not a worktree of {}", path.display(), repo.display()))
}

/// Locks the worktree with bale's reason. An existing bale lock counts as
/// success; a lock with any other reason is left untouched and is an error.
pub fn lock_worktree(repo: &Path, path: &Path) -> Result<()> {
    let info = find_worktree(repo, path)?;
    if let Some(reason) = info.foreign_lock() {
        bail!("{} is {reason}", path.display());
    }
    if info.locked.is_some() {
        return Ok(());
    }
    let p = path.to_string_lossy();
    Git::at(repo).run(&["worktree", "lock", "--reason", LOCK_REASON, &p])?;
    Ok(())
}

/// Unlocks only when the lock reason is bale's. Returns whether it unlocked.
pub fn unlock_worktree_if_ours(repo: &Path, path: &Path) -> Result<bool> {
    if find_worktree(repo, path)?.locked.as_deref() != Some(LOCK_REASON) {
        return Ok(false);
    }
    let p = path.to_string_lossy();
    Git::at(repo).run(&["worktree", "unlock", &p])?;
    Ok(true)
}
