//! git's post-checkout hook (`bale hook git post-checkout`), and its install
//! into a repo's hooks dir.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use bale_core::git::{self, Git};
use bale_core::ops::up::register;
use bale_core::BaleHome;

use crate::display_path;
use crate::install::{Plan, Step};

/// First line after the shebang of the git hook bale writes; marks it as
/// ours. It names the old `--git-hook` flag, but hooks already written carry
/// it, so it stays as the key that recognises them.
pub const GIT_HOOK_MARKER: &str = "# Installed by `bale install --git-hook`.";

/// post-checkout: registers the cwd when git just created it as a linked
/// worktree (`prev` all zeros, branch checkout). Never fails the checkout.
pub fn post_checkout(home: &BaleHome, prev: &str, _new: &str, flag: &str) -> ExitCode {
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

fn git_hook_script(exe: &Path) -> String {
    let exe = display_path(exe).replace('\\', "/").replace('\'', r"'\''");
    format!(
        "#!/bin/sh\n\
         {GIT_HOOK_MARKER}\n\
         # Registers new linked worktrees with bale, then runs the previous hook.\n\
         BALE='{exe}'\n\
         command -v \"$BALE\" >/dev/null 2>&1 || BALE=bale\n\
         if command -v \"$BALE\" >/dev/null 2>&1; then\n\
         \x20 \"$BALE\" hook git post-checkout \"$@\" || true\n\
         fi\n\
         PREV=\"$(dirname \"$0\")/post-checkout.pre-bale\"\n\
         if [ -x \"$PREV\" ]; then\n\
         \x20 exec \"$PREV\" \"$@\"\n\
         fi\n\
         exit 0\n"
    )
}

/// Installs bale's post-checkout hook in `repo`'s hooks dir, moving a
/// hook that is not bale's to `post-checkout.pre-bale` and chaining to it.
pub fn install_plan(repo: &Path, exe: &Path) -> Result<Plan> {
    let g = Git::at(repo);
    let dir = g.run(&["rev-parse", "--path-format=absolute", "--git-path", "hooks"])?;
    let dir = PathBuf::from(dir.trim_end_matches(['\n', '\r']));
    // `<scope>\t<value>`, e.g. `global\t~/.githooks`.
    let scope = g
        .run(&["config", "--show-scope", "--get", "core.hooksPath"])
        .ok()
        .and_then(|s| s.split_once('\t').map(|(scope, _)| scope.to_string()));
    let configured = scope.is_some();
    if configured {
        if let Ok(top) = g.run(&["rev-parse", "--show-toplevel"]) {
            let top = canon_lenient(Path::new(top.trim_end_matches(['\n', '\r'])));
            if canon_lenient(&dir).starts_with(&top) {
                bail!(
                    "core.hooksPath points at {} inside the working tree; bale does not \
                     change tracked hooks. Add this line to its post-checkout instead:\n  \
                     \"{}\" hook git post-checkout \"$@\" || true",
                    display_path(&dir),
                    display_path(exe).replace('\\', "/")
                );
            }
        }
    }

    let hook = dir.join("post-checkout");
    let prev = dir.join("post-checkout.pre-bale");
    let script = git_hook_script(exe);
    let mut steps = vec![Step::Mkdir(dir.clone())];
    if let Some(scope @ ("global" | "system")) = scope.as_deref() {
        steps.push(Step::Note(format!(
            "note: core.hooksPath comes from your {scope} git config, so this hook in {} \
             applies to every repo that uses it, not only {}",
            display_path(&dir),
            display_path(repo)
        )));
    }
    match fs::read(&hook) {
        Ok(bytes) => {
            let current = String::from_utf8_lossy(&bytes);
            if current.contains(GIT_HOOK_MARKER) {
                if current == script {
                    return Ok(Plan::new(vec![Step::Note(format!(
                        "{}: bale post-checkout hook already installed",
                        display_path(&hook)
                    ))]));
                }
                // Ours, from another bale binary path: rewrite in place.
            } else {
                if fs::symlink_metadata(&prev).is_ok() {
                    bail!(
                        "{} exists and {} is not bale's; move one of them, then rerun",
                        prev.display(),
                        hook.display()
                    );
                }
                steps.push(Step::Rename {
                    from: hook.clone(),
                    to: prev,
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", hook.display())),
    }
    steps.push(Step::Write {
        path: hook,
        content: script,
        executable: true,
    });
    Ok(Plan::new(steps))
}

/// Canonical form of `p`, or of its nearest existing ancestor joined with the
/// rest, so a hooks dir that does not exist yet still compares correctly.
fn canon_lenient(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = bale_core::paths::canonicalize(cur) {
            let mut out = c;
            out.extend(rest.iter().rev());
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_hook_script_runs_bale_once_and_chains() {
        let s = git_hook_script(Path::new("/opt/it's/bale"));
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains(GIT_HOOK_MARKER));
        assert!(s.contains(r"BALE='/opt/it'\''s/bale'"));
        assert_eq!(s.matches("hook git post-checkout").count(), 1);
        assert!(s.contains("exec \"$PREV\" \"$@\""));
    }
}
