//! T3 Code's project scripts (`bale hook t3 setup|settle`), and their install
//! into T3 Code's user settings.
//!
//! Contract, from T3 Code (Beta)'s `app.asar` (read 2026-10-28):
//! - T3 Code makes a thread's worktree itself with `git worktree add` and
//!   removes it with `git worktree remove`, with no hook around either. Removal
//!   is left to `bale sweep`.
//! - Its settings are `<base>/userdata/settings.json`, `<base>` being
//!   `$T3CODE_HOME` (trimmed, `~` expanded) or else `~/.t3`. The server watches
//!   the file and reloads it when it changes.
//! - `defaultProjectScripts` holds project scripts:
//!   `{id, name, command, icon, runOnWorktreeCreate, runOnSettle?, async?, …}`,
//!   with `icon` one of play, test, lint, configure, build, debug. A script with
//!   `runOnWorktreeCreate` runs after a thread's worktree is made; one with
//!   `runOnSettle: true` runs in the thread's worktree each time the thread
//!   settles (a turn ends). T3 Code picks the *first* script with each flag,
//!   so a second one never runs.
//! - A project's scripts are its `projectSettingsOverrides[id]
//!   .defaultProjectScripts` when set, else `defaultProjectScripts` when
//!   `projectSettingsFolded` is on, else the project's own scripts or legacy
//!   overrides.
//! - The command is typed into a terminal running the user's shell, in the
//!   worktree, with `T3CODE_WORKTREE_PATH` and `T3CODE_PROJECT_ROOT` set; each
//!   line is entered on its own.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use bale_core::ops::meta::{set_origin, touch, Touch};
use bale_core::ops::up::register;
use bale_core::registry::Origin;
use bale_core::BaleHome;
use serde_json::{json, Map, Value};

use crate::claude::sh_quote;
use crate::cmdline::{bale_call, own_exe, sh_guarded};
use crate::install::{
    backup_key, backup_path, json_uninstall_steps, read_config, repoint_note, Plan, Step,
};
use crate::{as_hook, display_path, Tool};

/// The worktree a script runs for: `T3CODE_WORKTREE_PATH`, or the cwd.
fn worktree_path() -> Result<PathBuf> {
    match std::env::var_os("T3CODE_WORKTREE_PATH") {
        Some(p) if !p.is_empty() => Ok(PathBuf::from(p)),
        _ => std::env::current_dir().context("reading the current directory"),
    }
}

/// setup: registers the worktree and records T3 Code as its origin. Never
/// fails T3 Code's script, and prints nothing unless something went wrong.
pub fn setup(home: &BaleHome) -> ExitCode {
    as_hook(Tool::T3, || {
        if let Err(e) = worktree_path().and_then(|path| adopt(home, &path)) {
            eprintln!("bale: t3 setup: {e:#}");
        }
    });
    ExitCode::SUCCESS
}

/// settle: records that a turn ended in the worktree (see [`touch`]). It never
/// ups or colds the tree, which would drop the build the next turn reuses;
/// sweep's idle policy decides that, counting from this turn's end. A
/// worktree bale does not track yet is registered first: settle runs only in
/// worktrees T3 Code made for a thread, which are the trees bale manages, so
/// this covers threads started before the install or whose setup did not run.
/// Never fails T3 Code's script, and prints nothing unless something went
/// wrong.
pub fn settle(home: &BaleHome) -> ExitCode {
    as_hook(Tool::T3, || {
        let result = worktree_path().and_then(|path| match touch(home, &path)? {
            Touch::Untracked => {
                adopt(home, &path)?;
                touch(home, &path).map(|_| ())
            }
            Touch::Touched | Touch::Locked => Ok(()),
        });
        if let Err(e) = result {
            eprintln!("bale: t3 settle: {e:#}");
        }
    });
    ExitCode::SUCCESS
}

/// Registers the tree at `path` with T3 Code as its origin.
fn adopt(home: &BaleHome, path: &Path) -> Result<()> {
    register(home, path)?;
    // T3 Code passes no thread id, and bale invents no title.
    let origin = Origin {
        tool: Tool::T3.name().to_string(),
        session: String::new(),
        title: None,
    };
    if let Err(e) = set_origin(home, &path.to_string_lossy(), origin) {
        eprintln!("bale: origin not recorded: {e:#}");
    }
    Ok(())
}

