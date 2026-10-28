//! `bale new`: create a linked worktree with `git worktree add` and register it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::load_config;
use crate::git::{self, Git};
use crate::ops::up::register;
use crate::registry::{workspace_id, Record, Registry};
use crate::BaleHome;

/// Creates a worktree named `name` for `repo` and registers it `Hot`.
///
/// The path is `<worktree_root>/<repo-dir-name>/<name>`, or
/// `<repo>/../<repo-dir-name>-worktrees/<name>` without a configured root,
/// where `<repo>` is the main checkout. The branch (`name` by default) is
/// created when it does not exist and checked out when it does. Refuses a
/// path that exists or that a record already names.
pub fn new_workspace(
    home: &BaleHome,
    repo: &Path,
    name: &str,
    branch: Option<&str>,
) -> Result<Record> {
    check_name(name)?;
    let branch = branch.unwrap_or(name);
    if branch.starts_with('-') {
        bail!("branch {branch:?} must not start with '-'");
    }
    let main = main_checkout(repo)?;
    let repo_name = main
        .file_name()
        .with_context(|| format!("{} has no directory name", main.display()))?
        .to_os_string();
    let cfg = load_config(home, Some(&main))?;
    let root = match cfg.worktree_root {
        Some(r) => r.join(&repo_name),
        None => {
            let mut dir = repo_name.clone();
            dir.push("-worktrees");
            main.parent()
                .with_context(|| format!("{} has no parent directory", main.display()))?
                .join(dir)
        }
    };
    let path = root.join(name);

    if fs::symlink_metadata(&path).is_ok() {
        bail!("{} already exists", path.display());
    }
    let id = workspace_id(&path);
    if let Some(r) = Registry::open(home.clone()).get(&id)? {
        bail!(
            "{} belongs to registered workspace {id} ({})",
            path.display(),
            r.level.name()
        );
    }
    let created = missing_ancestors(&root);
    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    let g = Git::at(&main);
    let p = path.to_string_lossy();
    let added = branch_exists(&g, branch).and_then(|exists| {
        if exists {
            g.run(&["worktree", "add", "--", &p, branch])
        } else {
            g.run(&["worktree", "add", "-b", branch, "--", &p])
        }
    });
    if let Err(e) = added {
        // Undo only the dirs made above; remove_dir leaves any non-empty one.
        for dir in &created {
            let _ = fs::remove_dir(dir);
        }
        return Err(e);
    }
    register(home, &path)
        .with_context(|| format!("{} was created but could not be registered", path.display()))
}

/// Rejects names that are empty, contain a path separator or `..`, or start
/// with `-`. Both separators are rejected on every OS.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("workspace name is empty");
    }
    if name.contains(['/', '\\']) || name.contains("..") || name.starts_with('-') {
        bail!("workspace name {name:?} must not contain '/', '\\' or '..', or start with '-'");
    }
    Ok(())
}

/// `dir` and its ancestors that do not exist yet, deepest first.
fn missing_ancestors(dir: &Path) -> Vec<PathBuf> {
    dir.ancestors()
        .take_while(|a| !a.as_os_str().is_empty() && fs::symlink_metadata(a).is_err())
        .map(Path::to_path_buf)
        .collect()
}

/// The repo's main checkout: the first entry of `git worktree list`.
fn main_checkout(repo: &Path) -> Result<PathBuf> {
    let repo = fs::canonicalize(repo).with_context(|| format!("resolving {}", repo.display()))?;
    let main = git::worktrees(&repo)?
        .into_iter()
        .find(|w| w.is_main)
        .with_context(|| format!("{} lists no worktrees", repo.display()))?;
    Ok(fs::canonicalize(&main.path).unwrap_or(main.path))
}

/// Whether `refs/heads/<branch>` exists. Matches the exact ref, since
/// for-each-ref also matches refs below it.
fn branch_exists(g: &Git, branch: &str) -> Result<bool> {
    let want = format!("refs/heads/{branch}");
    let out = g.run(&["for-each-ref", "--format=%(refname)", &want])?;
    Ok(out.lines().any(|l| l == want))
}
