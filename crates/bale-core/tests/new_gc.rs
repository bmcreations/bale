mod support;

use bale_core::git::{worktrees, LOCK_REASON};
use bale_core::lock::WorkspaceLock;
use bale_core::ops::cold::cold;
use bale_core::ops::gc::{gc, GcReport};
use bale_core::ops::new::new_workspace;
use bale_core::ops::up::{register, up, Outcome};
use bale_core::registry::{Level, Record, Registry};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use support::{git, Fixture};

fn registry(fx: &Fixture) -> Registry {
    Registry::open(fx.home())
}

fn set_level(fx: &Fixture, id: &str, level: Level) {
    let reg = registry(fx);
    let mut rec = reg.get(id).unwrap().unwrap();
    rec.level = level;
    reg.put(&rec).unwrap();
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

/// Leaves what an interrupted `bale open` step 7 leaves: a ref under
/// `refs/bale/<id>/` and a saved-state dir.
fn plant_saved_state(fx: &Fixture, id: &str) {
    git(
        &fx.repo,
        &["update-ref", &format!("refs/bale/{id}/head"), "HEAD"],
    );
    let saved = fx.home().saved_dir(id);
    fs::create_dir_all(saved.join("files")).unwrap();
    fs::write(saved.join("files").join("x"), "x").unwrap();
}

fn lock_reason(fx: &Fixture, path: &Path) -> Option<String> {
    let want = fs::canonicalize(path).unwrap();
    worktrees(&fx.repo)
        .unwrap()
        .into_iter()
        .find(|w| fs::canonicalize(&w.path).ok().as_ref() == Some(&want))
        .unwrap()
        .locked
}

fn head_ref(dir: &Path) -> String {
    git(dir, &["symbolic-ref", "HEAD"]).trim().to_string()
}

fn default_root(fx: &Fixture) -> PathBuf {
    fx.repo.parent().unwrap().join("my repo é-worktrees")
}

/// A registered worktree whose dir was deleted and pruned from git.
fn orphan(fx: &Fixture, name: &str) -> String {
    let wt = fx.add_worktree(name, name);
    let id = register(&fx.home(), &wt).unwrap().id;
    fs::remove_dir_all(&wt).unwrap();
    git(&fx.repo, &["worktree", "prune"]);
    id
}

#[test]
fn new_creates_and_registers() {
    let fx = Fixture::new();
    let rec = new_workspace(&fx.home(), &fx.repo, "x", None).unwrap();
    let want = fs::canonicalize(default_root(&fx).join("x")).unwrap();
    assert_eq!(rec.path, want);
    assert_eq!(head_ref(&want), "refs/heads/x");
    assert_eq!(rec.level, Level::Hot);
    assert_eq!(registry(&fx).get(&rec.id).unwrap(), Some(rec));
}

#[test]
fn new_existing_branch_checks_it_out() {
    let fx = Fixture::new();
    git(&fx.repo, &["branch", "feat"]);
    let rec = new_workspace(&fx.home(), &fx.repo, "w", Some("feat")).unwrap();
    assert_eq!(head_ref(&rec.path), "refs/heads/feat");
    assert_eq!(rec.branch.as_deref(), Some("feat"));
}

#[test]
fn new_with_branch_names_a_new_branch() {
    let fx = Fixture::new();
    let rec = new_workspace(&fx.home(), &fx.repo, "w", Some("topic/one")).unwrap();
    assert_eq!(head_ref(&rec.path), "refs/heads/topic/one");
}

#[test]
fn new_uses_configured_worktree_root() {
    let fx = Fixture::new();
    let root = fx.repo.parent().unwrap().join("roots");
    fs::create_dir_all(fx.home().0).unwrap();
    fs::write(
        fx.home().0.join("config.toml"),
        format!("worktree_root = '{}'\n", root.display()),
    )
    .unwrap();
    let rec = new_workspace(&fx.home(), &fx.repo, "x", None).unwrap();
    let want = fs::canonicalize(root.join("my repo é").join("x")).unwrap();
    assert_eq!(rec.path, want);
}

#[test]
fn new_rejects_bad_names() {
    let fx = Fixture::new();
    for name in ["", ".", "../x", "a/b", "a\\b", "..", "-f", "./x"] {
        // With a valid branch, so git's own branch-name check can't refuse it.
        let err = new_workspace(&fx.home(), &fx.repo, name, Some("foo"));
        assert!(err.is_err(), "{name:?} accepted");
    }
    assert!(!default_root(&fx).exists());
    assert!(!fx.repo.parent().unwrap().join("x").exists());
    assert!(registry(&fx).all().unwrap().is_empty());
}

#[test]
fn new_resolves_a_relative_worktree_root_against_the_main_checkout() {
    let fx = Fixture::new();
    fs::write(fx.repo.join(".bale.toml"), "worktree_root = \"../wt\"\n").unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert_ne!(
        fs::canonicalize(&cwd).unwrap(),
        fs::canonicalize(&fx.repo).unwrap()
    );
    let stray = cwd.join("../wt");
    let stray_before = stray.exists();
    let rec = new_workspace(&fx.home(), &fx.repo, "x", None).unwrap();
    let want = fs::canonicalize(fx.repo.join("../wt/my repo é/x")).unwrap();
    assert_eq!(rec.path, want);
    assert_eq!(head_ref(&want), "refs/heads/x");
    assert_eq!(stray.exists(), stray_before, "stray dir beside the cwd");
    assert!(!fx.repo.join("wt").exists());
}

#[test]
fn new_rejects_a_branch_starting_with_dash() {
    let fx = Fixture::new();
    assert!(new_workspace(&fx.home(), &fx.repo, "x", Some("-f")).is_err());
    assert!(!default_root(&fx).join("x").exists());
}

#[test]
fn new_refuses_an_existing_path() {
    let fx = Fixture::new();
    let path = default_root(&fx).join("x");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("mine"), "keep").unwrap();
    let err = new_workspace(&fx.home(), &fx.repo, "x", None).unwrap_err();
    assert!(format!("{err:#}").contains("exists"), "{err:#}");
    assert_eq!(fs::read_to_string(path.join("mine")).unwrap(), "keep");
    assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
    assert!(registry(&fx).all().unwrap().is_empty());
}

