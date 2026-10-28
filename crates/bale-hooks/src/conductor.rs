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
    match workspace_path().and_then(|path| register(home, &path)) {
        Ok(r) => println!("bale: {}: {}", r.id, r.level.name()),
        Err(e) => eprintln!("bale: conductor setup: {e:#}"),
    }
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

/// `existing` with bale's `want` chained in: after it for setup, so the
/// user's setup runs first; before it for archive, so bale colds the tree
/// before the user's archive runs.
fn chain(kind: &str, existing: &str, want: &str) -> String {
    let existing = existing.trim_end();
    if existing.trim().is_empty() {
        return want.to_string();
    }
    match kind {
        "setup" => format!("{existing} && {want}"),
        _ => format!("{want}; {existing}"),
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
    fn chain_puts_setup_after_and_archive_before() {
        assert_eq!(chain("setup", "npm i\n", "B"), "npm i && B");
        assert_eq!(chain("archive", "echo bye", "B"), "B; echo bye");
        assert_eq!(chain("setup", "  ", "B"), "B");
        assert_eq!(chain("archive", "", "B"), "B");
    }

    #[test]
    fn hook_command_quotes_the_exe() {
        assert_eq!(
            hook_command(Path::new("/opt/it's/bale"), "setup"),
            r"'/opt/it'\''s/bale' hook conductor setup"
        );
    }
}
