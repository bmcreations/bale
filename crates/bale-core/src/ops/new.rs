//! `bale new`: create a linked worktree with `git worktree add` and register it.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::clones::share::materialize;
use crate::config::{load_config, Config};
use crate::git::{self, Git};
use crate::ops::up::register_untraced;
use crate::registry::{workspace_id, Record, Registry};
use crate::BaleHome;

/// Creates a worktree named `name` for `repo` and registers it `Hot`.
///
/// The path is `<worktree_root>/<repo-dir-name>/<name>` (a relative
/// `worktree_root` is taken from the main checkout), or
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
    crate::trace::op(home, "new", None, || new_untraced(home, repo, name, branch))
}

fn new_untraced(home: &BaleHome, repo: &Path, name: &str, branch: Option<&str>) -> Result<Record> {
    check_name(name)?;
    let branch = branch.unwrap_or(name);
    if branch.starts_with('-') {
        bail!("branch {branch:?} must not start with '-'");
    }
    let (main, root, cfg) = workspace_root(home, repo)?;
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
    let g = Git::at(&main);
    // A sparse checkout is left to git: `read-tree HEAD` would write every
    // path. Without sharing, or where bale can't clone, there is nothing to
    // gain from filling it here.
    let fill = cfg!(any(target_os = "macos", target_os = "linux"))
        && cfg.share_checkouts
        && !sparse_checkout(&g)?;
    let created = missing_ancestors(&root);
    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    let p = path.to_string_lossy();
    let filled = branch_exists(&g, branch).and_then(|exists| {
        let mut args = vec!["worktree", "add"];
        if fill {
            args.push("--no-checkout");
        }
        if !exists {
            args.extend(["-b", branch]);
        }
        args.extend(["--", &p]);
        if exists {
            args.push(branch);
        }
        g.run(&args)?;
        if !fill {
            return Ok(());
        }
        materialize(home, &path, &cfg).inspect_err(|_| {
            // The tree and, when made here, its branch are new and hold no
            // user changes yet.
            let _ = g.run(&["worktree", "remove", "--force", "--", &p]);
            if !exists {
                let _ = g.run(&["branch", "-D", "--", branch]);
            }
        })?;
        // `--no-checkout` skips the hook git runs after a checkout.
        if let Err(e) = post_checkout(&path) {
            eprintln!("bale: post-checkout hook in {}: {e:#}", path.display());
        }
        Ok(())
    });
    if let Err(e) = filled {
        // Undo only the dirs made above; remove_dir leaves any non-empty one.
        for dir in &created {
            let _ = fs::remove_dir(dir);
        }
        return Err(e);
    }
    register_untraced(home, &path)
        .with_context(|| format!("{} was created but could not be registered", path.display()))
}

/// Where `new_workspace` would put a worktree named `name` for `repo`,
/// without creating anything.
pub fn workspace_path(home: &BaleHome, repo: &Path, name: &str) -> Result<PathBuf> {
    check_name(name)?;
    Ok(workspace_root(home, repo)?.1.join(name))
}

/// The main checkout, the dir new worktrees of it go in, and its config.
fn workspace_root(home: &BaleHome, repo: &Path) -> Result<(PathBuf, PathBuf, Config)> {
    let main = main_checkout(repo)?;
    let repo_name = main
        .file_name()
        .with_context(|| format!("{} has no directory name", main.display()))?
        .to_os_string();
    let cfg = load_config(home, Some(&main))?;
    let root = match &cfg.worktree_root {
        // A relative root is relative to the main checkout, never the cwd.
        Some(r) => main.join(r).join(&repo_name),
        None => {
            let mut dir = repo_name.clone();
            dir.push("-worktrees");
            main.parent()
                .with_context(|| format!("{} has no parent directory", main.display()))?
                .join(dir)
        }
    };
    Ok((main, root, cfg))
}

/// Whether `core.sparseCheckout` is on, in any of git's bool spellings.
fn sparse_checkout(g: &Git) -> Result<bool> {
    let v = g.run(&[
        "config",
        "--bool",
        "--default",
        "false",
        "--get",
        "core.sparseCheckout",
    ])?;
    Ok(v.trim() == "true")
}

/// Runs the repo's post-checkout hook in `tree` as git would after checking
/// out a new worktree: null old head, the new head, branch checkout.
fn post_checkout(tree: &Path) -> Result<()> {
    let g = Git::at(tree);
    let head = g.run(&["rev-parse", "HEAD"])?;
    let head = head.trim();
    // As long as the oid: SHA-1 and SHA-256 repos differ.
    let null = "0".repeat(head.len());
    g.run(&[
        "hook",
        "run",
        "--ignore-missing",
        "post-checkout",
        "--",
        &null,
        head,
        "1",
    ])?;
    Ok(())
}

/// Accepts only a single plain path component (no `.`, `..`, separator or
/// empty name) that does not start with `-`. `\` is rejected on every OS so
/// all platforms accept the same names.
fn check_name(name: &str) -> Result<()> {
    let mut parts = Path::new(name).components();
    let single = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    );
    if !single || name.contains('\\') || name.starts_with('-') {
        bail!("workspace name {name:?} must be a single directory name not starting with '-'");
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
    let repo = crate::paths::canonicalize(repo)
        .with_context(|| format!("resolving {}", repo.display()))?;
    let main = git::worktrees(&repo)?
        .into_iter()
        .find(|w| w.is_main)
        .with_context(|| format!("{} lists no worktrees", repo.display()))?;
    Ok(crate::paths::canonicalize(&main.path).unwrap_or(main.path))
}

/// Whether `refs/heads/<branch>` exists. Matches the exact ref, since
/// for-each-ref also matches refs below it.
fn branch_exists(g: &Git, branch: &str) -> Result<bool> {
    let want = format!("refs/heads/{branch}");
    let out = g.run(&["for-each-ref", "--format=%(refname)", &want])?;
    Ok(out.lines().any(|l| l == want))
}
