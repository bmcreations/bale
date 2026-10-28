#![allow(dead_code)]

use bale_core::BaleHome;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

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
