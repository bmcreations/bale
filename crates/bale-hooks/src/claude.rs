//! Claude Code's WorktreeCreate and WorktreeRemove hooks
//! (`bale hook claude create|remove`), and their install into
//! `~/.claude/settings.json` (`$CLAUDE_CONFIG_DIR/settings.json` when that
//! is set).
//!
//! Claude Code contract, from https://code.claude.com/docs/en/hooks (read
//! 2026-10-28, sections "WorktreeCreate" and "WorktreeRemove"):
//! - WorktreeCreate input adds `name` (a slug such as `bold-oak-a3f2`) to the
//!   common fields (`session_id`, `transcript_path`, `cwd`, `hook_event_name`).
//!   A command hook prints the path "as the last non-empty line of stdout";
//!   any other output goes to stderr. "Any non-zero exit code makes worktree
//!   creation fail." Claude Code refuses an absolute path containing `.` or
//!   `..` segments.
//! - Without the hook, Claude Code creates `.claude/worktrees/<name>/` at the
//!   repository root on branch `worktree-<name>`
//!   (https://code.claude.com/docs/en/worktrees); the fallback below does the same.
//! - WorktreeRemove input adds `worktree_path`, the absolute path the create
//!   hook returned. "Hook exits 0: the worktree counts as removed." On a
//!   non-zero exit "the removal fails if the directory at `worktree_path`
//!   still exists afterward". JSON output is discarded.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bale_core::git::{self, Git};
use bale_core::lock::WorkspaceLock;
use bale_core::ops::meta::{set_origin, touch, touch_restoring};
use bale_core::ops::new::{new_workspace, workspace_path};
use bale_core::ops::unstash::{
    needs_restore, take_failure_marker, tree_containing, unstash_with, LockTimeout, UnstashOpts,
};
use bale_core::ops::up::register;
use bale_core::registry::Origin;
use bale_core::BaleHome;
use serde_json::{json, Map, Value};

use crate::cmdline::bale_call;
use crate::install::{
    backup_key, backup_path, json_uninstall_steps, read_config, reformat_text, repoint_note, Plan,
    Step,
};
use crate::{display_path, remove_tree, Tool};

/// Reads the hook's JSON payload from stdin.
fn read_payload() -> Result<Value> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("reading the hook payload from stdin")?;
    serde_json::from_str(&text).context("parsing the hook payload")
}

fn field<'a>(payload: &'a Value, name: &str) -> Result<&'a str> {
    payload[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("hook payload has no {name:?} string"))
}

/// WorktreeCreate: creates the worktree with `bale new` on Claude Code's
/// branch name `worktree-<name>`, or with plain git where Claude Code would
/// have put it when bale fails, and prints only its absolute path on stdout.
pub fn create(home: &BaleHome) -> Result<ExitCode> {
    crate::as_hook(Tool::Claude, || create_untraced(home))
}

fn create_untraced(home: &BaleHome) -> Result<ExitCode> {
    let payload = read_payload()?;
    let name = field(&payload, "name")?;
    let cwd = PathBuf::from(field(&payload, "cwd")?);
    let branch = format!("worktree-{name}");
    // Paths that exist now belong to someone else; only one that appears
    // during this call can be the worktree bale itself made.
    let fresh: Vec<PathBuf> = [
        workspace_path(home, &cwd, name).ok(),
        fallback_path(&cwd, name).ok(),
    ]
    .into_iter()
    .flatten()
    .filter(|p| fs::symlink_metadata(p).is_err())
    .collect();
    let path = match new_workspace(home, &cwd, name, Some(&branch)) {
        Ok(record) => record.path,
        Err(e) => {
            eprintln!("bale: {e:#}");
            let path = match existing_worktree(&cwd, fresh.iter()) {
                // bale made it before failing; never add a second one.
                Some(path) => path,
                None => {
                    eprintln!("bale: falling back to git worktree add");
                    git_fallback(&cwd, name, &branch)?
                }
            };
            if let Err(e) = register(home, &path) {
                eprintln!("bale: not registered: {e:#}");
            }
            path
        }
    };
    record_session(home, &payload, &path);
    println!("{}", display_path(&path));
    Ok(ExitCode::SUCCESS)
}

/// The first of `candidates` that is a linked worktree of `cwd`'s repo.
/// Callers pass only paths that did not exist before this hook ran.
fn existing_worktree<'a>(
    cwd: &Path,
    candidates: impl Iterator<Item = &'a PathBuf>,
) -> Option<PathBuf> {
    let listed: Vec<PathBuf> = git::worktrees(cwd)
        .ok()?
        .into_iter()
        .filter(|w| !w.is_main)
        .map(|w| bale_core::paths::canonicalize(&w.path).unwrap_or(w.path))
        .collect();
    candidates
        .filter_map(|c| bale_core::paths::canonicalize(c).ok())
        .find(|c| listed.contains(c))
}

/// `<main checkout>/.claude/worktrees/<name>`. From the main checkout that is
/// its `--show-toplevel`, which is right for a submodule or a
/// `--separate-git-dir` repo too. From a linked worktree it is the first
/// `git worktree list` entry, so the fallback still roots at the main
/// checkout as Claude Code does. (For a submodule or `--separate-git-dir`
/// repo git lists the git dir there, and keeps no other record of the main
/// working tree.)
fn fallback_path(cwd: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name.starts_with('-')
        || name.contains(['/', '\\'])
        || name == "."
        || name == ".."
    {
        bail!("worktree name {name:?} must be a single directory name");
    }
    let main = if git::admin_dir(cwd)? == git::common_dir(cwd)? {
        let top = Git::at(cwd).run(&["rev-parse", "--show-toplevel"])?;
        PathBuf::from(top.trim_end_matches(['\n', '\r']))
    } else {
        git::worktrees(cwd)?
            .into_iter()
            .find(|w| w.is_main)
            .with_context(|| format!("{} lists no worktrees", cwd.display()))?
            .path
    };
    let main = bale_core::paths::canonicalize(&main).context("resolving the main checkout")?;
    Ok(main.join(".claude").join("worktrees").join(name))
}

