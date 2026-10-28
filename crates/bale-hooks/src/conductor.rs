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
use crate::cmdline::{bale_call, own_call, sh_guarded, BaleCall};
use crate::install::{
    backup_key, backup_path, read_config, tracked_by_git, tracked_note, Plan, Step,
};
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

/// The command bale's script for `kind` runs. It runs bale only when the
/// exe is there: the file is usually committed, and a teammate without bale
/// at this path gets status 0 instead of a failing setup or archive. With
/// bale present, bale's status is the command's.
fn hook_command(exe: &Path, kind: &str) -> String {
    let exe = sh_quote(&display_path(exe).replace('\\', "/"));
    sh_guarded(&exe, &format!("hook conductor {kind}"))
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
/// archive, keeping the user's commands, other keys and comments. The file
/// as it was is backed up in `backups` when it did not run bale yet, so
/// uninstall knows the user made it.
pub fn install_plan(repo: &Path, exe: &Path, backups: &Path) -> Result<Plan> {
    let path = repo.join(".conductor").join("settings.toml");
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
        bail!(
            "{} is a symlink; bale will not replace it with a regular file. Add \
             `{}` to the end of [scripts] setup and `{}` to the end of archive by hand",
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
    // Whether the file already ran bale; only a first install backs it up.
    let mut any_bale = false;
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
                    any_bale = true;
                    continue;
                }
                if let Some((user, bale)) = unchain(kind, current) {
                    any_bale = true;
                    // bale's own command from another exe path: point it
                    // here, the user's script byte for byte as it was.
                    let repointed = match user {
                        Some(_) => {
                            let at = current.len() - bale.len();
                            format!("{}{want}", &current[..at])
                        }
                        None => want,
                    };
                    let decor = value.decor().clone();
                    *value = Value::from(repointed);
                    *value.decor_mut() = decor;
                    changed = true;
                    continue;
                }
                if bale_call(current, &["hook", "conductor", kind]).is_some()
                    || current.contains(&format!(" hook conductor {kind}"))
                {
                    any_bale = true;
                    // Wrapped by the user, or in a shape bale did not write:
                    // never chain a second bale call.
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
            backup: (!any_bale).then(|| {
                backup_path(
                    backups,
                    &backup_key(repo, &Path::new(".conductor").join("settings.toml")),
                )
            }),
        },
        Step::Note(format!(
            "note: {} is usually committed; commit it so every Conductor workspace of \
             this repo runs bale",
            display_path(&path)
        )),
    ]);
    Ok(Plan::new(notes))
}

/// Whether `cmd` is exactly bale's script for `kind`, from any exe path:
/// the guarded form [`hook_command`] writes, or the bare call earlier
/// installs wrote.
fn is_bale_script(cmd: &str, kind: &str) -> bool {
    own_call(cmd, &["hook", "conductor", kind]) == Some(BaleCall::Plain)
}

/// `value` split the way [`chain`] built it: the user's script (`None` when
/// the value is bale's alone) and bale's command. `None` when the value is
/// not in that shape.
fn unchain<'v>(kind: &str, value: &'v str) -> Option<(Option<&'v str>, &'v str)> {
    if is_bale_script(value, kind) {
        return Some((None, value));
    }
    let sep = if kind == "setup" { "\n) && " } else { "\n); " };
    let (user, bale) = value.strip_prefix("( ")?.rsplit_once(sep)?;
    is_bale_script(bale, kind).then_some((Some(user), bale))
}

