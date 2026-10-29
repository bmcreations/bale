//! End-to-end tests for `bale hook` and `bale install`.
//!
//! Every command runs with `HOME`, `USERPROFILE` and `BALE_HOME` pointed at a
//! temp dir, and git runs without the user's global or system config, so
//! nothing here reads or writes the real home directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const ZEROS: &str = "0000000000000000000000000000000000000000";

/// A temp repo with one commit, plus a temp HOME and BALE_HOME.
struct Env {
    tmp: TempDir,
    repo: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("home")).unwrap();
        let repo = tmp.path().join("my repo é");
        fs::create_dir_all(&repo).unwrap();
        let env = Env { tmp, repo };
        env.git(&env.repo, &["init", "-q", "-b", "main"]);
        env.git(&env.repo, &["config", "user.name", "Bale Test"]);
        env.git(&env.repo, &["config", "user.email", "bale@example.com"]);
        env.git(&env.repo, &["config", "commit.gpgsign", "false"]);
        fs::write(env.repo.join("f"), "one\n").unwrap();
        env.git(&env.repo, &["add", "f"]);
        env.git(&env.repo, &["commit", "-q", "-m", "init"]);
        env
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn bale_home(&self) -> PathBuf {
        self.tmp.path().join("bale home")
    }

    fn isolate(&self, cmd: &mut Command) {
        cmd.env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("BALE_HOME", self.bale_home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home().join(".gitconfig"))
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("CONDUCTOR_WORKSPACE_PATH")
            .env_remove("CONDUCTOR_ROOT_PATH")
            .env_remove("CONDUCTOR_WORKSPACE_NAME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("T3CODE_HOME")
            .env_remove("T3CODE_WORKTREE_PATH")
            .env_remove("T3CODE_PROJECT_ROOT");
    }

    /// `bale` with extra environment variables, set after isolation.
    fn bale_env(&self, dir: &Path, args: &[&str], vars: &[(&str, &Path)]) -> Output {
        let mut cmd = Command::cargo_bin("bale").unwrap();
        cmd.args(args).current_dir(dir);
        self.isolate(&mut cmd);
        for (k, v) in vars {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn git_out(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(dir);
        self.isolate(&mut cmd);
        cmd.output().unwrap()
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.git_out(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn bale(&self, dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
        let mut cmd = Command::cargo_bin("bale").unwrap();
        cmd.args(args).current_dir(dir);
        self.isolate(&mut cmd);
        match stdin {
            Some(input) => {
                use std::io::Write;
                use std::process::Stdio;
                let mut child = cmd
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(input.as_bytes())
                    .unwrap();
                child.wait_with_output().unwrap()
            }
            None => cmd.output().unwrap(),
        }
    }

    fn add_worktree(&self, name: &str) -> PathBuf {
        let path = self.tmp.path().join(name);
        self.git(
            &self.repo,
            &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
        );
        path
    }

    /// The paths of every registered workspace.
    fn registered(&self) -> Vec<PathBuf> {
        let out = self.bale(&self.repo, &["ls", "--json"], None);
        assert!(out.status.success(), "{}", stderr(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .map(|r| PathBuf::from(r["path"].as_str().unwrap()))
            .collect()
    }

    fn is_registered(&self, path: &Path) -> bool {
        let want = canon(path);
        self.registered().iter().any(|p| canon(p) == want)
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Canonical path without Windows' `\\?\` prefix.
fn canon(p: &Path) -> PathBuf {
    bale_core::paths::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// A documented payload with `field` replaced by `value`.
fn payload(fixture: &str, edits: &[(&str, &Path)]) -> String {
    let text = fs::read_to_string(Path::new(FIXTURES).join(fixture)).unwrap();
    let mut v: Value = serde_json::from_str(&text).unwrap();
    for (field, value) in edits {
        v[*field] = Value::String(value.to_str().unwrap().to_string());
    }
    v.to_string()
}

/// Asserts stdout is exactly one absolute path and a newline; returns it.
fn single_path(out: &Output) -> PathBuf {
    let text = stdout(out);
    let line = text.strip_suffix('\n').expect("trailing newline");
    let line = line.strip_suffix('\r').unwrap_or(line);
    assert!(!line.contains('\n'), "more than one line: {text:?}");
    if cfg!(windows) {
        let b = line.as_bytes();
        assert!(
            b.len() > 3 && b[0].is_ascii_uppercase() && &b[1..3] == b":\\",
            "{text:?}"
        );
    } else {
        assert!(line.starts_with('/') && line.len() > 1, "{text:?}");
    }
    PathBuf::from(line)
}

#[test]
fn create_hook_prints_only_path() {
    let env = Env::new();
    let input = payload("worktree_create.json", &[("cwd", &env.repo)]);
    let out = env.bale(&env.repo, &["hook", "claude", "create"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    let path = single_path(&out);
    assert!(path.is_dir(), "{}", path.display());
    assert_eq!(path.file_name().unwrap(), "feature-auth");
    assert!(env.is_registered(&path));
    let list = env.git(&env.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        list.contains("branch refs/heads/worktree-feature-auth"),
        "{list}"
    );
}

#[test]
fn create_hook_falls_back_to_git_when_bale_fails() {
    let env = Env::new();
    // Occupy the path `bale new` would use so new_workspace refuses.
    let taken = env
        .tmp
        .path()
        .join("my repo é-worktrees")
        .join("feature-auth");
    fs::create_dir_all(&taken).unwrap();
    // From inside a linked worktree, the fallback still roots at the main checkout.
    let linked = env.add_worktree("linked");
    let input = payload("worktree_create.json", &[("cwd", &linked)]);
    let out = env.bale(&linked, &["hook", "claude", "create"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    let path = single_path(&out);
    assert_eq!(
        canon(&path),
        canon(&env.repo)
            .join(".claude")
            .join("worktrees")
            .join("feature-auth")
    );
    assert!(path.join("f").is_file());
    let list = env.git(&env.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        list.contains("branch refs/heads/worktree-feature-auth"),
        "{list}"
    );
}

#[test]
fn remove_hook_exit_matches_dir_state() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    fs::write(wt.join("f"), "dirty\n").unwrap();
    let input = payload("worktree_remove.json", &[("worktree_path", &wt)]);
    let out = env.bale(&env.repo, &["hook", "claude", "remove"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fs::symlink_metadata(&wt).is_err(), "dir still exists");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));

    // Already gone: counts as removed.
    let out = env.bale(&env.repo, &["hook", "claude", "remove"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
}

#[cfg(unix)]
#[test]
fn remove_hook_busy_tree_fails_and_stays() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(&wt)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let input = payload("worktree_remove.json", &[("worktree_path", &wt)]);
    let out = env.bale(&env.repo, &["hook", "claude", "remove"], Some(&input));
    let _ = child.kill();
    let _ = child.wait();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("busy"), "{}", stderr(&out));
    assert!(wt.join("f").is_file());
}

#[cfg(unix)]
#[test]
fn cold_ignore_pid_flag() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(&wt)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let target = wt.to_str().unwrap();
    let without = env.bale(&env.repo, &["cold", target], None);
    let pid = child.id().to_string();
    let with = env.bale(&env.repo, &["cold", target, "--ignore-pid", &pid], None);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(without.status.code(), Some(3), "{}", stderr(&without));
    assert!(with.status.success(), "{}", stderr(&with));
    assert!(!wt.exists());
}

#[test]
fn post_checkout_registers_new_worktree_only() {
    let env = Env::new();
    let fresh = env.add_worktree("fresh");
    let other = env.add_worktree("other");
    let sha = env.git(&env.repo, &["rev-parse", "HEAD"]);
    let sha = sha.trim();

    let out = env.bale(
        &fresh,
        &["hook", "git", "post-checkout", ZEROS, sha, "1"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.is_registered(&fresh));

    let out = env.bale(
        &other,
        &["hook", "git", "post-checkout", sha, sha, "1"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!env.is_registered(&other));

    // The main checkout (as after `git clone`) is never registered, and never fails.
    let out = env.bale(
        &env.repo,
        &["hook", "git", "post-checkout", ZEROS, sha, "1"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!env.is_registered(&env.repo));
}

#[test]
fn adopt_registers_a_linked_worktree() {
    let env = Env::new();
    let wt = env.add_worktree("adopted");
    let out = env.bale(&env.repo, &["adopt", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.trim_end().ends_with(": hot"), "{text:?}");
    assert!(env.is_registered(&wt));
}

#[test]
fn adopt_refuses_main_checkout() {
    let env = Env::new();
    let out = env.bale(&env.repo, &["adopt", env.repo.to_str().unwrap()], None);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("main checkout"), "{}", stderr(&out));
    assert!(env.registered().is_empty());
}

#[test]
fn adopt_defaults_to_cwd() {
    let env = Env::new();
    let wt = env.add_worktree("cwd-adopted");
    let out = env.bale(&wt, &["adopt"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.is_registered(&wt));
}

#[test]
fn new_print_path_prints_only_the_path() {
    let env = Env::new();
    let out = env.bale(
        &env.repo,
        &["new", "printed", "--print-path", "--branch", "printed-b"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let path = single_path(&out);
    assert!(path.is_dir(), "{path:?}");
    assert!(env.is_registered(&path));
    assert_eq!(path, canon(&path), "printed path is canonical");
    assert!(path.ends_with("printed"), "{path:?}");
}

/// Writes an executable hook script.
fn write_hook(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn hooks_dir(env: &Env) -> PathBuf {
    let out = env.git(
        &env.repo,
        &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
    );
    PathBuf::from(out.trim_end())
}

fn sh_path(p: &Path) -> String {
    p.to_str().unwrap().replace('\\', "/")
}

#[test]
fn git_hook_install_chains_existing() {
    let env = Env::new();
    let marker = env.tmp.path().join("marker");
    let hook = hooks_dir(&env).join("post-checkout");
    write_hook(&hook, &format!("echo ran > '{}'", sh_path(&marker)));
    let repo = env.repo.to_str().unwrap();

    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "git", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(hooks_dir(&env).join("post-checkout.pre-bale").is_file());

    let wt = env.add_worktree("chained");
    assert!(marker.is_file(), "previous hook did not run");
    assert!(env.is_registered(&wt));
}

#[test]
fn git_hook_install_twice_does_not_double_chain() {
    let env = Env::new();
    let original = "echo previous";
    write_hook(&hooks_dir(&env).join("post-checkout"), original);
    let repo = env.repo.to_str().unwrap();
    for _ in 0..2 {
        let out = env.bale(
            &env.repo,
            &["install", "--hooks", "git", "--repo", repo],
            None,
        );
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let dir = hooks_dir(&env);
    let prev = fs::read_to_string(dir.join("post-checkout.pre-bale")).unwrap();
    assert!(prev.contains(original), "{prev}");
    assert!(!prev.contains("bale"), "bale chained to itself: {prev}");
    let hook = fs::read_to_string(dir.join("post-checkout")).unwrap();
    assert_eq!(hook.matches("hook git post-checkout").count(), 1, "{hook}");
}

#[test]
fn git_hook_previous_failure_propagates() {
    let env = Env::new();
    write_hook(&hooks_dir(&env).join("post-checkout"), "exit 7");
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "git", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    // git reports any failing post-checkout as exit 1, not the hook's code.
    let out = env.git_out(&env.repo, &["checkout", "-q", "-b", "other"]);
    assert!(!out.status.success(), "{}", stderr(&out));

    // The hook itself exits with the previous hook's status.
    #[cfg(unix)]
    {
        let hook = hooks_dir(&env).join("post-checkout");
        let mut cmd = Command::new(&hook);
        cmd.args(["a", "b", "1"]).current_dir(&env.repo);
        env.isolate(&mut cmd);
        assert_eq!(cmd.status().unwrap().code(), Some(7));
    }
}

#[test]
fn git_hook_goes_in_hooks_path_outside_tree() {
    let env = Env::new();
    let shared = env.tmp.path().join("shared hooks");
    write_hook(&shared.join("post-checkout"), "echo previous");
    env.git(
        &env.repo,
        &["config", "core.hooksPath", shared.to_str().unwrap()],
    );
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "git", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(shared.join("post-checkout.pre-bale").is_file());
    let hook = fs::read_to_string(shared.join("post-checkout")).unwrap();
    assert!(hook.contains("hook git post-checkout"), "{hook}");
}

#[test]
fn git_hook_refuses_hooks_path_inside_tree() {
    let env = Env::new();
    let husky = env.repo.join(".husky");
    write_hook(&husky.join("post-checkout"), "echo tracked");
    env.git(&env.repo, &["config", "core.hooksPath", ".husky"]);
    let before = fs::read_to_string(husky.join("post-checkout")).unwrap();
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "git", "--repo", repo],
        None,
    );
    assert!(!out.status.success());
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("hook git post-checkout"), "{all}");
    assert_eq!(
        fs::read_to_string(husky.join("post-checkout")).unwrap(),
        before
    );
    assert!(!husky.join("post-checkout.pre-bale").exists());
}

const SETTINGS: &str = r#"{
  "model": "opus",
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo hello"
          }
        ]
      }
    ]
  },
  "alwaysThinkingEnabled": true
}
"#;

fn settings_path(env: &Env) -> PathBuf {
    env.home().join(".claude").join("settings.json")
}

#[test]
fn claude_hooks_install_is_idempotent_and_keeps_existing() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, SETTINGS).unwrap();

    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("+"),
        "no diff printed: {}",
        stdout(&out)
    );
    let first = fs::read_to_string(&path).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&path).unwrap(), first);

    assert!(!path.with_file_name("settings.json.bak-bale").exists());
    let backups = backups(&env);
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), SETTINGS);

    let v: Value = serde_json::from_str(&first).unwrap();
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["model", "hooks", "alwaysThinkingEnabled"]);
    assert_eq!(
        v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo hello"
    );
    for (event, kind) in [
        ("WorktreeCreate", "claude create"),
        ("WorktreeRemove", "claude remove"),
    ] {
        let groups = v["hooks"][event].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{event}: {groups:?}");
        let h = &groups[0]["hooks"][0];
        assert_eq!(h["type"], "command");
        let cmd = h["command"].as_str().unwrap();
        assert!(cmd.ends_with(&format!(" hook {kind}")), "{cmd}");
    }
}

#[test]
fn claude_hooks_install_refuses_invalid_json() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "{ not json").unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    assert!(!path.with_file_name("settings.json.bak-bale").exists());
}