/// `git worktree add` at [`fallback_path`] on `branch`, as Claude Code does
/// without a WorktreeCreate hook.
fn git_fallback(cwd: &Path, name: &str, branch: &str) -> Result<PathBuf> {
    let path = fallback_path(cwd, name)?;
    let dir = path.parent().expect("fallback path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let g = Git::at(dir.parent().and_then(Path::parent).expect("main checkout"));
    let p = path.to_string_lossy();
    let exists = g
        .run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .is_ok();
    if exists {
        g.run(&["worktree", "add", "--", &p, branch])?;
    } else {
        g.run(&["worktree", "add", "-b", branch, "--", &p])?;
    }
    Ok(bale_core::paths::canonicalize(&path).unwrap_or(path))
}

/// Records the Claude Code session that asked for the worktree at `path`.
/// Failure is a warning on stderr; the hook still succeeds.
fn record_session(home: &BaleHome, payload: &Value, path: &Path) {
    let Ok(session) = field(payload, "session_id") else {
        eprintln!("bale: hook payload has no session_id; session not recorded");
        return;
    };
    let origin = Origin {
        tool: Tool::Claude.name().to_string(),
        session: session.to_string(),
        title: None,
    };
    if let Err(e) = set_origin(home, &path.to_string_lossy(), origin) {
        eprintln!("bale: session not recorded: {e:#}");
    }
}

/// Claude Code's project dir name for a session started in `tree`: its
/// absolute path with every char outside `[A-Za-z0-9]` replaced by `-`.
pub fn encode_project_path(tree: &Path) -> String {
    tree.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Bytes read per step from the end of a transcript.
const CHUNK: usize = 256 * 1024;

/// Bytes read at most from the end of a transcript. Claude Code re-appends
/// the title line as a session runs, so it sits near the end; an untitled
/// transcript costs this much rather than its whole size.
const TAIL_BUDGET: u64 = 1024 * 1024;

/// `(session id, title)` of the Claude Code session that ran in `tree`:
/// `session`'s transcript under `<claude_home>/projects/<encoded tree>/`,
/// or, with no `session`, the newest transcript there. A named session
/// whose transcript is missing gets `None`, not the newest file, which may
/// belong to a later session in the same tree. The title is the last
/// `custom-title` line's `customTitle` within the last [`TAIL_BUDGET`]
/// bytes. `None` when there is no transcript or it has no title.
pub fn session_title(
    claude_home: &Path,
    tree: &Path,
    session: Option<&str>,
) -> Option<(String, String)> {
    let dir = claude_home.join("projects").join(encode_project_path(tree));
    let path = match session {
        Some(s) => {
            let safe = !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            Some(dir.join(format!("{s}.jsonl"))).filter(|p| safe && p.is_file())?
        }
        None => newest_transcript(&dir)?,
    };
    let id = path.file_stem()?.to_string_lossy().into_owned();
    Some((id, last_title(&path)?))
}

/// The `*.jsonl` file in `dir` with the latest mtime.
fn newest_transcript(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl") && p.is_file())
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, p)| p)
}

/// The `customTitle` of the last `custom-title` line in the transcript at
/// `path`, read backwards in [`CHUNK`]-sized steps so a long transcript
/// costs only its tail when the title was set late. Stops after
/// [`TAIL_BUDGET`] bytes.
fn last_title(path: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = fs::File::open(path).ok()?;
    let mut end = file.metadata().ok()?.len();
    let floor = end.saturating_sub(TAIL_BUDGET);
    // Bytes after the last newline seen so far: the start of a line whose
    // beginning lies in an earlier chunk.
    let mut carry: Vec<u8> = Vec::new();
    while end > floor {
        let start = end.saturating_sub(CHUNK as u64).max(floor);
        let mut buf = vec![0; (end - start) as usize];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut buf).ok()?;
        buf.extend_from_slice(&carry);
        let mut lines: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
        // The first piece may continue in the previous chunk, unless this
        // chunk starts the file.
        let first = if start > 0 { lines.remove(0) } else { &[][..] };
        for line in lines.iter().rev() {
            if let Some(title) = title_in(line) {
                return Some(title);
            }
        }
        carry = first.to_vec();
        end = start;
    }
    None
}

