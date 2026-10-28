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

/// Cold must not go through: the tree stays, Baled, and the skip reason
/// names `path`.
fn assert_refused(fx: &Fixture, wt: &Path, id: &str, path: &str) {
    let out = cold(&fx.home(), id).unwrap();
    let Outcome::Skipped(reason) = &out else {
        panic!("expected a refusal, got {out:?}");
    };
    assert!(reason.contains(path), "{reason}");
    assert!(wt.is_dir(), "the tree was deleted");
    assert_eq!(level(fx, id), Level::Baled);
}

/// Cold must refuse before it changes anything: the tree stays, the record
/// keeps its level, and the skip reason names `path`.
fn assert_refused_untouched(fx: &Fixture, wt: &Path, id: &str, path: &str) {
    let before = level(fx, id);
    let out = cold(&fx.home(), id).unwrap();
    let Outcome::Skipped(reason) = &out else {
        panic!("expected a refusal, got {out:?}");
    };
    assert!(reason.contains(path), "{reason}");
    assert!(wt.is_dir(), "the tree was deleted");
    assert_eq!(level(fx, id), before);
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

/// A clean filter that drops lines: the wip blob lacks them and checkout
/// would not write them back.
#[test]
fn lossy_clean_filter_is_refused() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            git(r, &["config", "filter.strip.clean", "sed /SECRET/d"]);
            git(r, &["config", "filter.strip.smudge", "cat"]);
            fs::write(r.join(".gitattributes"), "*.nb filter=strip\n").unwrap();
            commit_all(r);
        },
        |wt| fs::write(wt.join("a.nb"), "keep\nSECRET=1\n").unwrap(),
    );
    assert_refused(&fx, &wt, &id, "a.nb");
    assert_eq!(
        fs::read_to_string(wt.join("a.nb")).unwrap(),
        "keep\nSECRET=1\n"
    );
}

#[test]
fn crlf_file_under_autocrlf_input_is_refused() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            git(r, &["config", "core.autocrlf", "input"]);
        },
        |wt| fs::write(wt.join("c.txt"), "a\r\nb\r\n").unwrap(),
    );
    assert_refused(&fx, &wt, &id, "c.txt");
    assert_eq!(fs::read(wt.join("c.txt")).unwrap(), b"a\r\nb\r\n");
}

/// Checkout expands `$Id$`, so a new file holding it would come back
/// changed.
#[test]
fn unexpanded_ident_is_refused() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            fs::write(r.join(".gitattributes"), "*.id ident\n").unwrap();
            commit_all(r);
        },
        |wt| fs::write(wt.join("x.id"), "$Id$\n").unwrap(),
    );
    assert_refused(&fx, &wt, &id, "x.id");
    assert_eq!(fs::read_to_string(wt.join("x.id")).unwrap(), "$Id$\n");
}

/// A committed ident file as checkout wrote it comes back the same.
#[test]
fn checked_out_ident_file_goes_cold() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            fs::write(r.join(".gitattributes"), "*.id ident\n").unwrap();
            fs::write(r.join("x.id"), "$Id$\n").unwrap();
            commit_all(r);
        },
        |_| {},
    );
    let before = fs::read(wt.join("x.id")).unwrap();
    assert_ne!(before, b"$Id$\n");
    round_trip(&fx, &wt, &id);
    assert_eq!(fs::read(wt.join("x.id")).unwrap(), before);
}

/// The default on Windows: checkout writes CRLF for an LF blob, so CRLF
/// files round-trip and must not be refused.
#[test]
fn crlf_files_under_autocrlf_true_round_trip() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            git(r, &["config", "core.autocrlf", "true"]);
            fs::write(r.join(".gitattributes"), "* text=auto\n").unwrap();
            fs::write(r.join("t.txt"), "one\r\ntwo\r\n").unwrap();
            commit_all(r);
        },
        |wt| {
            fs::write(wt.join("t.txt"), "one\r\nthree\r\n").unwrap();
            fs::write(wt.join("new.txt"), "n\r\n").unwrap();
        },
    );
    round_trip(&fx, &wt, &id);
    assert_eq!(fs::read(wt.join("t.txt")).unwrap(), b"one\r\nthree\r\n");
    assert_eq!(fs::read(wt.join("new.txt")).unwrap(), b"n\r\n");
}