#[test]
fn new_refuses_the_path_of_a_registered_workspace() {
    let fx = Fixture::new();
    let rec = new_workspace(&fx.home(), &fx.repo, "x", None).unwrap();
    assert_eq!(
        cold(&fx.home(), &rec.id).unwrap(),
        Outcome::Done(Level::Cold)
    );
    assert!(!rec.path.exists());
    let err = new_workspace(&fx.home(), &fx.repo, "x", Some("other")).unwrap_err();
    assert!(format!("{err:#}").contains(&rec.id), "{err:#}");
    assert!(!rec.path.exists());
    assert_eq!(
        registry(&fx).get(&rec.id).unwrap().unwrap().level,
        Level::Cold
    );
}

#[test]
fn new_with_branch_checked_out_elsewhere_leaves_nothing() {
    let fx = Fixture::new();
    let err = new_workspace(&fx.home(), &fx.repo, "x", Some("main")).unwrap_err();
    assert!(format!("{err:#}").contains("main"), "{err:#}");
    assert!(!default_root(&fx).exists());
    assert!(registry(&fx).all().unwrap().is_empty());
    assert_eq!(worktrees(&fx.repo).unwrap().len(), 1);
}

#[test]
fn gc_keeps_cold_removes_orphans() {
    let fx = Fixture::new();
    let a = fx.add_worktree("a", "a");
    let a_id = register(&fx.home(), &a).unwrap().id;
    assert_eq!(cold(&fx.home(), &a_id).unwrap(), Outcome::Done(Level::Cold));
    assert!(!a.exists());

    let b_id = orphan(&fx, "b");
    plant_saved_state(&fx, &b_id);

    let report = gc(&fx.home(), false).unwrap();
    assert_eq!(report.removed, vec![b_id.clone()]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(registry(&fx).get(&b_id).unwrap().is_none());
    assert_eq!(bale_refs(&fx, &b_id), "");
    assert!(!fx.home().saved_dir(&b_id).exists());

    assert_eq!(
        registry(&fx).get(&a_id).unwrap().unwrap().level,
        Level::Cold
    );
    assert_ne!(bale_refs(&fx, &a_id), "");
    assert!(fx.home().saved_dir(&a_id).exists());
    assert_eq!(lock_reason_by_name(&fx, "a"), Some(LOCK_REASON.to_string()));
}

/// The lock reason of the linked worktree whose dir is named `name`.
fn lock_reason_by_name(fx: &Fixture, name: &str) -> Option<String> {
    worktrees(&fx.repo)
        .unwrap()
        .into_iter()
        .find(|w| w.path.file_name().is_some_and(|n| n == name))
        .unwrap()
        .locked
}

#[test]
fn gc_collects_only_hot_and_baled() {
    let fx = Fixture::new();
    let mut ids = Vec::new();
    for (name, level) in [
        ("hot", Level::Hot),
        ("baled", Level::Baled),
        ("baling", Level::Baling),
        ("cooling", Level::Cooling),
        ("cold", Level::Cold),
        ("opening", Level::Opening),
    ] {
        let id = orphan(&fx, name);
        set_level(&fx, &id, level);
        plant_saved_state(&fx, &id);
        ids.push((id, level));
    }
    let mut report = gc(&fx.home(), false).unwrap();
    report.removed.sort();
    let mut want: Vec<String> = ids
        .iter()
        .filter(|(_, l)| matches!(l, Level::Hot | Level::Baled))
        .map(|(id, _)| id.clone())
        .collect();
    want.sort();
    assert_eq!(report.removed, want);
    for (id, level) in &ids {
        if matches!(level, Level::Hot | Level::Baled) {
            continue;
        }
        assert_eq!(registry(&fx).get(id).unwrap().unwrap().level, *level);
        assert_ne!(bale_refs(&fx, id), "", "{level:?} refs deleted");
        assert!(fx.home().saved_dir(id).exists(), "{level:?} saved deleted");
    }
}

#[test]
fn gc_keeps_a_gone_dir_git_still_lists() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    fs::remove_dir_all(&wt).unwrap();
    let report = gc(&fx.home(), false).unwrap();
    assert_eq!(report, GcReport::default());
    assert!(registry(&fx).get(&id).unwrap().is_some());
}

