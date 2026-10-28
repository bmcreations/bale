mod support;

use bale_core::git::worktrees;
use bale_core::ops::cold::cold;
use bale_core::ops::open::open;
use bale_core::ops::up::{register, resolve, up, Outcome};
use bale_core::registry::{Level, Registry};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use support::{git, Fixture};

/// The same tree as cold.rs: a Gradle worktree `wt` on `feature` with a
/// modified tracked `f`, untracked `b.txt`, ignored `.env`, ignored `build/`
/// and ignored `node_modules/`, plus (unix) an ignored `.env.local` symlink.
fn dirty_worktree(fx: &Fixture) -> PathBuf {
    commit_ignore(fx);
    let wt = fx.add_worktree("wt", "feature");
    dirty(&wt);
    wt
}

fn commit_ignore(fx: &Fixture) {
    fs::write(
        fx.repo.join(".gitignore"),
        ".env\n.env.local\nbuild/\nnode_modules/\n",
    )
    .unwrap();
    fs::write(fx.repo.join("settings.gradle.kts"), "").unwrap();
    git(&fx.repo, &["add", ".gitignore", "settings.gradle.kts"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
}

fn dirty(wt: &Path) {
    fs::write(wt.join("f"), "modified\n").unwrap();
    fs::write(wt.join("b.txt"), "untracked\n").unwrap();
    fs::write(wt.join(".env"), "SECRET=1\n").unwrap();
    fs::create_dir_all(wt.join("build")).unwrap();
    fs::write(wt.join("build/out.bin"), "regen").unwrap();
    fs::create_dir_all(wt.join("node_modules/x")).unwrap();
    fs::write(wt.join("node_modules/x/i.js"), "js").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../main/.env.local", wt.join(".env.local")).unwrap();
}

fn status(wt: &Path) -> String {
    git(wt, &["status", "--porcelain", "--untracked-files=all"])
}

fn level(fx: &Fixture, id: &str) -> Level {
    Registry::open(fx.home()).get(id).unwrap().unwrap().level
}

fn set_level(fx: &Fixture, id: &str, level: Level) {
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(id).unwrap().unwrap();
    rec.level = level;
    reg.put(&rec).unwrap();
}

fn linked(fx: &Fixture) -> bale_core::git::WorktreeInfo {
    worktrees(&fx.repo)
        .unwrap()
        .into_iter()
        .find(|w| !w.is_main)
        .unwrap()
}

fn rev(dir: &Path, rev: &str) -> String {
    git(dir, &["rev-parse", rev]).trim().to_string()
}

fn bale_refs(fx: &Fixture, id: &str) -> String {
    git(
        &fx.repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/bale/{id}/"),
        ],
    )
}

/// Exit status of `git symbolic-ref -q HEAD` in `dir`: true when HEAD is on a branch.
fn on_branch(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Registers and colds `wt`, returning the id.
fn go_cold(fx: &Fixture, wt: &Path) -> String {
    let id = register(&fx.home(), wt).unwrap().id;
    assert_eq!(cold(&fx.home(), &id).unwrap(), Outcome::Done(Level::Cold));
    assert!(!wt.exists());
    id
}

fn assert_restored(fx: &Fixture, wt: &Path, id: &str, before: &str) {
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "modified\n");
    assert_eq!(fs::read_to_string(wt.join("b.txt")).unwrap(), "untracked\n");
    assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(wt.join(".env")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let link = wt.join(".env.local");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(&link).unwrap(),
            PathBuf::from("../main/.env.local")
        );
    }
    assert_eq!(status(wt), before);
    assert_eq!(linked(fx).locked, None);
    assert_eq!(bale_refs(fx, id), "");
    assert!(!fx.home().saved_dir(id).exists());
    assert_eq!(level(fx, id), Level::Hot);
}

#[test]
fn cold_then_open_roundtrip() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let before = status(&wt);
    let head = rev(&wt, "HEAD");
    let id = go_cold(&fx, &wt);

    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert_eq!(report.detached_reason, None);
    assert!(
        report.notes.iter().any(|n| n.contains("unstaged")),
        "{:?}",
        report.notes
    );
    assert_restored(&fx, &wt, &id, &before);
    assert_eq!(on_branch(&wt).as_deref(), Some("refs/heads/feature"));
    assert_eq!(rev(&wt, "HEAD"), head);
    assert_eq!(rev(&fx.repo, "feature"), head);
    let rec = Registry::open(fx.home()).get(&id).unwrap().unwrap();
    assert_eq!(rec.branch.as_deref(), Some("feature"));
}

