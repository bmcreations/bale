//! Codex's environment setup hook (`bale hook codex setup`).
//!
//! Contract, from https://learn.chatgpt.com/docs/environments/local-environment
//! (read 2026-10-28): a local environment's setup script is set in the Codex
//! app's Settings, its config is stored in the project's `.codex` folder, it
//! runs when Codex creates a new worktree, and per-platform scripts can
//! override the default. The docs name no script file and no variable for the
//! worktree path, so bale takes the worktree as an optional argument and
//! falls back to the cwd. `bale install --hooks codex` writes nothing; it
//! prints the command to add. Not run against Codex.

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
        let result = std::env::current_dir()
            .context("reading the current directory")
            .map(|cwd| match path {
                // A relative path is relative to where the script runs.
                Some(p) => cwd.join(p),
                None => cwd,
            })
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
/// nothing. The exe is quoted for a POSIX shell on every OS: Codex's docs
/// don't name the shell, so on Windows this assumes Git Bash or WSL.
pub fn install_plan(exe: &Path) -> Result<Plan> {
    let command = format!(
        "{} hook codex setup",
        sh_quote(&display_path(exe).replace('\\', "/"))
    );
    Ok(Plan::new(vec![
        Step::Note(command),
        Step::Note(
            "note: add the command above to the setup script of your Codex local \
             environment (Codex app Settings; it runs when Codex creates a worktree, config \
             is stored in the project's .codex folder). The script's file name and the \
             variable holding the worktree path are unconfirmed; pass the path as an \
             argument if the script's cwd is not the worktree"
                .to_string(),
        ),
    ]))
}

/// Install wrote nothing for Codex, so there is nothing to remove; says
/// where the user added bale's command.
pub fn uninstall_plan() -> Plan {
    Plan::new(vec![Step::Note(
        "note: bale wrote nothing for Codex; remove the `bale hook codex setup` command from \
         the setup script of your Codex local environment (Codex app Settings) by hand"
            .to_string(),
    )])
}
