//! Tool integrations over bale-core: one module per tool parses its hook
//! input, finds its caller pids and calls core.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use anyhow::{Context, Result};
use bale_core::busy::Holder;
use bale_core::ops::cold::cold_ignoring;
use bale_core::ops::up::{resolve, Outcome};
use bale_core::registry::Record;
use bale_core::BaleHome;

pub mod claude;
pub mod codex;
pub mod conductor;
pub mod cursor;
pub mod git;
pub mod install;
pub mod proc;

use install::Plan;

/// The session title an adapter knows for the tree, if any: the one stored
/// in its origin, else the one its tool shows. A record with no origin may
/// still be a Claude Code session started in the tree, so Claude Code's
/// transcripts for its path are checked. `home_dir` is the user's home.
pub fn describe(record: &Record, home_dir: &Path) -> Option<String> {
    let claude = || claude_home(home_dir);
    match &record.origin {
        Some(o) if o.title.is_some() => o.title.clone(),
        Some(o) if o.tool == Tool::Claude.name() => {
            claude::session_title(&claude(), &record.path, Some(&o.session)).map(|(_, t)| t)
        }
        Some(_) => None,
        None => claude::session_title(&claude(), &record.path, None).map(|(_, t)| t),
    }
}

/// Claude Code's config dir: `CLAUDE_CONFIG_DIR` when set, else
/// `<home_dir>/.claude`.
fn claude_home(home_dir: &Path) -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home_dir.join(".claude"),
    }
}

/// A tool bale hooks into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Git,
    Conductor,
    Cursor,
    Codex,
}

impl Tool {
    /// Process names that end the walk up from a hook to the tool running
    /// it (see [`proc::caller_pids`]).
    pub fn stop_names(self) -> &'static [&'static str] {
        match self {
            Tool::Claude => &["claude", "node"],
            Tool::Git => &[],
            Tool::Conductor => &["Conductor"],
            Tool::Cursor => &["Cursor", "Cursor Helper"],
            Tool::Codex => &["codex", "Codex"],
        }
    }

    /// The tool's name on the command line, as in `--hooks claude,git`.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Git => "git",
            Tool::Conductor => "conductor",
            Tool::Cursor => "cursor",
            Tool::Codex => "codex",
        }
    }

    const ALL: [Tool; 5] = [
        Tool::Claude,
        Tool::Git,
        Tool::Conductor,
        Tool::Cursor,
        Tool::Codex,
    ];

    /// The tool's name in messages.
    fn label(self) -> &'static str {
        match self {
            Tool::Claude => "Claude Code",
            Tool::Git => "git",
            Tool::Conductor => "Conductor",
            Tool::Cursor => "Cursor",
            Tool::Codex => "Codex",
        }
    }
}

impl FromStr for Tool {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Tool, String> {
        Tool::ALL
            .into_iter()
            .find(|t| t.name() == s)
            .ok_or_else(|| {
                let names: Vec<&str> = Tool::ALL.iter().map(|t| t.name()).collect();
                format!("unknown tool {s:?}; expected one of {}", names.join(", "))
            })
    }
}

/// The plan that hooks `tool` up to bale at `exe`. Claude Code's settings
/// live under `home`; git, Conductor and Cursor keep theirs in `repo`, which
/// they require; Codex needs neither.
pub fn install_plan<'a>(
    tool: Tool,
    exe: &Path,
    home: &Path,
    repo: Option<&'a Path>,
) -> Result<Plan> {
    let need = |repo: Option<&'a Path>| {
        repo.with_context(|| format!("--repo is required for {}", tool.name()))
    };
    match tool {
        Tool::Claude => claude::install_plan(home, exe),
        Tool::Git => git::install_plan(need(repo)?, exe),
        Tool::Conductor => conductor::install_plan(need(repo)?, exe),
        Tool::Cursor => cursor::install_plan(need(repo)?, exe),
        Tool::Codex => codex::install_plan(exe),
    }
}

/// `path` as a string, without Windows' `\\?\` verbatim prefix.
pub fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => s.into_owned(),
    }
}

/// Colds the tree at `path` for a remove or archive hook, ignoring the
/// processes that are only waiting on the hook. Exits 0 when the directory
/// no longer exists, 3 busy, 4 skipped, 1 error (as `bale cold`); a tree
/// still there after a clean cold exits 1.
pub fn remove_tree(home: &BaleHome, path: &Path, tool: Tool) -> ExitCode {
    as_hook(tool, || {
        bale_core::trace::with_intent("remove", || remove_tree_untraced(home, path, tool))
    })
}

