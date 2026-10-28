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
const CLAUDE_HOOKS: [(&str, &str); 2] = [
    ("WorktreeCreate", "claude-worktree-create"),
    ("WorktreeRemove", "claude-worktree-remove"),
];
const REFORMAT_NOTE: &str = "note: settings.json is reformatted (2-space JSON); \
                             settings.json.bak-bale keeps the original";

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
        /// The original file's permissions, restored after the rewrite.
        mode: Option<fs::Permissions>,
    },
    Run {
        args: Vec<String>,
        ignore_failure: bool,
        /// Total tries, 500 ms apart, before a failure counts.
        attempts: u32,
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
                    ..
                } => {
                    println!("update {}:", display_path(path));
                    print!("{}", diff(before.as_deref().unwrap_or(""), after));
                    if before.is_some() {
                        println!("{REFORMAT_NOTE}");
                    }
                }
                Step::Run {
                    args,
                    ignore_failure,
                    attempts,
                } => {
                    let tail = if *ignore_failure {
                        "  (failure ignored)".to_string()
                    } else if *attempts > 1 {
                        format!("  (up to {attempts} tries)")
                    } else {
                        String::new()
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
                    mode,
                } => {
                    print!("{}", diff(before.as_deref().unwrap_or(""), after));
                    if before.is_some() {
                        println!("{REFORMAT_NOTE}");
                    }
                    let backup = path.with_file_name("settings.json.bak-bale");
                    if before.is_some() && fs::symlink_metadata(&backup).is_err() {
                        fs::copy(path, &backup)
                            .with_context(|| format!("backing up {}", path.display()))?;
                    }
                    write_atomic(path, after)?;
                    if let Some(mode) = mode {
                        fs::set_permissions(path, mode.clone())
                            .with_context(|| format!("restoring the mode of {}", path.display()))?;
                    }
                    println!("updated {}", display_path(path));
                }
                Step::Run {
                    args,
                    ignore_failure,
                    attempts,
                } => {
                    let mut tries = 0;
                    let out = loop {
                        tries += 1;
                        let out = Command::new(&args[0])
                            .args(&args[1..])
                            .output()
                            .with_context(|| format!("running {}", shell_words(args)))?;
                        if out.status.success() || tries >= *attempts {
                            break out;
                        }
                        // launchd can still be tearing down the job bootout removed.
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    };
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

/// The running binary's absolute path as invoked (not canonicalized, so a
/// package manager's stable symlink such as `/opt/homebrew/bin/bale` is kept);
/// schedulers have a minimal PATH. Warns when it looks like a build or
/// versioned directory that will move.
fn bale_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the bale binary")?;
    let exe = PathBuf::from(display_path(&exe));
    if is_unstable_location(&exe) {
        eprintln!(
            "warning: {} is a build or versioned path; hooks and the schedule written now \
             stop working when it moves. Install from a stable path such as the one on PATH.",
            exe.display()
        );
    }
    Ok(exe)
}

/// Under a cargo `target/` dir, or a Homebrew `Cellar/<formula>/<version>` dir.
fn is_unstable_location(exe: &Path) -> bool {
    let parts: Vec<&std::ffi::OsStr> = exe.parent().map(|p| p.iter().collect()).unwrap_or_default();
    parts.iter().any(|p| *p == "target")
        || parts
            .iter()
            .position(|p| *p == "Cellar")
            .is_some_and(|i| parts.len() > i + 2)
}

/// The home dir, from `HOME` or `USERPROFILE` (the latter first on Windows),
/// so tests can redirect it.
fn home_dir() -> Result<PathBuf> {
    let order = if cfg!(windows) {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    order
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")
}

/// `PATH`, and `BALE_HOME` when set, captured at install time so the
/// scheduled sweep sees the same environment.
fn schedule_env() -> Vec<(&'static str, String)> {
    ["PATH", "BALE_HOME"]
        .into_iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (k, v))
        })
        .collect()
}