/// One of bale's two scripts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Setup,
    Settle,
}

impl Kind {
    const BOTH: [Kind; 2] = [Kind::Setup, Kind::Settle];

    /// The words of bale's command after the exe.
    fn args(self) -> [&'static str; 3] {
        match self {
            Kind::Setup => ["hook", "t3", "setup"],
            Kind::Settle => ["hook", "t3", "settle"],
        }
    }

    /// The id of the script bale adds when the settings have none to merge
    /// into.
    fn id(self) -> &'static str {
        match self {
            Kind::Setup => "bale-setup",
            Kind::Settle => "bale-settle",
        }
    }

    /// The setting that makes T3 Code run a script at this point.
    fn flag(self) -> &'static str {
        match self {
            Kind::Setup => "runOnWorktreeCreate",
            Kind::Settle => "runOnSettle",
        }
    }

    /// Whether T3 Code runs `script` at this point.
    fn runs(self, script: &Value) -> bool {
        script.get(self.flag()).and_then(Value::as_bool) == Some(true)
    }

    /// The whole script bale adds, `command` being [`hook_command`].
    fn entry(self, command: String) -> Value {
        match self {
            Kind::Setup => json!({
                "id": self.id(),
                "name": "bale setup",
                "command": command,
                "icon": "configure",
                "runOnWorktreeCreate": true,
            }),
            Kind::Settle => json!({
                "id": self.id(),
                "name": "bale settle",
                "command": command,
                "icon": "configure",
                "runOnWorktreeCreate": false,
                "runOnSettle": true,
            }),
        }
    }
}

/// The command bale's script for `kind` runs, on one line (T3 Code enters
/// each line on its own). It runs bale only when the exe is there, so a
/// settings file synced to a machine without bale at this path still runs
/// the user's part of a merged script.
fn hook_command(exe: &Path, kind: Kind) -> String {
    let exe = sh_quote(&display_path(exe).replace('\\', "/"));
    sh_guarded(&exe, &kind.args().join(" "))
}

/// The text between bale's call and the user's command in a merged script.
/// `fi;` ends bale's `if`, so the user's command runs whatever bale did.
const JOIN: &str = "; ";

/// `cmd` split into bale's own call for `kind` (from any exe path) and the
/// user's command after it, `None` when bale's call is the whole command.
/// `None` overall when `cmd` does not start with bale's call.
fn split(cmd: &str, kind: Kind) -> Option<(&str, Option<&str>)> {
    let args = kind.args();
    if own_exe(cmd, &args).is_some() {
        return Some((cmd, None));
    }
    let end = cmd.find(&format!("; fi{JOIN}"))? + "; fi".len();
    let (bale, rest) = (&cmd[..end], &cmd[end + JOIN.len()..]);
    own_exe(bale, &args)?;
    Some((bale, Some(rest)))
}

/// Whether `script` is the one bale added for `kind`: bale's id, running only
/// bale's own command.
fn is_bales(script: &Value, kind: Kind) -> bool {
    script.get("id").and_then(Value::as_str) == Some(kind.id())
        && script
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| split(c, kind) == Some((c, None)))
}

/// Whether `cmd` runs bale's `kind` command in any shape.
fn runs_bale(cmd: &str, kind: Kind) -> bool {
    bale_call(cmd, &kind.args()).is_some()
}

/// A script's name for messages: its `name`, else its `id`.
fn label(script: &Value) -> String {
    ["name", "id"]
        .iter()
        .find_map(|k| script.get(*k).and_then(Value::as_str))
        .unwrap_or("?")
        .to_string()
}

/// T3 Code's settings file under `base` (see `crate::t3_home`).
fn settings_path(base: &Path) -> PathBuf {
    base.join("userdata").join("settings.json")
}

/// The path, under the T3 Code base dir, that the settings backup is named for.
fn settings_tail() -> PathBuf {
    Path::new("userdata").join("settings.json")
}

