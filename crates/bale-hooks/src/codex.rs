//! Codex's environment setup hook (`bale hook codex setup`).
//!
//! Codex has no config file bale can edit: `bale install --hooks codex` only
//! prints the command for the user to add to the Codex environment's setup
//! script. The setup script's file and environment variable names are not
//! confirmed, so bale takes the worktree as an optional argument and falls
//! back to the cwd. Not run against Codex.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use bale_core::ops::meta::set_origin;
use bale_core::ops::up::register;
use bale_core::registry::Origin;
use bale_core::BaleHome;

use crate::claude::sh_quote;
use crate::install::{Plan, Step};
use crate::{as_hook, display_path, Tool};

/// setup: registers `path` (default: the cwd) and records Codex as its
/// origin. Never fails Codex's setup.
pub fn setup(home: &BaleHome, path: Option<PathBuf>) -> ExitCode {
    as_hook(Tool::Codex, || {
        let result = match path {
            Some(p) => Ok(p),
            None => std::env::current_dir().context("reading the current directory"),
        }
        .and_then(|path| {
            let r = register(home, &path)?;
            // Codex gives no session id, and bale invents no title.
            let origin = Origin {
                tool: Tool::Codex.name().to_string(),
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
            Err(e) => eprintln!("bale: codex setup: {e:#}"),
        }
    });
    ExitCode::SUCCESS
}

/// The command to add to the Codex environment's setup script. Writes
/// nothing.
pub fn install_plan(exe: &Path) -> Result<Plan> {
    let command = format!(
        "{} hook codex setup",
        sh_quote(&display_path(exe).replace('\\', "/"))
    );
    Ok(Plan::new(vec![
        Step::Note(command),
        Step::Note(
            "note: add the command above to the Codex environment's setup script; the \
             script's file name and the variable naming the worktree are unconfirmed, so \
             pass the worktree path as an argument if the cwd is not the worktree"
                .to_string(),
        ),
    ]))
}
