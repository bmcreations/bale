mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bale_core::git::{latest_subject, scope};
use bale_core::ops::cold::cold;
use bale_core::ops::meta::{set_origin, set_pinned};
use bale_core::ops::sweep::{sweep, Action};
use bale_core::ops::up::{register, Outcome};
use bale_core::registry::{Level, Origin, Record, Registry, Scope};
use bale_core::BaleHome;
use support::{git, Fixture};

fn record(fx: &Fixture, id: &str) -> Record {
    Registry::open(fx.home()).get(id).unwrap().unwrap()
}

/// Marks `main` as pushed (`refs/remotes/origin/main`) and adds worktree
/// `wt` on `feature` with two commits no remote ref has.
fn two_unpushed(fx: &Fixture) -> PathBuf {
    git(
        &fx.repo,
        &["update-ref", "refs/remotes/origin/main", "main"],
    );
    let wt = fx.add_worktree("wt", "feature");
    for (file, subject) in [("a", "first change"), ("b", "second change")] {
        fs::write(wt.join(file), "x\n").unwrap();
        git(&wt, &["add", file]);
        git(&wt, &["commit", "-m", subject]);
    }
    wt
}

#[test]
fn old_record_without_new_fields_loads() {
    let text = r#"{
  "id": "abcdefabcdef",
  "repo_common_dir": "/repo/.git",
  "path": "/repo-wt",
  "admin_dir": "/repo/.git/worktrees/wt",
  "branch": "feat/x",
  "head": "abc123",
  "level": "baled",
  "last_activity": { "secs_since_epoch": 1700000000, "nanos_since_epoch": 0 },
  "note": null
}"#;
    let r: Record = serde_json::from_str(text).unwrap();
    assert_eq!(r.origin, None);
    assert!(!r.pinned);
    assert_eq!(r.scope, None);
    // Unset fields stay out of the file, so older bale versions read it too.
    let out = serde_json::to_string(&r).unwrap();
    for key in ["origin", "pinned", "scope"] {
        assert!(!out.contains(key), "{out}");
    }
}

#[test]
fn new_fields_roundtrip() {
    let t = tempfile::tempdir().unwrap();
    let reg = Registry::open(BaleHome(t.path().to_path_buf()));
    let r = Record {
        id: "abcdefabcdef".into(),
        repo_common_dir: PathBuf::from("/repo/.git"),
        path: PathBuf::from("/repo-wt"),
        admin_dir: PathBuf::from("/repo/.git/worktrees/wt"),
        branch: None,
        head: "abc".into(),
        level: Level::Cold,
        last_activity: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        note: None,
        trash: Vec::new(),
        origin: Some(Origin {
            tool: "some-tool".into(),
            session: "s1".into(),
            title: Some("A title".into()),
        }),
        pinned: true,
        scope: Some(Scope {
            branch: Some("b".into()),
            unpushed: 3,
            subjects: vec!["s".into()],
            ..Scope::default()
        }),
        stash: None,
        restore_hook: false,
        spm: Vec::new(),
    };
    reg.put(&r).unwrap();
    assert_eq!(reg.get(&r.id).unwrap(), Some(r));
}

#[test]
fn scope_without_upstream_counts_unpushed_and_dirty_stats() {
    let fx = Fixture::new();
    let wt = two_unpushed(&fx);
    fs::write(wt.join("f"), "two\n").unwrap();
    fs::write(wt.join("new.txt"), "untracked\n").unwrap();

    let s = scope(&wt).unwrap();
    assert_eq!(s.branch.as_deref(), Some("feature"));
    assert_eq!(s.upstream, None);
    assert_eq!((s.ahead, s.behind), (0, 0));
    assert_eq!(s.unpushed, 2);
    assert_eq!(s.subjects, vec!["second change", "first change"]);
    // f modified (1 insertion, 1 deletion) plus one untracked file.
    assert_eq!(s.changed_files, 2);
    assert_eq!((s.insertions, s.deletions), (1, 1));
    let head_time: i64 = git(&wt, &["log", "-1", "--format=%ct"])
        .trim()
        .parse()
        .unwrap();
    assert_eq!(s.last_commit_unix, head_time);
}

#[test]
fn scope_with_upstream_counts_ahead_and_behind() {
    let fx = Fixture::new();
    let wt = two_unpushed(&fx);
    git(
        &fx.repo,
        &["remote", "add", "origin", "/nonexistent/bale-test"],
    );
    git(&wt, &["config", "branch.feature.remote", "origin"]);
    git(&wt, &["config", "branch.feature.merge", "refs/heads/main"]);
    // origin/main moves one commit past where feature branched.
    fs::write(fx.repo.join("g"), "g\n").unwrap();
    git(&fx.repo, &["add", "g"]);
    git(&fx.repo, &["commit", "-m", "upstream work"]);
    git(
        &fx.repo,
        &["update-ref", "refs/remotes/origin/main", "main"],
    );

    let s = scope(&wt).unwrap();
    assert_eq!(s.upstream.as_deref(), Some("origin/main"));
    assert_eq!((s.ahead, s.behind), (2, 1));
    assert_eq!(s.unpushed, 2);
    assert_eq!(s.subjects, vec!["second change", "first change"]);
    assert_eq!((s.changed_files, s.insertions, s.deletions), (0, 0, 0));
}

#[test]
fn scope_keeps_eight_newest_subjects() {
    let fx = Fixture::new();
    git(
        &fx.repo,
        &["update-ref", "refs/remotes/origin/main", "main"],
    );
    let wt = fx.add_worktree("wt", "feature");
    for i in 1..=10 {
        git(
            &wt,
            &["commit", "-q", "--allow-empty", "-m", &format!("c{i}")],
        );
    }
    let s = scope(&wt).unwrap();
    assert_eq!(s.unpushed, 10);
    let want: Vec<String> = (3..=10).rev().map(|i| format!("c{i}")).collect();
    assert_eq!(s.subjects, want);
}