/// Adds bale's setup and settle scripts to `defaultProjectScripts` in T3
/// Code's settings under `base`, keeping every other key in its order.
///
/// T3 Code runs only the first script with each flag, so when one exists
/// bale's guarded call goes in front of its command (`<bale>; <theirs>`)
/// rather than in a second script that would never run. Otherwise bale adds
/// its own script, `bale-setup` or `bale-settle`. A user script that runs
/// both on worktree create and on settle gets only bale's settle call, which
/// registers an untracked tree too. bale's call from another exe path is
/// repointed here. The file as it was is backed up in `backups` the first
/// time bale is added to it.
pub fn install_plan(base: &Path, exe: &Path, backups: &Path) -> Result<Plan> {
    let path = settings_path(base);
    let (before, mode) = match read_config(&path)? {
        Some((text, mode)) => (Some(text), Some(mode)),
        None => (None, None),
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
    let mut notes = context_notes(obj, &path);
    let scripts = obj
        .entry("defaultProjectScripts")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .with_context(|| {
            format!(
                "defaultProjectScripts in {} is not an array",
                path.display()
            )
        })?;

    let first = |scripts: &[Value], kind: Kind| scripts.iter().position(|s| kind.runs(s));
    // One user script that T3 Code runs at both points: bale's settle call
    // alone, since settle registers an untracked tree.
    let shared = match (first(scripts, Kind::Setup), first(scripts, Kind::Settle)) {
        (Some(a), Some(b)) => a == b && !Kind::BOTH.iter().any(|&k| is_bales(&scripts[a], k)),
        _ => false,
    };
    let mut changed = false;
    // Whether the file already ran bale; only a first install backs it up.
    let mut any_bale = false;
    for kind in Kind::BOTH {
        if shared && kind == Kind::Setup {
            continue;
        }
        let want = hook_command(exe, kind);
        let what = format!("defaultProjectScripts {}", kind.flag());
        let Some(i) = first(scripts, kind) else {
            // A bale script someone turned off is bale's to turn back on.
            match scripts
                .iter()
                .position(|s| s.get("id").and_then(Value::as_str) == Some(kind.id()))
            {
                Some(i) => {
                    any_bale = true;
                    scripts[i] = kind.entry(want);
                }
                None => scripts.push(kind.entry(want)),
            }
            changed = true;
            continue;
        };
        let name = label(&scripts[i]);
        let ours = scripts[i].get("id").and_then(Value::as_str) == Some(kind.id());
        if ours {
            // A script after bale's with the same flag never runs.
            if let Some(later) = scripts[i + 1..].iter().find(|s| kind.runs(s)) {
                notes.push(Step::Note(format!(
                    "note: {}: T3 Code runs only the first {} script, bale's; `{}` \
                     after it never runs. Move its command into bale's script, after \
                     bale's call, or put it first and reinstall",
                    display_path(&path),
                    kind.flag(),
                    label(later)
                )));
            }
        }
        let command = scripts[i]
            .get("command")
            .and_then(Value::as_str)
            .with_context(|| {
                format!(
                    "{what} script `{name}` in {} has no command",
                    path.display()
                )
            })?
            .to_string();
        let new = match split(&command, kind) {
            Some((bale, _)) if bale == want => {
                any_bale = true;
                continue;
            }
            Some((bale, rest)) => {
                any_bale = true;
                let old = own_exe(bale, &kind.args()).expect("split found bale's own call");
                notes.push(repoint_note(&path, &format!("{what} `{name}`"), &old, exe));
                match rest {
                    Some(rest) => format!("{want}{JOIN}{rest}"),
                    None => want,
                }
            }
            None if runs_bale(&command, kind) => {
                any_bale = true;
                notes.push(Step::Note(format!(
                    "note: left unchanged in {}: {what} script `{name}` already runs bale \
                     in a form bale did not write; edit it by hand to use {}",
                    display_path(&path),
                    display_path(exe)
                )));
                continue;
            }
            None => {
                notes.push(Step::Note(format!(
                    "note: {}: T3 Code runs only the first {} script, so bale's call \
                     goes in front of `{name}`; its command still runs after bale",
                    display_path(&path),
                    kind.flag()
                )));
                if shared {
                    notes.push(Step::Note(format!(
                        "note: `{name}` runs both on worktree create and on settle; bale \
                         adds only its settle call there, which also registers a new tree"
                    )));
                }
                format!("{want}{JOIN}{command}")
            }
        };
        scripts[i]["command"] = Value::String(new);
        changed = true;
    }

    if !changed {
        notes.push(Step::Note(format!(
            "{}: T3 Code scripts already installed",
            display_path(&path)
        )));
        return Ok(Plan::new(notes));
    }
    let after = format!("{}\n", serde_json::to_string_pretty(&root)?);
    let dir = path
        .parent()
        .expect("settings path has a parent")
        .to_path_buf();
    let mut steps = vec![
        Step::Mkdir(dir),
        Step::Settings {
            path: path.clone(),
            before,
            after,
            mode,
            backup: (!any_bale).then(|| backup_path(backups, &backup_key(base, &settings_tail()))),
        },
    ];
    steps.extend(notes);
    Ok(Plan::new(steps))
}

/// Notes on where the scripts take effect: the settings file T3 Code reads
/// (set by `T3CODE_HOME`), projects that use their own scripts instead, and a
/// shell that cannot run bale's sh command.
fn context_notes(obj: &Map<String, Value>, path: &Path) -> Vec<Step> {
    let mut notes = vec![Step::Note(format!(
        "note: T3 Code's settings: {}",
        display_path(path)
    ))];
    if obj.get("projectSettingsFolded").and_then(Value::as_bool) != Some(true) {
        notes.push(Step::Note(
            "note: projectSettingsFolded is off in T3 Code: a project with scripts of its \
             own, or a script override, uses those and skips bale's"
                .to_string(),
        ));
    }
    let overridden = obj
        .get("projectSettingsOverrides")
        .and_then(Value::as_object)
        .map_or(0, |o| {
            o.values()
                .filter(|v| v.get("defaultProjectScripts").is_some())
                .count()
        });
    if overridden > 0 {
        notes.push(Step::Note(format!(
            "note: {overridden} T3 Code project(s) set their own defaultProjectScripts and \
             skip bale's"
        )));
    }
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell = shell.rsplit(['/', '\\']).next().unwrap_or_default();
    if cfg!(windows) || matches!(shell, "fish" | "pwsh" | "nu" | "elvish" | "xonsh") {
        notes.push(Step::Note(
            "note: bale's T3 Code scripts are sh commands (`if [ -x … ]; then …; fi`); \
             T3 Code types them into your shell, which must accept sh syntax"
                .to_string(),
        ));
    }
    notes
}

/// Takes bale out of `defaultProjectScripts` in T3 Code's settings under
/// `base`: a script bale added goes, and bale's call is cut from the front of
/// a user's script it was merged into, leaving their command as it was. A
/// script that runs bale in a shape install does not write is left alone.
/// An array emptied by the removal goes, and the file when bale created it
/// and nothing is left, as `json_uninstall_steps` decides.
pub fn uninstall_plan(base: &Path, backups: &Path) -> Result<Plan> {
    let path = settings_path(base);
    let none = || {
        Plan::new(vec![Step::Note(format!(
            "{}: no bale T3 Code scripts to remove",
            display_path(&path)
        ))])
    };
    let Some((before, mode)) = read_config(&path)? else {
        return Ok(none());
    };
    let mut root: Value = serde_json::from_str(&before)
        .with_context(|| format!("{} is not valid JSON; leaving it unchanged", path.display()))?;
    let Some(scripts) = root
        .get_mut("defaultProjectScripts")
        .and_then(Value::as_array_mut)
    else {
        return Ok(none());
    };
    let mut changed = false;
    let mut steps = Vec::new();
    let n = scripts.len();
    scripts.retain(|s| !Kind::BOTH.iter().any(|&k| is_bales(s, k)));
    changed |= scripts.len() != n;
    for script in scripts.iter_mut() {
        let Some(mut command) = script.get("command").and_then(Value::as_str) else {
            continue;
        };
        let original = command;
        // Both of bale's calls can lead a script (setup's before settle's
        // when T3 Code ran an older install's); cut each.
        while let Some(rest) = Kind::BOTH
            .iter()
            .find_map(|&k| split(command, k).and_then(|(_, rest)| rest))
        {
            command = rest;
        }
        let left = Kind::BOTH.iter().find(|&&k| runs_bale(command, k));
        let new = command.to_string();
        if let Some(&kind) = left {
            steps.push(Step::Note(format!(
                "note: left in {}: script `{}` runs bale (`{}`) in a form bale did not \
                 write; remove it by hand",
                display_path(&path),
                label(script),
                kind.args().join(" ")
            )));
        }
        if new != original {
            script["command"] = Value::String(new);
            changed = true;
        }
    }
    if !changed {
        if steps.is_empty() {
            return Ok(none());
        }
        return Ok(Plan::new(steps));
    }
    let emptied: Vec<Vec<&str>> = if scripts.is_empty() {
        vec![vec!["defaultProjectScripts"]]
    } else {
        Vec::new()
    };
    let key = backup_key(base, &settings_tail());
    steps.extend(json_uninstall_steps(
        backups, &key, &path, before, mode, &root, &emptied,
    )?);
    Ok(Plan::new(steps))
}

/// Fails unless `script` is a T3 Code project script with every required
/// field, as T3 Code decodes it.
#[cfg(test)]
fn check_script(script: &Value) -> Result<()> {
    use anyhow::bail;
    for key in ["id", "name", "command"] {
        match script.get(key).and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() && s.trim() == s => {}
            _ => bail!("{key} is not a trimmed non-empty string in {script}"),
        }
    }
    let icon = script.get("icon").and_then(Value::as_str);
    if !matches!(
        icon,
        Some("play" | "test" | "lint" | "configure" | "build" | "debug")
    ) {
        bail!("bad icon in {script}");
    }
    if !script
        .get("runOnWorktreeCreate")
        .is_some_and(Value::is_boolean)
    {
        bail!("runOnWorktreeCreate is not a boolean in {script}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const EXE: &str = "/opt/bale/bale";

    fn exe() -> &'static Path {
        Path::new(EXE)
    }

    fn setup_cmd() -> String {
        hook_command(exe(), Kind::Setup)
    }

    fn settle_cmd() -> String {
        hook_command(exe(), Kind::Settle)
    }

    /// A T3 Code base dir, with `settings` as its settings.json when given.
    fn base(settings: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(text) = settings {
            let path = settings_path(dir.path());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        dir
    }

    fn backups(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("backups")
    }

    fn install(dir: &tempfile::TempDir, exe: &Path) {
        install_plan(dir.path(), exe, &backups(dir))
            .unwrap()
            .apply()
            .unwrap();
    }

    fn uninstall(dir: &tempfile::TempDir) {
        uninstall_plan(dir.path(), &backups(dir))
            .unwrap()
            .apply()
            .unwrap();
    }

    fn read(dir: &tempfile::TempDir) -> String {
        fs::read_to_string(settings_path(dir.path())).unwrap()
    }

    fn scripts(dir: &tempfile::TempDir) -> Vec<Value> {
        let v: Value = serde_json::from_str(&read(dir)).unwrap();
        v["defaultProjectScripts"].as_array().unwrap().clone()
    }

    const USER_SETUP: &str = r#"{
  "projectSettingsFolded": true,
  "projectSettingsOverrides": {},
  "defaultProjectScripts": [
    {
      "id": "deps",
      "name": "Install deps",
      "command": "npm install",
      "icon": "build",
      "runOnWorktreeCreate": true,
      "async": false
    },
    {
      "id": "dev",
      "name": "Dev server",
      "command": "npm run dev",
      "icon": "play",
      "runOnWorktreeCreate": false
    }
  ],
  "zeta": [1, 2]
}
"#;

    #[test]
    fn hook_command_is_one_guarded_line() {
        let q = r"'/opt/it'\''s/bale'";
        assert_eq!(
            hook_command(Path::new("/opt/it's/bale"), Kind::Settle),
            format!("if [ -x {q} ]; then {q} hook t3 settle; fi")
        );
        assert!(!setup_cmd().contains('\n'));
    }

    /// The exact settings bale writes into a file with no scripts.
    #[test]
    fn install_into_absent_scripts_adds_both_entries() {
        let dir = base(Some(
            "{\n  \"projectSettingsFolded\": true,\n  \"b\": 1\n}\n",
        ));
        install(&dir, exe());
        let q = "'/opt/bale/bale'";
        let want = format!(
            r#"{{
  "projectSettingsFolded": true,
  "b": 1,
  "defaultProjectScripts": [
    {{
      "id": "bale-setup",
      "name": "bale setup",
      "command": "if [ -x {q} ]; then {q} hook t3 setup; fi",
      "icon": "configure",
      "runOnWorktreeCreate": true
    }},
    {{
      "id": "bale-settle",
      "name": "bale settle",
      "command": "if [ -x {q} ]; then {q} hook t3 settle; fi",
      "icon": "configure",
      "runOnWorktreeCreate": false,
      "runOnSettle": true
    }}
  ]
}}
"#
        );
        assert_eq!(read(&dir), want);
        for s in scripts(&dir) {
            check_script(&s).unwrap();
        }
    }

    #[test]
    fn install_creates_a_missing_settings_file_and_uninstall_deletes_it() {
        let dir = base(None);
        install(&dir, exe());
        assert_eq!(scripts(&dir).len(), 2);
        uninstall(&dir);
        assert!(!settings_path(dir.path()).exists());
    }

    #[test]
    fn install_into_empty_scripts_then_uninstall_restores_the_file() {
        let text = "{\n  \"projectSettingsFolded\": true,\n  \"defaultProjectScripts\": []\n}\n";
        let dir = base(Some(text));
        install(&dir, exe());
        assert_eq!(scripts(&dir).len(), 2);
        uninstall(&dir);
        assert_eq!(read(&dir), text);
    }

    #[test]
    fn install_merges_into_the_users_setup_script_and_keeps_other_keys() {
        let dir = base(Some(USER_SETUP));
        install(&dir, exe());
        let s = scripts(&dir);
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!(s[0]["id"], "deps");
        assert_eq!(
            s[0]["command"],
            Value::String(format!("{}; npm install", setup_cmd()))
        );
        assert_eq!(s[0]["async"], false);
        assert_eq!(s[1]["command"], "npm run dev");
        assert_eq!(s[2]["id"], "bale-settle");
        let v: Value = serde_json::from_str(&read(&dir)).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "projectSettingsFolded",
                "projectSettingsOverrides",
                "defaultProjectScripts",
                "zeta"
            ]
        );
        assert_eq!(v["zeta"], json!([1, 2]));
        // The original is backed up.
        let backup = backup_path(&backups(&dir), &backup_key(dir.path(), &settings_tail()));
        assert_eq!(fs::read_to_string(backup).unwrap(), USER_SETUP);
    }

    #[test]
    fn reinstall_is_a_noop() {
        let dir = base(Some(USER_SETUP));
        install(&dir, exe());
        let first = read(&dir);
        let plan = install_plan(dir.path(), exe(), &backups(&dir)).unwrap();
        plan.apply().unwrap();
        assert_eq!(read(&dir), first);
    }

    #[test]
    fn reinstall_from_another_exe_repoints_bales_calls_only() {
        let dir = base(Some(USER_SETUP));
        install(&dir, Path::new("/old/bale"));
        install(&dir, exe());
        let s = scripts(&dir);
        assert_eq!(s.len(), 3);
        assert_eq!(
            s[0]["command"],
            Value::String(format!("{}; npm install", setup_cmd()))
        );
        assert_eq!(s[2]["command"], Value::String(settle_cmd()));
        assert!(!read(&dir).contains("/old/bale"));
        // The backup is still the user's file, not the first install's.
        let backup = backup_path(&backups(&dir), &backup_key(dir.path(), &settings_tail()));
        assert_eq!(fs::read_to_string(backup).unwrap(), USER_SETUP);
    }

    #[test]
    fn uninstall_cuts_bale_from_a_merged_script_and_restores_the_file() {
        let dir = base(Some(USER_SETUP));
        install(&dir, exe());
        uninstall(&dir);
        assert_eq!(read(&dir), USER_SETUP);
    }

    /// Without the backup, uninstall still strips bale from both shapes and
    /// keeps the user's script.
    #[test]
    fn uninstall_without_a_backup_strips_both_shapes() {
        let dir = base(Some(USER_SETUP));
        install(&dir, exe());
        fs::remove_dir_all(backups(&dir)).unwrap();
        uninstall(&dir);
        let s = scripts(&dir);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[0]["command"], "npm install");
        assert_eq!(s[1]["command"], "npm run dev");
        assert!(!read(&dir).contains("bale"));
    }

    #[test]
    fn a_script_run_on_create_and_settle_gets_only_the_settle_call() {
        let text = r#"{"defaultProjectScripts":[{"id":"u","name":"both","command":"make","icon":"build","runOnWorktreeCreate":true,"runOnSettle":true}]}"#;
        let dir = base(Some(text));
        install(&dir, exe());
        let s = scripts(&dir);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(
            s[0]["command"],
            Value::String(format!("{}; make", settle_cmd()))
        );
        install(&dir, exe());
        assert_eq!(scripts(&dir), s);
        uninstall(&dir);
        assert_eq!(read(&dir), text);
    }

    #[test]
    fn a_wrapped_bale_call_is_left_alone() {
        let text = r#"{"defaultProjectScripts":[{"id":"u","name":"mine","command":"env X=1 bale hook t3 setup","icon":"build","runOnWorktreeCreate":true}]}"#;
        let dir = base(Some(text));
        let plan = install_plan(dir.path(), exe(), &backups(&dir)).unwrap();
        plan.apply().unwrap();
        let s = scripts(&dir);
        assert_eq!(s[0]["command"], "env X=1 bale hook t3 setup");
        assert_eq!(s[1]["id"], "bale-settle");
    }

    #[test]
    fn split_reads_bales_call_and_the_rest() {
        let bale = setup_cmd();
        assert_eq!(split(&bale, Kind::Setup), Some((bale.as_str(), None)));
        let merged = format!("{bale}; npm i; echo x");
        assert_eq!(
            split(&merged, Kind::Setup),
            Some((bale.as_str(), Some("npm i; echo x")))
        );
        assert_eq!(split(&merged, Kind::Settle), None);
        assert_eq!(split("npm i; fi; x", Kind::Setup), None);
        let mixed = "if [ -x '/a/bale' ]; then '/b/bale' hook t3 setup; fi; npm i";
        assert_eq!(split(mixed, Kind::Setup), None);
    }

    /// The merged script runs the user's command after bale, whether or not
    /// bale is there, and keeps the user's status.
    #[cfg(unix)]
    #[test]
    fn merged_script_runs_the_users_command_without_bale() {
        let cmd = format!(
            "{}{JOIN}echo hi; exit 7",
            hook_command(Path::new("/nonexistent/bale"), Kind::Setup)
        );
        let out = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
        assert_eq!(out.status.code(), Some(7));
    }

    /// T3 Code decodes a missing projectSettingsFolded as false, so the note
    /// that projects may skip bale's scripts shows unless it is true.
    #[test]
    fn folded_note_shows_unless_folded_is_true() {
        let folded = |text: &str| {
            let v: Value = serde_json::from_str(text).unwrap();
            context_notes(v.as_object().unwrap(), Path::new("/s.json"))
                .iter()
                .any(|n| matches!(n, Step::Note(t) if t.contains("projectSettingsFolded")))
        };
        assert!(folded("{}"));
        assert!(folded(r#"{"projectSettingsFolded": false}"#));
        assert!(!folded(r#"{"projectSettingsFolded": true}"#));
    }

    #[test]
    fn a_disabled_bale_script_is_turned_back_on() {
        let text = format!(
            r#"{{"defaultProjectScripts":[{{"id":"bale-setup","name":"bale setup","command":"{}","icon":"configure","runOnWorktreeCreate":false}}]}}"#,
            setup_cmd()
        );
        let dir = base(Some(&text));
        install(&dir, exe());
        let s = scripts(&dir);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!(s[0]["runOnWorktreeCreate"], true);
    }
}