#[test]
fn gc_skips_a_workspace_another_process_holds() {
    let fx = Fixture::new();
    let id = orphan(&fx, "wt");
    plant_saved_state(&fx, &id);
    let held = WorkspaceLock::try_acquire(&fx.home(), &id)
        .unwrap()
        .unwrap();
    let report = gc(&fx.home(), false).unwrap();
    assert_eq!(report, GcReport::default());
    assert!(registry(&fx).get(&id).unwrap().is_some());
    assert_ne!(bale_refs(&fx, &id), "");
    drop(held);
    assert_eq!(gc(&fx.home(), false).unwrap().removed, vec![id]);
}

#[test]
fn gc_never_touches_a_user_lock() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &["worktree", "lock", "--reason", "mine", wt.to_str().unwrap()],
    );
    let report = gc(&fx.home(), false).unwrap();
    assert!(report.unlocked.is_empty(), "{report:?}");
    assert_eq!(lock_reason(&fx, &wt), Some("mine".to_string()));

    // Gone but locked: git keeps listing it, so the record stays too.
    fs::remove_dir_all(&wt).unwrap();
    git(&fx.repo, &["worktree", "prune"]);
    let report = gc(&fx.home(), false).unwrap();
    assert_eq!(report, GcReport::default());
    assert!(registry(&fx).get(&id).unwrap().is_some());
    assert_eq!(lock_reason_by_name(&fx, "wt"), Some("mine".to_string()));
}

#[test]
fn gc_releases_bales_stale_lock_on_a_hot_workspace() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            LOCK_REASON,
            wt.to_str().unwrap(),
        ],
    );
    plant_saved_state(&fx, &id);
    let report = gc(&fx.home(), false).unwrap();
    assert_eq!(report.unlocked, vec![id.clone()]);
    assert_eq!(report.cleaned, vec![id.clone()]);
    assert!(report.removed.is_empty() && report.errors.is_empty());
    assert_eq!(lock_reason(&fx, &wt), None);
    assert_eq!(bale_refs(&fx, &id), "");
    assert!(!fx.home().saved_dir(&id).exists());
    assert_eq!(registry(&fx).get(&id).unwrap().unwrap().level, Level::Hot);

    // Nothing left: a second run reports nothing.
    assert_eq!(gc(&fx.home(), false).unwrap(), GcReport::default());
}