/// The default on Windows, with a file an agent wrote with LF endings:
/// checkout would write it back as CRLF, so cold refuses and names it.
#[test]
fn lf_file_under_autocrlf_true_is_refused() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            git(r, &["config", "core.autocrlf", "true"]);
        },
        |wt| fs::write(wt.join("lf.txt"), "a\nb\n").unwrap(),
    );
    assert_refused(&fx, &wt, &id, "lf.txt");
    assert_eq!(fs::read(wt.join("lf.txt")).unwrap(), b"a\nb\n");
}

/// Git refuses a `.git` in any case as a path: depending on the platform
/// `add -A` drops `.GIT` or fails on it. Cold refuses before `up` runs.
#[test]
fn case_variant_git_dir_in_untracked_dir_is_refused() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |_| {},
        |wt| {
            fs::create_dir_all(wt.join("d/.GIT")).unwrap();
            fs::write(wt.join("d/.GIT/keep"), "k").unwrap();
        },
    );
    let named = Path::new("d").join(".GIT").display().to_string();
    assert_refused_untouched(&fx, &wt, &id, &named);
    assert_eq!(fs::read_to_string(wt.join("d/.GIT/keep")).unwrap(), "k");
}

#[test]
fn case_variant_git_dir_in_tracked_dir_is_refused() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            fs::create_dir_all(r.join("src")).unwrap();
            fs::write(r.join("src/a.txt"), "a").unwrap();
            commit_all(r);
        },
        |wt| {
            fs::create_dir_all(wt.join("src/.GIT")).unwrap();
            fs::write(wt.join("src/.GIT/keep"), "k").unwrap();
        },
    );
    let named = Path::new("src").join(".GIT").display().to_string();
    assert_refused_untouched(&fx, &wt, &id, &named);
    assert_eq!(fs::read_to_string(wt.join("src/.GIT/keep")).unwrap(), "k");
}

#[cfg(unix)]
#[test]
fn fifo_is_refused() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |_| {},
        |wt| {
            let ok = std::process::Command::new("mkfifo")
                .arg(wt.join("pipe"))
                .status()
                .unwrap()
                .success();
            assert!(ok);
        },
    );
    assert_refused(&fx, &wt, &id, "pipe");
    assert!(fs::symlink_metadata(wt.join("pipe")).is_ok());
}

/// open checks the restore against the wip tree and the manifest, not the
/// ignore rules in force when it runs.
#[test]
fn exclude_line_removed_while_cold_still_opens() {
    let fx = Fixture::new();
    let exclude = fx.repo.join(".git/info/exclude");
    let (wt, id) = worktree(
        &fx,
        |_| fs::write(&exclude, "local.cfg\n").unwrap(),
        |wt| fs::write(wt.join("local.cfg"), "mine\n").unwrap(),
    );
    assert_eq!(cold(&fx.home(), &id).unwrap(), Outcome::Done(Level::Cold));
    fs::write(&exclude, "").unwrap();

    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert_eq!(fs::read_to_string(wt.join("local.cfg")).unwrap(), "mine\n");
}

/// A file git tracks in the wip under a dir the walks deny (`node_modules`)
/// is still checked: here a clean filter strips its trailing spaces.
#[test]
fn tracked_file_in_node_modules_under_lossy_filter_is_refused() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |r| {
            git(r, &["config", "filter.strip.clean", "sed 's/ *$//'"]);
            git(r, &["config", "filter.strip.smudge", "cat"]);
            fs::write(r.join(".gitattributes"), "*.txt filter=strip\n").unwrap();
            commit_all(r);
        },
        |wt| {
            fs::create_dir_all(wt.join("node_modules")).unwrap();
            fs::write(wt.join("node_modules/n.txt"), "a   \n").unwrap();
        },
    );
    assert_refused(&fx, &wt, &id, "node_modules/n.txt");
    assert_eq!(
        fs::read_to_string(wt.join("node_modules/n.txt")).unwrap(),
        "a   \n"
    );
}

#[cfg(unix)]
#[test]
fn private_dir_keeps_its_mode() {
    let fx = Fixture::new();
    let (wt, id) = worktree(
        &fx,
        |_| {},
        |wt| {
            fs::create_dir_all(wt.join("priv")).unwrap();
            fs::write(wt.join("priv/s.txt"), "s\n").unwrap();
            chmod(&wt.join("priv"), 0o700);
        },
    );
    round_trip(&fx, &wt, &id);
    assert_eq!(fs::read_to_string(wt.join("priv/s.txt")).unwrap(), "s\n");
    assert_eq!(mode(&wt.join("priv")), 0o700);
}
