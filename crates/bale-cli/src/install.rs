//! `bale install`: the sweep schedule, and the opt-in Claude Code and git
//! hooks. Every effect is first built as a [`Plan`], which `--print-only`
//! prints and a real run applies, so the two cannot drift apart.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use bale_core::git::Git;
use serde_json::{json, Map, Value};

use crate::hook::display_path;

/// First line after the shebang of the git hook bale writes; marks it as ours.
const GIT_HOOK_MARKER: &str = "# Installed by `bale install --git-hook`.";
const LAUNCHD_LABEL: &str = "dev.bale.sweep";

/// One effect of an install.
enum Step {
    Mkdir(PathBuf),
    Write {
        path: PathBuf,
        content: String,
        executable: bool,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    /// Rewrites settings.json atomically, backing up `before` the first time.
    Settings {
        path: PathBuf,
        before: Option<String>,
        after: String,
    },
    Run {
        args: Vec<String>,
        ignore_failure: bool,
    },
    Note(String),
}

#[derive(Default)]
pub struct Plan(Vec<Step>);

impl Plan {
    /// Prints what applying would do.
    pub fn print(&self) {
        for step in &self.0 {
            match step {
                Step::Mkdir(p) => println!("mkdir -p {}", display_path(p)),
                Step::Write { path, content, .. } => {
                    println!("write {}:", display_path(path));
                    for line in content.lines() {
                        println!("    {line}");
                    }
                }
                Step::Rename { from, to } => {
                    println!("mv {} {}", display_path(from), display_path(to))
                }
                Step::Settings {
                    path,
                    before,
                    after,
                } => {
                    println!("update {}:", display_path(path));
                    print!("{}", diff(before.as_deref().unwrap_or(""), after));
                }
                Step::Run {
                    args,
                    ignore_failure,
                } => {
                    let tail = if *ignore_failure {
                        "  (failure ignored)"
                    } else {
                        ""
                    };
                    println!("run: {}{tail}", shell_words(args));
                }
                Step::Note(n) => println!("{n}"),
            }
        }
    }

    /// Carries out every step, stopping at the first failure.
    pub fn apply(&self) -> Result<()> {
        for step in &self.0 {
            match step {
                Step::Mkdir(p) => {
                    fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?
                }
                Step::Write {
                    path,
                    content,
                    executable,
                } => {
                    write_atomic(path, content)?;
                    if *executable {
                        make_executable(path)?;
                    }
                    println!("wrote {}", display_path(path));
                }
                Step::Rename { from, to } => {
                    fs::rename(from, to).with_context(|| {
                        format!("renaming {} to {}", from.display(), to.display())
                    })?;
                    println!("moved {} to {}", display_path(from), display_path(to));
                }
                Step::Settings {
                    path,
                    before,
                    after,
                } => {
                    print!("{}", diff(before.as_deref().unwrap_or(""), after));
                    let backup = path.with_file_name("settings.json.bak-bale");
                    if before.is_some() && fs::symlink_metadata(&backup).is_err() {
                        fs::copy(path, &backup)
                            .with_context(|| format!("backing up {}", path.display()))?;
                    }
                    write_atomic(path, after)?;
                    println!("updated {}", display_path(path));
                }
                Step::Run {
                    args,
                    ignore_failure,
                } => {
                    let out = Command::new(&args[0])
                        .args(&args[1..])
                        .output()
                        .with_context(|| format!("running {}", shell_words(args)))?;
                    if !out.status.success() && !ignore_failure {
                        bail!(
                            "{} failed: {}",
                            shell_words(args),
                            String::from_utf8_lossy(&out.stderr).trim()
                        );
                    }
                    println!("ran {}", shell_words(args));
                }
                Step::Note(n) => println!("{n}"),
            }
        }
        Ok(())
    }

    fn extend(&mut self, other: Plan) {
        self.0.extend(other.0);
    }
}

/// Builds the whole install. With neither hook flag it installs the sweep
/// schedule; otherwise only the requested hooks.
pub fn plan(claude_hooks: bool, git_hook: Option<&Path>) -> Result<Plan> {
    let exe = bale_exe()?;
    let home = home_dir()?;
    let mut plan = Plan::default();
    if !claude_hooks && git_hook.is_none() {
        plan.extend(schedule_plan(&home, &exe)?);
    }
    if claude_hooks {
        plan.extend(claude_hooks_plan(&home, &exe)?);
    }
    if let Some(repo) = git_hook {
        plan.extend(git_hook_plan(repo, &exe)?);
    }
    Ok(plan)
}

/// The running binary's absolute path; schedulers have a minimal PATH.
fn bale_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the bale binary")?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    Ok(PathBuf::from(display_path(&exe)))
}

/// `HOME`, else `USERPROFILE` (Windows), so tests can redirect it.
fn home_dir() -> Result<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")
}

