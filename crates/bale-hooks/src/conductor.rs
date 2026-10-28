//! Conductor's setup and archive scripts (`bale hook conductor setup|archive`),
//! and their install into a repo's `.conductor/settings.toml`.
//!
//! Unverified against the Conductor app: it is not installed where this was
//! written. The contract below is from conductor.build/docs/reference/scripts
//! (read 2026-10-28):
//! - `.conductor/settings.toml` holds a `[scripts]` table whose `setup` and
//!   `archive` strings run when a workspace is created and before it is
//!   archived. The file is usually committed with the repo.
//! - Scripts get `CONDUCTOR_WORKSPACE_PATH` (the workspace checkout),
//!   `CONDUCTOR_ROOT_PATH` and `CONDUCTOR_WORKSPACE_NAME`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use bale_core::ops::up::register;
use bale_core::BaleHome;
use toml_edit::{DocumentMut, Value};

use crate::claude::sh_quote;
use crate::install::{Plan, Step};
use crate::{display_path, remove_tree, Tool};

/// The workspace a script runs for: `CONDUCTOR_WORKSPACE_PATH`, or the cwd.
fn workspace_path() -> Result<PathBuf> {
    match std::env::var_os("CONDUCTOR_WORKSPACE_PATH") {
        Some(p) if !p.is_empty() => Ok(PathBuf::from(p)),
        _ => std::env::current_dir().context("reading the current directory"),
    }
}

/// setup: registers the workspace with bale. Never fails Conductor's setup.
pub fn setup(home: &BaleHome) -> ExitCode {
    crate::as_hook(Tool::Conductor, || {
        match workspace_path().and_then(|path| register(home, &path)) {
            Ok(r) => println!("bale: {}: {}", r.id, r.level.name()),
            Err(e) => eprintln!("bale: conductor setup: {e:#}"),
        }
    });
    ExitCode::SUCCESS
}

