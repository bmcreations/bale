//! Thin wrapper over the `git` binary: worktree listing, locking, status.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::registry::Scope;

/// Lock reason bale writes; only locks with this reason are ever released.
pub const LOCK_REASON: &str = "baled by bale";

const BACKOFF_MS: [u64; 5] = [100, 200, 400, 800, 1600];

/// Config every bale git call overrides. A user's or repo's setting must not
/// make bale see an unchanged tree where files exist: `status` must list
/// every untracked file and every submodule change, and no cache or
/// fsmonitor may stand in for reading the disk. `-c` outranks every config
/// file and `GIT_CONFIG_PARAMETERS`, and reaches the git processes `git
/// submodule foreach` starts, so status calls rely on it for untracked
/// files. It does not outrank `submodule.<name>.ignore` from `.gitmodules`,
/// so status calls also pass `--ignore-submodules`.
const OVERRIDES: [&str; 4] = [
    "status.showUntrackedFiles=all",
    "core.fsmonitor=false",
    "core.untrackedCache=false",
    "diff.ignoreSubmodules=none",
];

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

    fn output(&self, args: &[&str]) -> Result<std::process::Output> {
        Command::new("git")
            .args(OVERRIDES.iter().flat_map(|o| ["-c", o]))
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
            .with_context(|| format!("failed to run git {}", args.join(" ")))
    }

    /// Runs `git <args>` in the directory and returns stdout. Retries up to five
    /// times (100, 200, 400, 800, 1600 ms) when stderr reports a held
    /// `index.lock` or ref lock; any other failure returns immediately.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let mut attempt = 0;
        loop {
            let out = self.output(args)?;
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
            return Err(GitError::new(args, &self.dir, &stderr).into());
        }
    }
}

/// A git command that exited with failure.
#[derive(Debug)]
pub struct GitError {
    message: String,
}

