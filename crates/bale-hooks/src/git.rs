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
/// ours.
pub const GIT_HOOK_MARKER: &str = "# Installed by `bale install --hooks git`.";

/// The marker hooks written before `--hooks` carry; still recognised as bale's.
const OLD_GIT_HOOK_MARKER: &str = "# Installed by `bale install --git-hook`.";

/// post-checkout: registers the cwd when git just created it as a linked
/// worktree (`prev` all zeros, branch checkout). Never fails the checkout.
pub fn post_checkout(home: &BaleHome, prev: &str, _new: &str, flag: &str) -> ExitCode {
    crate::as_hook(crate::Tool::Git, || {
        post_checkout_untraced(home, prev, flag)
    })
}

fn post_checkout_untraced(home: &BaleHome, prev: &str, flag: &str) -> ExitCode {
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
    render_hook(GIT_HOOK_MARKER, "git post-checkout", &exe)
}

/// The hook script with `marker`, running `bale hook <sub>`, with `quoted`
/// as the text inside `BALE='...'`.
fn render_hook(marker: &str, sub: &str, quoted: &str) -> String {
    format!(
        "#!/bin/sh\n\
         {marker}\n\
         # Registers new linked worktrees with bale, then runs the previous hook.\n\
         BALE='{quoted}'\n\
         command -v \"$BALE\" >/dev/null 2>&1 || BALE=bale\n\
         if command -v \"$BALE\" >/dev/null 2>&1; then\n\
         \x20 \"$BALE\" hook {sub} \"$@\" || true\n\
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
    let dir = hooks_dir(&g)?;
    let scope = hooks_path_scope(&g);
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
            if is_bale_hook(&current) {
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

/// Where `core.hooksPath` is set (`global`, `local`, ...), if it is.
fn hooks_path_scope(g: &Git) -> Option<String> {
    // `<scope>\t<value>`, e.g. `global\t~/.githooks`.
    g.run(&["config", "--show-scope", "--get", "core.hooksPath"])
        .ok()
        .and_then(|s| s.split_once('\t').map(|(scope, _)| scope.to_string()))
}

/// The repo's hooks dir, absolute, honouring `core.hooksPath`.
fn hooks_dir(g: &Git) -> Result<PathBuf> {
    let dir = g.run(&["rev-parse", "--path-format=absolute", "--git-path", "hooks"])?;
    Ok(PathBuf::from(dir.trim_end_matches(['\n', '\r'])))
}

/// Whether `script` is a post-checkout hook bale wrote.
fn is_bale_hook(script: &str) -> bool {
    script
        .lines()
        .any(|l| l == GIT_HOOK_MARKER || l == OLD_GIT_HOOK_MARKER)
}

/// Whether `script` is exactly a hook some bale wrote: this one's, or an
/// earlier one's (the `--git-hook` marker, the `hook git-post-checkout`
/// command), for whatever exe its `BALE=` line names.
fn is_unedited_bale_hook(script: &str) -> bool {
    let Some(quoted) = script
        .lines()
        .find_map(|l| l.strip_prefix("BALE='")?.strip_suffix('\''))
    else {
        return false;
    };
    [
        (GIT_HOOK_MARKER, "git post-checkout"),
        (OLD_GIT_HOOK_MARKER, "git post-checkout"),
        (OLD_GIT_HOOK_MARKER, "git-post-checkout"),
    ]
    .iter()
    .any(|(marker, sub)| render_hook(marker, sub, quoted) == script)
}

/// Removes bale's post-checkout hook from `repo`'s hooks dir, putting back
/// the hook it chained to, `post-checkout.pre-bale`. A hook without bale's
/// marker is the user's and stays; so does bale's hook once the user edited
/// it.
pub fn uninstall_plan(repo: &Path) -> Result<Plan> {
    let g = Git::at(repo);
    let dir = hooks_dir(&g)?;
    let mut steps = Vec::new();
    if let Some(scope @ ("global" | "system")) = hooks_path_scope(&g).as_deref() {
        steps.push(Step::Note(format!(
            "note: core.hooksPath comes from your {scope} git config, so {} serves every \
             repo that uses it, not only {}",
            display_path(&dir),
            display_path(repo)
        )));
    }
    let hook = dir.join("post-checkout");
    let prev = dir.join("post-checkout.pre-bale");
    let script = match fs::read(&hook) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            steps.push(Step::Note(format!(
                "{}: no bale post-checkout hook to remove",
                display_path(&hook)
            )));
            return Ok(Plan::new(steps));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", hook.display())),
    };
    if !is_bale_hook(&script) {
        steps.push(Step::Note(format!(
            "note: left unchanged: {} is not bale's",
            display_path(&hook)
        )));
        return Ok(Plan::new(steps));
    }
    if !is_unedited_bale_hook(&script) {
        steps.push(Step::Note(format!(
            "note: left unchanged: {} is bale's hook but was edited; remove bale's lines \
             by hand{}",
            display_path(&hook),
            if fs::symlink_metadata(&prev).is_ok() {
                format!(", then move {} back over it", display_path(&prev))
            } else {
                String::new()
            }
        )));
        return Ok(Plan::new(steps));
    }
    // A rename replaces the hook in one step, so there is never no hook.
    steps.push(if fs::symlink_metadata(&prev).is_ok() {
        Step::Rename {
            from: prev,
            to: hook,
        }
    } else {
        Step::Remove(hook)
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
    fn hook_marker_names_the_current_flag_and_old_hooks_still_count() {
        let s = git_hook_script(Path::new("/opt/bale"));
        assert_eq!(
            s.lines().nth(1),
            Some("# Installed by `bale install --hooks git`.")
        );
        assert!(!s.contains("--git-hook"), "{s}");
        assert!(is_bale_hook(&s));
        let old = s.replace(
            "# Installed by `bale install --hooks git`.",
            "# Installed by `bale install --git-hook`.",
        );
        assert!(is_bale_hook(&old));
        assert!(!is_bale_hook("#!/bin/sh\necho mine\n"));
    }

    #[test]
    fn only_an_unedited_hook_counts_as_unedited() {
        let s = git_hook_script(Path::new("/opt/it's/bale"));
        assert!(is_unedited_bale_hook(&s));
        let old = s
            .replace(GIT_HOOK_MARKER, OLD_GIT_HOOK_MARKER)
            .replace("hook git post-checkout", "hook git-post-checkout");
        assert!(is_unedited_bale_hook(&old));
        assert!(!is_unedited_bale_hook(&format!("{s}echo more\n")));
        assert!(!is_unedited_bale_hook(&s.replace("exit 0", "exit 1")));
    }

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