#[test]
fn branch_moved_stays_detached() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let before = status(&wt);
    let head = rev(&wt, "HEAD");
    let id = go_cold(&fx, &wt);

    // The branch is checked out in the (locked) worktree, so move it by ref.
    let tree = rev(&fx.repo, "feature^{tree}");
    let moved = git(
        &fx.repo,
        &["commit-tree", &tree, "-p", "feature", "-m", "elsewhere"],
    );
    let moved = moved.trim();
    git(&fx.repo, &["update-ref", "refs/heads/feature", moved]);

    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    let reason = report.detached_reason.expect("detached");
    assert!(reason.contains("moved"), "{reason}");
    assert_eq!(on_branch(&wt), None);
    assert_eq!(rev(&wt, "HEAD"), head);
    assert_eq!(rev(&fx.repo, "refs/heads/feature"), moved);
    assert_restored(&fx, &wt, &id, &before);
}

#[test]
fn branch_checked_out_elsewhere_stays_detached() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    git(&fx.repo, &["symbolic-ref", "HEAD", "refs/heads/feature"]);

    let report = open(&fx.home(), &id).unwrap();
    let reason = report.detached_reason.expect("detached");
    assert!(reason.contains("checked out"), "{reason}");
    assert_eq!(on_branch(&wt), None);
}

#[test]
fn detached_worktree_reopens_detached() {
    let fx = Fixture::new();
    commit_ignore(&fx);
    let wt = fx.repo.parent().unwrap().join("wt");
    git(
        &fx.repo,
        &["worktree", "add", "--detach", wt.to_str().unwrap()],
    );
    dirty(&wt);
    let before = status(&wt);
    let head = rev(&wt, "HEAD");
    let id = go_cold(&fx, &wt);

    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert_eq!(report.detached_reason, None);
    assert_eq!(on_branch(&wt), None);
    assert_eq!(rev(&wt, "HEAD"), head);
    assert_restored(&fx, &wt, &id, &before);
}

#[test]
fn tampered_saved_file_fails_loudly() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    let saved = fx.home().saved_dir(&id).join("files/.env");
    fs::write(&saved, "SECRET=2\n").unwrap();

    let err = open(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains(".env"), "{err:#}");
    assert!(!wt.exists());
    assert_eq!(level(&fx, &id), Level::Cold);
    assert!(bale_refs(&fx, &id).contains("/wip"));
    assert!(fx.home().saved_dir(&id).join("manifest.json").exists());
}

#[test]
fn hot_open_is_a_no_op() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;
    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert_eq!(level(&fx, &id), Level::Hot);
    assert!(wt.join("f").exists());
}

#[test]
fn baled_opens_hot_and_unlocks() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    assert_eq!(up(&fx.home(), &id).unwrap(), Outcome::Done(Level::Baled));
    assert_eq!(linked(&fx).locked.as_deref(), Some("baled by bale"));

    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert_eq!(level(&fx, &id), Level::Hot);
    assert_eq!(linked(&fx).locked, None);
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "modified\n");
}

#[test]
fn baled_keeps_a_foreign_lock() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &["worktree", "lock", "--reason", "mine", wt.to_str().unwrap()],
    );
    set_level(&fx, &id, Level::Baled);
    open(&fx.home(), &id).unwrap();
    assert_eq!(linked(&fx).locked.as_deref(), Some("mine"));
    assert_eq!(level(&fx, &id), Level::Hot);
}

#[test]
fn cooling_and_baling_are_refused_untouched() {
    for (lvl, hint) in [(Level::Cooling, "bale cold"), (Level::Baling, "bale up")] {
        let fx = Fixture::new();
        let wt = dirty_worktree(&fx);
        let before = status(&wt);
        let id = register(&fx.home(), &wt).unwrap().id;
        set_level(&fx, &id, lvl);
        let err = open(&fx.home(), &id).unwrap_err();
        assert!(format!("{err:#}").contains(hint), "{err:#}");
        assert_eq!(level(&fx, &id), lvl);
        assert_eq!(status(&wt), before);
        assert_eq!(on_branch(&wt).as_deref(), Some("refs/heads/feature"));
    }
}

#[test]
fn non_empty_path_is_refused() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join("mine.txt"), "keep\n").unwrap();

    let err = open(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains("exists"), "{err:#}");
    assert_eq!(fs::read_to_string(wt.join("mine.txt")).unwrap(), "keep\n");
    assert!(!wt.join(".git").exists());
    assert_eq!(level(&fx, &id), Level::Cold);
    assert!(bale_refs(&fx, &id).contains("/wip"));
}

