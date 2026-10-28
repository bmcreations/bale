//! `cold` then `open` must give back the tree byte for byte, or `cold` must
//! refuse and leave it in place.
mod support;

use bale_core::ops::cold::cold;
use bale_core::ops::open::open;
use bale_core::ops::up::{register, Outcome};
use bale_core::registry::{Level, Registry};
use std::fs;
use std::path::{Path, PathBuf};
use support::{git, Fixture};

/// A worktree `wt` on `feature` after `setup` ran in the repo (to commit)
/// and then `prep` in the worktree.
fn worktree(fx: &Fixture, setup: impl Fn(&Path), prep: impl Fn(&Path)) -> (PathBuf, String) {
    setup(&fx.repo);
    let wt = fx.add_worktree("wt", "feature");
    prep(&wt);
    let id = register(&fx.home(), &wt).unwrap().id;
    (wt, id)
}

fn commit_all(repo: &Path) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "setup"]);
}

fn level(fx: &Fixture, id: &str) -> Level {
    Registry::open(fx.home()).get(id).unwrap().unwrap().level
}

/// Colds and opens `wt`, asserting both succeed.
fn round_trip(fx: &Fixture, wt: &Path, id: &str) {
    assert_eq!(cold(&fx.home(), id).unwrap(), Outcome::Done(Level::Cold));
    assert!(!wt.exists());
    let report = open(&fx.home(), id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
}

/// Cold must not go through: the tree stays, Baled.
fn assert_refused(fx: &Fixture, wt: &Path, id: &str) -> Outcome {
    let out = cold(&fx.home(), id).unwrap();
    assert!(
        matches!(out, Outcome::Busy(_) | Outcome::Skipped(_)),
        "expected a refusal, got {out:?}"
    );
    assert!(wt.is_dir(), "the tree was deleted");
    assert_ne!(level(fx, id), Level::Cold);
    out
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[test]
fn ignored_exec_file_keeps_its_mode() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            fs::write(r.join(".gitignore"), "x/\n").unwrap();
            commit_all(r);
        },
        |wt| {
            fs::create_dir_all(wt.join("x")).unwrap();
            fs::write(wt.join("x/run.sh"), "#!/bin/sh\n").unwrap();
            chmod(&wt.join("x/run.sh"), 0o755);
        },
    );
    round_trip(&fx, &wt, &id);
    assert_eq!(mode(&wt.join("x/run.sh")), 0o755);
}

#[cfg(unix)]
#[test]
fn untracked_private_file_keeps_its_mode() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |_| {},
        |wt| {
            fs::write(wt.join("key.pem"), "key\n").unwrap();
            chmod(&wt.join("key.pem"), 0o600);
        },
    );
    round_trip(&fx, &wt, &id);
    assert_eq!(fs::read_to_string(wt.join("key.pem")).unwrap(), "key\n");
    assert_eq!(mode(&wt.join("key.pem")), 0o600);
}

#[cfg(unix)]
#[test]
fn new_exec_file_under_filemode_false_keeps_its_mode() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            git(r, &["config", "core.fileMode", "false"]);
        },
        |wt| {
            fs::write(wt.join("new.sh"), "#!/bin/sh\n").unwrap();
            chmod(&wt.join("new.sh"), 0o755);
        },
    );
    round_trip(&fx, &wt, &id);
    assert_eq!(mode(&wt.join("new.sh")), 0o755);
}

#[test]
fn empty_dirs_round_trip() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            fs::write(r.join(".gitignore"), "x/\n").unwrap();
            commit_all(r);
        },
        |wt| {
            fs::create_dir_all(wt.join("e1")).unwrap();
            fs::create_dir_all(wt.join("e2/inner")).unwrap();
            fs::create_dir_all(wt.join("x/emptyign")).unwrap();
            fs::write(wt.join("x/keep"), "k").unwrap();
        },
    );
    round_trip(&fx, &wt, &id);
    assert!(wt.join("e1").is_dir());
    assert!(wt.join("e2/inner").is_dir());
    assert!(wt.join("x/emptyign").is_dir());
    assert_eq!(fs::read_to_string(wt.join("x/keep")).unwrap(), "k");
}