fn schedule_plan(home: &Path, exe: &Path) -> Result<Plan> {
    let exe_s = display_path(exe);
    let mut steps = Vec::new();
    if cfg!(target_os = "macos") {
        let agents = home.join("Library").join("LaunchAgents");
        let logs = home.join("Library").join("Logs").join("bale");
        let plist = agents.join(format!("{LAUNCHD_LABEL}.plist"));
        let uid = current_uid()?;
        steps.push(Step::Mkdir(agents));
        steps.push(Step::Mkdir(logs.clone()));
        steps.push(Step::Write {
            path: plist.clone(),
            content: launchd_plist(&exe_s, &display_path(&logs.join("sweep.log"))),
            executable: false,
        });
        steps.push(Step::Run {
            args: vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{LAUNCHD_LABEL}"),
            ],
            ignore_failure: true,
        });
        steps.push(Step::Run {
            args: vec![
                "launchctl".into(),
                "bootstrap".into(),
                format!("gui/{uid}"),
                display_path(&plist),
            ],
            ignore_failure: false,
        });
    } else if cfg!(windows) {
        steps.push(Step::Run {
            args: [
                "schtasks",
                "/Create",
                "/SC",
                "MINUTE",
                "/MO",
                "30",
                "/TN",
                "bale-sweep",
                "/TR",
            ]
            .iter()
            .map(|s| s.to_string())
            .chain([format!("\"{exe_s}\" sweep"), "/F".to_string()])
            .collect(),
            ignore_failure: false,
        });
    } else {
        let units = home.join(".config").join("systemd").join("user");
        steps.push(Step::Mkdir(units.clone()));
        steps.push(Step::Write {
            path: units.join("bale-sweep.service"),
            content: format!(
                "[Unit]\nDescription=bale sweep\n\n[Service]\nType=oneshot\nExecStart={} sweep\n",
                systemd_quote(&exe_s)
            ),
            executable: false,
        });
        steps.push(Step::Write {
            path: units.join("bale-sweep.timer"),
            content: "[Unit]\nDescription=Run bale sweep every 30 minutes\n\n[Timer]\n\
                      OnBootSec=5min\nOnUnitActiveSec=30min\n\n[Install]\nWantedBy=timers.target\n"
                .to_string(),
            executable: false,
        });
        for args in [
            &["systemctl", "--user", "daemon-reload"][..],
            &["systemctl", "--user", "enable", "--now", "bale-sweep.timer"],
        ] {
            steps.push(Step::Run {
                args: args.iter().map(|s| s.to_string()).collect(),
                ignore_failure: false,
            });
        }
    }
    Ok(Plan(steps))
}