/// The title a transcript line sets, if it is a `custom-title` line.
fn title_in(line: &[u8]) -> Option<String> {
    const NEEDLE: &[u8] = b"custom-title";
    if !line.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    if v["type"] != "custom-title" {
        return None;
    }
    v["customTitle"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// WorktreeRemove: colds the worktree (saving any uncommitted work), so
/// its directory is gone. Exits 0 only when the directory no longer exists.
pub fn remove(home: &BaleHome) -> Result<ExitCode> {
    let payload = read_payload()?;
    let path = PathBuf::from(field(&payload, "worktree_path")?);
    Ok(remove_tree(home, &path, Tool::Claude))
}

/// What a prompt or pre-tool hook needs from its payload: the tree to restore.
/// `None` (with a line on stderr) for a payload that must not block the user.
fn hook_cwd(event: &str) -> Option<PathBuf> {
    hook_input(event).map(|(cwd, _)| cwd)
}

/// [`hook_cwd`] with the payload it came from.
fn hook_input(event: &str) -> Option<(PathBuf, Value)> {
    let payload = match read_payload() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bale: {event} hook ignored: {e:#}");
            return None;
        }
    };
    match field(&payload, "cwd") {
        Ok(cwd) if Path::new(cwd).is_dir() => Some((PathBuf::from(cwd), payload.clone())),
        Ok(cwd) => {
            eprintln!("bale: {event} hook ignored: cwd {cwd:?} is not a directory");
            None
        }
        Err(e) => {
            eprintln!("bale: {event} hook ignored: {e:#}");
            None
        }
    }
}

/// UserPromptSubmit: starts restoring the stashed tree at the payload's `cwd`
/// in a detached `bale unstash --background` and returns without waiting, so
/// the prompt is not held for the disk writes; the first [`pre_tool`] that
/// could read build output waits for them. Activity is recorded first and an
/// in-progress stash is asked to stop. Prints nothing on success (stdout
/// would become model context). Exit 2 blocks the prompt and shows stderr:
/// a background restore that failed earlier is reported once that way, so
/// it is never silent.
pub fn prompt(home: &BaleHome) -> ExitCode {
    crate::as_hook(Tool::Claude, || {
        let Some(cwd) = hook_cwd("prompt") else {
            return ExitCode::SUCCESS;
        };
        let (path, id) = match tree_containing(home, &cwd) {
            Ok(Some(tree)) => tree,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("bale: {e:#}");
                return ExitCode::from(2);
            }
        };
        if report_failure(home, &path, &id) {
            return ExitCode::from(2);
        }
        let pending = match needs_restore(home, &id) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("bale: {e:#}");
                return ExitCode::from(2);
            }
        };
        // Before the restore starts, so a sweep cannot stash the tree again
        // right after the prompt.
        if let Err(e) = touch_restoring(home, &path) {
            eprintln!("bale: not recorded as activity: {e:#}");
        }
        if !pending {
            return ExitCode::SUCCESS;
        }
        if let Err(e) = WorkspaceLock::request(home, &id) {
            eprintln!(
                "bale: could not ask the holder of {} to stop: {e:#}",
                display_path(&path)
            );
        }
        if let Err(e) = spawn_background_unstash(&id) {
            // Nothing detached to wait for: restore here, as pre-tool does.
            eprintln!("bale: could not start a background restore ({e}); restoring now");
            return restore(home, &path, &id, &UnstashOpts::default());
        }
        ExitCode::SUCCESS
    })
}

/// PreToolUse (every tool, since MCP build tools read build output too):
/// waits for the tree's restore, the background one a prompt started or its
/// own if it gets there first, then counts the call as activity. A tree that
/// is not stashed costs a registry scan and a record read: no lock is taken
/// and the store is not opened.
pub fn pre_tool(home: &BaleHome, lock_wait_ms: Option<u64>) -> ExitCode {
    crate::as_hook(Tool::Claude, || {
        let Some((cwd, payload)) = hook_input("pre-tool") else {
            return ExitCode::SUCCESS;
        };
        learn_dd_roots(home, &payload, &cwd);
        let (path, id) = match tree_containing(home, &cwd) {
            Ok(Some(tree)) => tree,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("bale: {e:#}");
                return ExitCode::from(2);
            }
        };
        if report_failure(home, &path, &id) {
            return ExitCode::from(2);
        }
        match needs_restore(home, &id) {
            Ok(false) => {}
            Ok(true) => {
                let opts = UnstashOpts {
                    lock_wait: lock_wait_ms.map(Duration::from_millis),
                    ..UnstashOpts::default()
                };
                if restore(home, &path, &id, &opts) != ExitCode::SUCCESS {
                    return ExitCode::from(2);
                }
                // The background run that just finished may have failed.
                if report_failure(home, &path, &id) {
                    return ExitCode::from(2);
                }
            }
            Err(e) => {
                eprintln!("bale: {e:#}");
                return ExitCode::from(2);
            }
        }
        share_packages(home, &payload, &cwd, &id);
        if let Err(e) = touch_restoring(home, &path) {
            eprintln!("bale: not recorded as activity: {e:#}");
        }
        ExitCode::SUCCESS
    })
}

/// Keeps the tree's shared Swift packages link current before a tool runs:
/// relinks when a `Package.resolved` changed, and links the folder a Bash
/// command's `-derivedDataPath` names. Best-effort and silent; the unchanged
/// case is one record read and one stat per linked project. A tree with
/// several projects uses the first project's key for `-derivedDataPath`.
#[cfg(target_os = "macos")]
fn share_packages(home: &BaleHome, payload: &Value, cwd: &Path, id: &str) {
    bale_core::spm_link::relink_tree(home, id);
    let Some(command) = payload["tool_input"]["command"].as_str() else {
        return;
    };
    if !command.contains(DD_FLAG) {
        return;
    }
    let user_home = bale_core::paths::home_dir();
    for root in derived_data_paths(command, cwd, user_home.as_deref()) {
        bale_core::spm_link::link_derived_data_path(home, id, &root);
    }
}

#[cfg(not(target_os = "macos"))]
fn share_packages(_home: &BaleHome, _payload: &Value, _cwd: &Path, _id: &str) {}

/// Records the `-derivedDataPath` of a Bash command as a DerivedData root
/// for sweep to reap once its project is gone. Best-effort: the hook's exit
/// code and output never depend on it, and a command without the flag costs
/// one substring search.
fn learn_dd_roots(home: &BaleHome, payload: &Value, cwd: &Path) {
    let Some(command) = payload["tool_input"]["command"].as_str() else {
        return;
    };
    if !command.contains(DD_FLAG) {
        return;
    }
    let user_home = bale_core::paths::home_dir();
    for root in derived_data_paths(command, cwd, user_home.as_deref()) {
        let _ = bale_core::derived_data::learn_root(home, &root);
    }
}

const DD_FLAG: &str = "-derivedDataPath";