/// Runs `f` with core's usage trace naming the trigger `hook:<tool>`.
pub(crate) fn as_hook<R>(tool: Tool, f: impl FnOnce() -> R) -> R {
    bale_core::trace::with_trigger(&format!("hook:{}", tool.name()), f)
}

fn remove_tree_untraced(home: &BaleHome, path: &Path, tool: Tool) -> ExitCode {
    if fs::symlink_metadata(path).is_err() {
        bale_core::trace::missing(home, "cold", path);
        return ExitCode::SUCCESS;
    }
    // Keep this process's cwd out of the tree.
    if let Some(parent) = path.parent() {
        let _ = std::env::set_current_dir(parent);
    }
    let ignore = proc::caller_pids(std::process::id(), tool.stop_names(), proc::process_info);
    let code = match resolve(home, &path.to_string_lossy())
        .and_then(|id| cold_ignoring(home, &id, &ignore))
    {
        Ok(Outcome::Done(_)) => ExitCode::SUCCESS,
        Ok(Outcome::Busy(holders)) => {
            eprint!("{}", busy_text(path, &holders, tool));
            ExitCode::from(3)
        }
        Ok(Outcome::Skipped(reason)) => {
            eprintln!("bale: {} skipped ({reason})", path.display());
            ExitCode::from(4)
        }
        Err(e) => {
            eprintln!("bale: {e:#}");
            ExitCode::from(1)
        }
    };
    if fs::symlink_metadata(path).is_err() {
        return ExitCode::SUCCESS;
    }
    if code == ExitCode::SUCCESS {
        eprintln!("bale: {} still exists", path.display());
        return ExitCode::from(1);
    }
    code
}

/// What bale says for cold's empty `Busy`, after the tree's path or id: the
/// tree changed after it was saved, or the OS refused to move it because
/// something had it open that lsof did not report.
pub const CHANGED_DURING_COLD: &str =
    "changed or was in use while bale was making it cold, left untouched; try again";

/// What a remove hook prints for a busy tree. No holders means the tree
/// changed or was in use while cold was saving it (see
/// [`CHANGED_DURING_COLD`]).
fn busy_text(path: &Path, holders: &[Holder], tool: Tool) -> String {
    if holders.is_empty() {
        return format!("bale: {} {CHANGED_DURING_COLD}\n", path.display());
    }
    let mut text = format!(
        "bale: {} is busy, left untouched; these processes hold it \
         ({}'s background tasks and servers count):\n",
        path.display(),
        tool.label()
    );
    for h in holders {
        text.push_str(&format!(
            "  pid {} {} {}\n",
            h.pid,
            h.command,
            h.path.display()
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tool_names_round_trip() {
        for t in Tool::ALL {
            assert_eq!(t.name().parse::<Tool>(), Ok(t));
        }
        assert!("vim".parse::<Tool>().unwrap_err().contains("vim"));
    }

    #[test]
    fn repo_is_required_for_git_conductor_cursor() {
        let p = Path::new("/nowhere");
        for t in [Tool::Git, Tool::Conductor, Tool::Cursor] {
            let err = install_plan(t, p, p, None).err().unwrap().to_string();
            assert_eq!(err, format!("--repo is required for {}", t.name()));
        }
    }

    #[test]
    fn codex_needs_no_repo() {
        let p = Path::new("/nowhere");
        assert!(install_plan(Tool::Codex, p, p, None).is_ok());
    }

    #[test]
    fn busy_without_holders_says_the_tree_changed() {
        let text = busy_text(Path::new("/w/t"), &[], Tool::Claude);
        assert!(!text.contains("these processes hold it"), "{text}");
        assert!(text.contains(CHANGED_DURING_COLD), "{text}");
    }

    #[test]
    fn busy_with_holders_lists_them() {
        let holder = Holder {
            pid: 7,
            command: "gradle".into(),
            path: PathBuf::from("/w/t"),
        };
        let text = busy_text(Path::new("/w/t"), &[holder], Tool::Claude);
        assert!(text.contains("these processes hold it"), "{text}");
        assert!(text.contains("Claude Code's background tasks"), "{text}");
        assert!(text.contains("pid 7 gradle /w/t"), "{text}");
    }
}