impl GitError {
    fn new(args: &[&str], dir: &Path, stderr: &str) -> GitError {
        GitError {
            message: format!(
                "git {} failed in {}: {}",
                args.join(" "),
                dir.display(),
                stderr.trim()
            ),
        }
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GitError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub is_main: bool,
    /// `Some(reason)` when locked (reason may be empty).
    pub locked: Option<String>,
    pub prunable: bool,
    /// The entry is a bare repository, which has no checkout.
    pub bare: bool,
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
                bare: false,
            });
            continue;
        }
        let Some(cur) = list.last_mut() else { continue };
        if let Some(h) = line.strip_prefix("HEAD ") {
            cur.head = h.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "bare" {
            cur.bare = true;
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

/// The branch checked out at `dir` (without `refs/heads/`), or `None` when
/// HEAD is detached.
pub fn current_branch(dir: &Path) -> Result<Option<String>> {
    let args = ["symbolic-ref", "-q", "HEAD"];
    let out = Git::at(dir).output(&args)?;
    if out.status.code() == Some(1) && out.stderr.is_empty() {
        return Ok(None);
    }
    if !out.status.success() {
        return Err(GitError::new(&args, dir, &String::from_utf8_lossy(&out.stderr)).into());
    }
    let full = String::from_utf8_lossy(&out.stdout);
    let full = full.trim_end_matches(['\n', '\r']);
    Ok(Some(
        full.strip_prefix("refs/heads/").unwrap_or(full).to_string(),
    ))
}

/// True when `status --porcelain` shows anything, untracked files and
/// submodule changes included.
pub fn is_dirty(dir: &Path) -> Result<bool> {
    Ok(!Git::at(dir)
        .run(&[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--ignore-submodules=none",
        ])?
        .trim()
        .is_empty())
}

/// The entry `git worktree list` in `repo` gives for `path`.
pub fn find_worktree(repo: &Path, path: &Path) -> Result<WorktreeInfo> {
    let want = crate::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    worktrees(repo)?
        .into_iter()
        .find(|w| crate::paths::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone()) == want)
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

/// Most commit subjects a [`Scope`] keeps.
const SCOPE_SUBJECTS: &str = "8";

/// The newest subject [`scope`] would list first, at the cost of one or two
/// git processes instead of scope's seven: the newest commit in
/// `@{upstream}..HEAD`, or with no upstream, in `HEAD --not --remotes`.
/// `None` when that range is empty.
pub fn latest_subject(root: &Path) -> Result<Option<String>> {
    let g = Git::at(root);
    let log = ["log", "-1", "--format=%s"];
    // Fails when no upstream is set (or it names a ref that is gone).
    let out = match g.run(&[&log[..], &["@{upstream}..HEAD"]].concat()) {
        Ok(out) => out,
        Err(_) => g.run(&[&log[..], &["HEAD", "--not", "--remotes"]].concat())?,
    };
    Ok(out.lines().next().map(str::to_string))
}

/// The tree's branch, upstream, unpushed commits and uncommitted changes.
pub fn scope(root: &Path) -> Result<Scope> {
    let g = Git::at(root);
    let count = |args: &[&str]| -> Result<u32> {
        let out = g.run(args)?;
        out.trim()
            .parse()
            .with_context(|| format!("git {} printed {out:?}", args.join(" ")))
    };
    // Fails when no upstream is set (or it names a ref that is gone).
    let upstream = g
        .run(&[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let (ahead, behind) = match &upstream {
        Some(_) => {
            let out = g.run(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
            let mut n = out.split_whitespace().map(str::parse::<u32>);
            match (n.next(), n.next()) {
                (Some(Ok(a)), Some(Ok(b))) => (a, b),
                _ => bail!("git rev-list --left-right printed {out:?}"),
            }
        }
        None => (0, 0),
    };
    let unpushed = count(&["rev-list", "--count", "HEAD", "--not", "--remotes"])?;
    let log = ["log", "--format=%s", "-n", SCOPE_SUBJECTS];
    let subjects = match &upstream {
        Some(_) => g.run(&[&log[..], &["@{upstream}..HEAD"]].concat())?,
        None => g.run(&[&log[..], &["HEAD", "--not", "--remotes"]].concat())?,
    };
    let last = g.run(&["log", "-1", "--format=%ct", "HEAD"])?;
    let last_commit_unix = last
        .trim()
        .parse()
        .with_context(|| format!("git log printed {last:?}"))?;
    let (changed, insertions, deletions) =
        parse_shortstat(&g.run(&["--no-optional-locks", "diff", "--shortstat", "HEAD"])?);
    let untracked = g.run(&[
        "--no-optional-locks",
        "ls-files",
        "-z",
        "--others",
        "--exclude-standard",
    ])?;
    let untracked = untracked.split('\0').filter(|p| !p.is_empty()).count() as u32;
    Ok(Scope {
        branch: current_branch(root)?,
        upstream,
        ahead,
        behind,
        unpushed,
        subjects: subjects.lines().map(str::to_string).collect(),
        last_commit_unix,
        changed_files: changed + untracked,
        insertions,
        deletions,
    })
}

/// `(files, insertions, deletions)` from `git diff --shortstat`, e.g.
/// ` 2 files changed, 3 insertions(+), 1 deletion(-)`; zeros when empty.
fn parse_shortstat(text: &str) -> (u32, u32, u32) {
    let (mut files, mut ins, mut del) = (0, 0, 0);
    for part in text.trim().split(',') {
        let mut words = part.split_whitespace();
        let (Some(n), Some(what)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(n) = n.parse() else { continue };
        if what.starts_with("file") {
            files = n;
        } else if what.starts_with("insertion") {
            ins = n;
        } else if what.starts_with("deletion") {
            del = n;
        }
    }
    (files, ins, del)
}

#[cfg(test)]
mod tests {
    use super::parse_shortstat;

    #[test]
    fn shortstat_parses_every_shape() {
        assert_eq!(parse_shortstat(""), (0, 0, 0));
        assert_eq!(
            parse_shortstat(" 2 files changed, 3 insertions(+), 1 deletion(-)\n"),
            (2, 3, 1)
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 1 deletion(-)\n"),
            (1, 0, 1)
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 4 insertions(+)\n"),
            (1, 4, 0)
        );
    }
}