#[test]
fn set_pinned_and_origin_by_path_and_id() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let home = fx.home();
    // A path registers the tree, as adopt does.
    set_pinned(&home, wt.to_str().unwrap(), true).unwrap();
    let id = register(&home, &wt).unwrap().id;
    assert!(record(&fx, &id).pinned);
    set_pinned(&home, &id, false).unwrap();
    assert!(!record(&fx, &id).pinned);

    let origin = Origin {
        tool: "some-tool".into(),
        session: "abc123".into(),
        title: None,
    };
    set_origin(&home, &id, origin.clone()).unwrap();
    assert_eq!(record(&fx, &id).origin, Some(origin));
}

/// A Gradle worktree with build output `up` would remove.
fn idle_gradle_tree(fx: &Fixture) -> PathBuf {
    fs::write(
        fx.repo.join(".gitignore"),
        "app/build/\nsettings.gradle.kts\n",
    )
    .unwrap();
    git(&fx.repo, &["add", ".gitignore"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
    fs::create_dir_all(&fx.home().0).unwrap();
    fs::write(
        fx.home().0.join("config.toml"),
        "idle_baled_hours = 0\nidle_cold_days = 0\n",
    )
    .unwrap();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    fs::create_dir_all(wt.join("app/build")).unwrap();
    fs::write(wt.join("app/build/out.bin"), "regen").unwrap();
    wt
}

#[test]
fn sweep_keeps_a_pinned_idle_tree() {
    let fx = Fixture::new();
    let wt = idle_gradle_tree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    set_pinned(&fx.home(), &id, true).unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    let row = rows.iter().find(|r| r.0 == id).unwrap();
    assert_eq!(row.1, Action::Keep, "{rows:?}");
    assert_eq!(row.2, "pinned");
    assert_eq!(record(&fx, &id).level, Level::Hot);
    assert!(wt.join("app/build/out.bin").exists());

    // Unpinned, the same tree is swept.
    set_pinned(&fx.home(), &id, false).unwrap();
    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(rows.iter().find(|r| r.0 == id).unwrap().1, Action::Cold);
}

/// A pinned tree stranded mid-up or mid-cold is still left alone, but its
/// row says so and names the command that finishes it.
#[test]
fn sweep_reports_a_pinned_tree_stranded_mid_transition() {
    let fx = Fixture::new();
    let wt = idle_gradle_tree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    set_pinned(&fx.home(), &id, true).unwrap();
    for (level, doing, cmd) in [
        (Level::Baling, "baling", "up"),
        (Level::Cooling, "going cold", "cold"),
    ] {
        let reg = Registry::open(fx.home());
        let mut r = record(&fx, &id);
        r.level = level;
        reg.put(&r).unwrap();

        for dry_run in [true, false] {
            let rows = sweep(&fx.home(), dry_run).unwrap();
            let row = rows.iter().find(|r| r.0 == id).unwrap();
            let want = format!(
                "interrupted while {doing}; pinned, so sweep leaves it: run `bale {cmd} {id}`"
            );
            assert_eq!(row.1, Action::Report(want.clone()), "{rows:?}");
            assert_eq!(row.2, want);
            assert_eq!(record(&fx, &id).level, level);
        }
    }
    assert!(wt.join("app/build/out.bin").exists());
}

#[test]
fn latest_subject_matches_the_newest_scope_subject() {
    let fx = Fixture::new();
    let wt = two_unpushed(&fx);
    assert_eq!(
        latest_subject(&wt).unwrap().as_deref(),
        Some("second change")
    );

    // With an upstream, the range is @{upstream}..HEAD.
    git(
        &fx.repo,
        &["remote", "add", "origin", "/nonexistent/bale-test"],
    );
    git(&wt, &["config", "branch.feature.remote", "origin"]);
    git(&wt, &["config", "branch.feature.merge", "refs/heads/main"]);
    assert_eq!(
        latest_subject(&wt).unwrap().as_deref(),
        Some("second change")
    );
    assert_eq!(
        latest_subject(&wt).unwrap(),
        scope(&wt).unwrap().subjects.into_iter().next()
    );

    // Nothing unpushed: no subject.
    git(
        &fx.repo,
        &["update-ref", "refs/remotes/origin/main", "feature"],
    );
    assert_eq!(latest_subject(&wt).unwrap(), None);
}

#[test]
fn manual_cold_still_works_on_a_pinned_tree() {
    let fx = Fixture::new();
    let wt = idle_gradle_tree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    set_pinned(&fx.home(), &id, true).unwrap();
    assert_eq!(cold(&fx.home(), &id).unwrap(), Outcome::Done(Level::Cold));
    assert!(record(&fx, &id).pinned);
}

#[test]
fn cold_saves_scope() {
    let fx = Fixture::new();
    let wt = two_unpushed(&fx);
    fs::write(wt.join("f"), "two\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    assert_eq!(record(&fx, &id).scope, None);

    assert_eq!(cold(&fx.home(), &id).unwrap(), Outcome::Done(Level::Cold));
    assert!(!Path::new(&wt).exists());
    let s = record(&fx, &id).scope.expect("scope saved");
    assert_eq!(s.branch.as_deref(), Some("feature"));
    assert_eq!(s.unpushed, 2);
    assert_eq!(s.subjects, vec!["second change", "first change"]);
    assert_eq!((s.changed_files, s.insertions, s.deletions), (1, 1, 1));
}