#[test]
fn empty_existing_dir_is_fine() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let before = status(&wt);
    let id = go_cold(&fx, &wt);
    fs::create_dir_all(&wt).unwrap();
    open(&fx.home(), &id).unwrap();
    assert_restored(&fx, &wt, &id, &before);
}

#[test]
fn missing_wip_ref_is_refused_before_creating_anything() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    git(
        &fx.repo,
        &["update-ref", "-d", &format!("refs/bale/{id}/wip")],
    );
    let err = open(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains("wip"), "{err:#}");
    assert!(!wt.exists());
    assert_eq!(level(&fx, &id), Level::Cold);
}

#[test]
fn missing_manifest_is_refused_before_creating_anything() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    fs::remove_file(fx.home().saved_dir(&id).join("manifest.json")).unwrap();
    let err = open(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains("manifest"), "{err:#}");
    assert!(!wt.exists());
    assert_eq!(level(&fx, &id), Level::Cold);
    assert!(bale_refs(&fx, &id).contains("/wip"));
}

#[test]
fn foreign_admin_dir_is_refused() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    let admin = Registry::open(fx.home())
        .get(&id)
        .unwrap()
        .unwrap()
        .admin_dir;
    let other = fx.repo.parent().unwrap().join("elsewhere/.git");
    fs::write(admin.join("gitdir"), format!("{}\n", other.display())).unwrap();
    let err = open(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains("gitdir"), "{err:#}");
    assert!(!wt.exists());
    assert_eq!(level(&fx, &id), Level::Cold);
}

/// Colds `wt`, then leaves the state an interrupted `open` would: the dir,
/// its `.git`, HEAD detached, the tracked tree and `.env` written; `b.txt`
/// not yet.
fn partial_open(fx: &Fixture, wt: &Path) -> String {
    let id = go_cold(fx, wt);
    let admin = Registry::open(fx.home())
        .get(&id)
        .unwrap()
        .unwrap()
        .admin_dir;
    fs::create_dir_all(wt).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    // Open detaches HEAD before anything else.
    let head = rev(wt, &format!("refs/bale/{id}/head"));
    git(wt, &["update-ref", "--no-deref", "HEAD", &head]);
    fs::write(wt.join("f"), "modified\n").unwrap();
    fs::copy(fx.home().saved_dir(&id).join("files/.env"), wt.join(".env")).unwrap();
    set_level(fx, &id, Level::Opening);
    id
}

#[test]
fn opening_resumes_a_partial_restore() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let before = status(&wt);
    let id = partial_open(&fx, &wt);

    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert_restored(&fx, &wt, &id, &before);
    assert_eq!(on_branch(&wt).as_deref(), Some("refs/heads/feature"));
}

#[test]
fn opening_resumes_when_nothing_was_created() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let before = status(&wt);
    let id = go_cold(&fx, &wt);
    set_level(&fx, &id, Level::Opening);
    open(&fx.home(), &id).unwrap();
    assert_restored(&fx, &wt, &id, &before);
}

#[test]
fn opening_refuses_foreign_content() {
    let cases: [(&str, &str, &str); 3] = [
        ("f", "someone else's\n", "f"),
        ("extra.txt", "new\n", "extra.txt"),
        (".env", "SECRET=9\n", ".env"),
    ];
    for (name, content, named) in cases {
        let fx = Fixture::new();
        let wt = dirty_worktree(&fx);
        let id = partial_open(&fx, &wt);
        fs::write(wt.join(name), content).unwrap();

        let err = open(&fx.home(), &id).unwrap_err();
        assert!(format!("{err:#}").contains(named), "{name}: {err:#}");
        assert_eq!(fs::read_to_string(wt.join(name)).unwrap(), content);
        assert_eq!(level(&fx, &id), Level::Opening);
        assert!(bale_refs(&fx, &id).contains("/wip"));
        assert!(fx.home().saved_dir(&id).join("manifest.json").exists());
    }
}

#[test]
fn opening_refuses_dir_without_our_git_file() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    set_level(&fx, &id, Level::Opening);
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join("f"), "modified\n").unwrap();

    let err = open(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains(".git"), "{err:#}");
    assert!(!wt.join(".git").exists());
    assert_eq!(level(&fx, &id), Level::Opening);
}

#[test]
fn up_and_cold_skip_an_opening_record() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;
    set_level(&fx, &id, Level::Opening);
    for out in [up(&fx.home(), &id).unwrap(), cold(&fx.home(), &id).unwrap()] {
        assert!(
            matches!(&out, Outcome::Skipped(r) if r.contains("bale open")),
            "{out:?}"
        );
    }
    assert!(wt.join("f").exists());
}

