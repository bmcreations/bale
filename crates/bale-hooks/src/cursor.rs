//! Cursor's worktree setup hook (`bale hook cursor setup`) and its install
//! into a repo's `.cursor/worktrees.json`.
//!
//! Contract, from https://cursor.com/docs/configuration/worktrees (read
//! 2026-10-28): Cursor reads `worktrees.json` from the worktree, then from the
//! project root. It holds `setup-worktree`, `setup-worktree-unix` and
//! `setup-worktree-windows`; the OS-specific key wins over `setup-worktree`.
//! A value is either an array of commands "executed sequentially in the
//! worktree", or a string path to a script relative to `worktrees.json`, which
//! bale refuses to edit. Cursor has no remove hook; removal is covered by the
//! git hook and `bale sweep`. Not run against the Cursor app.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use bale_core::ops::meta::set_origin;
use bale_core::ops::up::register;
use bale_core::registry::Origin;
use bale_core::BaleHome;
use serde_json::{Map, Value};

use crate::claude::sh_quote;
use crate::cmdline::{bale_call, BaleCall};
use crate::install::{backup_path, json_uninstall_steps, read_config, windows_quote, Plan, Step};
use crate::{as_hook, display_path, Tool};

/// setup: registers the worktree Cursor ran it in (the cwd) and records
/// Cursor as its origin. Never fails Cursor's setup.
pub fn setup(home: &BaleHome) -> ExitCode {
    as_hook(Tool::Cursor, || {
        let result = std::env::current_dir()
            .context("reading the current directory")
            .and_then(|path| {
                let r = register(home, &path)?;
                // Cursor gives no session id, and bale invents no title.
                let origin = Origin {
                    tool: Tool::Cursor.name().to_string(),
                    session: String::new(),
                    title: None,
                };
                if let Err(e) = set_origin(home, &path.to_string_lossy(), origin) {
                    eprintln!("bale: origin not recorded: {e:#}");
                }
                Ok(r)
            });
        match result {
            Ok(r) => println!("bale: {}: {}", r.id, r.level.name()),
            Err(e) => eprintln!("bale: cursor setup: {e:#}"),
        }
    });
    ExitCode::SUCCESS
}

/// The command line bale's entry runs. `windows` picks the quoting of the
/// shell Cursor uses for the OS the entry is installed for. The docs don't
/// name the Windows shell; their array example uses cmd-style `%VAR%`, so the
/// exe is quoted for cmd.exe with no PowerShell `& ` prefix, which cmd rejects.
fn hook_command(exe: &Path, windows: bool) -> String {
    let exe = if windows {
        windows_quote(&display_path(exe))
    } else {
        sh_quote(&display_path(exe).replace('\\', "/"))
    };
    format!("{exe} hook cursor setup")
}

/// The key bale appends to: the OS-specific one when the file has it (it
/// is the one Cursor runs there), else `setup-worktree`.
fn target_key(obj: &Map<String, Value>, windows: bool) -> &'static str {
    let os = if windows {
        "setup-worktree-windows"
    } else {
        "setup-worktree-unix"
    };
    if obj.contains_key(os) {
        os
    } else if obj.contains_key("setup-worktree") {
        "setup-worktree"
    } else if obj.contains_key("setup-worktree-unix") || obj.contains_key("setup-worktree-windows")
    {
        // The file is split by OS and lacks ours: add ours, leave the other.
        os
    } else {
        "setup-worktree"
    }
}

/// Appends bale's setup command to `<repo>/.cursor/worktrees.json`, keeping
/// the other keys and their order; the original is backed up in `backups`.
pub fn install_plan(repo: &Path, exe: &Path, backups: &Path) -> Result<Plan> {
    install_plan_for(repo, exe, backups, cfg!(windows))
}

fn install_plan_for(repo: &Path, exe: &Path, backups: &Path, windows: bool) -> Result<Plan> {
    let path = repo.join(".cursor").join("worktrees.json");
    let want = hook_command(exe, windows);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
        bail!(
            "{} is a symlink; bale will not replace it with a regular file. Add \
             `{want}` to its setup-worktree array by hand",
            path.display()
        );
    }
    let mode = meta.map(|m| m.permissions());
    let before = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut root: Value = match &before {
        Some(text) => serde_json::from_str(text).with_context(|| {
            format!("{} is not valid JSON; leaving it unchanged", path.display())
        })?,
        None => Value::Object(Map::new()),
    };
    let obj = root
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", path.display()))?;
    let key = target_key(obj, windows);
    // bale's entry, plain or wrapped, in whichever setup key holds it.
    let existing = SETUP_KEYS.iter().find_map(|&k| {
        obj.get(k)?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .find(|c| bale_call(c, &HOOK_ARGS).is_some())
            .map(|c| (k, c.to_string()))
    });
    match existing {
        Some((k, c)) if k == key && c == want => {
            return Ok(Plan::new(vec![Step::Note(format!(
                "{}: Cursor hook already installed",
                display_path(&path)
            ))]));
        }
        Some((k, c)) => {
            // bale from another path, wrapped by the user, or under a key
            // Cursor does not run here: never add a second bale call.
            return Ok(Plan::new(vec![Step::Note(format!(
                "note: left unchanged in {}: {k} entry `{c}` already runs bale; edit \
                 it by hand to use {} under {key}",
                display_path(&path),
                display_path(exe)
            ))]));
        }
        None => {}
    }
    let commands = obj
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .with_context(|| {
            format!(
                "\"{key}\" in {} is not an array of commands (a script path?); \
                 add `{want}` to that script by hand",
                path.display()
            )
        })?;
    commands.push(Value::String(want));

    let after = format!("{}\n", serde_json::to_string_pretty(&root)?);
    let dir: PathBuf = path
        .parent()
        .expect("worktrees path has a parent")
        .to_path_buf();
    Ok(Plan::new(vec![
        Step::Mkdir(dir),
        Step::Settings {
            path: path.clone(),
            before,
            after,
            mode,
            backup: Some(backup_path(backups, &path)),
        },
        Step::Note(format!(
            "note: {} is usually committed; commit it so every Cursor worktree of this \
             repo runs bale. The entry holds this machine's absolute path to bale, so \
             teammates need bale at the same path",
            display_path(&path)
        )),
    ]))
}