/// archive: colds the workspace (registering it first if needed), so its
/// directory is gone. Exits as [`remove_tree`]: 0 gone, 3 busy, 4 skipped,
/// 1 error.
pub fn archive(home: &BaleHome) -> ExitCode {
    match workspace_path() {
        // `resolve` in remove_tree registers an existing, unregistered path.
        Ok(path) => remove_tree(home, &path, Tool::Conductor),
        Err(e) => {
            eprintln!("bale: conductor archive: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// The command bale's script for `kind` runs.
fn hook_command(exe: &Path, kind: &str) -> String {
    format!(
        "{} hook conductor {kind}",
        sh_quote(&display_path(exe).replace('\\', "/"))
    )
}

/// `existing` with bale's `want` chained after it. The user's script runs
/// in a subshell closed on its own line: a trailing `# comment` or heredoc
/// cannot swallow bale's command, and its `exit`, `set -e` or `cd` stays
/// inside, so bale runs from the directory Conductor started in.
fn chain(kind: &str, existing: &str, want: &str) -> String {
    let existing = existing.trim_end();
    if existing.trim().is_empty() {
        return want.to_string();
    }
    match kind {
        // bale registers only after the user's setup succeeds; a failing
        // setup keeps its status.
        "setup" => format!("( {existing}\n) && {want}"),
        // The user's archive runs first, while the tree still exists: once
        // bale colds it the directory is gone, and a cleanup step such as
        // `docker compose down` may need it. bale runs even when that step
        // fails or exits, and its status (0 gone, 3 busy, 4 skipped, 1 error)
        // is the script's.
        _ => format!("( {existing}\n); {want}"),
    }
}

/// Chains bale into `<repo>/.conductor/settings.toml`'s `[scripts]` setup and
/// archive, keeping the user's commands, other keys and comments.
pub fn install_plan(repo: &Path, exe: &Path) -> Result<Plan> {
    let path = repo.join(".conductor").join("settings.toml");
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
        bail!(
            "{} is a symlink; bale will not replace it with a regular file. Add \
             `{}` to the end of [scripts] setup and `{}` to the start of archive by hand",
            path.display(),
            hook_command(exe, "setup"),
            hook_command(exe, "archive")
        );
    }
    let mode = meta.map(|m| m.permissions());
    let before = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: DocumentMut =
        before.as_deref().unwrap_or("").parse().with_context(|| {
            format!("{} is not valid TOML; leaving it unchanged", path.display())
        })?;
    let scripts = doc
        .entry("scripts")
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .with_context(|| format!("scripts in {} is not a table", path.display()))?;

    let mut changed = false;
    let mut notes = Vec::new();
    for kind in ["setup", "archive"] {
        let want = hook_command(exe, kind);
        match scripts.get_mut(kind) {
            None => {
                scripts.insert(kind, toml_edit::value(want));
                changed = true;
            }
            Some(item) => {
                let value = item
                    .as_value_mut()
                    .filter(|v| v.is_str())
                    .with_context(|| {
                        format!("scripts.{kind} in {} is not a string", path.display())
                    })?;
                let current = value.as_str().unwrap_or_default();
                if current.contains(&want) {
                    continue;
                }
                if current.contains(&format!(" hook conductor {kind}")) {
                    // bale from another path, or wrapped by the user: never
                    // chain a second bale call.
                    notes.push(Step::Note(format!(
                        "note: left unchanged in {}: scripts.{kind} `{current}` already \
                         runs bale; edit it by hand to use {}",
                        display_path(&path),
                        display_path(exe)
                    )));
                    continue;
                }
                // Keep the comments around the value.
                let decor = value.decor().clone();
                *value = Value::from(chain(kind, current, &want));
                *value.decor_mut() = decor;
                changed = true;
            }
        }
    }

    if !changed {
        if notes.is_empty() {
            notes.push(Step::Note(format!(
                "{}: Conductor hooks already installed",
                display_path(&path)
            )));
        }
        return Ok(Plan::new(notes));
    }
    let after = doc.to_string();
    let dir = path
        .parent()
        .expect("settings path has a parent")
        .to_path_buf();
    notes.extend([
        Step::Mkdir(dir),
        Step::Settings {
            path: path.clone(),
            before,
            after,
            mode,
        },
        Step::Note(format!(
            "note: {} is usually committed; commit it so every Conductor workspace of \
             this repo runs bale",
            display_path(&path)
        )),
    ]);
    Ok(Plan::new(notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_runs_the_users_script_in_a_subshell() {
        assert_eq!(chain("setup", "npm i\n", "B"), "( npm i\n) && B");
        assert_eq!(chain("archive", "echo bye", "B"), "( echo bye\n); B");
        assert_eq!(chain("setup", "  ", "B"), "B");
        assert_eq!(chain("archive", "", "B"), "B");
    }

    /// Runs `script` with sh in this process's cwd; returns its stdout and
    /// exit code.
    #[cfg(unix)]
    fn sh(script: &str) -> (String, i32) {
        let out = std::process::Command::new("sh")
            .args(["-c", script])
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code().unwrap(),
        )
    }

    /// Each existing setup shape, with and without a failing last command:
    /// bale runs exactly when the user's script succeeds, the user's output
    /// is unchanged, and a failure keeps its status.
    #[cfg(unix)]
    #[test]
    fn setup_chain_runs_bale_only_after_success() {
        let want = "echo BALE";
        let cases = [
            (
                "npm i # c",
                "( npm i # c\n) && echo BALE",
                "echo one # c",
                "echo one # c\nfalse",
            ),
            (
                "cat <<EOF\nx\nEOF",
                "( cat <<EOF\nx\nEOF\n) && echo BALE",
                "cat <<EOF\nx\nEOF",
                "cat <<EOF\nx\nEOF\nfalse",
            ),
            (
                "echo one\necho two",
                "( echo one\necho two\n) && echo BALE",
                "echo one\necho two",
                "echo one\nfalse",
            ),
            ("exit 0", "( exit 0\n) && echo BALE", "exit 0", "exit 1"),
        ];
        for (existing, chained, ok, failing) in cases {
            assert_eq!(chain("setup", existing, want), chained);
            let (alone, _) = sh(ok);
            let (out, code) = sh(&chain("setup", ok, want));
            assert_eq!((out, code), (format!("{alone}BALE\n"), 0), "{ok:?}");
            let (alone, alone_code) = sh(failing);
            let (out, code) = sh(&chain("setup", failing, want));
            assert_eq!((out, code), (alone, alone_code), "{failing:?}");
            assert_ne!(code, 0, "{failing:?}");
        }
    }

    /// The user's archive runs first, while the tree still exists; bale runs
    /// even when it fails or exits, and bale's status is the hook's.
    #[cfg(unix)]
    #[test]
    fn archive_chain_runs_user_first_and_returns_bales_status() {
        let want = "echo BALE; (exit 3)";
        let cases = [
            ("echo bye # c", "bye\n"),
            ("echo bye\nfalse", "bye\n"),
            ("cat <<EOF\nbye\nEOF", "bye\n"),
            ("exit 1", ""),
            ("set -e; false; echo never", ""),
        ];
        for (existing, user_out) in cases {
            let chained = chain("archive", existing, want);
            assert_eq!(chained, format!("( {existing}\n); {want}"));
            let (out, code) = sh(&chained);
            assert_eq!(
                (out, code),
                (format!("{user_out}BALE\n"), 3),
                "{existing:?}"
            );
        }
    }

    /// A `cd` in the user's archive does not move bale.
    #[cfg(unix)]
    #[test]
    fn archive_chain_runs_bale_from_the_starting_directory() {
        let start = std::env::current_dir().unwrap().canonicalize().unwrap();
        let (out, code) = sh(&chain("archive", "cd /\npwd -P", "pwd -P"));
        assert_eq!(code, 0);
        assert_eq!(out, format!("/\n{}\n", start.display()));
    }

    #[test]
    fn hook_command_quotes_the_exe() {
        assert_eq!(
            hook_command(Path::new("/opt/it's/bale"), "setup"),
            r"'/opt/it'\''s/bale' hook conductor setup"
        );
    }
}