fn admin_of(fx: &Fixture, id: &str) -> PathBuf {
    Registry::open(fx.home())
        .get(id)
        .unwrap()
        .unwrap()
        .admin_dir
}

#[test]
fn resolve_by_path_keeps_a_nested_cold_record() {
    let fx = Fixture::new();
    commit_ignore(&fx);
    let wt = fx.repo.join(".worktrees/x");
    git(
        &fx.repo,
        &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
    );
    fs::write(wt.join("f"), "modified\n").unwrap();
    let before = status(&wt);
    let id = go_cold(&fx, &wt);
    let admin = admin_of(&fx, &id);
    // An empty dir at the path: git run there would climb to the main checkout.
    fs::create_dir_all(&wt).unwrap();

    assert_eq!(resolve(&fx.home(), wt.to_str().unwrap()).unwrap(), id);
    assert_eq!(admin_of(&fx, &id), admin);
    assert_eq!(level(&fx, &id), Level::Cold);
    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert_eq!(status(&wt), before);
    assert_eq!(on_branch(&wt).as_deref(), Some("refs/heads/feature"));
}

#[test]
fn resolve_by_path_on_partial_opening_keeps_branch() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let before = status(&wt);
    let id = partial_open(&fx, &wt);

    assert_eq!(resolve(&fx.home(), wt.to_str().unwrap()).unwrap(), id);
    let rec = Registry::open(fx.home()).get(&id).unwrap().unwrap();
    assert_eq!(rec.branch.as_deref(), Some("feature"));
    assert_eq!(rec.level, Level::Opening);
    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.detached_reason, None);
    assert_eq!(on_branch(&wt).as_deref(), Some("refs/heads/feature"));
    assert_restored(&fx, &wt, &id, &before);
}

#[test]
fn resolve_by_path_registers_a_new_worktree() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = resolve(&fx.home(), wt.to_str().unwrap()).unwrap();
    assert_eq!(level(&fx, &id), Level::Hot);
}

#[test]
fn staged_force_added_ignored_file_survives_two_round_trips() {
    // `node_modules` is ignored and never saved as an ignored file, so only
    // the index keeps this file across a cold.
    let fx = Fixture::new();
    fs::write(fx.repo.join(".gitignore"), "node_modules/\n").unwrap();
    git(&fx.repo, &["add", ".gitignore"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
    let wt = fx.add_worktree("wt", "feature");
    fs::create_dir_all(wt.join("node_modules")).unwrap();
    fs::write(wt.join("node_modules/keep.txt"), "keep\n").unwrap();
    git(&wt, &["add", "-f", "node_modules/keep.txt"]);
    let before = status(&wt);

    let id = go_cold(&fx, &wt);
    open(&fx.home(), &id).unwrap();
    assert_ne!(git(&wt, &["ls-files", "node_modules/keep.txt"]), "");
    assert_eq!(status(&wt), before);

    go_cold(&fx, &wt);
    open(&fx.home(), &id).unwrap();
    assert_eq!(
        fs::read_to_string(wt.join("node_modules/keep.txt")).unwrap(),
        "keep\n"
    );
}

#[test]
fn broken_admin_dir_never_reaches_the_main_checkout() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    let main_head = rev(&fx.repo, "HEAD");
    fs::remove_file(admin_of(&fx, &id).join("HEAD")).unwrap();

    open(&fx.home(), &id).unwrap_err();
    assert_eq!(on_branch(&fx.repo).as_deref(), Some("refs/heads/main"));
    assert_eq!(rev(&fx.repo, "HEAD"), main_head);
    assert_eq!(level(&fx, &id), Level::Opening);
    assert!(bale_refs(&fx, &id).contains("/wip"));
}

#[cfg(unix)]
#[test]
fn symlinked_path_is_refused() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = go_cold(&fx, &wt);
    let elsewhere = fx.repo.parent().unwrap().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &wt).unwrap();

    let err = open(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains("symlink"), "{err:#}");
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    assert_eq!(level(&fx, &id), Level::Cold);
}

#[test]
fn uninitialized_submodule_is_noted() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    git(
        &wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub.repo.to_str().unwrap(),
            "lib",
        ],
    );
    git(&wt, &["commit", "-m", "sub"]);
    fs::write(wt.join("f"), "modified\n").unwrap();
    let id = go_cold(&fx, &wt);

    let report = open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("git submodule update --init")),
        "{:?}",
        report.notes
    );
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "modified\n");
}