/// The keys Cursor reads setup commands from.
const SETUP_KEYS: [&str; 3] = [
    "setup-worktree",
    "setup-worktree-unix",
    "setup-worktree-windows",
];

/// The words of bale's setup command after the exe.
const HOOK_ARGS: [&str; 3] = ["hook", "cursor", "setup"];

/// Removes bale's setup entry from each setup array of
/// `<repo>/.cursor/worktrees.json`; an entry the user wrapped around bale
/// stays. A key emptied by the removal goes, and the file when bale created
/// it and nothing is left, as `json_uninstall_steps` decides.
pub fn uninstall_plan(repo: &Path, backups: &Path) -> Result<Plan> {
    let path = repo.join(".cursor").join("worktrees.json");
    let Some((before, mode)) = read_config(&path)? else {
        return Ok(Plan::new(vec![Step::Note(format!(
            "{}: no Cursor hook to remove",
            display_path(&path)
        ))]));
    };
    let mut root: Value = serde_json::from_str(&before)
        .with_context(|| format!("{} is not valid JSON; leaving it unchanged", path.display()))?;
    let obj = root
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", path.display()))?;
    let mut changed = false;
    let mut steps = Vec::new();
    let mut emptied = Vec::new();
    for key in SETUP_KEYS {
        let Some(commands) = obj.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        let n = commands.len();
        commands.retain(|c| {
            match c
                .as_str()
                .and_then(|c| Some((c, bale_call(c, &HOOK_ARGS)?)))
            {
                Some((_, BaleCall::Plain)) => false,
                Some((c, BaleCall::Wrapped)) => {
                    steps.push(Step::Note(format!(
                        "note: left unchanged in {}: {key} entry `{c}` runs bale but is not \
                     bale's plain entry; remove it by hand",
                        display_path(&path)
                    )));
                    true
                }
                None => true,
            }
        });
        if commands.len() != n {
            changed = true;
            if commands.is_empty() {
                emptied.push(vec![key]);
            }
        }
    }
    if !changed {
        steps.push(Step::Note(format!(
            "{}: no bale Cursor hook to remove",
            display_path(&path)
        )));
        return Ok(Plan::new(steps));
    }
    steps.extend(json_uninstall_steps(
        backups, &path, before, mode, &root, &emptied,
    )?);
    Ok(Plan::new(steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(json: &str) -> Map<String, Value> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn target_key_prefers_the_matching_os_key() {
        let both =
            obj(r#"{"setup-worktree":[],"setup-worktree-unix":[],"setup-worktree-windows":[]}"#);
        assert_eq!(target_key(&both, false), "setup-worktree-unix");
        assert_eq!(target_key(&both, true), "setup-worktree-windows");
        let generic = obj(r#"{"setup-worktree":[]}"#);
        assert_eq!(target_key(&generic, true), "setup-worktree");
        let only_other = obj(r#"{"setup-worktree-windows":["x"]}"#);
        assert_eq!(target_key(&only_other, false), "setup-worktree-unix");
        assert_eq!(target_key(&obj("{}"), false), "setup-worktree");
    }

    #[test]
    fn os_specific_file_gets_only_the_matching_key_changed() {
        let dir = tempfile::tempdir().unwrap();
        let cursor = dir.path().join(".cursor");
        fs::create_dir_all(&cursor).unwrap();
        fs::write(
            cursor.join("worktrees.json"),
            r#"{"setup-worktree-unix":["npm i"],"setup-worktree-windows":["npm.cmd i"]}"#,
        )
        .unwrap();
        let exe = Path::new("/opt/bale");
        install_plan_for(dir.path(), exe, &dir.path().join("backups"), true)
            .unwrap()
            .apply()
            .unwrap();
        let after = fs::read_to_string(cursor.join("worktrees.json")).unwrap();
        let v: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(v["setup-worktree-unix"], serde_json::json!(["npm i"]));
        assert_eq!(
            v["setup-worktree-windows"],
            serde_json::json!(["npm.cmd i", "\"/opt/bale\" hook cursor setup"])
        );
    }

    #[test]
    fn hook_command_quotes_the_exe_per_shell() {
        assert_eq!(
            hook_command(Path::new("/opt/it's/bale"), false),
            r"'/opt/it'\''s/bale' hook cursor setup"
        );
        assert_eq!(
            hook_command(Path::new(r"C:\Program Files\bale\bale.exe"), true),
            r#""C:\Program Files\bale\bale.exe" hook cursor setup"#
        );
    }

    #[test]
    fn windows_entry_is_recognised_as_bales() {
        let cmd = hook_command(Path::new(r"C:\Program Files\bale\bale.exe"), true);
        assert_eq!(bale_call(&cmd, &HOOK_ARGS), Some(BaleCall::Plain));
    }
}