fn schedule_plan(home: &Path, exe: &Path) -> Result<Plan> {
    let exe_s = display_path(exe);
    let env = schedule_env();
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
            content: launchd_plist(&exe_s, &display_path(&logs.join("sweep.log")), &env),
            executable: false,
        });
        steps.push(Step::Run {
            args: vec![
                "launchctl".into(),
                "bootout".into(),
                format!("gui/{uid}/{LAUNCHD_LABEL}"),
            ],
            ignore_failure: true,
            attempts: 1,
        });
        steps.push(Step::Run {
            args: vec![
                "launchctl".into(),
                "bootstrap".into(),
                format!("gui/{uid}"),
                display_path(&plist),
            ],
            ignore_failure: false,
            attempts: 5,
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
            attempts: 1,
        });
    } else {
        let units = home.join(".config").join("systemd").join("user");
        steps.push(Step::Mkdir(units.clone()));
        steps.push(Step::Write {
            path: units.join("bale-sweep.service"),
            content: format!(
                "[Unit]\nDescription=bale sweep\n\n[Service]\nType=oneshot\n{}ExecStart={} sweep\n",
                env.iter()
                    .map(|(k, v)| format!(
                        "Environment={}\n",
                        systemd_env_quote(&format!("{k}={v}"))
                    ))
                    .collect::<String>(),
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
                attempts: 1,
            });
        }
    }
    if !cfg!(windows) && !env.is_empty() {
        let keys: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        steps.push(Step::Note(format!(
            "note: the schedule runs with the {} captured now; rerun `bale install` \
             after changing {}",
            keys.join(" and "),
            if keys.len() > 1 { "them" } else { "it" }
        )));
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

fn launchd_plist(exe: &str, log: &str, env: &[(&str, String)]) -> String {
    let env_dict = if env.is_empty() {
        String::new()
    } else {
        let entries: String = env
            .iter()
            .map(|(k, v)| {
                format!(
                    "    <key>{}</key>\n    <string>{}</string>\n",
                    xml_escape(k),
                    xml_escape(v)
                )
            })
            .collect();
        format!("  <key>EnvironmentVariables</key>\n  <dict>\n{entries}  </dict>\n")
    };
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
{}</dict>
</plist>
"#,
        xml_escape(exe),
        xml_escape(log),
        xml_escape(log),
        env_dict
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

/// A systemd `Environment=` assignment: double-quoted, `\`, `"` and `%` escaped.
fn systemd_env_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// `s` single-quoted for sh, which Claude Code runs command hooks with.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command line a Claude Code hook entry runs.
fn hook_command(exe: &Path, kind: &str) -> String {
    format!(
        "{} hook {kind}",
        sh_quote(&display_path(exe).replace('\\', "/"))
    )
}

/// The exe path in `cmd` when `cmd` is exactly bale's own entry for `kind`:
/// a single-quoted (sh) or double-quoted path to `bale` or `bale.exe`,
/// then `hook <kind>`. Anything else, such as a command wrapped in `env` or
/// `nice`, is the user's and gives `None`.
fn bale_hook_path(cmd: &str, kind: &str) -> Option<String> {
    let quoted = cmd.strip_suffix(&format!(" hook {kind}"))?;
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

/// One settings.json hook group running `bale hook <kind>`.
fn hook_group(exe: &Path, kind: &str) -> Value {
    json!({ "hooks": [{ "type": "command", "command": hook_command(exe, kind) }] })
}

fn claude_hooks_plan(home: &Path, exe: &Path) -> Result<Plan> {
    let path = home.join(".claude").join("settings.json");
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if meta.as_ref().is_some_and(|m| m.file_type().is_symlink()) {
        let hooks: Map<String, Value> = CLAUDE_HOOKS
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
    for (event, kind) in CLAUDE_HOOKS {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} in {} is not an array", path.display()))?;
        let want = hook_command(exe, kind);
        let marker = format!("hook {kind}");
        let mut ours = false;
        for h in groups
            .iter_mut()
            .filter_map(|g| g["hooks"].as_array_mut())
            .flatten()
        {
            let Some(cmd) = h["command"].as_str() else {
                continue;
            };
            if !cmd.contains(&marker) {
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
                // bale's own entry from another binary path: point it here.
                h["command"] = Value::String(want.clone());
                changed = true;
            }
        }
        if ours {
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
        groups.push(hook_group(exe, kind));
        changed = true;
    }

    if !changed {
        notes.push(Step::Note(format!(
            "{}: Claude Code hooks already installed",
            display_path(&path)
        )));
        return Ok(Plan(notes));
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
    Ok(Plan(notes))
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
    // `<scope>\t<value>`, e.g. `global\t~/.githooks`.
    let scope = g
        .run(&["config", "--show-scope", "--get", "core.hooksPath"])
        .ok()
        .and_then(|s| s.split_once('\t').map(|(scope, _)| scope.to_string()));
    let configured = scope.is_some();
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
    if let Some(scope @ ("global" | "system")) = scope.as_deref() {
        steps.push(Step::Note(format!(
            "note: core.hooksPath comes from your {scope} git config, so this hook in {} \
             applies to every repo that uses it, not only {}",
            display_path(&dir),
            display_path(repo)
        )));
    }
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
        if let Ok(c) = bale_core::paths::canonicalize(cur) {
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
    shell_words_for(args, cfg!(windows))
}

/// `args` as one command line. An arg with a space, tab or quote is
/// double-quoted. For sh a backslash in it is escaped; on Windows a
/// backslash is literal except in a run before a `"`, which is doubled, so a
/// path prints as typed.
fn shell_words_for(args: &[String], windows: bool) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && !a.contains([' ', '"', '\'', '\t']) {
                a.clone()
            } else if windows {
                windows_quote(a)
            } else {
                format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `a` double-quoted the way `CommandLineToArgvW` reads it back.
fn windows_quote(a: &str) -> String {
    let mut out = String::from('"');
    let mut slashes = 0;
    for c in a.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
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
    fn cellar_version_dir_is_unstable() {
        assert!(is_unstable_location(Path::new(
            "/opt/homebrew/Cellar/bale/0.3.1/bin/bale"
        )));
        assert!(!is_unstable_location(Path::new("/opt/homebrew/bin/bale")));
    }

    #[test]
    fn windows_words_keep_path_backslashes() {
        let args = [
            "/TR".to_string(),
            r#""D:\a b\bale.exe" sweep"#.to_string(),
            r"C:\dir with space\".to_string(),
        ];
        assert_eq!(
            shell_words_for(&args, true),
            r#"/TR "\"D:\a b\bale.exe\" sweep" "C:\dir with space\\""#
        );
    }

    #[test]
    fn unix_words_escape_backslashes() {
        let args = [r"a b\c".to_string()];
        assert_eq!(shell_words_for(&args, false), r#""a b\\c""#);
    }

    #[test]
    fn sh_quote_escapes_a_single_quote() {
        assert_eq!(sh_quote("/opt/it's/bale"), r"'/opt/it'\''s/bale'");
    }

    #[test]
    fn bale_hook_path_accepts_only_a_quoted_bale_path() {
        let kind = "claude-worktree-create";
        assert_eq!(
            bale_hook_path(r"'/opt/it'\''s/bale' hook claude-worktree-create", kind).as_deref(),
            Some("/opt/it's/bale")
        );
        assert_eq!(
            bale_hook_path(
                r#""C:/Program Files/bale/bale.exe" hook claude-worktree-create"#,
                kind
            )
            .as_deref(),
            Some("C:/Program Files/bale/bale.exe")
        );
        for other in [
            "env X=1 '/x/bale' hook claude-worktree-create",
            "nice '/x/bale' hook claude-worktree-create",
            "/x/bale hook claude-worktree-create",
            "'/x/not-bale' hook claude-worktree-create",
            "'/x/bale' hook claude-worktree-remove",
            "'/x/bale' hook claude-worktree-create --flag",
            "'/x/a' '/x/bale' hook claude-worktree-create",
        ] {
            assert_eq!(bale_hook_path(other, kind), None, "{other}");
        }
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
