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
use bale_core::ops::cold::cold_ignoring;
use bale_core::ops::new::{new_workspace, workspace_path};
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

/// WorktreeCreate: creates the worktree with `bale new` on Claude Code's
/// branch name `worktree-<name>`, or with plain git where Claude Code would
/// have put it when bale fails, and prints only its absolute path on stdout.
pub fn claude_worktree_create(home: &BaleHome) -> Result<ExitCode> {
    let payload = read_payload()?;
    let name = field(&payload, "name")?;
    let cwd = PathBuf::from(field(&payload, "cwd")?);
    let branch = format!("worktree-{name}");
    // Paths that exist now belong to someone else; only one that appears
    // during this call can be the worktree bale itself made.
    let fresh: Vec<PathBuf> = [
        workspace_path(home, &cwd, name).ok(),
        fallback_path(&cwd, name).ok(),
    ]
    .into_iter()
    .flatten()
    .filter(|p| fs::symlink_metadata(p).is_err())
    .collect();
    let path = match new_workspace(home, &cwd, name, Some(&branch)) {
        Ok(record) => record.path,
        Err(e) => {
            eprintln!("bale: {e:#}");
            let path = match existing_worktree(&cwd, fresh.iter()) {
                // bale made it before failing; never add a second one.
                Some(path) => path,
                None => {
                    eprintln!("bale: falling back to git worktree add");
                    git_fallback(&cwd, name, &branch)?
                }
            };
            if let Err(e) = register(home, &path) {
                eprintln!("bale: not registered: {e:#}");
            }
            path
        }
    };
    println!("{}", display_path(&path));
    Ok(ExitCode::SUCCESS)
}

/// The first of `candidates` that is a linked worktree of `cwd`'s repo.
/// Callers pass only paths that did not exist before this hook ran.
fn existing_worktree<'a>(
    cwd: &Path,
    candidates: impl Iterator<Item = &'a PathBuf>,
) -> Option<PathBuf> {
    let listed: Vec<PathBuf> = git::worktrees(cwd)
        .ok()?
        .into_iter()
        .filter(|w| !w.is_main)
        .map(|w| bale_core::paths::canonicalize(&w.path).unwrap_or(w.path))
        .collect();
    candidates
        .filter_map(|c| bale_core::paths::canonicalize(c).ok())
        .find(|c| listed.contains(c))
}

/// `<main checkout>/.claude/worktrees/<name>`. From the main checkout that is
/// its `--show-toplevel`, which is right for a submodule or a
/// `--separate-git-dir` repo too. From a linked worktree it is the first
/// `git worktree list` entry, so the fallback still roots at the main
/// checkout as Claude Code does. (For a submodule or `--separate-git-dir`
/// repo git lists the git dir there, and keeps no other record of the main
/// working tree.)
fn fallback_path(cwd: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name.starts_with('-')
        || name.contains(['/', '\\'])
        || name == "."
        || name == ".."
    {
        bail!("worktree name {name:?} must be a single directory name");
    }
    let main = if git::admin_dir(cwd)? == git::common_dir(cwd)? {
        let top = Git::at(cwd).run(&["rev-parse", "--show-toplevel"])?;
        PathBuf::from(top.trim_end_matches(['\n', '\r']))
    } else {
        git::worktrees(cwd)?
            .into_iter()
            .find(|w| w.is_main)
            .with_context(|| format!("{} lists no worktrees", cwd.display()))?
            .path
    };
    let main = bale_core::paths::canonicalize(&main).context("resolving the main checkout")?;
    Ok(main.join(".claude").join("worktrees").join(name))
}

/// `git worktree add` at [`fallback_path`] on `branch`, as Claude Code does
/// without a WorktreeCreate hook.
fn git_fallback(cwd: &Path, name: &str, branch: &str) -> Result<PathBuf> {
    let path = fallback_path(cwd, name)?;
    let dir = path.parent().expect("fallback path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let g = Git::at(dir.parent().and_then(Path::parent).expect("main checkout"));
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
        g.run(&["worktree", "add", "--", &p, branch])?;
    } else {
        g.run(&["worktree", "add", "-b", branch, "--", &p])?;
    }
    Ok(bale_core::paths::canonicalize(&path).unwrap_or(path))
}

/// The pids whose hold on the tree comes from running this hook: this
/// process and its parent. When the parent is a shell (Claude Code runs
/// `sh -c`, which only stays around for a compound command), also the
/// grandparent, which is Claude Code. When the parent is not a shell,
/// `sh -c` exec'd bale and the parent is Claude Code itself (named `node`
/// for an npm install), so the walk stops there. Nothing above that (such
/// as the user's shell that started Claude Code, or another `claude`
/// process) is included. `info` gives a pid's parent pid and process name
/// or exe.
pub(crate) fn invoking_pids(own: u32, info: impl Fn(u32) -> Option<(u32, String)>) -> Vec<u32> {
    let mut pids = vec![own];
    let Some((parent, _)) = info(own) else {
        return pids;
    };
    if parent <= 1 {
        return pids;
    }
    pids.push(parent);
    let Some((grandparent, parent_name)) = info(parent) else {
        return pids;
    };
    if is_shell(&parent_name) && grandparent > 1 {
        pids.push(grandparent);
    }
    pids
}

/// `sh`, `bash`, `dash`, `zsh` or `fish`, by basename; a login shell's
/// leading `-` is ignored.
fn is_shell(name: &str) -> bool {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name);
    matches!(
        base.trim_start_matches('-'),
        "sh" | "bash" | "dash" | "zsh" | "fish"
    )
}