/// The argument after each `-derivedDataPath` in `command`, as an absolute
/// path: quotes removed, a leading `~` expanded to `user_home`, a relative
/// path joined to `cwd`. An argument that needs the shell to resolve
/// (`$VAR`, `$(..)`, backticks), is missing, or looks like another flag is
/// left out.
fn derived_data_paths(command: &str, cwd: &Path, user_home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (at, _) in command.match_indices(DD_FLAG) {
        let starts_word = command[..at]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '\'' | '"' | ';' | '&' | '|' | '('));
        let rest = &command[at + DD_FLAG.len()..];
        if !starts_word || !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(arg) = shell_word(rest.trim_start()) else {
            continue;
        };
        if arg.is_empty() || arg.starts_with('-') || arg.contains(['$', '`']) {
            continue;
        }
        let path = if arg == "~" || arg.starts_with("~/") {
            match user_home {
                Some(h) => h.join(arg[1..].trim_start_matches('/')),
                None => continue,
            }
        } else {
            cwd.join(&arg)
        };
        let path = lexical(&path);
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// The first shell word of `s` with quotes and backslash escapes resolved;
/// `None` for an unterminated quote.
fn shell_word(s: &str) -> Option<String> {
    let mut word = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() || matches!(c, ';' | '&' | '|' | ')') => break,
            '\\' => word.push(chars.next()?),
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    c => word.push(c),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => word.push(chars.next()?),
                    c => word.push(c),
                }
            },
            c => word.push(c),
        }
    }
    Some(word)
}

/// `path` without `.` components and with `..` applied, without touching
/// the disk (the folder may not exist yet).
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Prints and removes the failure a background restore left for the tree;
/// true when there was one.
fn report_failure(home: &BaleHome, path: &Path, id: &str) -> bool {
    let Some(text) = take_failure_marker(home, id) else {
        return false;
    };
    eprintln!(
        "bale: the background restore of {} failed:\n{}",
        display_path(path),
        text.trim_end()
    );
    true
}

/// Restores the tree here and waits for it; the exit code to hook with.
fn restore(home: &BaleHome, path: &Path, id: &str, opts: &UnstashOpts) -> ExitCode {
    match unstash_with(home, id, opts) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) if e.downcast_ref::<LockTimeout>().is_some() => {
            eprintln!(
                "bale: still busy with {}; resend the prompt in a moment",
                display_path(path)
            );
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("bale: could not restore {}: {e:#}", display_path(path));
            ExitCode::from(2)
        }
    }
}

/// Starts `bale unstash --background <id>` detached from this hook: no
/// stdio, and its own process group (a console-less new group on Windows), so
/// Claude Code ending the hook does not take the restore down with it.
fn spawn_background_unstash(id: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["unstash", "--background", id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        // std spawns with handle inheritance on, so the child would inherit
        // this hook's stdout/stderr pipes despite `Stdio::null()`, and Claude
        // Code would wait for the restore to close them.
        disinherit_std_handles();
    }
    // Not waited for: the hook exits and init reaps it.
    cmd.spawn().map(drop)
}