#[test]
fn gc_leaves_bales_lock_on_a_baled_workspace() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            LOCK_REASON,
            wt.to_str().unwrap(),
        ],
    );
    set_level(&fx, &id, Level::Baled);
    assert_eq!(gc(&fx.home(), false).unwrap(), GcReport::default());
    assert_eq!(lock_reason(&fx, &wt), Some(LOCK_REASON.to_string()));
}

fn lock_files(fx: &Fixture) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match fs::read_dir(fx.home().locks_dir()) {
        Ok(rd) => rd.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

#[test]
fn gc_dry_run_creates_no_lock_files() {
    let fx = Fixture::new();
    let id = orphan(&fx, "wt");
    fs::remove_dir_all(fx.home().locks_dir()).unwrap();
    let report = gc(&fx.home(), true).unwrap();
    assert_eq!(report.removed, vec![id]);
    assert!(!fx.home().locks_dir().exists());
}

#[test]
fn gc_dry_run_skips_a_workspace_another_process_holds() {
    let fx = Fixture::new();
    let id = orphan(&fx, "wt");
    let _held = WorkspaceLock::try_acquire(&fx.home(), &id)
        .unwrap()
        .unwrap();
    assert_eq!(gc(&fx.home(), true).unwrap(), GcReport::default());
}

#[test]
fn gc_dry_run_changes_nothing() {
    let fx = Fixture::new();
    let gone = orphan(&fx, "gone");
    plant_saved_state(&fx, &gone);
    let wt = fx.add_worktree("wt", "wt");
    let stale = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            LOCK_REASON,
            wt.to_str().unwrap(),
        ],
    );
    plant_saved_state(&fx, &stale);

    let locks_before = lock_files(&fx);
    let report = gc(&fx.home(), true).unwrap();
    assert_eq!(lock_files(&fx), locks_before, "dry run created lock files");
    assert_eq!(report.removed, vec![gone.clone()]);
    assert_eq!(report.unlocked, vec![stale.clone()]);
    assert_eq!(report.cleaned, vec![stale.clone()]);
    assert!(report.errors.is_empty());

    for id in [&gone, &stale] {
        assert!(registry(&fx).get(id).unwrap().is_some());
        assert_ne!(bale_refs(&fx, id), "");
        assert!(fx.home().saved_dir(id).join("files/x").exists());
    }
    assert_eq!(lock_reason(&fx, &wt), Some(LOCK_REASON.to_string()));
}

#[test]
fn gc_keeps_the_record_when_the_repo_is_gone() {
    let fx = Fixture::new();
    let id = orphan(&fx, "wt");
    fs::remove_dir_all(&fx.repo).unwrap();
    let report = gc(&fx.home(), false).unwrap();
    assert!(report.removed.is_empty());
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(report.errors[0].0, id);
    assert!(registry(&fx).get(&id).unwrap().is_some());
}

#[test]
fn gc_does_not_follow_git_into_a_parent_repo() {
    let fx = Fixture::new();
    let id = orphan(&fx, "wt");
    // A repo above the fixture, and an empty dir where the common dir was:
    // git run there would climb to the parent and list no such worktree.
    let parent = fx.repo.parent().unwrap();
    git(parent, &["init", "-b", "main"]);
    fs::remove_dir_all(fx.repo.join(".git")).unwrap();
    fs::create_dir(fx.repo.join(".git")).unwrap();
    let report = gc(&fx.home(), false).unwrap();
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert!(registry(&fx).get(&id).unwrap().is_some());
}

#[test]
fn gc_rejects_a_record_with_an_invalid_id() {
    let fx = Fixture::new();
    let id = orphan(&fx, "wt");
    let mut rec: Record = registry(&fx).get(&id).unwrap().unwrap();
    // An id naming bale's own registry dir must never reach saved_dir().
    rec.id = "registry".into();
    rec.last_activity = SystemTime::now();
    registry(&fx).put(&rec).unwrap();
    let report = gc(&fx.home(), false).unwrap();
    assert_eq!(report.removed, vec![id.clone()]);
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(report.errors[0].0, "registry");
    assert!(registry(&fx).get("registry").unwrap().is_some());
}

#[test]
fn gc_reports_a_corrupt_record_and_continues() {
    let fx = Fixture::new();
    let id = orphan(&fx, "wt");
    fs::write(fx.home().records_dir().join("bad.json"), "{").unwrap();
    let report = gc(&fx.home(), false).unwrap();
    assert_eq!(report.removed, vec![id]);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].0, "bad");
}

