//! Cursor's worktree setup hook (`bale hook cursor setup`) and its install
//! into a repo's `.cursor/worktrees.json`.
//!
//! Unverified against the Cursor app. The contract used here: `worktrees.json`
//! holds `setup-worktree` (or the OS-specific `setup-worktree-unix` /
//! `setup-worktree-windows`), an array of shell commands, or a path to a
//! script, that Cursor runs in a new worktree. Cursor has no remove hook;
//! removal is covered by the git hook and `bale sweep`.

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
use crate::install::{windows_quote, Plan, Step};
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
/// shell Cursor uses for the OS the entry is installed for.
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
/// the other keys and their order.
pub fn install_plan(repo: &Path, exe: &Path) -> Result<Plan> {
    install_plan_for(repo, exe, cfg!(windows))
}

fn install_plan_for(repo: &Path, exe: &Path, windows: bool) -> Result<Plan> {
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

    let existing = commands
        .iter()
        .filter_map(Value::as_str)
        .find(|c| c.contains(" hook cursor setup"));
    match existing {
        Some(c) if c == want => {
            return Ok(Plan::new(vec![Step::Note(format!(
                "{}: Cursor hook already installed",
                display_path(&path)
            ))]));
        }
        Some(c) => {
            // bale from another path, or wrapped by the user: never add a
            // second bale call.
            return Ok(Plan::new(vec![Step::Note(format!(
                "note: left unchanged in {}: {key} entry `{c}` already runs bale; edit \
                 it by hand to use {}",
                display_path(&path),
                display_path(exe)
            ))]));
        }
        None => commands.push(Value::String(want)),
    }

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
        },
        Step::Note(format!(
            "note: {} is usually committed; commit it so every Cursor worktree of this \
             repo runs bale",
            display_path(&path)
        )),
    ]))
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
        install_plan_for(dir.path(), exe, true)
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
}
