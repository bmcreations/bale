//! Claude Code's WorktreeCreate and WorktreeRemove hooks
//! (`bale hook claude create|remove`), and their install into
//! `~/.claude/settings.json`.
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

use anyhow::{bail, Context, Result};
use bale_core::git::{self, Git};
use bale_core::ops::new::{new_workspace, workspace_path};
use bale_core::ops::up::register;
use bale_core::BaleHome;
use serde_json::{json, Map, Value};

use crate::install::{Plan, Step};
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

/// WorktreeRemove: colds the worktree (saving any uncommitted work), so
/// its directory is gone. Exits 0 only when the directory no longer exists.
pub fn remove(home: &BaleHome) -> Result<ExitCode> {
    let payload = read_payload()?;
    let path = PathBuf::from(field(&payload, "worktree_path")?);
    Ok(remove_tree(home, &path, Tool::Claude))
}

/// Each settings.json event bale hooks, with its `bale hook claude` subcommand.
const HOOKS: [(&str, &str); 2] = [("WorktreeCreate", "create"), ("WorktreeRemove", "remove")];

/// `s` single-quoted for sh, which Claude Code runs command hooks with.
fn sh_quote(s: &str) -> String {
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

/// Whether `cmd` runs bale's hook for `kind` in any form, plain or wrapped.
fn mentions_hook(cmd: &str, kind: &str) -> bool {
    hook_tails(kind).iter().any(|t| cmd.contains(t.as_str()))
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
    matches!(base, "bale" | "bale.exe").then_some(path)
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

/// One settings.json hook group running `bale hook claude <kind>`.
fn hook_group(exe: &Path, kind: &str) -> Value {
    json!({ "hooks": [{ "type": "command", "command": hook_command(exe, kind) }] })
}

/// Adds bale's WorktreeCreate and WorktreeRemove entries to
/// `<home>/.claude/settings.json`, keeping every other hook. bale's remove
/// entry goes before any user WorktreeRemove hook. A user WorktreeCreate hook
/// is refused: Claude Code would run both, and two hooks creating the same
/// worktree conflict.
pub fn install_plan(home: &Path, exe: &Path) -> Result<Plan> {
    let path = home.join(".claude").join("settings.json");
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
    let mut notes = Vec::new();
    for (event, kind) in HOOKS {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} in {} is not an array", path.display()))?;
        let want = hook_command(exe, kind);
        let mut ours = false;
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
            if bale_hook_path(cmd, kind).is_none() {
                // The user's own command around bale, e.g. `env ... bale hook ...`.
                notes.push(Step::Note(format!(
                    "note: left unchanged in {}: {event} hook `{cmd}` runs bale but is \
                     not bale's plain entry",
                    display_path(&path)
                )));
            } else if cmd != want {
                // bale's own entry from another binary path or the old name:
                // point it here.
                h["command"] = Value::String(want.clone());
                changed = true;
            }
        }
        if ours {
            continue;
        }
        if kind == "create" && !groups.is_empty() {
            bail!(
                "{} already has a {event} hook; remove it first, then rerun",
                path.display()
            );
        }
        // Before the user's remove hooks, so bale colds the tree first.
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
            path,
            before,
            after,
            mode,
        },
    ]);
    Ok(Plan::new(notes))
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
    fn hook_command_uses_the_claude_subcommand() {
        assert_eq!(
            hook_command(Path::new("/opt/bale"), "remove"),
            "'/opt/bale' hook claude remove"
        );
    }
}