#[test]
fn gc_collects_a_baled_tree_the_user_removed_with_git() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    assert_eq!(up(&fx.home(), &id).unwrap(), Outcome::Done(Level::Baled));
    plant_saved_state(&fx, &id);
    // Unlocked at Baled, so a plain `git worktree remove` works.
    git(&fx.repo, &["worktree", "remove", wt.to_str().unwrap()]);

    let report = gc(&fx.home(), false).unwrap();
    assert_eq!(report.removed, vec![id.clone()], "{report:?}");
    assert!(registry(&fx).get(&id).unwrap().is_none());
    assert_eq!(bale_refs(&fx, &id), "");
    assert!(!fx.home().saved_dir(&id).exists());
}

/// Earlier builds locked a tree at `up`: gone but still bale-locked, git
/// keeps listing it, so gc keeps the record (as it always has).
#[test]
fn gc_keeps_a_gone_baled_tree_with_bales_old_lock() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    assert_eq!(up(&fx.home(), &id).unwrap(), Outcome::Done(Level::Baled));
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            LOCK_REASON,
            wt.to_str().unwrap(),
        ],
    );
    fs::remove_dir_all(&wt).unwrap();
    git(&fx.repo, &["worktree", "prune"]);
    assert_eq!(gc(&fx.home(), false).unwrap(), GcReport::default());
    assert!(registry(&fx).get(&id).unwrap().is_some());
}

/// Plants a check dir beside `wt` the way a crashed cold check leaves one:
/// a copy of the tree, with a read-only dir inside.
fn plant_check_dir(wt: &Path, name: &str) -> PathBuf {
    let dir = wt.parent().unwrap().join(name);
    fs::create_dir_all(dir.join("tree/ro")).unwrap();
    fs::write(dir.join("tree/ro/.env"), "SECRET=1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join("tree/ro"), fs::Permissions::from_mode(0o500)).unwrap();
    }
    dir
}

/// A check dir a crashed cold left holds copies of the tree's secrets; gc
/// removes this workspace's, and leaves other workspaces' and lookalikes.
#[test]
fn gc_removes_stale_check_dirs_of_the_workspace() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;
    let ours = plant_check_dir(&wt, &format!(".bale-check-{id}-a1B2c3"));
    let theirs = plant_check_dir(&wt, ".bale-check-0123456789ab-a1B2c3");
    let lookalike = plant_check_dir(&wt, &format!(".bale-check-{id}-notes.d"));

    // Reported beside the recorded (canonical) path.
    let want = vec![(id.clone(), fs::canonicalize(&ours).unwrap())];
    let dry = gc(&fx.home(), true).unwrap();
    assert!(ours.exists(), "a dry run removed it");
    assert_eq!(dry.checks, want);
    let real = gc(&fx.home(), false).unwrap();
    assert!(!ours.exists());
    assert_eq!(real.checks, want);
    assert!(theirs.join("tree/ro/.env").exists());
    assert!(lookalike.join("tree/ro/.env").exists());
}

/// A Hot record whose path now holds a folder that is not its checkout
/// (git in it climbs into the main repo) keeps its saved state: the work
/// open restored there is gone, and the save may be the only copy.
#[test]
fn gc_keeps_saved_state_when_the_path_is_not_the_checkout() {
    let fx = Fixture::new();
    let wt = fx.repo.join(".claude/worktrees/w");
    git(
        &fx.repo,
        &["worktree", "add", "-b", "w", wt.to_str().unwrap()],
    );
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            LOCK_REASON,
            wt.to_str().unwrap(),
        ],
    );
    plant_saved_state(&fx, &id);
    fs::remove_dir_all(&wt).unwrap();
    fs::create_dir_all(wt.join(".idea")).unwrap();
    fs::write(wt.join(".idea/workspace.xml"), "<x/>\n").unwrap();

    let report = gc(&fx.home(), false).unwrap();
    assert!(report.cleaned.is_empty(), "{report:?}");
    assert!(report.unlocked.is_empty(), "{report:?}");
    assert!(report.removed.is_empty(), "{report:?}");
    assert_ne!(bale_refs(&fx, &id), "");
    assert!(fx.home().saved_dir(&id).join("files/x").exists());
    assert_eq!(lock_reason(&fx, &wt), Some(LOCK_REASON.to_string()));
    assert!(registry(&fx).get(&id).unwrap().is_some());
}
