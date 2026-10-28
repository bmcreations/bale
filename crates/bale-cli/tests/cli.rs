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
            .env_remove("CLAUDE_CONFIG_DIR");
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

    let backup = path.with_file_name("settings.json.bak-bale");
    assert_eq!(fs::read_to_string(backup).unwrap(), SETTINGS);

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
        assert!(text.contains("<integer>1800</integer>"), "{text}");
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
      "WorktreeCreate":[{"hooks":[{"type":"command","command":"env BALE_HOME=/x '/old/bale' hook claude-worktree-create"}]}],
      "WorktreeRemove":[{"hooks":[{"type":"command","command":"nice '/old/bale' hook claude-worktree-remove"}]}]}}"#;
    fs::write(&path, wrapped).unwrap();
    let out = env.bale(&env.repo, &["install", "--hooks", "claude"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&path).unwrap(), wrapped);
    let text = stdout(&out);
    assert!(text.contains("left unchanged"), "{text}");
    assert!(text.contains("nice '/old/bale'"), "{text}");
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
    assert_eq!(v["hooks"]["PreToolUse"], user["hooks"]["PreToolUse"]);
    // bale's archive-side entry runs before the user's.
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
    let after = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = after.lines().collect();
    assert_eq!(lines[0], "# Team scripts for Conductor.");
    assert_eq!(lines[1], "[scripts]");
    assert_eq!(lines[2], "# Install deps first.");
    // toml_edit writes a value with a newline as a multi-line string.
    assert_eq!(lines[3..5], ["setup = \"\"\"", "( npm install"], "{after}");
    assert!(
        lines[5].starts_with(") && '")
            && lines[5].ends_with(" hook conductor setup\"\"\" # keep this comment"),
        "{after}"
    );
    assert_eq!(lines[6], "run = \"npm start\"");
    // The user's archive runs first, while the tree still exists.
    assert_eq!(lines[7..9], ["archive = \"\"\"", "( echo bye"], "{after}");
    assert!(
        lines[9].starts_with("); '") && lines[9].ends_with(" hook conductor archive\"\"\""),
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
    assert!(first.contains(" hook conductor setup\""), "{first}");
    assert!(first.contains(" hook conductor archive\""), "{first}");

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
        cmds[0].as_str().unwrap().ends_with(" hook cursor setup"),
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
        cmds[1].as_str().unwrap().ends_with(" hook cursor setup"),
        "{text}"
    );
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["zeta", "setup-worktree", "alpha"]);
    assert!(path.with_file_name("worktrees.json.bak-bale").exists());
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