/// Every path under `dir`, sorted.
fn listing(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            out.push(p.clone());
            if p.is_dir() {
                walk(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

#[test]
fn print_only_shows_schedule_and_changes_nothing() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, SETTINGS).unwrap();
    let before = listing(&env.home());
    let hooks_before = listing(&hooks_dir(&env));
    let repo = env.repo.to_str().unwrap();

    let out = env.bale(&env.repo, &["install", "--print-only"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    // The exe path is used as run, not canonicalized.
    let exe = env!("CARGO_BIN_EXE_bale");
    assert!(text.contains(exe), "{text}");
    // A cargo build dir is not a stable install location.
    assert!(stderr(&out).contains("warning"), "{}", stderr(&out));
    if !cfg!(windows) {
        assert!(text.contains("BALE_HOME"), "{text}");
        assert!(text.contains(env.bale_home().to_str().unwrap()), "{text}");
        assert!(text.contains("PATH"), "{text}");
        assert!(text.contains("captured"), "{text}");
    }
    if cfg!(target_os = "macos") {
        assert!(text.contains("<key>EnvironmentVariables</key>"), "{text}");
    } else if !cfg!(windows) {
        assert!(text.contains("Environment="), "{text}");
    }
    if cfg!(target_os = "macos") {
        assert!(text.contains("dev.bale.sweep.plist"), "{text}");
        assert!(text.contains("<integer>300</integer>"), "{text}");
        assert!(text.contains("launchctl bootstrap gui/"), "{text}");
    } else if cfg!(windows) {
        assert!(
            text.contains("schtasks /Create /SC MINUTE /MO 30 /TN bale-sweep"),
            "{text}"
        );
    } else {
        assert!(text.contains("OnUnitActiveSec=30min"), "{text}");
        assert!(text.contains("OnBootSec=5min"), "{text}");
        assert!(
            text.contains("systemctl --user enable --now bale-sweep.timer"),
            "{text}"
        );
    }

    let out = env.bale(
        &env.repo,
        &[
            "install",
            "--print-only",
            "--hooks",
            "claude,git",
            "--repo",
            repo,
        ],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("WorktreeCreate"), "{text}");
    assert!(text.contains("post-checkout"), "{text}");

    assert_eq!(listing(&env.home()), before);
    assert_eq!(fs::read_to_string(&path).unwrap(), SETTINGS);
    assert_eq!(listing(&hooks_dir(&env)), hooks_before);
}

#[test]
fn create_hook_reuses_worktree_bale_already_made() {
    let env = Env::new();
    // Hold the workspace lock so `register` fails after `git worktree add`.
    let target = env
        .tmp
        .path()
        .join("my repo é-worktrees")
        .join("feature-auth");
    let home = bale_core::BaleHome(env.bale_home());
    let id = bale_core::registry::workspace_id(&target);
    let _lock = bale_core::lock::WorkspaceLock::try_acquire(&home, &id)
        .unwrap()
        .expect("lock");
    let input = payload("worktree_create.json", &[("cwd", &env.repo)]);
    let out = env.bale(&env.repo, &["hook", "claude", "create"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    let path = single_path(&out);
    assert_eq!(canon(&path), canon(&target));
    let list = env.git(&env.repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 2, "{list}");
    assert!(!env.repo.join(".claude").exists());
}

/// Runs the remove hook from `sh -c 'cd <wt> && <cmd>'`, so its parent
/// shell has its cwd in the worktree, as when Claude Code runs it there.
#[cfg(unix)]
fn remove_via_shell(env: &Env, wt: &Path, extra: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;
    let input = payload("worktree_remove.json", &[("worktree_path", wt)]);
    let script = format!(
        "cd \"$WT\" || exit 9\n{extra}\n\"$BALE\" hook claude remove\nstatus=$?\nexit $status\n"
    );
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .env("WT", wt)
        .env("BALE", env!("CARGO_BIN_EXE_bale"))
        .current_dir(&env.repo);
    env.isolate(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
#[test]
fn remove_hook_ignores_its_own_parent_shell() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let out = remove_via_shell(&env, &wt, "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fs::symlink_metadata(&wt).is_err(), "dir still exists");
}

#[cfg(unix)]
#[test]
fn remove_hook_still_sees_non_ancestor_holders() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    // A background child of the same shell, like a dev server Claude Code started.
    let out = remove_via_shell(
        &env,
        &wt,
        "sleep 30 &\nBG=$!\ntrap 'kill $BG' EXIT\nsleep 0.3",
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("busy") && err.contains("sleep"), "{err}");
    assert!(wt.join("f").is_file());
}

#[test]
fn claude_hooks_install_creates_missing_settings_without_backup() {
    let env = Env::new();
    let path = settings_path(&env);
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(v["hooks"]["WorktreeCreate"].is_array(), "{v}");
    assert!(!path.with_file_name("settings.json.bak-bale").exists());
    assert!(backups(&env).is_empty());
}

#[test]
fn claude_hooks_install_refuses_other_worktree_hook() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let other = r#"{"hooks":{"WorktreeCreate":[{"hooks":[{"type":"command","command":"my-tool create"}]}]}}"#;
    fs::write(&path, other).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), other);
    assert!(!path.with_file_name("settings.json.bak-bale").exists());
    assert!(backups(&env).is_empty());
}

#[test]
fn claude_hooks_install_refuses_non_object_and_trailing_data() {
    for text in [
        "[]",
        "{} {}",
        "{\"a\": 1}\n// trailing comment\n",
        "{ /* c */ \"a\": 1 }",
    ] {
        let env = Env::new();
        let path = settings_path(&env);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
        assert!(!out.status.success(), "accepted {text:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
}

#[test]
fn claude_hooks_install_rewrites_stale_bale_entries() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let stale = r#"{"hooks":{
      "WorktreeCreate":[{"hooks":[{"type":"command","command":"'/old/Cellar/bale/0.1/bin/bale' hook claude-worktree-create"}]}],
      "WorktreeRemove":[{"hooks":[{"type":"command","command":"'/old/Cellar/bale/0.1/bin/bale' hook claude-worktree-remove"}]}]}}"#;
    fs::write(&path, stale).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for event in ["WorktreeCreate", "WorktreeRemove"] {
        let groups = v["hooks"][event].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{v}");
        let cmd = groups[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(!cmd.contains("/old/"), "{cmd}");
        assert!(cmd.contains("bale"), "{cmd}");
        assert!(!cmd.contains("claude-worktree"), "{cmd}");
    }
}

#[cfg(unix)]
#[test]
fn claude_hooks_install_refuses_symlinked_settings() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let real = env.tmp.path().join("dotfiles-settings.json");
    fs::write(&real, SETTINGS).unwrap();
    std::os::unix::fs::symlink(&real, &path).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("symlink"), "{err}");
    // The refusal shows the entries to add by hand.
    assert!(
        err.contains("WorktreeCreate") && err.contains("WorktreeRemove"),
        "{err}"
    );
    assert!(err.contains("hook claude remove"), "{err}");
    assert!(err.contains(env!("CARGO_BIN_EXE_bale")), "{err}");
    assert!(fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(&real).unwrap(), SETTINGS);
}

#[cfg(unix)]
#[test]
fn claude_hooks_install_keeps_file_mode() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, SETTINGS).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("reformatted"), "{}", stdout(&out));
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);
}

#[test]
fn git_hook_install_rewrites_stale_bale_path() {
    let env = Env::new();
    let hook = hooks_dir(&env).join("post-checkout");
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "git", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let current = fs::read_to_string(&hook).unwrap();
    let exe = env!("CARGO_BIN_EXE_bale").replace('\\', "/");
    assert!(current.contains(&exe), "{current}");
    fs::write(&hook, current.replace(&exe, "/old/bin/bale")).unwrap();

    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "git", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let rewritten = fs::read_to_string(&hook).unwrap();
    assert!(rewritten.contains(&exe) && !rewritten.contains("/old/bin/bale"));
    assert!(!hooks_dir(&env).join("post-checkout.pre-bale").exists());
}

/// A bale hook the user edited stays as it is, with a note naming it.
#[test]
fn git_hook_install_keeps_an_edited_bale_hook() {
    let env = Env::new();
    let hook = hooks_dir(&env).join("post-checkout");
    install(&env, "git", true);
    let edited = fs::read_to_string(&hook).unwrap() + "echo my-edit\n";
    fs::write(&hook, &edited).unwrap();

    let out = install(&env, "git", true);
    assert_eq!(fs::read_to_string(&hook).unwrap(), edited);
    assert!(
        stdout(&out).contains("left unchanged")
            && stdout(&out).contains(&hook.display().to_string()),
        "{}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("wrote"), "{}", stdout(&out));
}

#[test]
fn git_hook_notes_global_hooks_path() {
    let env = Env::new();
    let shared = env.tmp.path().join("user hooks");
    fs::create_dir_all(&shared).unwrap();
    fs::write(
        env.home().join(".gitconfig"),
        format!("[core]\n\thooksPath = {}\n", sh_path(&shared)),
    )
    .unwrap();
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "git", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("global git config"), "{all}");
    assert!(shared.join("post-checkout").is_file());
}

#[cfg(unix)]
#[test]
fn remove_hook_blocks_on_holder_above_non_shell_parent() {
    // holder sh (cwd in wt) -> perl, like Claude Code as `node` -> `sh -c`
    // with one command, which execs bale. The holder is the user's shell
    // in this layout and must still block the remove.
    use std::io::Write;
    use std::process::Stdio;
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let input = payload("worktree_remove.json", &[("worktree_path", &wt)]);
    let script = r#"cd "$WT" || exit 9
perl -e 'system(@ARGV); exit($? >> 8)' sh -c "'$BALE' hook claude remove"
exit $?
"#;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .env("WT", &wt)
        .env("BALE", env!("CARGO_BIN_EXE_bale"))
        .current_dir(&env.repo);
    env.isolate(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("busy"), "{err}");
    assert!(wt.join("f").is_file());
}

#[test]
fn create_hook_does_not_adopt_a_preexisting_worktree() {
    let env = Env::new();
    // The user's own worktree already sits where `bale new` would go.
    let theirs = env
        .tmp
        .path()
        .join("my repo é-worktrees")
        .join("feature-auth");
    env.git(
        &env.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "user-work",
            theirs.to_str().unwrap(),
        ],
    );
    fs::write(theirs.join("f"), "user edits\n").unwrap();
    let input = payload("worktree_create.json", &[("cwd", &env.repo)]);
    let out = env.bale(&env.repo, &["hook", "claude", "create"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    let path = single_path(&out);
    assert_eq!(
        canon(&path),
        canon(&env.repo)
            .join(".claude")
            .join("worktrees")
            .join("feature-auth")
    );
    assert!(!env.is_registered(&theirs));
    assert_eq!(
        fs::read_to_string(theirs.join("f")).unwrap(),
        "user edits\n"
    );
    let branch = env.git(&theirs, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(branch.trim(), "user-work");
    let list = env.git(&env.repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 3, "{list}");
}

#[test]
fn create_hook_fallback_roots_at_main_checkout_with_separate_git_dir() {
    let env = Env::new();
    let main = env.tmp.path().join("sep");
    let gitdir = env.tmp.path().join("store").join("sep.git");
    fs::create_dir_all(gitdir.parent().unwrap()).unwrap();
    env.git(
        env.tmp.path(),
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            gitdir.to_str().unwrap(),
            main.to_str().unwrap(),
        ],
    );
    env.git(&main, &["config", "user.name", "Bale Test"]);
    env.git(&main, &["config", "user.email", "bale@example.com"]);
    env.git(&main, &["config", "commit.gpgsign", "false"]);
    fs::write(main.join("f"), "one\n").unwrap();
    env.git(&main, &["add", "f"]);
    env.git(&main, &["commit", "-q", "-m", "init"]);
    // Occupy bale's target so the fallback runs: bale-core roots it at the
    // git dir today (`git worktree list` names it as the main entry), and
    // at the working tree once that is fixed.
    for root in ["store/sep.git-worktrees", "sep-worktrees"] {
        fs::create_dir_all(env.tmp.path().join(root).join("feature-auth")).unwrap();
    }
    let input = payload("worktree_create.json", &[("cwd", &main)]);
    let out = env.bale(&main, &["hook", "claude", "create"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    let path = single_path(&out);
    assert_eq!(
        canon(&path),
        canon(&main)
            .join(".claude")
            .join("worktrees")
            .join("feature-auth")
    );
}

#[test]
fn claude_hooks_install_leaves_wrapped_bale_entries() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let wrapped = r#"{"hooks":{
      "WorktreeCreate":[{"hooks":[{"type":"command","command":"env BALE_HOME=/x '/old/bale' hook claude create"}]}],
      "WorktreeRemove":[{"hooks":[{"type":"command","command":"nice '/old/bale' hook claude remove"}]}]}}"#;
    fs::write(&path, wrapped).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    // The wrapped entries stay as they were; only the new events are added.
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let old: Value = serde_json::from_str(wrapped).unwrap();
    assert_eq!(v["hooks"]["WorktreeCreate"], old["hooks"]["WorktreeCreate"]);
    assert_eq!(v["hooks"]["WorktreeRemove"], old["hooks"]["WorktreeRemove"]);
    for event in ["UserPromptSubmit", "PreToolUse", "Stop"] {
        assert_eq!(v["hooks"][event].as_array().unwrap().len(), 1, "{event}");
    }
    let text = stdout(&out);
    assert!(text.contains("left unchanged"), "{text}");
    assert!(text.contains("nice '/old/bale'"), "{text}");
}

#[test]
fn claude_hooks_install_rewrites_wrapped_old_form_entries() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let wrapped = r#"{"hooks":{
      "WorktreeCreate":[{"hooks":[{"type":"command","command":"env BALE_HOME=/x '/old/bale' hook claude-worktree-create"}]}],
      "WorktreeRemove":[{"hooks":[{"type":"command","command":"nice '/old/bale' hook claude-worktree-remove"}]}]}}"#;
    fs::write(&path, wrapped).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("stale"), "{text}");
    assert!(!text.contains("already installed"), "{text}");
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        v["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"],
        "env BALE_HOME=/x '/old/bale' hook claude create"
    );
    assert_eq!(
        v["hooks"]["WorktreeRemove"][0]["hooks"][0]["command"],
        "nice '/old/bale' hook claude remove"
    );
}

#[test]
fn claude_hooks_install_replaces_old_form_entries() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let exe = env!("CARGO_BIN_EXE_bale").replace('\\', "/");
    let old = serde_json::json!({"hooks": {
        "WorktreeCreate": [{"hooks": [{"type": "command",
            "command": format!("'{exe}' hook claude-worktree-create")}]}],
        "WorktreeRemove": [{"hooks": [{"type": "command",
            "command": format!("'{exe}' hook claude-worktree-remove")}]}]}});
    fs::write(&path, serde_json::to_string(&old).unwrap()).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for (event, kind) in [("WorktreeCreate", "create"), ("WorktreeRemove", "remove")] {
        let groups = v["hooks"][event].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{v}");
        let hooks = groups[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1, "{v}");
        let cmd = hooks[0]["command"].as_str().unwrap();
        assert_eq!(cmd, format!("'{exe}' hook claude {kind}"));
    }
}

