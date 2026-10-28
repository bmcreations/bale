//! `bale hook`: entry points for Claude Code's WorktreeCreate/WorktreeRemove
//! hooks and git's post-checkout hook.
//!
//! Claude Code contract, from https://code.claude.com/docs/en/hooks (read
//! 2026-10-28, sections "WorktreeCreate" and "WorktreeRemove"):
//! - WorktreeCreate input adds `name` (a slug such as `bold-oak-a3f2`) to the
//!   common fields (`session_id`, `transcript_path`, `cwd`, `hook_event_name`).
//!   A command hook prints the path "as the last non-empty line of stdout";
//!   any other output goes to stderr. "Any non-zero exit code makes worktree
//!   creation fail." Claude Code refuses an absolute path containing `.` or
//!   `..` segments.
//! - Without the hook, Claude Code creates `.claude/worktrees/<name>/` at the
//!   repository root on branch `worktree-<name>`
//!   (https://code.claude.com/docs/en/worktrees); the fallback below does the same.
//! - WorktreeRemove input adds `worktree_path`, the absolute path the create
//!   hook returned. "Hook exits 0: the worktree counts as removed." On a
//!   non-zero exit "the removal fails if the directory at `worktree_path`
//!   still exists afterward". JSON output is discarded.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use bale_core::git::{self, Git};
use bale_core::ops::cold::cold;
use bale_core::ops::new::new_workspace;
use bale_core::ops::up::{register, resolve, Outcome};
use bale_core::BaleHome;
use serde_json::Value;

/// Reads the hook's JSON payload from stdin.
fn read_payload() -> Result<Value> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("reading the hook payload from stdin")?;
    serde_json::from_str(&text).context("parsing the hook payload")
}

fn field<'a>(payload: &'a Value, name: &str) -> Result<&'a str> {
    payload[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("hook payload has no {name:?} string"))
}

/// `path` as a string, without Windows' `\\?\` verbatim prefix.
pub fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => s.into_owned(),
    }
}

/// WorktreeCreate: creates the worktree with `bale new`, or with plain git
/// where Claude Code would have put it when bale fails, and prints only its
/// absolute path on stdout.
pub fn claude_worktree_create(home: &BaleHome) -> Result<ExitCode> {
    let payload = read_payload()?;
    let name = field(&payload, "name")?;
    let cwd = PathBuf::from(field(&payload, "cwd")?);
    let path = match new_workspace(home, &cwd, name, None) {
        Ok(record) => record.path,
        Err(e) => {
            eprintln!("bale: {e:#}; falling back to git worktree add");
            let path = git_fallback(&cwd, name)?;
            if let Err(e) = register(home, &path) {
                eprintln!("bale: not registered: {e:#}");
            }
            path
        }
    };
    println!("{}", display_path(&path));
    Ok(ExitCode::SUCCESS)
}

/// `git worktree add` at `<repo root>/.claude/worktrees/<name>` on branch
/// `worktree-<name>`, as Claude Code does without a WorktreeCreate hook.
fn git_fallback(cwd: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name.starts_with('-')
        || name.contains(['/', '\\'])
        || name == "."
        || name == ".."
    {
        bail!("worktree name {name:?} must be a single directory name");
    }
    let top = Git::at(cwd).run(&["rev-parse", "--show-toplevel"])?;
    let top = fs::canonicalize(top.trim_end_matches(['\n', '\r']))
        .context("resolving the repository root")?;
    let dir = top.join(".claude").join("worktrees");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(name);
    let branch = format!("worktree-{name}");
    let g = Git::at(&top);
    let p = path.to_string_lossy();
    let exists = g
        .run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .is_ok();
    if exists {
        g.run(&["worktree", "add", "--", &p, &branch])?;
    } else {
        g.run(&["worktree", "add", "-b", &branch, "--", &p])?;
    }
    Ok(fs::canonicalize(&path).unwrap_or(path))
}

/// WorktreeRemove: colds the worktree (saving any uncommitted work), so
/// its directory is gone. Exits 0 only when the directory no longer exists.
pub fn claude_worktree_remove(home: &BaleHome) -> Result<ExitCode> {
    let payload = read_payload()?;
    let path = PathBuf::from(field(&payload, "worktree_path")?);
    if fs::symlink_metadata(&path).is_err() {
        return Ok(ExitCode::SUCCESS);
    }
    // Keep this process's cwd out of the tree so it is not a busy holder.
    if let Some(parent) = path.parent() {
        let _ = std::env::set_current_dir(parent);
    }
    let code = match resolve(home, &path.to_string_lossy()).and_then(|id| cold(home, &id)) {
        Ok(Outcome::Done(_)) => ExitCode::SUCCESS,
        Ok(Outcome::Busy(holders)) => {
            eprintln!("bale: {} is busy, left untouched", path.display());
            for h in holders {
                eprintln!("  pid {} {} {}", h.pid, h.command, h.path.display());
            }
            ExitCode::from(3)
        }
        Ok(Outcome::Skipped(reason)) => {
            eprintln!("bale: {} skipped ({reason})", path.display());
            ExitCode::from(4)
        }
        Err(e) => {
            eprintln!("bale: {e:#}");
            ExitCode::from(1)
        }
    };
    if fs::symlink_metadata(&path).is_err() {
        return Ok(ExitCode::SUCCESS);
    }
    if code == ExitCode::SUCCESS {
        eprintln!("bale: {} still exists", path.display());
        return Ok(ExitCode::from(1));
    }
    Ok(code)
}

/// post-checkout: registers the cwd when git just created it as a linked
/// worktree (`prev` all zeros, branch checkout). Never fails the checkout.
pub fn git_post_checkout(home: &BaleHome, prev: &str, flag: &str) -> ExitCode {
    if flag == "1" && !prev.is_empty() && prev.chars().all(|c| c == '0') {
        let result = std::env::current_dir()
            .context("reading the current directory")
            .and_then(|cwd| {
                // A clone fires this hook in the main checkout; skip it quietly.
                if git::admin_dir(&cwd)? == git::common_dir(&cwd)? {
                    return Ok(());
                }
                register(home, &cwd).map(|_| ())
            });
        if let Err(e) = result {
            eprintln!("bale: post-checkout: {e:#}");
        }
    }
    ExitCode::SUCCESS
}
