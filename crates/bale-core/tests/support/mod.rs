#![allow(dead_code)]

use bale_core::BaleHome;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Points HOME at an empty temp dir, once per test binary, unless it already
/// lies in the temp dir (a test set it). `sweep` reads and, for orphaned
/// DerivedData, deletes under the user's home; a test must never reach the
/// real one. Tests that need a particular HOME set it themselves.
fn isolate_home() {
    let temp = bale_core::paths::canonicalize(std::env::temp_dir()).unwrap_or_default();
    let in_temp = bale_core::paths::home_dir()
        .and_then(|h| bale_core::paths::canonicalize(h).ok())
        .is_some_and(|h| h.starts_with(&temp));
    if in_temp {
        return;
    }
    static HOME: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let dir = TempDir::new().expect("tempdir");
        std::env::set_var("HOME", dir.path());
        std::env::set_var("USERPROFILE", dir.path());
        dir
    });
}

/// A temp git repo with one commit on `main`; the path contains a space and `é`.
pub struct Fixture {
    tmp: TempDir,
    pub repo: PathBuf,
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

impl Fixture {
    pub fn new() -> Fixture {
        Fixture::init(TempDir::new().expect("tempdir"))
    }

    fn init(tmp: TempDir) -> Fixture {
        isolate_home();
        let repo = tmp.path().join("my repo é");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "Bale Test"]);
        git(&repo, &["config", "user.email", "bale@example.com"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("f"), "one\n").unwrap();
        git(&repo, &["add", "f"]);
        git(&repo, &["commit", "-m", "init"]);
        Fixture { tmp, repo }
    }

    /// Adds a linked worktree on a new `branch`, returning its path.
    pub fn add_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.tmp.path().join(name);
        git(
            &self.repo,
            &["worktree", "add", "-b", branch, path.to_str().unwrap()],
        );
        path
    }

    pub fn home(&self) -> BaleHome {
        BaleHome(self.tmp.path().join("bale home"))
    }
}

impl Fixture {
    /// Like `new`, but under cargo's per-target tmp dir, which is outside the
    /// system temp dir (so `migrate` does not see it as a temp location).
    pub fn outside_temp() -> Fixture {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        std::fs::create_dir_all(&base).unwrap();
        Fixture::init(TempDir::new_in(base).expect("tempdir"))
    }
}

/// Writes `dir/info.plist` as Xcode DerivedData has it (XML), creating `dir`.
/// `workspace_path` is its `WorkspacePath`; `None` leaves the key out.
pub fn write_plist(dir: &Path, workspace_path: Option<&Path>) {
    plist_value(dir, workspace_path)
        .to_file_xml(dir.join("info.plist"))
        .unwrap();
}

/// [`write_plist`] in the binary format.
pub fn write_plist_binary(dir: &Path, workspace_path: Option<&Path>) {
    plist_value(dir, workspace_path)
        .to_file_binary(dir.join("info.plist"))
        .unwrap();
}

fn plist_value(dir: &Path, workspace_path: Option<&Path>) -> plist::Value {
    std::fs::create_dir_all(dir).unwrap();
    let mut dict = plist::Dictionary::new();
    if let Some(p) = workspace_path {
        dict.insert(
            "WorkspacePath".into(),
            plist::Value::String(p.to_string_lossy().into_owned()),
        );
    }
    plist::Value::Dictionary(dict)
}
