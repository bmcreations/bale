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
            .env("LC_ALL", "C");
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
    let c = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match c.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) => PathBuf::from(rest),
        None => c,
    }
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
    let out = env.bale(&env.repo, &["hook", "claude-worktree-create"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    let path = single_path(&out);
    assert!(path.is_dir(), "{}", path.display());
    assert_eq!(path.file_name().unwrap(), "feature-auth");
    assert!(env.is_registered(&path));
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
    let input = payload("worktree_create.json", &[("cwd", &env.repo)]);
    let out = env.bale(&env.repo, &["hook", "claude-worktree-create"], Some(&input));
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
    let out = env.bale(&env.repo, &["hook", "claude-worktree-remove"], Some(&input));
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fs::symlink_metadata(&wt).is_err(), "dir still exists");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));

    // Already gone: counts as removed.
    let out = env.bale(&env.repo, &["hook", "claude-worktree-remove"], Some(&input));
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
    let out = env.bale(&env.repo, &["hook", "claude-worktree-remove"], Some(&input));
    let _ = child.kill();
    let _ = child.wait();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("busy"), "{}", stderr(&out));
    assert!(wt.join("f").is_file());
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
        &["hook", "git-post-checkout", ZEROS, sha, "1"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(env.is_registered(&fresh));

    let out = env.bale(&other, &["hook", "git-post-checkout", sha, sha, "1"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!env.is_registered(&other));

    // The main checkout (as after `git clone`) is never registered, and never fails.
    let out = env.bale(
        &env.repo,
        &["hook", "git-post-checkout", ZEROS, sha, "1"],
        None,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!env.is_registered(&env.repo));
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

    let out = env.bale(&env.repo, &["install", "--git-hook", repo], None);
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
        let out = env.bale(&env.repo, &["install", "--git-hook", repo], None);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let dir = hooks_dir(&env);
    let prev = fs::read_to_string(dir.join("post-checkout.pre-bale")).unwrap();
    assert!(prev.contains(original), "{prev}");
    assert!(!prev.contains("bale"), "bale chained to itself: {prev}");
    let hook = fs::read_to_string(dir.join("post-checkout")).unwrap();
    assert_eq!(hook.matches("hook git-post-checkout").count(), 1, "{hook}");
}

#[test]
fn git_hook_previous_failure_propagates() {
    let env = Env::new();
    write_hook(&hooks_dir(&env).join("post-checkout"), "exit 7");
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(&env.repo, &["install", "--git-hook", repo], None);
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
    let out = env.bale(&env.repo, &["install", "--git-hook", repo], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(shared.join("post-checkout.pre-bale").is_file());
    let hook = fs::read_to_string(shared.join("post-checkout")).unwrap();
    assert!(hook.contains("hook git-post-checkout"), "{hook}");
}

#[test]
fn git_hook_refuses_hooks_path_inside_tree() {
    let env = Env::new();
    let husky = env.repo.join(".husky");
    write_hook(&husky.join("post-checkout"), "echo tracked");
    env.git(&env.repo, &["config", "core.hooksPath", ".husky"]);
    let before = fs::read_to_string(husky.join("post-checkout")).unwrap();
    let repo = env.repo.to_str().unwrap();
    let out = env.bale(&env.repo, &["install", "--git-hook", repo], None);
    assert!(!out.status.success());
    let all = format!("{}{}", stdout(&out), stderr(&out));
    assert!(all.contains("hook git-post-checkout"), "{all}");
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

    let out = env.bale(&env.repo, &["install", "--claude-hooks"], None);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("+"),
        "no diff printed: {}",
        stdout(&out)
    );
    let first = fs::read_to_string(&path).unwrap();
    let out = env.bale(&env.repo, &["install", "--claude-hooks"], None);
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
        ("WorktreeCreate", "claude-worktree-create"),
        ("WorktreeRemove", "claude-worktree-remove"),
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
    let out = env.bale(&env.repo, &["install", "--claude-hooks"], None);
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
    let exe = canon(Path::new(env!("CARGO_BIN_EXE_bale")));
    assert!(text.contains(exe.to_str().unwrap()), "{text}");
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
            "--claude-hooks",
            "--git-hook",
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