/// Marks this process's stdin, stdout and stderr handles as not inheritable.
#[cfg(windows)]
fn disinherit_std_handles() {
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
    extern "system" {
        fn GetStdHandle(which: u32) -> isize;
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain Win32 calls on this process's own std handles; an
        // invalid handle only makes SetHandleInformation fail, which is
        // ignored.
        unsafe {
            let handle = GetStdHandle(which);
            if handle != 0 && handle != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Stop: records the turn's end as the tree's last activity. No hashing and
/// no store access; it never blocks Claude Code, so failures only print.
pub fn stop(home: &BaleHome) -> ExitCode {
    crate::as_hook(Tool::Claude, || {
        let Some(cwd) = hook_cwd("stop") else {
            return ExitCode::SUCCESS;
        };
        match tree_containing(home, &cwd) {
            Ok(Some((path, _))) => {
                if let Err(e) = touch(home, &path) {
                    eprintln!("bale: not recorded as activity: {e:#}");
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("bale: {e:#}"),
        }
        ExitCode::SUCCESS
    })
}

/// Each settings.json event bale hooks, with its `bale hook claude` subcommand.
const HOOKS: [(&str, &str); 5] = [
    ("WorktreeCreate", "create"),
    ("WorktreeRemove", "remove"),
    ("UserPromptSubmit", "prompt"),
    ("PreToolUse", "pre-tool"),
    ("Stop", "stop"),
];

/// Seconds Claude Code lets the prompt and pre-tool hooks run: above
/// unstash's 60 s wait for a tree's lock, so bale's own timeout answers first
/// (pre-tool waits for a whole restore; prompt only starts one).
const RESTORE_TIMEOUT_SECS: u64 = 90;

/// The `timeout` (seconds) bale's entry for `kind` sets, if any.
fn hook_timeout(kind: &str) -> Option<u64> {
    matches!(kind, "prompt" | "pre-tool").then_some(RESTORE_TIMEOUT_SECS)
}

/// `s` single-quoted for sh, which Claude Code runs command hooks with.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command line bale's hook entry for `kind` runs.
fn hook_command(exe: &Path, kind: &str) -> String {
    format!(
        "{} hook claude {kind}",
        sh_quote(&display_path(exe).replace('\\', "/"))
    )
}

/// The `hook ...` tails that mark bale's entry for `kind`: the current one
/// and the `claude-worktree-<kind>` name it replaced, so a reinstall
/// rewrites an old entry instead of adding a second.
fn hook_tails(kind: &str) -> [String; 2] {
    [
        format!(" hook claude {kind}"),
        format!(" hook claude-worktree-{kind}"),
    ]
}

/// Whether `cmd` runs bale's hook for `kind` in any form, plain or wrapped,
/// read from the command's words: bale's exe, then `hook claude <kind>` or
/// the old `hook claude-worktree-<kind>`.
fn mentions_hook(cmd: &str, kind: &str) -> bool {
    let old = format!("claude-worktree-{kind}");
    bale_call(cmd, &["hook", "claude", kind]).is_some() || bale_call(cmd, &["hook", &old]).is_some()
}

/// `cmd` with a wrapped call of the old `hook claude-worktree-<kind>` name
/// renamed to `hook claude <kind>`, the rest of the user's command kept as
/// written; `None` when `cmd` is not such a call.
fn renamed_wrapped(cmd: &str, kind: &str) -> Option<String> {
    let old = format!("claude-worktree-{kind}");
    bale_call(cmd, &["hook", &old])?;
    let [new_tail, old_tail] = hook_tails(kind);
    let at = cmd.rfind(old_tail.as_str())?;
    let out = format!("{}{new_tail}{}", &cmd[..at], &cmd[at + old_tail.len()..]);
    bale_call(&out, &["hook", "claude", kind]).map(|_| out)
}

/// The exe path in `cmd` when `cmd` is exactly bale's own entry for `kind`
/// (current or old name): a single-quoted (sh) or double-quoted path to
/// `bale` or `bale.exe`, then the hook subcommand. Anything else, such as a
/// command wrapped in `env` or `nice`, is the user's and gives `None`.
fn bale_hook_path(cmd: &str, kind: &str) -> Option<String> {
    let quoted = hook_tails(kind)
        .iter()
        .find_map(|t| cmd.strip_suffix(t.as_str()))?;
    let path = match quoted.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) if !inner.contains(['"', '\\']) => inner.to_string(),
        Some(_) => return None,
        None => sh_unquote(quoted)?,
    };
    let base = path.rsplit(['/', '\\']).next()?;
    // bale writes an absolute path; a relative one is the user's.
    (crate::cmdline::is_absolute(&path) && matches!(base, "bale" | "bale.exe")).then_some(path)
}

/// The inverse of [`sh_quote`]: `'...'` segments joined by `\'`.
fn sh_unquote(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = s;
    loop {
        rest = rest.strip_prefix('\'')?;
        let end = rest.find('\'')?;
        out.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        if rest.is_empty() {
            return Some(out);
        }
        rest = rest.strip_prefix("\\'")?;
        out.push('\'');
    }
}

/// Claude Code's user settings.json, as `(root, tail)`: the file is
/// `root/tail`, and its backup is keyed by both. That is
/// `(<home>, .claude/settings.json)` by default, and the parent of
/// `CLAUDE_CONFIG_DIR` and `<its name>/settings.json` when that is set.
fn settings_location(home: &Path) -> (PathBuf, PathBuf) {
    let dir = crate::claude_home(home);
    match (dir.parent(), dir.file_name()) {
        (Some(root), Some(name)) => (root.to_path_buf(), Path::new(name).join("settings.json")),
        _ => (dir, PathBuf::from("settings.json")),
    }
}

/// One settings.json hook group running `bale hook claude <kind>`.
fn hook_group(exe: &Path, kind: &str) -> Value {
    let mut entry = json!({ "type": "command", "command": hook_command(exe, kind) });
    if let Some(secs) = hook_timeout(kind) {
        entry["timeout"] = json!(secs);
    }
    json!({ "hooks": [entry] })
}

/// Adds bale's entries (WorktreeCreate, WorktreeRemove, UserPromptSubmit,
/// PreToolUse on every tool, Stop) to
/// Claude Code's user settings.json (see [`settings_location`]), keeping
/// every other hook. A user WorktreeRemove hook stays and runs in parallel
/// with bale's, which install notes. A user WorktreeCreate hook
/// is refused: Claude Code would run both, and two hooks creating the same
/// worktree conflict.
pub fn install_plan(home: &Path, exe: &Path, backups: &Path) -> Result<Plan> {
    let (base, tail) = settings_location(home);
    let path = base.join(&tail);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
        let hooks: Map<String, Value> = HOOKS
            .iter()
            .map(|(event, kind)| (event.to_string(), json!([hook_group(exe, kind)])))
            .collect();
        bail!(
            "{} is a symlink; bale will not replace it with a regular file. Add these \
             hooks to the file it points at by hand, or replace the link with a copy:\n{}",
            path.display(),
            serde_json::to_string_pretty(&json!({ "hooks": hooks }))?
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
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .with_context(|| format!("\"hooks\" in {} is not an object", path.display()))?;

    let mut changed = false;
    // Whether the file already ran bale; only a first install backs it up.
    let mut any_ours = false;
    let mut notes = Vec::new();
    for (event, kind) in HOOKS {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} in {} is not an array", path.display()))?;
        let want = hook_command(exe, kind);
        let mut ours = false;
        // bale's own pre-tool group used to match only Bash; it matches every
        // tool now. A group holding anything else is the user's.
        if kind == "pre-tool" {
            for g in groups.iter_mut() {
                let ours_alone = g["matcher"] == "Bash"
                    && g["hooks"].as_array().is_some_and(|hs| {
                        hs.len() == 1
                            && hs[0]["command"]
                                .as_str()
                                .is_some_and(|c| bale_hook_path(c, kind).is_some())
                    });
                if ours_alone {
                    g.as_object_mut().map(|o| o.remove("matcher"));
                    changed = true;
                }
            }
        }
        for h in groups
            .iter_mut()
            .filter_map(|g| g["hooks"].as_array_mut())
            .flatten()
        {
            let Some(cmd) = h["command"].as_str() else {
                continue;
            };
            if !mentions_hook(cmd, kind) {
                continue;
            }
            ours = true;
            if let Some(old) = bale_hook_path(cmd, kind) {
                if cmd != want {
                    // bale's own entry from another binary path or the old
                    // name: point it here.
                    notes.push(repoint_note(&path, &format!("{event} hook"), &old, exe));
                    h["command"] = Value::String(want.clone());
                    changed = true;
                }
                if let Some(secs) = hook_timeout(kind).filter(|s| h["timeout"] != json!(s)) {
                    h["timeout"] = json!(secs);
                    changed = true;
                }
            } else if let Some(renamed) = renamed_wrapped(cmd, kind) {
                // The user's command around bale, naming bale's old hook:
                // only bale's words change.
                notes.push(Step::Note(format!(
                    "note: rewrote stale {event} hook `{cmd}` in {}",
                    display_path(&path)
                )));
                h["command"] = Value::String(renamed);
                changed = true;
            } else {
                // The user's own command around bale, e.g. `env ... bale hook ...`.
                notes.push(Step::Note(format!(
                    "note: left unchanged in {}: {event} hook `{cmd}` runs bale but is \
                     not bale's plain entry",
                    display_path(&path)
                )));
            }
        }
        if ours {
            any_ours = true;
            continue;
        }
        if kind == "create" && !groups.is_empty() {
            bail!(
                "{} already has a {event} hook; remove it first, then rerun",
                path.display()
            );
        }
        if kind == "remove" && !groups.is_empty() {
            // Claude Code runs an event's matching hooks in parallel, so
            // where bale's entry sits sets no order.
            notes.push(Step::Note(format!(
                "note: {} already has a {event} hook; Claude Code runs it in parallel \
                 with bale's, and bale reports the tree as busy (exit 3) while that \
                 hook's process still has the tree open",
                display_path(&path)
            )));
        }
        // Listed first; Claude Code runs matching hooks in parallel, so this
        // is layout, not order.
        groups.insert(0, hook_group(exe, kind));
        changed = true;
    }

    if !changed {
        notes.push(Step::Note(format!(
            "{}: Claude Code hooks already installed",
            display_path(&path)
        )));
        return Ok(Plan::new(notes));
    }
    let after = serde_json::to_string_pretty(&root)? + "\n";
    notes.extend([
        Step::Mkdir(
            path.parent()
                .expect("settings path has a parent")
                .to_path_buf(),
        ),
        Step::Settings {
            backup: (!any_ours).then(|| backup_path(backups, &backup_key(&base, &tail))),
            path: path.clone(),
            before: before.clone(),
            after,
            mode,
        },
    ]);
    // A repoint takes no backup, but still rewrites the user's layout.
    if any_ours && before.is_some() {
        notes.extend(reformat_text(&path, None).map(Step::Note));
    }
    Ok(Plan::new(notes))
}

/// Removes bale's own entries (the events of [`install_plan`]) from
/// Claude Code's user settings.json, as install recognises them; a command the
/// user wrapped around bale stays. Groups, events and `hooks` emptied by the
/// removal go too, and the file when bale created it and nothing is left,
/// as `json_uninstall_steps` decides.
pub fn uninstall_plan(home: &Path, backups: &Path) -> Result<Plan> {
    let (base, tail) = settings_location(home);
    let path = base.join(&tail);
    let Some((before, mode)) = read_config(&path)? else {
        return Ok(Plan::new(vec![Step::Note(format!(
            "{}: no Claude Code hooks to remove",
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
    let mut emptied_events = Vec::new();
    if let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) {
        for (event, kind) in HOOKS {
            let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
                continue;
            };
            let mut emptied = vec![false; groups.len()];
            for (i, group) in groups.iter_mut().enumerate() {
                let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                    continue;
                };
                let n = entries.len();
                entries.retain(|h| {
                    let Some(cmd) = h["command"].as_str() else {
                        return true;
                    };
                    if bale_hook_path(cmd, kind).is_some() {
                        return false;
                    }
                    if mentions_hook(cmd, kind) {
                        steps.push(Step::Note(format!(
                            "note: left unchanged in {}: {event} hook `{cmd}` runs bale but \
                             is not bale's plain entry; remove it by hand",
                            display_path(&path)
                        )));
                    }
                    true
                });
                emptied[i] = entries.len() != n && entries.is_empty();
                changed |= entries.len() != n;
            }
            if emptied.contains(&true) {
                // A group the removal emptied held only bale's entry.
                let mut flags = emptied.into_iter();
                groups.retain(|_| !flags.next().unwrap_or(false));
                if groups.is_empty() {
                    emptied_events.push(vec!["hooks", event]);
                }
            }
        }
    }
    if !changed {
        steps.push(Step::Note(format!(
            "{}: no bale Claude Code hooks to remove",
            display_path(&path)
        )));
        return Ok(Plan::new(steps));
    }
    emptied_events.push(vec!["hooks"]);
    let key = backup_key(&base, &tail);
    steps.extend(json_uninstall_steps(
        backups,
        &key,
        &path,
        before,
        mode,
        &root,
        &emptied_events,
    )?);
    Ok(Plan::new(steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_quote_escapes_a_single_quote() {
        assert_eq!(sh_quote("/opt/it's/bale"), r"'/opt/it'\''s/bale'");
    }

    #[test]
    fn bale_hook_path_accepts_only_a_quoted_bale_path() {
        assert_eq!(
            bale_hook_path(r"'/opt/it'\''s/bale' hook claude create", "create").as_deref(),
            Some("/opt/it's/bale")
        );
        assert_eq!(
            bale_hook_path(
                r#""C:/Program Files/bale/bale.exe" hook claude create"#,
                "create"
            )
            .as_deref(),
            Some("C:/Program Files/bale/bale.exe")
        );
        for other in [
            "env X=1 '/x/bale' hook claude create",
            "nice '/x/bale' hook claude create",
            "/x/bale hook claude create",
            "'/x/not-bale' hook claude create",
            "'bale' hook claude create",
            "'bin/bale' hook claude create",
            "'/x/bale' hook claude remove",
            "'/x/bale' hook claude create --flag",
            "'/x/a' '/x/bale' hook claude create",
        ] {
            assert_eq!(bale_hook_path(other, "create"), None, "{other}");
        }
    }

    #[test]
    fn bale_hook_path_accepts_the_old_name() {
        assert_eq!(
            bale_hook_path("'/x/bale' hook claude-worktree-remove", "remove").as_deref(),
            Some("/x/bale")
        );
        assert_eq!(
            bale_hook_path("'/x/bale' hook claude-worktree-remove", "create"),
            None
        );
        assert!(mentions_hook(
            "nice '/x/bale' hook claude-worktree-create",
            "create"
        ));
        assert!(mentions_hook("'/x/bale' hook claude create", "create"));
        assert!(!mentions_hook("'/x/bale' hook claude remove", "create"));
    }

    #[test]
    fn mentions_hook_reads_the_command_words() {
        assert!(mentions_hook("nice '/x/bale' hook claude create", "create"));
        assert!(mentions_hook("env A=1 bale hook claude create", "create"));
        for not in [
            "echo ' hook claude create'",
            "'/x/bale' hook claude creates",
            "'/x/my-tool' hook claude create",
        ] {
            assert!(!mentions_hook(not, "create"), "{not}");
        }
    }

    #[test]
    fn hook_command_uses_the_claude_subcommand() {
        assert_eq!(
            hook_command(Path::new("/opt/bale"), "remove"),
            "'/opt/bale' hook claude remove"
        );
    }

    /// The real path and project dir name the brief verified.
    #[test]
    fn encode_project_path_matches_claude_code() {
        assert_eq!(
            encode_project_path(Path::new(
                "/Users/bmc/dev/bmcreations/code/code-ios-app/.claude/worktrees/sweet-greider-a23df3"
            )),
            "-Users-bmc-dev-bmcreations-code-code-ios-app--claude-worktrees-sweet-greider-a23df3"
        );
        assert_eq!(encode_project_path(Path::new("/a b/é_x")), "-a-b---x");
    }

    fn title_line(title: &str) -> String {
        format!(
            "{}\n",
            json!({"type": "custom-title", "customTitle": title, "sessionId": "s"})
        )
    }

    /// `<claude_home>/projects/<encoded tree>`, created, with the tree.
    fn project(claude_home: &Path) -> (PathBuf, PathBuf) {
        let tree = PathBuf::from("/work/repo/.claude/worktrees/sweet-greider-a23df3");
        let dir = claude_home
            .join("projects")
            .join(encode_project_path(&tree));
        fs::create_dir_all(&dir).unwrap();
        (tree, dir)
    }

    fn set_mtime(path: &Path, secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn title_is_the_last_custom_title_line() {
        let home = tempfile::tempdir().unwrap();
        let (tree, dir) = project(home.path());
        let text = [
            r#"{"type":"user","message":"hi"}"#.to_string() + "\n",
            title_line("First name"),
            r#"{"type":"assistant","message":"mentions custom-title"}"#.to_string() + "\n",
            title_line("Polls iOS remaining work"),
            // Unparseable and wrong-type lines after it are ignored.
            "{\"type\":\"custom-title\",\"customTitle\": broken\n".to_string(),
            r#"{"type":"user","message":"\"type\":\"custom-title\""}"#.to_string() + "\n",
        ]
        .concat();
        fs::write(dir.join("c00bee35.jsonl"), text).unwrap();
        assert_eq!(
            session_title(home.path(), &tree, Some("c00bee35")),
            Some(("c00bee35".into(), "Polls iOS remaining work".into()))
        );
    }

    #[test]
    fn named_session_is_read_and_newest_transcript_serves_unnamed() {
        let home = tempfile::tempdir().unwrap();
        let (tree, dir) = project(home.path());
        for (sid, title, mtime) in [("old", "Old work", 1_000), ("new", "New work", 2_000)] {
            let path = dir.join(format!("{sid}.jsonl"));
            fs::write(&path, title_line(title)).unwrap();
            set_mtime(&path, mtime);
        }
        assert_eq!(
            session_title(home.path(), &tree, None),
            Some(("new".into(), "New work".into()))
        );
        assert_eq!(
            session_title(home.path(), &tree, Some("old")),
            Some(("old".into(), "Old work".into()))
        );
        // A named session with no transcript here has no title: the newest
        // file may be another session's.
        assert_eq!(session_title(home.path(), &tree, Some("gone")), None);
    }

    #[test]
    fn title_early_in_a_large_transcript_is_found() {
        let home = tempfile::tempdir().unwrap();
        let (tree, dir) = project(home.path());
        let filler = format!("{}\n", json!({"type": "user", "message": "x".repeat(1000)}));
        let text = [title_line("Early title"), filler.repeat(900)].concat();
        assert!(text.len() > 3 * CHUNK && text.len() < TAIL_BUDGET as usize);
        fs::write(dir.join("s.jsonl"), text).unwrap();
        assert_eq!(
            session_title(home.path(), &tree, Some("s")).map(|t| t.1),
            Some("Early title".into())
        );
    }

    /// Only the last [`TAIL_BUDGET`] bytes are read, so an untitled
    /// transcript of any size costs at most that much.
    #[test]
    fn title_beyond_the_tail_budget_is_not_read() {
        let home = tempfile::tempdir().unwrap();
        let (tree, dir) = project(home.path());
        let filler = format!("{}\n", json!({"type": "user", "message": "x".repeat(1000)}));
        let tail = filler.repeat(TAIL_BUDGET as usize / filler.len() + 1);
        assert!(tail.len() > TAIL_BUDGET as usize);
        fs::write(
            dir.join("s.jsonl"),
            [title_line("Too early"), tail].concat(),
        )
        .unwrap();
        assert_eq!(session_title(home.path(), &tree, Some("s")), None);
    }

    /// A title line split across two 256 KiB chunks is still read whole.
    #[test]
    fn title_straddling_a_chunk_boundary_is_found() {
        let home = tempfile::tempdir().unwrap();
        let (tree, dir) = project(home.path());
        let title = title_line("Straddling title");
        let tail = r#"{"type":"user"}"#.to_string() + "\n";
        // The boundary falls 10 bytes into the title line.
        let tail_len = CHUNK - (title.len() - 10);
        let tail = tail.repeat(tail_len / tail.len() + 1)[..tail_len - 1].to_string() + "\n";
        let head = format!("{}\n", json!({"type": "user", "message": "y".repeat(5000)}));
        let text = [head, title.clone(), tail].concat();
        let start = text.find(&title).unwrap();
        assert!(start < text.len() - CHUNK && text.len() - CHUNK < start + title.len());
        fs::write(dir.join("s.jsonl"), text).unwrap();
        assert_eq!(
            session_title(home.path(), &tree, None).map(|t| t.1),
            Some("Straddling title".into())
        );
    }

    #[test]
    fn no_project_dir_or_no_title_is_none() {
        let home = tempfile::tempdir().unwrap();
        let tree = Path::new("/work/repo/wt");
        assert_eq!(session_title(home.path(), tree, None), None);
        let (tree, dir) = project(home.path());
        fs::write(dir.join("s.jsonl"), "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(session_title(home.path(), &tree, Some("s")), None);
    }

    #[cfg(unix)]
    fn dd_args(command: &str) -> Vec<PathBuf> {
        derived_data_paths(
            command,
            Path::new("/work/tree"),
            Some(Path::new("/home/me")),
        )
    }

    #[cfg(unix)]
    #[test]
    fn derived_data_path_takes_the_next_argument() {
        assert_eq!(
            dd_args("xcodebuild -scheme A -derivedDataPath /tmp/fc-1-dd build"),
            vec![PathBuf::from("/tmp/fc-1-dd")]
        );
        assert_eq!(
            dd_args("xcodebuild -derivedDataPath '/tmp/my dd' -quiet"),
            vec![PathBuf::from("/tmp/my dd")]
        );
        assert_eq!(
            dd_args("cd x && xcodebuild -derivedDataPath \"/tmp/q dd\""),
            vec![PathBuf::from("/tmp/q dd")]
        );
        assert_eq!(
            dd_args("xcodebuild -derivedDataPath /tmp/a\\ b"),
            vec![PathBuf::from("/tmp/a b")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn derived_data_path_expands_tilde_and_resolves_relative() {
        assert_eq!(
            dd_args("xcodebuild -derivedDataPath ~/dd"),
            vec![PathBuf::from("/home/me/dd")]
        );
        assert_eq!(
            dd_args("xcodebuild -derivedDataPath ./build/../dd"),
            vec![PathBuf::from("/work/tree/dd")]
        );
        assert_eq!(
            dd_args("xcodebuild -derivedDataPath dd"),
            vec![PathBuf::from("/work/tree/dd")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn derived_data_path_skips_what_it_cannot_resolve() {
        for cmd in [
            "xcodebuild build",
            "xcodebuild -derivedDataPath",
            "xcodebuild -derivedDataPath -quiet",
            "xcodebuild -derivedDataPath $TMPDIR/dd",
            "xcodebuild -derivedDataPath \"$(mktemp -d)\"",
            "xcodebuild -derivedDataPath 'unterminated",
            "echo not-derivedDataPath /tmp/x",
        ] {
            assert!(dd_args(cmd).is_empty(), "{cmd}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn derived_data_path_finds_every_use() {
        assert_eq!(
            dd_args("xcodebuild -derivedDataPath /a build; xcodebuild -derivedDataPath /b test"),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn derived_data_path_is_resolved_against_the_hook_cwd_and_linked() {
        use bale_core::registry::{workspace_id, Level, Record, Registry, SpmLink};
        use std::time::SystemTime;

        let tmp = tempfile::tempdir().unwrap();
        let home = BaleHome(tmp.path().join("bale"));
        let tree = bale_core::paths::canonicalize(tmp.path())
            .unwrap()
            .join("tree");
        fs::create_dir_all(&tree).unwrap();
        let shared = home.spm_dir().join("k1");
        fs::create_dir_all(&shared).unwrap();
        let id = workspace_id(&tree);
        Registry::open(home.clone())
            .put(&Record {
                id: id.clone(),
                repo_common_dir: PathBuf::from("/r/.git"),
                path: tree.clone(),
                admin_dir: PathBuf::from("/r/.git/worktrees/t"),
                branch: None,
                head: "0".repeat(40),
                level: Level::Hot,
                last_activity: SystemTime::now(),
                note: None,
                trash: Vec::new(),
                origin: None,
                pinned: false,
                scope: None,
                stash: None,
                restore_hook: false,
                spm: vec![SpmLink {
                    project: tree.join("A.xcodeproj"),
                    resolved: tree.join("Package.resolved"),
                    mtime: SystemTime::now(),
                    size: 0,
                    key: "k1".into(),
                    folder: tree.join("dd0"),
                }],
            })
            .unwrap();

        let payload =
            json!({"tool_input": {"command": "xcodebuild -derivedDataPath build/dd build"}});
        share_packages(&home, &payload, &tree, &id);
        let link = tree.join("build/dd/SourcePackages");
        assert_eq!(fs::read_link(&link).unwrap(), shared);

        // A command without the flag, or with an existing folder, changes nothing.
        let other = json!({"tool_input": {"command": "ls"}});
        share_packages(&home, &other, &tree, &id);
        share_packages(&home, &payload, &tree, &id);
        assert_eq!(fs::read_link(&link).unwrap(), shared);
        assert!(!tree.join("SourcePackages").exists());
    }
}