const USER_HOOKS: &str = r#"{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "my-guard check"
          }
        ]
      }
    ],
    "WorktreeRemove": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "my-tool cleanup"
          }
        ]
      }
    ]
  }
}
"#;

#[test]
fn install_hooks_claude_keeps_user_hooks() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, USER_HOOKS).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let first = fs::read_to_string(&path).unwrap();
    let v: Value = serde_json::from_str(&first).unwrap();
    let user: Value = serde_json::from_str(USER_HOOKS).unwrap();
    // bale's entry (no matcher) is listed first; the user's stays as it was.
    let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2, "{v}");
    assert!(pre[0].get("matcher").is_none(), "{v}");
    assert!(pre[0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with(" hook claude pre-tool"));
    assert_eq!(pre[1], user["hooks"]["PreToolUse"][0]);
    // Claude Code runs matching hooks in parallel, so install says the
    // user's remove hook races bale's.
    assert!(
        stdout(&out).contains("in parallel") && stdout(&out).contains("exit 3"),
        "{}",
        stdout(&out)
    );
    // bale's entry is listed first; that sets no order.
    let remove = v["hooks"]["WorktreeRemove"].as_array().unwrap();
    assert_eq!(remove.len(), 2, "{v}");
    let bale = remove[0]["hooks"][0]["command"].as_str().unwrap();
    assert!(bale.ends_with(" hook claude remove"), "{bale}");
    assert_eq!(remove[1], user["hooks"]["WorktreeRemove"][0]);
    let create = v["hooks"]["WorktreeCreate"].as_array().unwrap();
    assert_eq!(create.len(), 1, "{v}");

    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stdout(&out).contains("\n+ "), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("already installed"),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), first);
}

#[test]
fn install_conductor_without_repo_errors() {
    let env = Env::new();
    let before = listing(&env.home());
    for tool in ["conductor", "cursor", "git"] {
        let out = env.bale(&env.repo, &["install", "--hooks", tool], None);
        assert!(!out.status.success(), "{tool}");
        let err = stderr(&out);
        assert!(
            err.contains(&format!("--repo is required for {tool}")),
            "{tool}: {err}"
        );
    }
    assert_eq!(listing(&env.home()), before);
}

#[test]
fn install_rejects_unknown_hook_name() {
    let env = Env::new();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude,vim"], None);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("vim"), "{}", stderr(&out));
    assert!(!settings_path(&env).exists());
}

#[test]
fn up_names_a_build_dir_it_kept() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    fs::create_dir_all(wt.join("build")).unwrap();
    fs::write(wt.join("build/notes.md"), "notes\n").unwrap();
    let out = env.bale(&env.repo, &["up", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains(": baled"), "{text}");
    assert!(text.contains("kept ") && text.contains("build"), "{text}");
    assert!(wt.join("build/notes.md").exists());
}

/// `status.showUntrackedFiles=no` in the user's global config must not hide
/// an untracked file inside a submodule from `cold`.
#[test]
fn cold_keeps_a_submodule_file_hidden_by_global_config() {
    let env = Env::new();
    let sub = env.tmp.path().join("sub");
    fs::create_dir_all(&sub).unwrap();
    env.git(&sub, &["init", "-q", "-b", "main"]);
    env.git(&sub, &["config", "user.name", "Bale Test"]);
    env.git(&sub, &["config", "user.email", "bale@example.com"]);
    fs::write(sub.join("a.txt"), "a\n").unwrap();
    env.git(&sub, &["add", "a.txt"]);
    env.git(
        &sub,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "s"],
    );
    let wt = env.add_worktree("feat");
    env.git(
        &wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub.to_str().unwrap(),
            "lib",
        ],
    );
    env.git(&wt, &["commit", "-q", "-m", "sub"]);
    fs::write(
        env.home().join(".gitconfig"),
        "[status]\n\tshowUntrackedFiles = no\n",
    )
    .unwrap();
    fs::write(wt.join("lib/pre.txt"), "pre\n").unwrap();

    let up = env.bale(&env.repo, &["up", wt.to_str().unwrap()], None);
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );
    let out = env.bale(&env.repo, &["cold", wt.to_str().unwrap()], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dirty submodule: lib"), "{stdout}");
    assert_eq!(fs::read_to_string(wt.join("lib/pre.txt")).unwrap(), "pre\n");
}

/// A filter that only applies on the worktree's branch (`includeIf
/// "onbranch:..."` in the global config) must apply when open checks the
/// tree out, as it did when cold checked the restore.
#[cfg(unix)]
#[test]
fn open_checks_out_under_the_branchs_config() {
    let env = Env::new();
    fs::write(
        env.home().join("rev.inc"),
        "[filter \"rev\"]\n\tclean = rev\n\tsmudge = rev\n",
    )
    .unwrap();
    fs::write(
        env.home().join(".gitconfig"),
        format!(
            "[includeIf \"onbranch:feat\"]\n\tpath = {}\n",
            env.home().join("rev.inc").display()
        ),
    )
    .unwrap();
    fs::write(env.repo.join(".gitattributes"), "*.txt filter=rev\n").unwrap();
    env.git(&env.repo, &["add", ".gitattributes"]);
    env.git(&env.repo, &["commit", "-q", "-m", "attrs"]);
    let wt = env.add_worktree("feat");
    fs::write(wt.join("notes.txt"), "hello world\n").unwrap();
    let wt_arg = wt.to_str().unwrap();

    for args in [["up", wt_arg], ["cold", wt_arg]] {
        let out = env.bale(&env.repo, &args, None);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    }
    assert!(!wt.exists(), "cold did not go through");
    let out = env.bale(&env.repo, &["open", wt_arg], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(wt.join("notes.txt")).unwrap(),
        "hello world\n"
    );
}

/// A filter added to config between cold and open changes the bytes the
/// checkout writes, and git's own diff (cleaning them back) sees no change.
/// open must compare raw bytes with what cold saw and stop, keeping the
/// saved state.
#[cfg(unix)]
#[test]
fn open_refuses_bytes_changed_by_config_added_after_cold() {
    let env = Env::new();
    fs::write(env.repo.join(".gitattributes"), "*.txt filter=rev\n").unwrap();
    env.git(&env.repo, &["add", ".gitattributes"]);
    env.git(&env.repo, &["commit", "-q", "-m", "attrs"]);
    let wt = env.add_worktree("feat");
    fs::write(wt.join("notes.txt"), "hello world\n").unwrap();
    let wt_arg = wt.to_str().unwrap();
    for args in [["up", wt_arg], ["cold", wt_arg]] {
        let out = env.bale(&env.repo, &args, None);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    }
    assert!(!wt.exists(), "cold did not go through");

    fs::write(
        env.home().join(".gitconfig"),
        "[filter \"rev\"]\n\tclean = rev\n\tsmudge = rev\n",
    )
    .unwrap();
    let out = env.bale(&env.repo, &["open", wt_arg], None);
    assert!(!out.status.success(), "open went through");
    assert!(stderr(&out).contains("notes.txt"), "{}", stderr(&out));

    let out = env.bale(&env.repo, &["ls", "--json"], None);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rec = v
        .as_array()
        .unwrap()
        .iter()
        .find(|r| canon(Path::new(r["path"].as_str().unwrap())) == canon(&wt))
        .unwrap()
        .clone();
    assert_eq!(rec["level"], "opening");
    let id = rec["id"].as_str().unwrap();
    env.git(
        &env.repo,
        &["rev-parse", "--verify", &format!("refs/bale/{id}/wip")],
    );
    assert!(env.bale_home().join(id).join("manifest.json").exists());
}

/// `core.symlinks=false` set between cold and open makes checkout write a
/// tracked symlink as a regular file holding its target, which git's own
/// diff accepts. open must check the link against what cold saw and stop,
/// keeping the saved state.
#[cfg(unix)]
#[test]
fn open_refuses_symlink_written_as_a_file() {
    let env = Env::new();
    let wt = env.add_worktree("feat");
    fs::write(wt.join("r"), "target\n").unwrap();
    std::os::unix::fs::symlink("r", wt.join("link")).unwrap();
    env.git(&wt, &["add", "r", "link"]);
    env.git(&wt, &["commit", "-q", "-m", "link"]);
    let wt_arg = wt.to_str().unwrap();
    for args in [["up", wt_arg], ["cold", wt_arg]] {
        let out = env.bale(&env.repo, &args, None);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    }
    assert!(!wt.exists(), "cold did not go through");

    env.git(&env.repo, &["config", "core.symlinks", "false"]);
    let out = env.bale(&env.repo, &["open", wt_arg], None);
    assert!(!out.status.success(), "open went through");
    assert!(stderr(&out).contains("link"), "{}", stderr(&out));

    let out = env.bale(&env.repo, &["ls", "--json"], None);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rec = v
        .as_array()
        .unwrap()
        .iter()
        .find(|r| canon(Path::new(r["path"].as_str().unwrap())) == canon(&wt))
        .unwrap()
        .clone();
    assert_eq!(rec["level"], "opening");
    let id = rec["id"].as_str().unwrap();
    env.git(
        &env.repo,
        &["rev-parse", "--verify", &format!("refs/bale/{id}/wip")],
    );
    assert!(env.bale_home().join(id).join("manifest.json").exists());
}

const CONDUCTOR_WS: &str = "CONDUCTOR_WORKSPACE_PATH";

#[test]
fn conductor_setup_adopts_from_env() {
    let env = Env::new();
    let wt = env.add_worktree("conductor-ws");
    let out = env.bale_env(
        &env.repo,
        &["hook", "conductor", "setup"],
        &[(CONDUCTOR_WS, &wt)],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.is_registered(&wt));
}

#[test]
fn conductor_setup_outside_worktree_exits_0() {
    let env = Env::new();
    let outside = env.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let out = env.bale(&outside, &["hook", "conductor", "setup"], None);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("bale"), "{}", stderr(&out));
    assert!(env.registered().is_empty());
}

const T3_WT: &str = "T3CODE_WORKTREE_PATH";