fn current_uid() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("running id -u")?;
    if !out.status.success() {
        bail!("id -u failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn launchd_plist(exe: &str, log: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>sweep</string>
  </array>
  <key>StartInterval</key>
  <integer>1800</integer>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
        xml_escape(exe),
        xml_escape(log),
        xml_escape(log)
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A systemd ExecStart word: double-quoted, with `\`, `"`, `%` and `$` escaped.
fn systemd_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// The command line a Claude Code hook entry runs.
fn hook_command(exe: &Path, kind: &str) -> String {
    format!("\"{}\" hook {kind}", display_path(exe).replace('\\', "/"))
}

fn claude_hooks_plan(home: &Path, exe: &Path) -> Result<Plan> {
    let path = home.join(".claude").join("settings.json");
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
    for (event, kind) in [
        ("WorktreeCreate", "claude-worktree-create"),
        ("WorktreeRemove", "claude-worktree-remove"),
    ] {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} in {} is not an array", path.display()))?;
        let commands: Vec<&str> = groups
            .iter()
            .filter_map(|g| g["hooks"].as_array())
            .flatten()
            .filter_map(|h| h["command"].as_str())
            .collect();
        if commands.iter().any(|c| c.contains(&format!("hook {kind}"))) {
            continue;
        }
        if !groups.is_empty() {
            // Claude Code would run both; two hooks creating or removing the
            // same worktree conflict.
            bail!(
                "{} already has a {event} hook; remove it first, then rerun",
                path.display()
            );
        }
        groups.push(json!({
            "hooks": [{ "type": "command", "command": hook_command(exe, kind) }]
        }));
        changed = true;
    }

    if !changed {
        return Ok(Plan(vec![Step::Note(format!(
            "{}: Claude Code hooks already installed",
            display_path(&path)
        ))]));
    }
    let after = serde_json::to_string_pretty(&root)? + "\n";
    Ok(Plan(vec![
        Step::Mkdir(
            path.parent()
                .expect("settings path has a parent")
                .to_path_buf(),
        ),
        Step::Settings {
            path,
            before,
            after,
        },
    ]))
}

fn git_hook_script(exe: &Path) -> String {
    let exe = display_path(exe).replace('\\', "/").replace('\'', r"'\''");
    format!(
        "#!/bin/sh\n\
         {GIT_HOOK_MARKER}\n\
         # Registers new linked worktrees with bale, then runs the previous hook.\n\
         BALE='{exe}'\n\
         command -v \"$BALE\" >/dev/null 2>&1 || BALE=bale\n\
         if command -v \"$BALE\" >/dev/null 2>&1; then\n\
         \x20 \"$BALE\" hook git-post-checkout \"$@\" || true\n\
         fi\n\
         PREV=\"$(dirname \"$0\")/post-checkout.pre-bale\"\n\
         if [ -x \"$PREV\" ]; then\n\
         \x20 exec \"$PREV\" \"$@\"\n\
         fi\n\
         exit 0\n"
    )
}

fn git_hook_plan(repo: &Path, exe: &Path) -> Result<Plan> {
    let g = Git::at(repo);
    let dir = g.run(&["rev-parse", "--path-format=absolute", "--git-path", "hooks"])?;
    let dir = PathBuf::from(dir.trim_end_matches(['\n', '\r']));
    let configured = g
        .run(&["config", "--get", "core.hooksPath"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if configured {
        if let Ok(top) = g.run(&["rev-parse", "--show-toplevel"]) {
            let top = canon_lenient(Path::new(top.trim_end_matches(['\n', '\r'])));
            if canon_lenient(&dir).starts_with(&top) {
                bail!(
                    "core.hooksPath points at {} inside the working tree; bale does not \
                     change tracked hooks. Add this line to its post-checkout instead:\n  \
                     \"{}\" hook git-post-checkout \"$@\" || true",
                    display_path(&dir),
                    display_path(exe).replace('\\', "/")
                );
            }
        }
    }

    let hook = dir.join("post-checkout");
    let prev = dir.join("post-checkout.pre-bale");
    let script = git_hook_script(exe);
    let mut steps = vec![Step::Mkdir(dir.clone())];
    match fs::read(&hook) {
        Ok(bytes) => {
            let current = String::from_utf8_lossy(&bytes);
            if current.contains(GIT_HOOK_MARKER) {
                if current == script {
                    return Ok(Plan(vec![Step::Note(format!(
                        "{}: bale post-checkout hook already installed",
                        display_path(&hook)
                    ))]));
                }
                // Ours, from another bale binary path: rewrite in place.
            } else {
                if fs::symlink_metadata(&prev).is_ok() {
                    bail!(
                        "{} exists and {} is not bale's; move one of them, then rerun",
                        prev.display(),
                        hook.display()
                    );
                }
                steps.push(Step::Rename {
                    from: hook.clone(),
                    to: prev,
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", hook.display())),
    }
    steps.push(Step::Write {
        path: hook,
        content: script,
        executable: true,
    });
    Ok(Plan(steps))
}

/// Canonical form of `p`, or of its nearest existing ancestor joined with the
/// rest, so a hooks dir that does not exist yet still compares correctly.
fn canon_lenient(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = fs::canonicalize(cur) {
            let mut out = c;
            out.extend(rest.iter().rev());
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Writes `content` to a temp file beside `path`, then renames it over `path`.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temp file in {}", dir.display()))?;
    tmp.write_all(content.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Args joined for display, double-quoting any that need it.
fn shell_words(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains([' ', '"', '\'', '\t']) {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A line diff of `before` to `after`: changed lines with two lines of context.
fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    // LCS table; settings files are small.
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            ops.push(('+', b[j]));
            j += 1;
        } else {
            ops.push(('-', a[i]));
            i += 1;
        }
    }
    const CONTEXT: usize = 2;
    let near_change = |k: usize| {
        let lo = k.saturating_sub(CONTEXT);
        let hi = (k + CONTEXT + 1).min(ops.len());
        ops[lo..hi].iter().any(|(c, _)| *c != ' ')
    };
    let mut out = String::new();
    let mut skipped = false;
    for (k, (c, line)) in ops.iter().enumerate() {
        if near_change(k) {
            if skipped {
                out.push_str("  ...\n");
                skipped = false;
            }
            out.push_str(&format!("{c} {line}\n"));
        } else {
            skipped = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_marks_added_lines_with_context() {
        let d = diff("a\nb\nc\nd\ne\nf\n", "a\nb\nc\nX\nd\ne\nf\n");
        assert_eq!(d, "  ...\n  b\n  c\n+ X\n  d\n  e\n");
    }

    #[test]
    fn systemd_quote_escapes_specials() {
        assert_eq!(systemd_quote("/a b/%x$y"), "\"/a b/%%x$$y\"");
    }

    #[test]
    fn git_hook_script_runs_bale_once_and_chains() {
        let s = git_hook_script(Path::new("/opt/it's/bale"));
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains(GIT_HOOK_MARKER));
        assert!(s.contains(r"BALE='/opt/it'\''s/bale'"));
        assert_eq!(s.matches("hook git-post-checkout").count(), 1);
        assert!(s.contains("exec \"$PREV\" \"$@\""));
    }
}
