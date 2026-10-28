//! Own test binary: mutates process env, which would race with other tests.
mod support;

use bale_core::git::Git;
use support::Fixture;

#[test]
fn inherited_git_env_is_ignored() {
    let a = Fixture::new();
    let b = Fixture::new();
    std::fs::write(b.repo.join("only-in-b"), "x").unwrap();
    std::env::set_var("GIT_DIR", b.repo.join(".git"));
    std::env::set_var("GIT_WORK_TREE", &b.repo);
    std::env::set_var("GIT_INDEX_FILE", b.repo.join(".git").join("index"));
    let out = Git::at(&a.repo).run(&["status", "--porcelain"]).unwrap();
    std::env::remove_var("GIT_DIR");
    std::env::remove_var("GIT_WORK_TREE");
    std::env::remove_var("GIT_INDEX_FILE");
    assert!(out.trim().is_empty(), "targeted wrong repo: {out}");
}