/// The `bale ls --json` row for the tree at `path`.
fn ls_json_row(env: &Env, path: &Path) -> Value {
    let out = env.bale(&env.repo, &["ls", "--json"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let rows: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    let want = canon(path);
    rows.into_iter()
        .find(|r| canon(Path::new(r["path"].as_str().unwrap())) == want)
        .unwrap_or_else(|| panic!("{} is not registered", path.display()))
}

/// The trace's `touch` op lines.
fn touch_lines(env: &Env) -> Vec<Value> {
    trace_lines(env)
        .into_iter()
        .filter(|l| l["kind"] == "op" && l["op"] == "touch")
        .collect()
}

#[test]
fn t3_setup_registers_from_env_quietly() {
    let env = Env::new();
    let wt = env.add_worktree("t3-env");
    let out = env.bale_env(&env.repo, &["hook", "t3", "setup"], &[(T3_WT, &wt)]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert_eq!(ls_json_row(&env, &wt)["origin"]["tool"], "t3");
}

#[test]
fn t3_setup_registers_the_cwd() {
    let env = Env::new();
    let wt = env.add_worktree("t3-cwd");
    let out = env.bale(&wt, &["hook", "t3", "setup"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(ls_json_row(&env, &wt)["origin"]["tool"], "t3");
}

#[test]
fn t3_setup_outside_worktree_exits_0() {
    let env = Env::new();
    let outside = env.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let out = env.bale(&outside, &["hook", "t3", "setup"], None);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("bale"), "{}", stderr(&out));
    assert!(env.registered().is_empty());
}

/// settle moves last_activity and traces a touch, leaving the tree hot with
/// its build output.
#[test]
fn t3_settle_touches_a_tracked_tree_and_keeps_it_hot() {
    let env = Env::new();
    let wt = env.add_worktree("t3-settle");
    fs::create_dir_all(wt.join("build")).unwrap();
    fs::write(wt.join("build/out.o"), "x").unwrap();
    let out = env.bale(&wt, &["hook", "t3", "setup"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let before = ls_json_row(&env, &wt);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out = env.bale_env(&env.repo, &["hook", "t3", "settle"], &[(T3_WT, &wt)]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!((stdout(&out).as_str(), stderr(&out).as_str()), ("", ""));
    let after = ls_json_row(&env, &wt);
    assert_ne!(after["last_activity"], before["last_activity"]);
    assert_eq!(after["level"], before["level"]);
    assert!(wt.join("build/out.o").exists());
    let touches = touch_lines(&env);
    assert_eq!(touches.len(), 1, "{touches:?}");
    assert_eq!(touches[0]["trigger"], "hook:t3");
    assert_eq!(touches[0]["outcome"], "ok");
    assert_eq!(touches[0]["id"], before["id"]);
}

/// A tree T3 Code made before bale's setup script ran is registered on its
/// first settle.
#[test]
fn t3_settle_registers_an_untracked_tree() {
    let env = Env::new();
    let wt = env.add_worktree("t3-late");
    let out = env.bale(&wt, &["hook", "t3", "settle"], None);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert_eq!(ls_json_row(&env, &wt)["origin"]["tool"], "t3");
    assert_eq!(touch_lines(&env).len(), 1);
}

#[test]
fn t3_settle_outside_worktree_exits_0() {
    let env = Env::new();
    let outside = env.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let out = env.bale(&outside, &["hook", "t3", "settle"], None);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(env.registered().is_empty());
    assert!(touch_lines(&env).is_empty());
}

/// install and uninstall edit `~/.t3/userdata/settings.json`, or the one
/// under T3CODE_HOME, keeping the user's other settings; --print-only
/// writes nothing.
#[test]
fn t3_install_and_uninstall_round_trip() {
    let env = Env::new();
    let original = "{\n  \"projectSettingsFolded\": true,\n  \"theme\": \"dark\"\n}\n";
    for t3_home in [None, Some(env.tmp.path().join("t3 home"))] {
        let base = t3_home.clone().unwrap_or_else(|| env.home().join(".t3"));
        let path = base.join("userdata").join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, original).unwrap();
        let vars: Vec<(&str, &Path)> = t3_home
            .as_deref()
            .map(|h| vec![("T3CODE_HOME", h)])
            .unwrap_or_default();
        let run = |args: &[&str]| {
            let out = env.bale_env(&env.repo, args, &vars);
            assert!(out.status.success(), "{args:?}: {}", stderr(&out));
            out
        };
        let out = run(&["install", "--hooks", "t3", "--print-only"]);
        assert!(stdout(&out).contains("hook t3 settle"), "{}", stdout(&out));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        run(&["install", "--hooks", "t3"]);
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        let ids: Vec<&str> = v["defaultProjectScripts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["bale-setup", "bale-settle"]);
        let out = run(&["install", "--hooks", "t3"]);
        assert!(
            stdout(&out).contains("already installed"),
            "{}",
            stdout(&out)
        );
        run(&["uninstall", "--hooks", "t3"]);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
}

#[test]
fn conductor_archive_colds_and_open_restores() {
    let env = Env::new();
    fs::write(env.repo.join(".gitignore"), ".env\n").unwrap();
    env.git(&env.repo, &["add", ".gitignore"]);
    env.git(&env.repo, &["commit", "-q", "-m", "ignore"]);
    let wt = env.add_worktree("archived");
    fs::write(wt.join("draft.txt"), "uncommitted\n").unwrap();
    fs::write(wt.join(".env"), "SECRET=1\n").unwrap();

    // Not registered before: archive registers, then colds.
    assert!(!env.is_registered(&wt));
    let out = env.bale_env(
        &env.repo,
        &["hook", "conductor", "archive"],
        &[(CONDUCTOR_WS, &wt)],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fs::symlink_metadata(&wt).is_err(), "dir still exists");

    let out = env.bale(&env.repo, &["open", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(wt.join("draft.txt")).unwrap(),
        "uncommitted\n"
    );
    assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1\n");
}

#[cfg(unix)]
#[test]
fn conductor_archive_busy_exits_3() {
    let env = Env::new();
    let wt = env.add_worktree("busy");
    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(&wt)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let out = env.bale_env(
        &env.repo,
        &["hook", "conductor", "archive"],
        &[(CONDUCTOR_WS, &wt)],
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("busy"), "{}", stderr(&out));
    assert!(wt.join("f").is_file());
}

fn conductor_settings(env: &Env) -> PathBuf {
    env.repo.join(".conductor").join("settings.toml")
}

const CONDUCTOR_TOML: &str = "\
# Team scripts for Conductor.
[scripts]
# Install deps first.
setup = \"npm install\" # keep this comment
run = \"npm start\"
archive = \"echo bye\"

[other]
key = 1 # unrelated
";

#[test]
fn install_conductor_keeps_existing_scripts_and_comments() {
    let env = Env::new();
    let path = conductor_settings(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, CONDUCTOR_TOML).unwrap();
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "conductor", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("committed"), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("this machine's absolute path to bale"),
        "{}",
        stdout(&out)
    );
    let after = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = after.lines().collect();
    assert_eq!(lines[0], "# Team scripts for Conductor.");
    assert_eq!(lines[1], "[scripts]");
    assert_eq!(lines[2], "# Install deps first.");
    // toml_edit writes a value with a newline as a multi-line string.
    assert_eq!(lines[3..5], ["setup = \"\"\"", "( npm install"], "{after}");
    assert!(
        lines[5].starts_with(") && if [ -x '")
            && lines[5].ends_with(" hook conductor setup; fi\"\"\" # keep this comment"),
        "{after}"
    );
    assert_eq!(lines[6], "run = \"npm start\"");
    // The user's archive runs first, while the tree still exists.
    assert_eq!(lines[7..9], ["archive = \"\"\"", "( echo bye"], "{after}");
    assert!(
        lines[9].starts_with("); if [ -x '")
            && lines[9].ends_with(" hook conductor archive; fi\"\"\""),
        "{after}"
    );
    assert_eq!(
        lines[10..],
        ["", "[other]", "key = 1 # unrelated"],
        "{after}"
    );
    let backup = path.with_file_name("settings.toml.bak-bale");
    assert!(!backup.exists(), "backup left in the repo");

    // A reinstall over the chained values changes nothing.
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "conductor", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("already installed"),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), after);
}

#[test]
fn install_conductor_twice_is_noop() {
    let env = Env::new();
    let path = conductor_settings(&env);
    let repo = env.repo.to_str().unwrap();
    let args = ["install", "--hooks", "conductor", "--repo", repo];
    let out = env.bale(&env.repo, &args, None);
    assert!(out.status.success(), "{}", stderr(&out));
    let first = fs::read_to_string(&path).unwrap();
    assert!(first.contains("[scripts]"), "{first}");
    assert!(first.contains(" hook conductor setup; fi\""), "{first}");
    assert!(first.contains(" hook conductor archive; fi\""), "{first}");

    let out = env.bale(&env.repo, &args, None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stdout(&out).contains("\n+ "), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("already installed"),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), first);
}

#[test]
fn install_conductor_does_not_chain_bale_twice() {
    let env = Env::new();
    let path = conductor_settings(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text = "[scripts]\n\
                setup = \"npm i && '/old/bale' hook conductor setup\"\n\
                archive = \"'/old/bale' hook conductor archive; echo bye\"\n";
    fs::write(&path, text).unwrap();
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "conductor", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("left unchanged"), "{}", stdout(&out));
    assert!(
        !stdout(&out).contains("already installed"),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

/// `feat` worktree with two commits no remote ref has (`main` counts as
/// pushed through `refs/remotes/origin/main`).
fn unpushed_worktree(env: &Env) -> PathBuf {
    env.git(
        &env.repo,
        &["update-ref", "refs/remotes/origin/main", "main"],
    );
    let wt = env.add_worktree("feat");
    for (file, subject) in [("a", "first change"), ("b", "add the session column")] {
        fs::write(wt.join(file), "x\n").unwrap();
        env.git(&wt, &["add", file]);
        env.git(&wt, &["commit", "-q", "-m", subject]);
    }
    wt
}

/// The `bale ls` row whose last column is `wt`'s path.
fn ls_row(env: &Env, wt: &Path) -> String {
    let out = env.bale(&env.repo, &["ls"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let want = canon(wt);
    stdout(&out)
        .lines()
        .find(|l| {
            l.split("  ")
                .last()
                .is_some_and(|p| canon(Path::new(p.trim())) == want)
        })
        .unwrap_or_else(|| panic!("no row for {}: {}", wt.display(), stdout(&out)))
        .to_string()
}

#[test]
fn pin_and_unpin_mark_the_tree_in_ls() {
    let env = Env::new();
    let wt = env.add_worktree("feat");
    // Default target is the current directory, as with adopt.
    let out = env.bale(&wt, &["pin"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).ends_with(": pinned\n"), "{}", stdout(&out));
    let row = ls_row(&env, &wt);
    assert!(row.contains("*hot"), "{row}");

    let out = env.bale(&env.repo, &["unpin", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).ends_with(": unpinned\n"), "{}", stdout(&out));
    let row = ls_row(&env, &wt);
    assert!(!row.contains('*'), "{row}");
    assert!(row.contains(" hot "), "{row}");
}

#[test]
fn ls_shows_session_column_and_json_fields() {
    let env = Env::new();
    let wt = unpushed_worktree(&env);
    let out = env.bale(&env.repo, &["adopt", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = env.bale(&env.repo, &["ls"], None);
    let header = stdout(&out).lines().next().unwrap().to_string();
    let cols: Vec<&str> = header.split_whitespace().collect();
    assert_eq!(
        cols,
        ["ID", "LEVEL", "ACTIVE", "BRANCH", "SESSION", "PATH"],
        "{header}"
    );
    // No title known: the newest commit subject stands in.
    let row = ls_row(&env, &wt);
    assert!(row.contains("add the session column"), "{row}");

    let out = env.bale(&env.repo, &["ls", "--json"], None);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rec = &v.as_array().unwrap()[0];
    assert_eq!(rec["pinned"], false);
    assert!(rec["origin"].is_null(), "{rec}");
    assert_eq!(rec["scope"]["unpushed"], 2);
    assert_eq!(rec["scope"]["subjects"][0], "add the session column");
}

#[test]
fn ls_truncates_long_subjects_to_50_chars() {
    let env = Env::new();
    let wt = env.add_worktree("feat");
    env.git(
        &env.repo,
        &["update-ref", "refs/remotes/origin/main", "main"],
    );
    let long = "x".repeat(80);
    env.git(&wt, &["commit", "-q", "--allow-empty", "-m", &long]);
    env.bale(&env.repo, &["adopt", wt.to_str().unwrap()], None);
    let row = ls_row(&env, &wt);
    assert!(!row.contains(&"x".repeat(51)), "{row}");
    assert!(row.contains(&"x".repeat(49)), "{row}");
}

#[test]
fn show_prints_live_then_saved_scope() {
    let env = Env::new();
    let wt = unpushed_worktree(&env);
    fs::write(wt.join("f"), "two\n").unwrap();
    let arg = wt.to_str().unwrap();
    env.bale(&env.repo, &["pin", arg], None);

    let out = env.bale(&env.repo, &["show", arg], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for want in [
        "level:    hot",
        "pinned:   yes",
        "scope (live from git):",
        "branch:   feat (no upstream)",
        "2 commits on no remote",
        "add the session column",
        "first change",
        "1 file changed, 1 insertion(+), 1 deletion(-)",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    let id = text
        .lines()
        .find_map(|l| l.strip_prefix("id:"))
        .unwrap()
        .trim()
        .to_string();

    let out = env.bale(&env.repo, &["cold", arg], None);
    assert!(out.status.success(), "{}", stderr(&out));
    // Cold: the checkout is gone, so show prints what cold saved, by id.
    let out = env.bale(&env.repo, &["show", &id], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for want in [
        "level:    cold",
        "scope (saved when it went cold):",
        "2 commits on no remote",
        "add the session column",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}

#[test]
fn show_refuses_an_unregistered_tree_without_registering_it() {
    let env = Env::new();
    let wt = env.add_worktree("feat");
    let out = env.bale(&env.repo, &["show", wt.to_str().unwrap()], None);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not registered"), "{}", stderr(&out));
    assert!(!env.is_registered(&wt));
}

/// The id `bale ls --json` gives the tree at `wt`.
fn json_record(env: &Env, wt: &Path) -> Value {
    let out = env.bale(&env.repo, &["ls", "--json"], None);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .find(|r| canon(Path::new(r["path"].as_str().unwrap())) == canon(wt))
        .unwrap_or_else(|| panic!("{wt:?} not in {v}"))
        .clone()
}

/// Writes a Claude Code transcript for `tree` under `claude_home`.
fn transcript(claude_home: &Path, tree: &Path, session: &str, title: &str) {
    let dir = claude_home
        .join("projects")
        .join(bale_hooks::claude::encode_project_path(&canon(tree)));
    fs::create_dir_all(&dir).unwrap();
    let line =
        serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": session});
    fs::write(
        dir.join(format!("{session}.jsonl")),
        format!("{{\"type\":\"user\"}}\n{line}\n"),
    )
    .unwrap();
}

#[test]
fn create_hook_records_the_claude_session() {
    let env = Env::new();
    let input = payload("worktree_create.json", &[("cwd", &env.repo)]);
    let out = env.bale(&env.repo, &["hook", "claude", "create"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    let path = single_path(&out);
    let rec = json_record(&env, &path);
    assert_eq!(
        rec["origin"],
        serde_json::json!({"tool": "claude", "session": "abc123"})
    );

    // The title comes from the session's transcript once Claude Code names it.
    transcript(
        &env.home().join(".claude"),
        &path,
        "abc123",
        "Polls iOS remaining work",
    );
    let row = ls_row(&env, &path);
    assert!(row.contains("Polls iOS remaining work"), "{row}");
    let out = env.bale(&env.repo, &["show", path.to_str().unwrap()], None);
    let text = stdout(&out);
    assert!(text.contains("origin:   claude session abc123"), "{text}");
    assert!(
        text.contains("title:    Polls iOS remaining work"),
        "{text}"
    );
}

#[test]
fn ls_finds_a_title_for_a_tree_without_origin_and_honors_claude_config_dir() {
    let env = Env::new();
    let wt = unpushed_worktree(&env);
    env.bale(&env.repo, &["adopt", wt.to_str().unwrap()], None);
    let config = env.tmp.path().join("claude config");
    transcript(&config, &wt, "s1", "Named elsewhere");
    // Not under $HOME/.claude, so only CLAUDE_CONFIG_DIR finds it.
    let row = ls_row(&env, &wt);
    assert!(row.contains("add the session column"), "{row}");
    let out = env.bale_env(&env.repo, &["ls"], &[("CLAUDE_CONFIG_DIR", &config)]);
    assert!(stdout(&out).contains("Named elsewhere"), "{}", stdout(&out));
}

#[test]
fn trace_path_prints_the_trace_dir() {
    let env = Env::new();
    let out = env.bale(&env.repo, &["trace", "path"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim_end(),
        env.bale_home().join("trace").to_string_lossy()
    );
}

#[test]
fn sweep_succeeds_with_one_warning_when_the_trace_cannot_be_written() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let out = env.bale(&env.repo, &["adopt", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    // Adopting already traced; a regular file in place of the dir makes
    // every later write fail.
    fs::remove_dir_all(env.bale_home().join("trace")).unwrap();
    fs::write(env.bale_home().join("trace"), "not a dir").unwrap();
    let out = env.bale(&env.repo, &["sweep"], None);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("nothing to do"), "{}", stdout(&out));
    let warnings = stderr(&out).matches("usage trace").count();
    assert_eq!(warnings, 1, "{}", stderr(&out));
}

#[test]
fn cli_ops_are_traced_with_the_cli_trigger() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let out = env.bale(&env.repo, &["up", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let dir = env.bale_home().join("trace");
    let text: String = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .collect();
    let ops: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["kind"] == "op" && v["op"] == "up")
        .collect();
    assert_eq!(ops.len(), 1, "{text}");
    assert_eq!(ops[0]["trigger"], "cli");
    assert_eq!(ops[0]["outcome"], "ok");
}

/// Every parsed line of every trace file under the env's BALE_HOME.
fn trace_lines(env: &Env) -> Vec<Value> {
    let Ok(dir) = fs::read_dir(env.bale_home().join("trace")) else {
        return Vec::new();
    };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .flat_map(|e| {
            fs::read_to_string(e.path())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn sweep_sizes_global_derived_data_apart_from_the_tree() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let out = env.bale(&env.repo, &["adopt", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    // Xcode's shared DerivedData for a project inside the tree.
    let dd = env
        .home()
        .join("Library/Developer/Xcode/DerivedData/App-abc");
    fs::create_dir_all(dd.join("Build/Products")).unwrap();
    fs::write(dd.join("Build/Products/out.bin"), vec![7u8; 300_000]).unwrap();
    // Xcode records the project's real path, which exists.
    fs::create_dir(wt.join("App.xcodeproj")).unwrap();
    let workspace = wt
        .join("App.xcodeproj")
        .to_string_lossy()
        .replace('&', "&amp;");
    fs::write(
        dd.join("info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
             <key>WorkspacePath</key><string>{workspace}</string></dict></plist>\n"
        ),
    )
    .unwrap();

    let out = env.bale(&env.repo, &["sweep"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let all = trace_lines(&env);
    let tree = all.iter().find(|v| v["kind"] == "tree").unwrap();
    assert!(
        tree["global_dd_bytes"].as_u64().unwrap() >= 300_000,
        "{tree}"
    );
    // The checkout itself holds one small file.
    assert!(tree["tree_bytes"].as_u64().unwrap() < 300_000, "{tree}");
    assert!(tree["regen_bytes"].as_u64().unwrap() < 300_000, "{tree}");
}

#[test]
fn sweep_totals_global_derived_data_no_tree_claims() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let out = env.bale(&env.repo, &["adopt", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let dd_root = env.home().join("Library/Developer/Xcode/DerivedData");
    let add = |name: &str, project: &Path, bytes: usize| {
        let dd = dd_root.join(name);
        fs::create_dir_all(dd.join("Build/Products")).unwrap();
        fs::write(dd.join("Build/Products/out.bin"), vec![7u8; bytes]).unwrap();
        fs::create_dir_all(project).unwrap();
        let workspace = project.to_string_lossy().replace('&', "&amp;");
        fs::write(
            dd.join("info.plist"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
                 <key>WorkspacePath</key><string>{workspace}</string></dict></plist>\n"
            ),
        )
        .unwrap();
    };
    add("App-abc", &wt.join("App.xcodeproj"), 300_000);
    // Built from a worktree bale never registered.
    add(
        "Other-def",
        &env.home().join("other/Other.xcodeproj"),
        500_000,
    );

    let out = env.bale(&env.repo, &["sweep"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let all = trace_lines(&env);
    let tree = all.iter().find(|v| v["kind"] == "tree").unwrap();
    let ours = tree["global_dd_bytes"].as_u64().unwrap();
    assert!((300_000..500_000).contains(&ours), "{tree}");
    assert!(tree.get("global_dd_truncated").is_none(), "{tree}");
    let sweep = all.iter().find(|v| v["kind"] == "sweep").unwrap();
    let unattributed = sweep["global_dd_unattributed_bytes"].as_u64().unwrap();
    assert!((500_000..800_000).contains(&unattributed), "{sweep}");
    assert_eq!(
        sweep["global_dd_total_bytes"].as_u64().unwrap(),
        ours + unattributed,
        "{sweep}"
    );
}

#[test]
fn remove_hook_ops_carry_the_remove_intent() {
    let env = Env::new();
    let wt = env.add_worktree("wt");
    let input = payload("worktree_remove.json", &[("worktree_path", &wt)]);
    let out = env.bale(&env.repo, &["hook", "claude", "remove"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    // Already gone.
    let out = env.bale(&env.repo, &["hook", "claude", "remove"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));

    let all = trace_lines(&env);
    let colds: Vec<&Value> = all
        .iter()
        .filter(|v| v["kind"] == "op" && v["op"] == "cold")
        .collect();
    assert_eq!(colds.len(), 2, "{all:#?}");
    assert!(colds.iter().all(|v| v["intent"] == "remove"), "{all:#?}");
    assert!(
        colds.iter().all(|v| v["trigger"] == "hook:claude"),
        "{all:#?}"
    );
    assert!(colds.iter().any(|v| v["outcome"] == "ok"), "{all:#?}");
    let missing = colds.iter().find(|v| v["outcome"] == "missing").unwrap();
    assert_eq!(missing["path"], wt.to_string_lossy().as_ref());
}

fn cursor_worktrees(env: &Env) -> PathBuf {
    env.repo.join(".cursor").join("worktrees.json")
}

#[test]
fn cursor_setup_adopts_cwd() {
    let env = Env::new();
    let wt = env.add_worktree("cursor-ws");
    let out = env.bale(&wt, &["hook", "cursor", "setup"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.is_registered(&wt));
    let rec = json_record(&env, &wt);
    assert_eq!(
        rec["origin"],
        serde_json::json!({"tool": "cursor", "session": ""})
    );
}

#[test]
fn cursor_setup_outside_worktree_exits_0() {
    let env = Env::new();
    let outside = env.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let out = env.bale(&outside, &["hook", "cursor", "setup"], None);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("bale"), "{}", stderr(&out));
    assert!(env.registered().is_empty());
}

#[test]
fn install_cursor_creates_file() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "cursor", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value =
        serde_json::from_str(&fs::read_to_string(cursor_worktrees(&env)).unwrap()).unwrap();
    let cmds = v["setup-worktree"].as_array().unwrap();
    assert_eq!(cmds.len(), 1, "{v}");
    assert!(
        cmds[0].as_str().unwrap().contains(" hook cursor setup"),
        "{v}"
    );
}

#[test]
fn install_cursor_appends_to_existing_array() {
    let env = Env::new();
    let path = cursor_worktrees(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"{"zeta":1,"setup-worktree":["npm install"],"alpha":2}"#,
    )
    .unwrap();
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "cursor", "--repo", repo],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = fs::read_to_string(&path).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let cmds = v["setup-worktree"].as_array().unwrap();
    assert_eq!(cmds.len(), 2, "{text}");
    assert_eq!(cmds[0], "npm install");
    assert!(
        cmds[1].as_str().unwrap().contains(" hook cursor setup"),
        "{text}"
    );
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["zeta", "setup-worktree", "alpha"]);
    // The backup sits with bale's state, not untracked in the repo.
    assert!(!path.with_file_name("worktrees.json.bak-bale").exists());
    let backups = backups(&env);
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(
        fs::read_to_string(&backups[0]).unwrap(),
        r#"{"zeta":1,"setup-worktree":["npm install"],"alpha":2}"#
    );
    assert!(
        stdout(&out).contains(&backups[0].display().to_string()),
        "{}",
        stdout(&out)
    );
}

#[test]
fn install_cursor_finds_bale_in_any_setup_key_by_its_words() {
    let env = Env::new();
    let path = cursor_worktrees(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // The OS key is the target; bale already runs from the generic key.
    let os = if cfg!(windows) {
        "setup-worktree-windows"
    } else {
        "setup-worktree-unix"
    };
    let text =
        format!(r#"{{"{os}":["npm i"],"setup-worktree":["'/old/bale' hook cursor setup"]}}"#);
    fs::write(&path, &text).unwrap();
    let out = install(&env, "cursor", true);
    assert!(stdout(&out).contains("left unchanged"), "{}", stdout(&out));
    assert_eq!(fs::read_to_string(&path).unwrap(), text);

    // Words that only look like bale's in a string are not bale's entry.
    let text = r#"{"setup-worktree":["echo ' hook cursor setup'"]}"#;
    fs::write(&path, text).unwrap();
    install(&env, "cursor", true);
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let cmds = v["setup-worktree"].as_array().unwrap();
    assert_eq!(cmds.len(), 2, "{v}");
    assert_eq!(cmds[0], "echo ' hook cursor setup'", "{v}");
    // bale's own entry, quoted for the shell Cursor runs it in.
    let exe = env!("CARGO_BIN_EXE_bale");
    let guarded = if cfg!(windows) {
        format!("if exist \"{exe}\" \"{exe}\" hook cursor setup")
    } else {
        format!("if [ -x '{exe}' ]; then '{exe}' hook cursor setup; fi")
    };
    assert_eq!(cmds[1], guarded.as_str(), "{v}");
}

/// bale's entry under the key Cursor runs, in the bare form earlier installs
/// wrote or from another path, is repointed to this exe in the guarded form;
/// uninstall takes either form out.
#[test]
fn install_cursor_repoints_and_guards_bales_entry() {
    let env = Env::new();
    let path = cursor_worktrees(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let exe = env!("CARGO_BIN_EXE_bale");
    let (bare, guarded) = if cfg!(windows) {
        let q = format!("\"{exe}\"");
        (
            format!("{q} hook cursor setup"),
            format!("if exist {q} {q} hook cursor setup"),
        )
    } else {
        let q = format!("'{exe}'");
        (
            format!("{q} hook cursor setup"),
            format!("if [ -x {q} ]; then {q} hook cursor setup; fi"),
        )
    };
    for (old, note) in [
        (
            bare.clone(),
            format!("rewrote bale's setup-worktree entry for bale at {exe}"),
        ),
        (
            "'/old/bale' hook cursor setup".to_string(),
            format!("bale's setup-worktree entry now runs bale at {exe} instead of /old/bale"),
        ),
    ] {
        let text = serde_json::json!({"setup-worktree": ["npm i", old]}).to_string();
        fs::write(&path, &text).unwrap();
        let out = install(&env, "cursor", true);
        assert!(!stdout(&out).contains("left unchanged"), "{}", stdout(&out));
        assert!(stdout(&out).contains(&note), "{}", stdout(&out));
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            v["setup-worktree"],
            serde_json::json!(["npm i", guarded]),
            "{old}"
        );
        let out = install(&env, "cursor", true);
        assert!(
            stdout(&out).contains("already installed"),
            "{}",
            stdout(&out)
        );
    }

    for entry in [bare, guarded] {
        let text = serde_json::json!({"setup-worktree": ["npm i", entry]}).to_string();
        fs::write(&path, &text).unwrap();
        let out = uninstall(&env, "cursor", true);
        assert!(!stdout(&out).contains("left unchanged"), "{}", stdout(&out));
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["setup-worktree"], serde_json::json!(["npm i"]), "{entry}");
    }
}

/// bale only ever writes an absolute exe path, so an entry that names bale
/// by a bare name or relative path, or guards it in a shape bale does not
/// write, is the user's: install and uninstall both leave it, with a note.
#[test]
fn cursor_leaves_entries_bale_did_not_write() {
    let env = Env::new();
    let path = cursor_worktrees(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for entry in [
        "bale hook cursor setup",
        "if [ -x /usr/local/bin/bale ]; then /usr/local/bin/bale hook cursor setup; fi",
    ] {
        let text = serde_json::json!({"setup-worktree": ["npm i", entry]}).to_string();
        fs::write(&path, &text).unwrap();
        let out = install(&env, "cursor", true);
        assert!(stdout(&out).contains("left unchanged"), "{}", stdout(&out));
        assert_eq!(fs::read_to_string(&path).unwrap(), text, "{entry}");

        let out = uninstall(&env, "cursor", true);
        assert!(stdout(&out).contains("left unchanged"), "{}", stdout(&out));
        assert_eq!(fs::read_to_string(&path).unwrap(), text, "{entry}");
    }

    // Alone in a file bale did not back up: the file stays.
    let text = serde_json::json!({"setup-worktree": [
        "if [ -x /usr/local/bin/bale ]; then /usr/local/bin/bale hook cursor setup; fi"
    ]})
    .to_string();
    fs::write(&path, &text).unwrap();
    uninstall(&env, "cursor", true);
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn install_cursor_refuses_script_path_value() {
    let env = Env::new();
    let path = cursor_worktrees(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = r#"{"setup-worktree":".cursor/setup.sh"}"#;
    fs::write(&path, original).unwrap();
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "cursor", "--repo", repo],
        None,
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("setup-worktree"), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn install_cursor_twice_is_noop() {
    let env = Env::new();
    let path = cursor_worktrees(&env);
    let repo = env.repo.to_str().unwrap();
    let args = ["install", "--hooks", "cursor", "--repo", repo];
    let out = env.bale(&env.repo, &args, None);
    assert!(out.status.success(), "{}", stderr(&out));
    let first = fs::read_to_string(&path).unwrap();

    let out = env.bale(&env.repo, &args, None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("already installed"),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), first);
}

#[test]
fn codex_setup_adopts_path_arg() {
    let env = Env::new();
    let wt = env.add_worktree("codex-ws");
    let elsewhere = env.tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let out = env.bale(
        &elsewhere,
        &["hook", "codex", "setup", wt.to_str().unwrap()],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.is_registered(&wt));
    let rec = json_record(&env, &wt);
    assert_eq!(
        rec["origin"],
        serde_json::json!({"tool": "codex", "session": ""})
    );
}

#[test]
fn codex_setup_outside_worktree_exits_0() {
    let env = Env::new();
    let outside = env.tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let out = env.bale(&outside, &["hook", "codex", "setup"], None);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("bale"), "{}", stderr(&out));
    assert!(env.registered().is_empty());
}

#[test]
fn install_codex_writes_nothing() {
    let env = Env::new();
    let before = listing(&env.home());
    let repo_files = |env: &Env| -> Vec<PathBuf> {
        listing(&env.repo)
            .into_iter()
            .filter(|p| !p.strip_prefix(&env.repo).unwrap().starts_with(".git"))
            .collect()
    };
    let repo_before = repo_files(&env);
    let out = env.bale(&env.repo, &["install", "--hooks", "codex"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.lines().any(|l| l.ends_with(" hook codex setup")),
        "{text}"
    );
    assert!(text.contains("unconfirmed"), "{text}");
    assert_eq!(listing(&env.home()), before);
    assert_eq!(repo_files(&env), repo_before);
    assert!(!env.repo.join(".codex").exists());
}

#[test]
fn codex_setup_resolves_relative_path_arg() {
    let env = Env::new();
    let wt = env.add_worktree("codex-rel");
    let out = env.bale(
        wt.parent().unwrap(),
        &["hook", "codex", "setup", "codex-rel"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.is_registered(&wt), "{}", stdout(&out));
    assert_eq!(json_record(&env, &wt)["origin"]["tool"], "codex");
}

/// The backups bale keeps under `$BALE_HOME/backups`.
fn backups(env: &Env) -> Vec<PathBuf> {
    let dir = env.bale_home().join("backups");
    if !dir.exists() {
        return Vec::new();
    }
    listing(&dir)
}

#[test]
fn install_conductor_repoints_bales_own_chained_command() {
    let env = Env::new();
    let path = conductor_settings(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // As an install from /old/bale wrote them.
    fs::write(
        &path,
        "[scripts]\n\
         setup = \"( npm i # c\\n) && '/old/bale' hook conductor setup\" # keep\n\
         archive = \"'/old/bale' hook conductor archive\"\n",
    )
    .unwrap();
    let out = install(&env, "conductor", true);
    assert!(!stdout(&out).contains("left unchanged"), "{}", stdout(&out));
    for kind in ["setup", "archive"] {
        let note = format!(
            "scripts.{kind} now runs bale at {} instead of /old/bale",
            env!("CARGO_BIN_EXE_bale")
        );
        assert!(stdout(&out).contains(&note), "{}", stdout(&out));
    }
    let exe = env!("CARGO_BIN_EXE_bale").replace('\\', "/");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!(
            "[scripts]\n\
             setup = \"\"\"\n( npm i # c\n) && if [ -x '{exe}' ]; then '{exe}' hook conductor setup; fi\"\"\" # keep\n\
             archive = \"if [ -x '{exe}' ]; then '{exe}' hook conductor archive; fi\"\n"
        )
    );
}

/// A script naming bale without an absolute path is the user's: install
/// and uninstall leave it, with a note.
#[test]
fn conductor_leaves_a_path_relative_bale_script() {
    let env = Env::new();
    let path = conductor_settings(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text = "[scripts]\nsetup = \"npm i\"\narchive = \"bale hook conductor archive\"\n";
    fs::write(&path, text).unwrap();
    let out = install(&env, "conductor", true);
    assert!(stdout(&out).contains("left unchanged"), "{}", stdout(&out));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("archive = \"bale hook conductor archive\"\n"),
        "{}",
        fs::read_to_string(&path).unwrap()
    );
    let out = uninstall(&env, "conductor", true);
    assert!(stdout(&out).contains("left unchanged"), "{}", stdout(&out));
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

/// An install from before the guard, at this exe's path, gets the guarded
/// form; uninstall takes either form out.
#[test]
fn install_conductor_guards_an_unguarded_install() {
    let env = Env::new();
    let path = conductor_settings(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let exe = env!("CARGO_BIN_EXE_bale").replace('\\', "/");
    let unguarded = format!(
        "[scripts]\n\
         setup = \"\"\"\n( npm i\n) && '{exe}' hook conductor setup\"\"\"\n\
         archive = \"'{exe}' hook conductor archive\"\n"
    );
    fs::write(&path, &unguarded).unwrap();
    let out = install(&env, "conductor", true);
    assert!(
        !stdout(&out).contains("already installed"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!(
            "[scripts]\n\
             setup = \"\"\"\n( npm i\n) && if [ -x '{exe}' ]; then '{exe}' hook conductor setup; fi\"\"\"\n\
             archive = \"if [ -x '{exe}' ]; then '{exe}' hook conductor archive; fi\"\n"
        )
    );

    fs::write(&path, &unguarded).unwrap();
    let out = uninstall(&env, "conductor", true);
    assert!(!stdout(&out).contains("left unchanged"), "{}", stdout(&out));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "[scripts]\nsetup = \"npm i\"\n"
    );
}

#[test]
fn install_claude_notes_that_repo_is_ignored() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap();
    for cmd in ["install", "uninstall"] {
        let out = env.bale(
            &env.repo,
            &[cmd, "--hooks", "claude", "--repo", repo, "--print-only"],
            None,
        );
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(
            stdout(&out).contains("--repo is ignored")
                && stdout(&out).contains("CLAUDE_CONFIG_DIR"),
            "{cmd}: {}",
            stdout(&out)
        );
    }
    let out = env.bale(&env.repo, &["uninstall", "--help"], None);
    assert!(
        stdout(&out).contains("CLAUDE_CONFIG_DIR"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn hooks_list_is_deduplicated() {
    let env = Env::new();
    let path = settings_path(&env);
    let out = env.bale(
        &env.repo,
        &["install", "--hooks", "claude,claude", "--print-only"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let update = format!("update {}:", path.display());
    assert_eq!(stdout(&out).matches(&update).count(), 1, "{}", stdout(&out));
    install(&env, "claude", false);
    let out = env.bale(
        &env.repo,
        &["uninstall", "--hooks", "claude,claude", "--print-only"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out)
            .matches(&format!("rm {}", path.display()))
            .count(),
        1,
        "{}",
        stdout(&out)
    );
}

/// With CLAUDE_CONFIG_DIR set, install and uninstall use the settings.json
/// there, as Claude Code does, and leave `~/.claude` alone.
#[test]
fn claude_hooks_follow_claude_config_dir() {
    let env = Env::new();
    // Not created yet: install makes it, uninstall finds the same backup.
    let config = env.tmp.path().join("claude config");
    let path = config.join("settings.json");
    let vars = [("CLAUDE_CONFIG_DIR", config.as_path())];
    let run = |cmd: &str| {
        let out = env.bale_env(&env.repo, &[cmd, "--hooks", "claude"], &vars);
        assert!(out.status.success(), "{cmd}: {}", stderr(&out));
        out
    };
    let out = run("install");
    // No other remove hook, so no note about one.
    assert!(!stdout(&out).contains("in parallel"), "{}", stdout(&out));
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(" hook claude remove"), "{text}");
    assert!(!settings_path(&env).exists());
    let out = run("uninstall");
    assert!(!path.exists(), "{}", stdout(&out));

    // A file the user made comes back byte for byte.
    let original = "{\"theme\": \"dark\"}\n";
    fs::write(&path, original).unwrap();
    run("install");
    assert_ne!(fs::read_to_string(&path).unwrap(), original);
    run("uninstall");
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert!(!settings_path(&env).exists());
}

/// `bale uninstall --hooks <tool> [--repo <repo>]`.
fn uninstall(env: &Env, tool: &str, repo: bool) -> Output {
    let mut args = vec!["uninstall", "--hooks", tool];
    let repo_s = env.repo.to_str().unwrap();
    if repo {
        args.extend(["--repo", repo_s]);
    }
    let out = env.bale(&env.repo, &args, None);
    assert!(out.status.success(), "{}", stderr(&out));
    out
}

/// `bale install --hooks <tool> [--repo <repo>]`, which must succeed.
fn install(env: &Env, tool: &str, repo: bool) -> Output {
    let mut args = vec!["install", "--hooks", tool];
    let repo_s = env.repo.to_str().unwrap();
    if repo {
        args.extend(["--repo", repo_s]);
    }
    let out = env.bale(&env.repo, &args, None);
    assert!(out.status.success(), "{}", stderr(&out));
    out
}

#[test]
fn uninstall_claude_keeps_user_hooks() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Not in serde_json's layout: the original bytes come back.
    let compact =
        serde_json::to_string(&serde_json::from_str::<Value>(USER_HOOKS).unwrap()).unwrap();
    fs::write(&path, &compact).unwrap();
    install(&env, "claude", false);
    let out = uninstall(&env, "claude", false);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        compact,
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("bale open"), "{}", stdout(&out));

    // Edited after install: only bale's entries go; a wrapped one stays.
    install(&env, "claude", false);
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    v["model"] = "opus".into();
    v["hooks"]["WorktreeRemove"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"hooks": [{"type": "command",
            "command": "nice '/x/bale' hook claude remove"}]}));
    fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    let out = uninstall(&env, "claude", false);
    assert!(stdout(&out).contains("left unchanged"), "{}", stdout(&out));
    let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let user: Value = serde_json::from_str(USER_HOOKS).unwrap();
    assert_eq!(after["model"], "opus");
    assert_eq!(after["hooks"]["PreToolUse"], user["hooks"]["PreToolUse"]);
    assert!(after["hooks"].get("WorktreeCreate").is_none(), "{after}");
    let remove = after["hooks"]["WorktreeRemove"].as_array().unwrap();
    assert_eq!(remove.len(), 2, "{after}");
    assert_eq!(remove[0], user["hooks"]["WorktreeRemove"][0]);
    assert_eq!(
        remove[1]["hooks"][0]["command"],
        "nice '/x/bale' hook claude remove"
    );

    // A settings file bale created is deleted.
    let env = Env::new();
    let path = settings_path(&env);
    install(&env, "claude", false);
    assert!(path.is_file());
    uninstall(&env, "claude", false);
    assert!(!path.exists());
}

#[test]
fn uninstall_conductor_restores_scripts() {
    let env = Env::new();
    let path = conductor_settings(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for original in [CONDUCTOR_TOML, "# no scripts yet\n[other]\nkey = 1\n"] {
        fs::write(&path, original).unwrap();
        install(&env, "conductor", true);
        assert_ne!(fs::read_to_string(&path).unwrap(), original);
        let out = uninstall(&env, "conductor", true);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "{}",
            stdout(&out)
        );
    }

    // Written by hand around bale: left alone.
    let by_hand = "[scripts]\nsetup = \"npm i && '/old/bale' hook conductor setup\"\n";
    fs::write(&path, by_hand).unwrap();
    let out = uninstall(&env, "conductor", true);
    assert!(stdout(&out).contains("left unchanged"), "{}", stdout(&out));
    assert_eq!(fs::read_to_string(&path).unwrap(), by_hand);

    // A file bale created is deleted.
    fs::remove_file(&path).unwrap();
    install(&env, "conductor", true);
    assert!(path.is_file());
    uninstall(&env, "conductor", true);
    assert!(!path.exists());
}

#[test]
fn uninstall_cursor_removes_only_bale_entry() {
    let env = Env::new();
    let path = cursor_worktrees(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = r#"{"zeta":1,"setup-worktree":["npm install"],"alpha":2}"#;
    fs::write(&path, original).unwrap();
    install(&env, "cursor", true);
    uninstall(&env, "cursor", true);
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    // Edited after install: the user's commands stay, bale's key goes.
    fs::write(&path, r#"{"other":1}"#).unwrap();
    install(&env, "cursor", true);
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    v["other"] = 2.into();
    v["setup-worktree-unix"] = serde_json::json!(["echo hi"]);
    fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    uninstall(&env, "cursor", true);
    let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        after,
        serde_json::json!({"other": 2, "setup-worktree-unix": ["echo hi"]})
    );

    // A file bale created is deleted.
    fs::remove_file(&path).unwrap();
    install(&env, "cursor", true);
    uninstall(&env, "cursor", true);
    assert!(!path.exists());
}

#[test]
fn uninstall_git_hook_leaves_foreign_hook() {
    let env = Env::new();
    let hook = hooks_dir(&env).join("post-checkout");
    let prev = hooks_dir(&env).join("post-checkout.pre-bale");
    write_hook(&hook, "echo mine");
    let foreign = fs::read(&hook).unwrap();

    // Not bale's: untouched.
    let out = uninstall(&env, "git", true);
    assert!(stdout(&out).contains("not bale's"), "{}", stdout(&out));
    assert_eq!(fs::read(&hook).unwrap(), foreign);

    // Chained by install: the user's hook moves back.
    install(&env, "git", true);
    assert!(prev.is_file());
    uninstall(&env, "git", true);
    assert_eq!(fs::read(&hook).unwrap(), foreign);
    assert!(!prev.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&hook).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    // bale's own hook alone: removed.
    fs::remove_file(&hook).unwrap();
    install(&env, "git", true);
    assert!(hook.is_file());
    uninstall(&env, "git", true);
    assert!(!hook.exists());
}

#[test]
fn uninstall_schedule_print_only_lists_steps() {
    let env = Env::new();
    let wt = env.add_worktree("cold-one");
    let out = env.bale(&env.repo, &["cold", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let logs = env.home().join("Library/Logs/bale");
    if cfg!(target_os = "macos") {
        fs::create_dir_all(&logs).unwrap();
    }
    let before = listing(&env.home());
    let bale_before = listing(&env.bale_home());

    let out = env.bale(&env.repo, &["uninstall", "--print-only"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let home = env.home();
    if cfg!(target_os = "macos") {
        let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
        assert!(
            text.contains(&format!(
                "run: launchctl bootout gui/{}/dev.bale.sweep",
                uid.trim()
            )),
            "{text}"
        );
        let plist = home.join("Library/LaunchAgents/dev.bale.sweep.plist");
        assert!(text.contains(&format!("rm {}", plist.display())), "{text}");
        assert!(
            text.contains(&format!("{} is left in place", logs.display())),
            "{text}"
        );
    } else if cfg!(windows) {
        assert!(
            text.contains("run: schtasks /Delete /TN bale-sweep /F"),
            "{text}"
        );
    } else {
        assert!(
            text.contains("run: systemctl --user disable --now bale-sweep.timer"),
            "{text}"
        );
        let units = home.join(".config/systemd/user");
        for unit in ["bale-sweep.service", "bale-sweep.timer"] {
            assert!(
                text.contains(&format!("rm {}", units.join(unit).display())),
                "{text}"
            );
        }
    }
    assert!(text.contains("1 worktree is cold"), "{text}");
    assert!(text.contains("bale open"), "{text}");
    assert_eq!(listing(&env.home()), before);
    assert_eq!(listing(&env.bale_home()), bale_before);
}

#[test]
fn install_refreshes_the_backup_after_an_uninstall() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, r#"{"model":"a"}"#).unwrap();
    install(&env, "claude", false);
    uninstall(&env, "claude", false);
    assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"model":"a"}"#);

    // The user's edit is what the next install backs up and uninstall restores.
    let edit = r#"{"model":"b","permissions":{"allow":["X"]}}"#;
    fs::write(&path, edit).unwrap();
    install(&env, "claude", false);
    let backups = backups(&env);
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), edit);
    uninstall(&env, "claude", false);
    assert_eq!(fs::read_to_string(&path).unwrap(), edit);

    // The user deletes the file; the file bale then creates is bale's to delete.
    fs::remove_file(&path).unwrap();
    install(&env, "claude", false);
    uninstall(&env, "claude", false);
    assert!(!path.exists());
}

#[test]
fn uninstall_keeps_files_the_user_made() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for original in ["{}", r#"{"hooks":{}}"#] {
        fs::write(&path, original).unwrap();
        install(&env, "claude", false);
        let out = uninstall(&env, "claude", false);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "{}",
            stdout(&out)
        );
    }

    let path = cursor_worktrees(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let committed = r#"{"setup-worktree":[]}"#;
    fs::write(&path, committed).unwrap();
    install(&env, "cursor", true);
    let out = uninstall(&env, "cursor", true);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        committed,
        "{}",
        stdout(&out)
    );

    // Edited after install, so the backup no longer matches: the file stays.
    install(&env, "cursor", true);
    let mut v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let bale_entry = v["setup-worktree"][0].clone();
    v.as_object_mut().unwrap().shift_remove("setup-worktree");
    v["setup-worktree-unix"] = serde_json::json!([bale_entry]);
    fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    uninstall(&env, "cursor", true);
    let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after, serde_json::json!({}));

    let path = conductor_settings(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for original in ["", "[scripts]\n"] {
        fs::write(&path, original).unwrap();
        install(&env, "conductor", true);
        let out = uninstall(&env, "conductor", true);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "{}",
            stdout(&out)
        );
    }
}

#[test]
fn uninstall_restores_the_first_matching_backup_and_drops_an_old_one() {
    let env = Env::new();
    let path = cursor_worktrees(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let original = r#"{"setup-worktree":["npm i"]}"#;
    fs::write(&path, original).unwrap();
    install(&env, "cursor", true);
    // An install from before backups moved kept the original beside the
    // file; a stale backup sits where bale keeps them now.
    let backups = backups(&env);
    assert_eq!(backups.len(), 1, "{backups:?}");
    let old = path.with_file_name("worktrees.json.bak-bale");
    fs::write(&old, original).unwrap();
    fs::write(&backups[0], r#"{"stale":true}"#).unwrap();
    let out = uninstall(&env, "cursor", true);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        original,
        "{}",
        stdout(&out)
    );
    assert!(!old.exists(), "{}", stdout(&out));
}

#[test]
fn uninstall_conductor_keeps_a_scripts_table_with_comments() {
    let env = Env::new();
    let path = conductor_settings(&env);
    install(&env, "conductor", true);
    // The user notes something in the [scripts] table bale created.
    let text = fs::read_to_string(&path).unwrap();
    let text = text.replacen("[scripts]\n", "[scripts]\n# run npm i here later\n", 1);
    fs::write(&path, &text).unwrap();
    let out = uninstall(&env, "conductor", true);
    let after = fs::read_to_string(&path).unwrap();
    assert!(after.contains("[scripts]"), "{after}\n{}", stdout(&out));
    assert!(after.contains("# run npm i here later"), "{after}");
    assert!(!after.contains("hook conductor"), "{after}");
}

/// The post-checkout hook `bale install --git-hook` wrote before `--hooks`.
const OLD_GIT_HOOK: &str = "#!/bin/sh
# Installed by `bale install --git-hook`.
# Registers new linked worktrees with bale, then runs the previous hook.
BALE='/old/bale'
command -v \"$BALE\" >/dev/null 2>&1 || BALE=bale
if command -v \"$BALE\" >/dev/null 2>&1; then
  \"$BALE\" hook git-post-checkout \"$@\" || true
fi
PREV=\"$(dirname \"$0\")/post-checkout.pre-bale\"
if [ -x \"$PREV\" ]; then
  exec \"$PREV\" \"$@\"
fi
exit 0
";

#[test]
fn uninstall_git_removes_only_an_unedited_bale_hook() {
    let env = Env::new();
    let hook = hooks_dir(&env).join("post-checkout");
    let prev = hooks_dir(&env).join("post-checkout.pre-bale");
    write_hook(&hook, "echo mine");
    install(&env, "git", true);
    // The user adds a line to bale's hook: nothing is removed.
    let mut edited = fs::read_to_string(&hook).unwrap();
    edited.push_str("echo added\n");
    fs::write(&hook, &edited).unwrap();
    let out = uninstall(&env, "git", true);
    assert!(stdout(&out).contains("edited"), "{}", stdout(&out));
    assert_eq!(fs::read_to_string(&hook).unwrap(), edited);
    assert!(prev.is_file());

    // A hook from a bale before `--hooks`, unedited: removed.
    fs::remove_file(&prev).unwrap();
    fs::write(&hook, OLD_GIT_HOOK).unwrap();
    uninstall(&env, "git", true);
    assert!(!hook.exists());
}

#[test]
fn uninstall_git_notes_global_hooks_path() {
    let env = Env::new();
    let shared = env.tmp.path().join("user hooks");
    fs::create_dir_all(&shared).unwrap();
    fs::write(
        env.home().join(".gitconfig"),
        format!("[core]\n\thooksPath = {}\n", sh_path(&shared)),
    )
    .unwrap();
    install(&env, "git", true);
    let out = uninstall(&env, "git", true);
    assert!(
        stdout(&out).contains("global git config"),
        "{}",
        stdout(&out)
    );
    assert!(!shared.join("post-checkout").exists());
}

/// Commits `text` as `.cursor/worktrees.json` in the env's repo.
fn commit_cursor_worktrees(env: &Env, text: &str) -> PathBuf {
    let path = cursor_worktrees(env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    env.git(&env.repo, &["add", "-A"]);
    env.git(&env.repo, &["commit", "-q", "-m", "cursor"]);
    path
}

#[cfg(unix)]
#[test]
fn uninstall_through_the_real_path_finds_a_backup_made_through_a_symlink() {
    let env = Env::new();
    let committed = r#"{"setup-worktree":[]}"#;
    let path = commit_cursor_worktrees(&env, committed);
    let link = env.tmp.path().join("link");
    std::os::unix::fs::symlink(&env.repo, &link).unwrap();
    let out = env.bale(
        &env.repo,
        &[
            "install",
            "--hooks",
            "cursor",
            "--repo",
            link.to_str().unwrap(),
        ],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let out = uninstall(&env, "cursor", true);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        committed,
        "{}",
        stdout(&out)
    );
    assert_eq!(env.git(&env.repo, &["status", "--short"]), "");
}

#[test]
fn uninstall_with_dot_finds_a_backup_made_through_dot_dot() {
    let env = Env::new();
    let committed = r#"{"setup-worktree":[]}"#;
    let path = commit_cursor_worktrees(&env, committed);
    let dotdot = env.repo.join(".cursor").join("..");
    let out = env.bale(
        &env.repo,
        &[
            "install",
            "--hooks",
            "cursor",
            "--repo",
            dotdot.to_str().unwrap(),
        ],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let out = env.bale(
        &env.repo,
        &["uninstall", "--hooks", "cursor", "--repo", "."],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        committed,
        "{}",
        stdout(&out)
    );
    assert_eq!(env.git(&env.repo, &["status", "--short"]), "");
}

#[test]
fn uninstall_never_deletes_a_tracked_file_without_a_backup() {
    let env = Env::new();
    // bale made both files and the user committed them; the backups are gone.
    install(&env, "cursor", true);
    install(&env, "conductor", true);
    env.git(&env.repo, &["add", "-A"]);
    env.git(&env.repo, &["commit", "-q", "-m", "hooks"]);
    let _ = fs::remove_dir_all(env.bale_home().join("backups"));
    for (tool, path) in [
        ("cursor", cursor_worktrees(&env)),
        ("conductor", conductor_settings(&env)),
    ] {
        let out = uninstall(&env, tool, true);
        let text = stdout(&out);
        assert!(path.exists(), "{tool}: {text}");
        assert!(
            !text.lines().any(|l| l.starts_with("removed ")),
            "{tool}: {text}"
        );
        assert!(text.contains("note: git tracks"), "{tool}: {text}");
        let after = fs::read_to_string(&path).unwrap();
        assert!(!after.contains("hook"), "{tool}: {after}");
    }
}

#[test]
fn install_keeps_a_stale_backup_under_an_old_name() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let precious = r#"{"model":"precious"}"#;
    fs::write(&path, precious).unwrap();
    install(&env, "claude", false);
    fs::remove_file(&path).unwrap();
    let out = install(&env, "claude", false);
    let text = stdout(&out);
    let backups = backups(&env);
    assert_eq!(backups.len(), 1, "{backups:?}\n{text}");
    let old = &backups[0];
    let name = old.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with(".old"), "{name}");
    assert_eq!(fs::read_to_string(old).unwrap(), precious);
    assert!(text.contains(&name), "{text}");
}

#[test]
fn claude_repoint_prints_the_reformat_note() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"{"hooks":{"WorktreeCreate":[{"hooks":[{"type":"command","command":"'/old/bale' hook claude create"}]}],"WorktreeRemove":[{"hooks":[{"type":"command","command":"'/old/bale' hook claude remove"}]}]}}"#,
    )
    .unwrap();
    let out = install(&env, "claude", false);
    let text = stdout(&out);
    assert!(
        text.contains("note: settings.json is reformatted (2-space JSON)"),
        "{text}"
    );
    for event in ["WorktreeCreate", "WorktreeRemove"] {
        let note = format!(
            "{event} hook now runs bale at {} instead of /old/bale",
            env!("CARGO_BIN_EXE_bale")
        );
        assert!(text.contains(&note), "{text}");
    }
    assert!(backups(&env).is_empty());
}

#[cfg(unix)]
#[test]
fn uninstall_conductor_keeps_file_when_backup_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let path = conductor_settings(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "[scripts]\n").unwrap();
    install(&env, "conductor", true);
    let backups = env.bale_home().join("backups");
    let entries: Vec<_> = fs::read_dir(&backups)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(!entries.is_empty());
    for b in &entries {
        fs::set_permissions(b, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let out = uninstall(&env, "conductor", true);
    for b in &entries {
        fs::set_permissions(b, fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(path.is_file(), "{}", stdout(&out));
}

// Warm tier: `stash`, `unstash` and the Claude Code prompt, pre-tool and
// stop hooks.

const MIB: usize = 1 << 20;

/// Deterministic, poorly compressible bytes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

const BIG_A: &str = "build/a.bin";
const BIG_B: &str = "build/b.bin";

/// A registered Gradle worktree with two 2 MiB ignored files under `build/`.
fn stash_tree(env: &Env) -> PathBuf {
    let wt = env.add_worktree("warm");
    fs::write(wt.join("settings.gradle"), "").unwrap();
    fs::write(wt.join(".gitignore"), "build/\n").unwrap();
    fs::create_dir_all(wt.join("build")).unwrap();
    fs::create_dir_all(wt.join("sub")).unwrap();
    fs::write(wt.join(BIG_A), noise(2 * MIB, 1)).unwrap();
    fs::write(wt.join(BIG_B), noise(2 * MIB, 2)).unwrap();
    let out = env.bale(&env.repo, &["adopt", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    wt
}

fn stash_it(env: &Env, wt: &Path) -> Output {
    env.bale(&env.repo, &["stash", wt.to_str().unwrap()], None)
}

fn hook_json(cwd: &Path, event: &str) -> String {
    serde_json::json!({
        "session_id": "s",
        "cwd": cwd,
        "hook_event_name": event,
        "unknown_field": [1, 2],
    })
    .to_string()
}

fn tree_id(wt: &Path) -> String {
    bale_core::registry::workspace_id(&canon(wt))
}

fn bale_lock(env: &Env, wt: &Path) -> Option<bale_core::lock::WorkspaceLock> {
    let home = bale_core::BaleHome(env.bale_home());
    bale_core::lock::WorkspaceLock::try_acquire(&home, &tree_id(wt)).unwrap()
}

/// Polls until `done` holds; panics after 30 s.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Waits until no background restore for `wt` is running, so no detached
/// process outlives the test (or its temp dir).
fn wait_no_background(wt: &Path) {
    wait_until("background unstash to exit", || !background_running(wt));
}

/// Whether a `bale unstash --background` for `wt` is running.
fn background_running(wt: &Path) -> bool {
    let pattern = format!("unstash --background {}", tree_id(wt));
    #[cfg(unix)]
    let mut cmd = {
        let mut cmd = Command::new("pgrep");
        cmd.args(["-f", &pattern]);
        cmd
    };
    // No pgrep on Windows; match on bale.exe so PowerShell's own command
    // line (which holds the pattern) doesn't count.
    #[cfg(windows)]
    let mut cmd = {
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-Command",
            &format!(
                "if (Get-CimInstance Win32_Process | Where-Object {{ $_.Name -eq 'bale.exe' -and $_.CommandLine -like '*{pattern}*' }}) {{ exit 0 }} else {{ exit 1 }}"
            ),
        ]);
        cmd
    };
    cmd.output().unwrap().status.success()
}

fn prompt_hook(env: &Env, wt: &Path) -> Output {
    env.bale(
        &env.repo,
        &["hook", "claude", "prompt"],
        Some(&hook_json(wt, "UserPromptSubmit")),
    )
}

fn pre_tool_hook(env: &Env, wt: &Path) -> Output {
    env.bale(
        &env.repo,
        &["hook", "claude", "pre-tool"],
        Some(&hook_json(wt, "PreToolUse")),
    )
}

fn len_of(p: &Path) -> u64 {
    fs::metadata(p).unwrap().len()
}

fn blobs(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            blobs(&p, out);
        } else if p.extension().is_some_and(|x| x == "zst") {
            out.push(p);
        }
    }
}

#[test]
fn stash_and_unstash_commands_round_trip() {
    let env = Env::new();
    let wt = stash_tree(&env);
    let (a, b) = (
        fs::read(wt.join(BIG_A)).unwrap(),
        fs::read(wt.join(BIG_B)).unwrap(),
    );

    let out = stash_it(&env, &wt);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("stashed 2 files, "), "{text:?}");
    assert!(!text.contains("skipped"), "{text:?}");
    assert_eq!(len_of(&wt.join(BIG_A)), 0);

    let out = env.bale(&env.repo, &["unstash", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("restored 2 files, "), "{text:?}");
    assert_eq!(fs::read(wt.join(BIG_A)).unwrap(), a);
    assert_eq!(fs::read(wt.join(BIG_B)).unwrap(), b);
}

#[test]
fn stash_command_reports_skipped_files() {
    let env = Env::new();
    let wt = stash_tree(&env);
    // Tracked despite the ignore rule: stash leaves it alone and says so.
    fs::write(wt.join("build/tracked.bin"), noise(2 * MIB, 3)).unwrap();
    env.git(&wt, &["add", "-f", "build/tracked.bin"]);

    let out = stash_it(&env, &wt);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("stashed 2 files, "), "{text:?}");
    assert!(
        text.trim_end()
            .ends_with("(1 files skipped: not ignored by git)"),
        "{text:?}"
    );
    assert_eq!(len_of(&wt.join("build/tracked.bin")), (2 * MIB) as u64);
}

#[test]
fn stash_command_says_why_it_skipped() {
    let env = Env::new();
    let wt = stash_tree(&env);
    assert!(stash_it(&env, &wt).status.success());
    let out = stash_it(&env, &wt);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "not stashed: already stashed\n");
}

#[test]
fn hook_prompt_unstashes_tree_at_cwd() {
    let env = Env::new();
    let wt = stash_tree(&env);
    let a = fs::read(wt.join(BIG_A)).unwrap();
    assert!(stash_it(&env, &wt).status.success());

    let input = hook_json(&wt.join("sub"), "UserPromptSubmit");
    let out = env.bale(&env.repo, &["hook", "claude", "prompt"], Some(&input));
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    // The restore runs in the background.
    wait_until("files restored", || len_of(&wt.join(BIG_B)) > 0);
    wait_no_background(&wt);
    assert_eq!(fs::read(wt.join(BIG_A)).unwrap(), a);
}

#[test]
fn prompt_returns_before_restore_finishes() {
    let env = Env::new();
    let wt = stash_tree(&env);
    let a = fs::read(wt.join(BIG_A)).unwrap();
    assert!(stash_it(&env, &wt).status.success());
    // Holding the tree's lock stands in for a slow restore.
    let held = bale_lock(&env, &wt).expect("lock is free");

    let start = std::time::Instant::now();
    let out = prompt_hook(&env, &wt);
    assert!(start.elapsed().as_secs() < 5, "prompt waited");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        len_of(&wt.join(BIG_A)),
        0,
        "restored while the lock was held"
    );

    drop(held);
    wait_until("files restored", || len_of(&wt.join(BIG_B)) > 0);
    wait_no_background(&wt);
    assert_eq!(fs::read(wt.join(BIG_A)).unwrap(), a);
}

#[test]
fn pre_tool_waits_for_background_restore() {
    use std::process::Stdio;
    let env = Env::new();
    let wt = stash_tree(&env);
    assert!(stash_it(&env, &wt).status.success());
    let held = bale_lock(&env, &wt).expect("lock is free");
    assert_eq!(prompt_hook(&env, &wt).status.code(), Some(0));

    let mut cmd = Command::cargo_bin("bale").unwrap();
    cmd.args(["hook", "claude", "pre-tool"])
        .current_dir(&env.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    env.isolate(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(hook_json(&wt, "PreToolUse").as_bytes())
            .unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(child.try_wait().unwrap().is_none(), "pre-tool did not wait");

    drop(held);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    // It returns only once the files are back.
    assert!(len_of(&wt.join(BIG_A)) > 0 && len_of(&wt.join(BIG_B)) > 0);
    wait_no_background(&wt);
}

#[test]
fn prompt_not_stashed_spawns_nothing() {
    let env = Env::new();
    let wt = stash_tree(&env);
    let out = prompt_hook(&env, &wt);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert_eq!(stderr(&out), "");
    assert!(!background_running(&wt), "a background restore was spawned");
    assert!(!env.bale_home().join("restore-failed").exists());
}

#[test]
fn pre_tool_not_stashed_is_fast() {
    let env = Env::new();
    let wt = stash_tree(&env);
    let runs = 20;
    let start = std::time::Instant::now();
    for _ in 0..runs {
        let out = pre_tool_hook(&env, &wt);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    }
    let avg = start.elapsed() / runs;
    eprintln!("pre-tool, not stashed: {avg:?} per call (process included)");
    assert!(avg < std::time::Duration::from_millis(100), "{avg:?}");
    // No store was opened for it.
    assert!(!env.bale_home().join("store").exists());
}

fn bash_payload(cwd: &Path, command: &str) -> String {
    serde_json::json!({
        "session_id": "s",
        "cwd": cwd,
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command },
    })
    .to_string()
}

#[cfg(unix)]
#[test]
fn pre_tool_learns_derived_data_path_from_a_bash_command() {
    let env = Env::new();
    let cwd = env.repo.clone();
    let run = |command: &str| {
        let out = env.bale(
            &cwd,
            &["hook", "claude", "pre-tool"],
            Some(&bash_payload(&cwd, command)),
        );
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(stdout(&out), "");
        assert_eq!(stderr(&out), "");
    };
    let roots = env.bale_home().join("dd-roots");
    run("ls -la");
    assert!(!roots.exists());
    run("xcodebuild -scheme App -derivedDataPath /tmp/fc-1-dd build");
    run("xcodebuild -derivedDataPath rel-dd test");
    run("xcodebuild -derivedDataPath /tmp/fc-1-dd build");
    assert_eq!(
        fs::read_to_string(&roots).unwrap(),
        format!("/tmp/fc-1-dd\n{}\n", cwd.join("rel-dd").display())
    );
}

/// A hook-learned `-derivedDataPath` root whose project was deleted is reaped
/// by the next sweep once past the grace period; `--dry-run` only reports.
#[cfg(target_os = "macos")]
#[test]
fn sweep_reaps_a_derived_data_root_the_hook_learned() {
    let env = Env::new();
    let dd = env.home().join("fc-9-dd");
    fs::create_dir_all(dd.join("Build/Products")).unwrap();
    fs::write(dd.join("Build/Products/out"), vec![0u8; 4096]).unwrap();
    let missing = env.home().join("deleted-wt/App.xcodeproj");
    fs::write(
        dd.join("info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
             <key>WorkspacePath</key><string>{}</string></dict></plist>\n",
            missing.display()
        ),
    )
    .unwrap();
    let out = env.bale(
        &env.repo,
        &["hook", "claude", "pre-tool"],
        Some(&bash_payload(
            &env.repo,
            &format!("xcodebuild -derivedDataPath {} build", dd.display()),
        )),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    for p in [dd.clone(), dd.join("info.plist"), dd.join("Build")] {
        fs::File::open(p).unwrap().set_modified(old).unwrap();
    }

    let out = env.bale(&env.repo, &["sweep", "--dry-run"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("reap  would reap"),
        "{}",
        stdout(&out)
    );
    assert!(dd.exists());

    let out = env.bale(&env.repo, &["sweep"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("reap  reaped"), "{}", stdout(&out));
    assert!(!dd.exists());
}

#[cfg(unix)]
#[test]
fn pre_tool_ignores_a_derived_data_path_it_cannot_write() {
    let env = Env::new();
    // A directory where the file belongs makes the append fail.
    fs::create_dir_all(env.bale_home().join("dd-roots")).unwrap();
    let out = env.bale(
        &env.repo,
        &["hook", "claude", "pre-tool"],
        Some(&bash_payload(
            &env.repo,
            "xcodebuild -derivedDataPath /tmp/x",
        )),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!((stdout(&out).as_str(), stderr(&out).as_str()), ("", ""));
}

#[test]
fn hook_prompt_outside_any_tree_is_noop() {
    let env = Env::new();
    let _wt = stash_tree(&env);
    let out = env.bale(
        &env.repo,
        &["hook", "claude", "prompt"],
        Some(&hook_json(&env.home(), "UserPromptSubmit")),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}

#[test]
fn hook_prompt_with_bad_payload_does_not_block() {
    let env = Env::new();
    let wt = stash_tree(&env);
    assert!(stash_it(&env, &wt).status.success());
    for input in [
        "not json",
        "{}",
        r#"{"cwd": 7}"#,
        r#"{"cwd": "/no/such/dir"}"#,
    ] {
        let out = env.bale(&env.repo, &["hook", "claude", "prompt"], Some(input));
        assert_eq!(out.status.code(), Some(0), "{input}: {}", stderr(&out));
        assert_eq!(stdout(&out), "", "{input}");
        assert_eq!(stderr(&out).trim_end().lines().count(), 1, "{input}");
    }
    assert_eq!(len_of(&wt.join(BIG_A)), 0);
}

#[test]
fn background_failure_blocks_next_hook() {
    let env = Env::new();
    let wt = stash_tree(&env);
    let originals = [
        (BIG_A, fs::read(wt.join(BIG_A)).unwrap()),
        (BIG_B, fs::read(wt.join(BIG_B)).unwrap()),
    ];
    assert!(stash_it(&env, &wt).status.success());
    let mut found = Vec::new();
    blobs(&env.bale_home().join("store"), &mut found);
    found.sort();
    assert_eq!(found.len(), 2, "{found:?}");
    let mut bytes = fs::read(&found[0]).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    fs::write(&found[0], bytes).unwrap();

    // The prompt itself does not see the failure.
    let out = prompt_hook(&env, &wt);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    wait_until("background run to finish", || {
        env.bale_home().join("restore-failed").exists()
    });
    wait_no_background(&wt);
    let (lost, kept): (Vec<_>, Vec<_>) = originals
        .iter()
        .partition(|(rel, _)| len_of(&wt.join(rel)) == 0);
    assert_eq!(lost.len(), 1, "exactly the corrupt file stays a stub");
    assert_eq!(fs::read(wt.join(kept[0].0)).unwrap(), kept[0].1);

    // The next hook reports it once, naming the file.
    let out = pre_tool_hook(&env, &wt);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    let lost_rel = lost[0].0.replace('/', std::path::MAIN_SEPARATOR_STR);
    assert!(stderr(&out).contains(&lost_rel), "{}", stderr(&out));
    let out = pre_tool_hook(&env, &wt);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
}

#[test]
fn failure_marker_blocks_prompt_and_survives_a_later_restore() {
    let env = Env::new();
    let wt = stash_tree(&env);
    assert!(stash_it(&env, &wt).status.success());
    let dir = env.bale_home().join("restore-failed");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{}.txt", tree_id(&wt))),
        "boom: build/a.bin\n",
    )
    .unwrap();

    // A successful synchronous restore leaves the marker for the user.
    let out = env.bale(&env.repo, &["unstash", wt.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(len_of(&wt.join(BIG_A)) > 0);

    let out = prompt_hook(&env, &wt);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("boom: build/a.bin"),
        "{}",
        stderr(&out)
    );
    let out = prompt_hook(&env, &wt);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn hook_pre_tool_lock_timeout_exits_2() {
    let env = Env::new();
    let wt = stash_tree(&env);
    assert!(stash_it(&env, &wt).status.success());
    let home = bale_core::BaleHome(env.bale_home());
    let id = bale_core::registry::workspace_id(&canon(&wt));
    let _held = bale_core::lock::WorkspaceLock::try_acquire(&home, &id)
        .unwrap()
        .expect("lock is free");

    let out = env.bale(
        &env.repo,
        &["hook", "claude", "pre-tool", "--lock-wait-ms", "300"],
        Some(&hook_json(&wt, "PreToolUse")),
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    let err = stderr(&out);
    assert!(err.contains("still busy"), "{err}");
    assert!(!err.contains("lock file"), "{err}");
    assert_eq!(len_of(&wt.join(BIG_A)), 0);
}

#[test]
fn hook_pre_tool_bumps_activity() {
    let env = Env::new();
    let wt = stash_tree(&env);
    let before = ls_json_row(&env, &wt)["last_activity"].clone();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out = env.bale(
        &env.repo,
        &["hook", "claude", "pre-tool"],
        Some(&hook_json(&wt.join("sub"), "PreToolUse")),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert_ne!(ls_json_row(&env, &wt)["last_activity"], before);
}

#[test]
fn pre_tool_hook_lets_the_next_sweep_stash() {
    let env = Env::new();
    let wt = stash_tree(&env);
    fs::write(
        env.bale_home().join("config.toml"),
        "idle_stash_minutes = 0\n",
    )
    .unwrap();
    let home = bale_core::BaleHome(env.bale_home());
    let reg = bale_core::registry::Registry::open(home);
    let id = tree_id(&wt);

    // No restoring hook has run: the sweep leaves the build outputs alone.
    assert!(!reg.get(&id).unwrap().unwrap().restore_hook);
    let out = env.bale(&env.repo, &["sweep"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(len_of(&wt.join(BIG_A)), 2 * MIB as u64);
    assert!(reg.get(&id).unwrap().unwrap().stash.is_none());

    let out = pre_tool_hook(&env, &wt);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(reg.get(&id).unwrap().unwrap().restore_hook);

    let out = env.bale(&env.repo, &["sweep"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(len_of(&wt.join(BIG_A)), 0);
    let rec = reg.get(&id).unwrap().unwrap();
    assert!(rec.stash.is_some() && rec.restore_hook);
}

#[test]
fn hook_pre_tool_unstashes_tree() {
    let env = Env::new();
    let wt = stash_tree(&env);
    assert!(stash_it(&env, &wt).status.success());
    let out = env.bale(
        &env.repo,
        &["hook", "claude", "pre-tool"],
        Some(&hook_json(&wt, "PreToolUse")),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(len_of(&wt.join(BIG_A)) > 0);
}

#[test]
fn hook_stop_only_touches_activity() {
    let env = Env::new();
    let wt = stash_tree(&env);
    assert!(stash_it(&env, &wt).status.success());
    let store = env.bale_home().join("store");
    let store_mtime = fs::metadata(&store).unwrap().modified().unwrap();
    let before = ls_json_row(&env, &wt)["last_activity"].clone();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let out = env.bale(
        &env.repo,
        &["hook", "claude", "stop"],
        Some(&hook_json(&wt, "Stop")),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert_ne!(ls_json_row(&env, &wt)["last_activity"], before);
    assert_eq!(
        fs::metadata(&store).unwrap().modified().unwrap(),
        store_mtime
    );
    // Stop restores nothing.
    assert_eq!(len_of(&wt.join(BIG_A)), 0);
}

#[test]
fn install_plan_sets_timeout() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"echo mine"}]}],
            "PreToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"echo edit"}]}]}}"#,
    )
    .unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

    // User entries stay, without a timeout bale did not set.
    let prompt = v["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(prompt.len(), 2, "{prompt:?}");
    let ours = |groups: &[Value]| -> Value {
        groups
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .find(|h| h["command"].as_str().unwrap().contains("bale"))
            .unwrap()
            .clone()
    };
    let user = prompt
        .iter()
        .flat_map(|g| g["hooks"].as_array().unwrap())
        .find(|h| h["command"] == "echo mine")
        .unwrap();
    assert!(user.get("timeout").is_none());
    assert_eq!(ours(prompt)["timeout"], 90);
    assert!(ours(prompt)["command"]
        .as_str()
        .unwrap()
        .ends_with(" hook claude prompt"));

    let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2, "{pre:?}");
    let group = pre
        .iter()
        .find(|g| {
            g["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("hook claude pre-tool")
        })
        .unwrap();
    assert!(group.get("matcher").is_none(), "{group}");
    assert_eq!(group["hooks"][0]["timeout"], 90);
    assert_eq!(pre.iter().filter(|g| g["matcher"] == "Edit").count(), 1);

    let stop = &v["hooks"]["Stop"][0]["hooks"][0];
    assert!(stop["command"]
        .as_str()
        .unwrap()
        .ends_with(" hook claude stop"));
    assert!(stop.get("timeout").is_none());

    // Uninstall removes only bale's entries.
    let out = env.bale(&env.repo, &["uninstall", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "echo mine"
    );
    assert_eq!(v["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
    assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Edit");
    assert!(v["hooks"].get("Stop").is_none());
    assert!(v["hooks"].get("WorktreeCreate").is_none());
}

#[test]
fn reinstall_replaces_the_old_bash_matcher() {
    let env = Env::new();
    let path = settings_path(&env);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let exe = env!("CARGO_BIN_EXE_bale").replace('\\', "/");
    let old = serde_json::json!({"hooks": {"PreToolUse": [
        {"matcher": "Bash", "hooks": [{"type": "command",
            "command": format!("'{exe}' hook claude pre-tool"), "timeout": 90}]},
        {"matcher": "Bash", "hooks": [{"type": "command", "command": "my-guard check"}]}]}});
    fs::write(&path, serde_json::to_string(&old).unwrap()).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2, "{v}");
    assert!(pre[0].get("matcher").is_none(), "{v}");
    assert_eq!(
        pre[0]["hooks"][0]["command"],
        format!("'{exe}' hook claude pre-tool")
    );
    // The user's own Bash entry is not touched.
    assert_eq!(pre[1]["matcher"], "Bash");
    assert_eq!(pre[1]["hooks"][0]["command"], "my-guard check");

    // Installed again, nothing changes; uninstall leaves only the user's.
    let again = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(
        stdout(&again).contains("already installed"),
        "{}",
        stdout(&again)
    );
    let out = env.bale(&env.repo, &["uninstall", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "my-guard check"
    );
}