/// `(parent pid, command)` for `pid` from `ps`; `None` where unavailable.
fn process_info(pid: u32) -> Option<(u32, String)> {
    if cfg!(windows) {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (ppid, comm) = text.trim().split_once(char::is_whitespace)?;
    Some((ppid.trim().parse().ok()?, comm.trim().to_string()))
}

/// WorktreeRemove: colds the worktree (saving any uncommitted work), so
/// its directory is gone. Exits 0 only when the directory no longer exists.
pub fn claude_worktree_remove(home: &BaleHome) -> Result<ExitCode> {
    let payload = read_payload()?;
    let path = PathBuf::from(field(&payload, "worktree_path")?);
    if fs::symlink_metadata(&path).is_err() {
        return Ok(ExitCode::SUCCESS);
    }
    // Keep this process's cwd out of the tree, and ignore the processes
    // that are only waiting on this hook.
    if let Some(parent) = path.parent() {
        let _ = std::env::set_current_dir(parent);
    }
    let ignore = invoking_pids(std::process::id(), process_info);
    let code = match resolve(home, &path.to_string_lossy())
        .and_then(|id| cold_ignoring(home, &id, &ignore))
    {
        Ok(Outcome::Done(_)) => ExitCode::SUCCESS,
        Ok(Outcome::Busy(holders)) => {
            eprint!("{}", busy_text(&path, &holders));
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

/// What bale says for cold's empty `Busy`, after the tree's path or id: the
/// tree changed after it was saved, or the OS refused to move it because
/// something had it open that lsof did not report.
pub(crate) const CHANGED_DURING_COLD: &str =
    "changed or was in use while bale was making it cold, left untouched; try again";

/// What the remove hook prints for a busy tree. No holders means the tree
/// changed or was in use while cold was saving it (see
/// [`CHANGED_DURING_COLD`]).
fn busy_text(path: &Path, holders: &[bale_core::busy::Holder]) -> String {
    if holders.is_empty() {
        return format!("bale: {} {CHANGED_DURING_COLD}\n", path.display());
    }
    let mut text = format!(
        "bale: {} is busy, left untouched; these processes hold it \
         (Claude Code's background tasks and servers count):\n",
        path.display()
    );
    for h in holders {
        text.push_str(&format!(
            "  pid {} {} {}\n",
            h.pid,
            h.command,
            h.path.display()
        ));
    }
    text
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn busy_without_holders_says_the_tree_changed() {
        let text = busy_text(Path::new("/w/t"), &[]);
        assert!(!text.contains("these processes hold it"), "{text}");
        assert!(text.contains(CHANGED_DURING_COLD), "{text}");
    }

    #[test]
    fn busy_with_holders_lists_them() {
        let holder = bale_core::busy::Holder {
            pid: 7,
            command: "gradle".into(),
            path: PathBuf::from("/w/t"),
        };
        let text = busy_text(Path::new("/w/t"), &[holder]);
        assert!(text.contains("these processes hold it"), "{text}");
        assert!(text.contains("pid 7 gradle /w/t"), "{text}");
    }

    fn table(rows: &[(u32, u32, &str)]) -> impl Fn(u32) -> Option<(u32, String)> {
        let map: HashMap<u32, (u32, String)> = rows
            .iter()
            .map(|(pid, ppid, name)| (*pid, (*ppid, name.to_string())))
            .collect();
        move |pid| map.get(&pid).cloned()
    }

    #[test]
    fn stops_at_claude_and_never_reaches_the_users_shell() {
        // zsh(10) -> claude(20) -> sh(30) -> bale(40)
        let info = table(&[
            (10, 1, "-zsh"),
            (20, 10, "/Users/me/.local/bin/claude"),
            (30, 20, "/bin/sh"),
            (40, 30, "bale"),
        ]);
        assert_eq!(invoking_pids(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn never_climbs_to_a_higher_claude() {
        // claude(5) -> zsh(10) -> node(20) -> sh(30) -> bale(40): an outer
        // Claude Code session above the user's shell stays a holder.
        let info = table(&[
            (5, 1, "claude"),
            (10, 5, "zsh"),
            (20, 10, "node"),
            (30, 20, "sh"),
            (40, 30, "bale"),
        ]);
        assert_eq!(invoking_pids(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn falls_back_to_the_grandparent() {
        // zsh(10) -> node(20) -> sh(30) -> bale(40)
        let info = table(&[
            (10, 1, "zsh"),
            (20, 10, "node"),
            (30, 20, "sh"),
            (40, 30, "bale"),
        ]);
        assert_eq!(invoking_pids(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn non_shell_parent_ends_the_chain() {
        // zsh(10) -> node(20) -> bale(40): `sh -c` exec'd bale, so the
        // grandparent is the user's shell and stays a holder.
        let info = table(&[(10, 1, "-zsh"), (20, 10, "node"), (40, 20, "bale")]);
        assert_eq!(invoking_pids(40, info), vec![40, 20]);
    }

    #[test]
    fn login_shell_parent_takes_the_grandparent() {
        // node(20) -> -bash(30) -> bale(40)
        let info = table(&[
            (10, 1, "zsh"),
            (20, 10, "node"),
            (30, 20, "-bash"),
            (40, 30, "bale"),
        ]);
        assert_eq!(invoking_pids(40, info), vec![40, 30, 20]);
    }

    #[test]
    fn parent_that_is_claude_ends_the_chain() {
        let info = table(&[(10, 1, "zsh"), (20, 10, "claude"), (40, 20, "bale")]);
        assert_eq!(invoking_pids(40, info), vec![40, 20]);
    }

    #[test]
    fn unknown_process_table_excludes_only_self() {
        assert_eq!(invoking_pids(40, |_| None), vec![40]);
    }
}