/// Takes bale out of `<repo>/.conductor/settings.toml`'s `[scripts]` setup
/// and archive: a chained value gets the user's script back, and a value
/// that was bale's alone goes. A value that runs bale in a shape install
/// does not write is left alone.
///
/// Install's backup in `backups` means the user made the file: it is then
/// kept even when empty, and so is a `[scripts]` table the backup has.
/// Without one, a `[scripts]` table left empty goes unless it holds
/// comments, and so does a file left empty.
pub fn uninstall_plan(repo: &Path, backups: &Path) -> Result<Plan> {
    let path = repo.join(".conductor").join("settings.toml");
    let none = || {
        Plan::new(vec![Step::Note(format!(
            "{}: no bale Conductor scripts to remove",
            display_path(&path)
        ))])
    };
    let Some((before, mode)) = read_config(&path)? else {
        return Ok(none());
    };
    let mut doc: DocumentMut = before
        .parse()
        .with_context(|| format!("{} is not valid TOML; leaving it unchanged", path.display()))?;
    let Some(scripts) = doc.get_mut("scripts").and_then(|s| s.as_table_like_mut()) else {
        return Ok(none());
    };
    let mut changed = false;
    let mut steps = Vec::new();
    // Comments above the keys removed, which go with them.
    let mut comments = String::new();
    for kind in ["setup", "archive"] {
        let Some(value) = scripts.get_mut(kind).and_then(|i| i.as_value_mut()) else {
            continue;
        };
        let Some(current) = value.as_str() else {
            continue;
        };
        match unchain(kind, current) {
            Some((None, _)) => {
                if let Some(prefix) = scripts
                    .key(kind)
                    .and_then(|k| k.leaf_decor().prefix())
                    .and_then(|p| p.as_str())
                    .filter(|p| p.contains('#'))
                {
                    comments.push_str(prefix);
                }
                scripts.remove(kind);
                changed = true;
            }
            Some((Some(user), _)) => {
                let decor = value.decor().clone();
                *value = Value::from(user.to_string());
                *value.decor_mut() = decor;
                changed = true;
            }
            None if bale_call(current, &["hook", "conductor", kind]).is_some() => {
                steps.push(Step::Note(format!(
                    "note: left unchanged in {}: scripts.{kind} `{current}` runs bale in a \
                     form bale did not write; remove it by hand",
                    display_path(&path)
                )));
            }
            None => {}
        }
    }
    if !changed {
        if steps.is_empty() {
            return Ok(none());
        }
        return Ok(Plan::new(steps));
    }
    let key = backup_key(repo, &Path::new(".conductor").join("settings.toml"));
    let has_backup = fs::symlink_metadata(backup_path(backups, &key)).is_ok();
    let backup = fs::read_to_string(backup_path(backups, &key)).ok();
    if scripts.is_empty() {
        let in_backup = backup
            .as_deref()
            .and_then(|b| b.parse::<DocumentMut>().ok())
            .is_some_and(|b| b.contains_key("scripts"));
        // `scripts = {}` inline has no header to keep comments under.
        let mut header = false;
        if let Some(table) = doc["scripts"].as_table_mut() {
            header = [table.decor().prefix(), table.decor().suffix()]
                .into_iter()
                .flatten()
                .any(|d| d.as_str().is_some_and(|d| d.contains('#')));
            if !comments.is_empty() {
                // An empty table keeps them under its header.
                let suffix = table
                    .decor()
                    .suffix()
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let comments = comments.trim_end_matches(['\n', '\r']);
                table
                    .decor_mut()
                    .set_suffix(format!("{suffix}\n{comments}"));
                header = true;
            }
        }
        if !in_backup && !header {
            doc.remove("scripts");
        }
    }
    let after = doc.to_string();
    if after.trim().is_empty() && !has_backup && !tracked_by_git(&path) {
        steps.push(Step::Remove(path));
    } else {
        if after.trim().is_empty() && !has_backup {
            steps.push(tracked_note(&path));
        }
        steps.push(Step::Settings {
            path,
            before: Some(before),
            after,
            mode: Some(mode),
            backup: None,
        });
    }
    Ok(Plan::new(steps))
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
    fn hook_command_quotes_the_exe_and_guards_it() {
        let q = r"'/opt/it'\''s/bale'";
        assert_eq!(
            hook_command(Path::new("/opt/it's/bale"), "setup"),
            format!("if [ -x {q} ]; then {q} hook conductor setup; fi")
        );
    }

    /// A teammate without bale at the installer's path: both scripts end in
    /// status 0, alone and chained, so Conductor carries on as without bale.
    #[cfg(unix)]
    #[test]
    fn scripts_exit_0_when_bale_is_missing() {
        let missing = Path::new("/nonexistent/bale");
        for kind in ["setup", "archive"] {
            let want = hook_command(missing, kind);
            assert_eq!(sh(&want), (String::new(), 0), "{kind}");
            assert_eq!(
                sh(&chain(kind, "echo hi", &want)),
                ("hi\n".to_string(), 0),
                "{kind}"
            );
        }
    }

    /// With bale present, its status is the archive script's: 3 when busy.
    #[cfg(unix)]
    #[test]
    fn archive_keeps_bales_status_when_bale_is_present() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bale-conductor-stub-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("bale");
        fs::write(&stub, "#!/bin/sh\necho \"BALE $*\"\nexit 3\n").unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let want = hook_command(&stub, "archive");
        let alone = sh(&want);
        let chained = sh(&chain("archive", "echo bye", &want));
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(alone, ("BALE hook conductor archive\n".to_string(), 3));
        assert_eq!(
            chained,
            ("bye\nBALE hook conductor archive\n".to_string(), 3)
        );
    }

    /// Uninstall and repoint find bale's command in the guarded form and in
    /// the unguarded form earlier installs wrote.
    #[test]
    fn unchain_reads_guarded_and_unguarded_forms() {
        let old = |kind: &str| format!("'/old/bale' hook conductor {kind}");
        let new = |kind: &str| hook_command(Path::new("/old/bale"), kind);
        for kind in ["setup", "archive"] {
            for bale in [old(kind), new(kind)] {
                assert_eq!(unchain(kind, &bale), Some((None, bale.as_str())), "{bale}");
                let chained = chain(kind, "npm i", &bale);
                assert_eq!(
                    unchain(kind, &chained),
                    Some((Some("npm i"), bale.as_str())),
                    "{chained}"
                );
            }
            // The guard and the call naming different exes is not bale's.
            let mixed = format!("if [ -x '/a/bale' ]; then '/b/bale' hook conductor {kind}; fi");
            assert_eq!(unchain(kind, &mixed), None);
        }
    }
}
