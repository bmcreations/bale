mod support;

use bale_core::git::{self, Git};
use support::{git as sh, Fixture};

fn canon(p: &std::path::Path) -> std::path::PathBuf {
    bale_core::paths::canonicalize(p).unwrap()
}

#[test]
fn lists_main_and_linked() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt one", "feat");
    let list = git::worktrees(&fx.repo).unwrap();
    assert_eq!(list.len(), 2);
    let main = list.iter().find(|w| w.is_main).unwrap();
    assert_eq!(canon(&main.path), canon(&fx.repo));
    assert_eq!(main.branch.as_deref(), Some("main"));
    assert_eq!(main.head.len(), 40);
    let linked = list.iter().find(|w| !w.is_main).unwrap();
    assert_eq!(canon(&linked.path), canon(&wt));
    assert_eq!(linked.branch.as_deref(), Some("feat"));
    assert_eq!(linked.locked, None);
    assert!(!linked.prunable);
}

#[test]
fn detached_worktree_has_no_branch() {
    let fx = Fixture::new();
    let p = fx.repo.parent().unwrap().join("det");
    sh(
        &fx.repo,
        &["worktree", "add", "--detach", p.to_str().unwrap()],
    );
    let list = git::worktrees(&fx.repo).unwrap();
    let w = list.iter().find(|w| !w.is_main).unwrap();
    assert_eq!(w.branch, None);
}

#[test]
fn lock_respects_foreign_lock() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feat");
    sh(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "someone else",
            wt.to_str().unwrap(),
        ],
    );
    let err = git::lock_worktree(&fx.repo, &wt).unwrap_err();
    assert!(format!("{err:#}").contains("someone else"), "{err:#}");
    assert!(!git::unlock_worktree_if_ours(&fx.repo, &wt).unwrap());
    let list = git::worktrees(&fx.repo).unwrap();
    let w = list.iter().find(|w| !w.is_main).unwrap();
    assert_eq!(w.locked.as_deref(), Some("someone else"));
}

#[test]
fn lock_and_unlock_ours() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feat");
    git::lock_worktree(&fx.repo, &wt).unwrap();
    let list = git::worktrees(&fx.repo).unwrap();
    let w = list.iter().find(|w| !w.is_main).unwrap();
    assert_eq!(w.locked.as_deref(), Some("baled by bale"));
    assert!(git::unlock_worktree_if_ours(&fx.repo, &wt).unwrap());
    let list = git::worktrees(&fx.repo).unwrap();
    assert_eq!(list.iter().find(|w| !w.is_main).unwrap().locked, None);
}

#[test]
fn prunable_is_reported() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feat");
    std::fs::remove_dir_all(&wt).unwrap();
    let list = git::worktrees(&fx.repo).unwrap();
    assert!(list.iter().find(|w| !w.is_main).unwrap().prunable);
}

#[test]
fn dirty_includes_untracked() {
    let fx = Fixture::new();
    assert!(!git::is_dirty(&fx.repo).unwrap());
    std::fs::write(fx.repo.join("new"), "x").unwrap();
    assert!(git::is_dirty(&fx.repo).unwrap());
}

#[test]
fn common_and_admin_dirs() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feat");
    let common = git::common_dir(&wt).unwrap();
    assert_eq!(canon(&common), canon(&fx.repo.join(".git")));
    let admin = git::admin_dir(&wt).unwrap();
    assert_ne!(canon(&admin), canon(&common));
    assert!(admin.starts_with(&common) || canon(&admin).starts_with(canon(&common)));
    assert_eq!(
        canon(&git::admin_dir(&fx.repo).unwrap()),
        canon(&fx.repo.join(".git"))
    );
}

#[test]
fn retry_on_index_lock() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feat");
    let admin = git::admin_dir(&wt).unwrap();
    let lock = admin.join("index.lock");
    std::fs::write(&lock, "").unwrap();
    std::fs::write(wt.join("f"), "two\n").unwrap();
    let l2 = lock.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::remove_file(l2).unwrap();
    });
    Git::at(&wt).run(&["add", "f"]).unwrap();
    h.join().unwrap();
}

#[test]
fn other_errors_return_immediately_with_stderr() {
    let fx = Fixture::new();
    let start = std::time::Instant::now();
    let err = Git::at(&fx.repo)
        .run(&["rev-parse", "nope-ref"])
        .unwrap_err();
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unknown revision") || msg.contains("ambiguous argument"),
        "{msg}"
    );
}

#[test]
fn home_is_under_fixture() {
    let fx = Fixture::new();
    assert!(!fx.home().0.starts_with(&fx.repo));
}

#[test]
fn bare_repo_is_listed_as_bare() {
    let fx = Fixture::new();
    let bare = fx.repo.parent().unwrap().join("bare.git");
    sh(
        &fx.repo,
        &[
            "clone",
            "--bare",
            fx.repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let wt = fx.repo.parent().unwrap().join("from-bare");
    sh(&bare, &["worktree", "add", wt.to_str().unwrap(), "main"]);
    let list = git::worktrees(&bare).unwrap();
    assert!(list[0].bare && list[0].is_main, "{list:?}");
    assert!(!list[1].bare, "{list:?}");
    assert!(!git::worktrees(&fx.repo).unwrap().iter().any(|w| w.bare));
}

#[test]
fn is_dirty_does_not_need_index_lock() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feat");
    // Make the index stat-dirty so a plain `git status` would want to refresh it.
    std::fs::write(wt.join("f"), "one\n").unwrap();
    std::fs::write(git::admin_dir(&wt).unwrap().join("index.lock"), "").unwrap();
    let start = std::time::Instant::now();
    assert!(!git::is_dirty(&wt).unwrap());
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}
