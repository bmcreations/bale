mod support;

use bale_core::ops::migrate::{apply, plan, PlanRow};
use bale_core::ops::up::register;
use bale_core::registry::{workspace_id, Level, Registry};
use std::fs;
use std::path::{Path, PathBuf};
use support::{git, Fixture};

/// Commits `.gitignore` on main so Gradle markers and build output don't make
/// worktrees dirty. Must run before any worktree is added.
fn ignore_build(fx: &Fixture) {
    fs::write(
        fx.repo.join(".gitignore"),
        "app/build/\nsettings.gradle.kts\n",
    )
    .unwrap();
    git(&fx.repo, &["add", ".gitignore"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
}

/// A linked Gradle worktree with build output.
fn gradle_worktree(fx: &Fixture, name: &str) -> PathBuf {
    let wt = fx.add_worktree(name, name);
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    fs::create_dir_all(wt.join("app/build")).unwrap();
    fs::write(wt.join("app/build/out.bin"), "regen").unwrap();
    wt
}

/// Normal, prunable (dir removed) and dirty linked worktrees.
fn three_worktrees(fx: &Fixture) -> (PathBuf, PathBuf, PathBuf) {
    ignore_build(fx);
    let normal = gradle_worktree(fx, "normal");
    let gone = gradle_worktree(fx, "gone");
    fs::remove_dir_all(&gone).unwrap();
    let dirty = gradle_worktree(fx, "dirty");
    fs::write(dirty.join("f"), "uncommitted\n").unwrap();
    (normal, gone, dirty)
}

fn row<'a>(rows: &'a [PlanRow], path: &Path) -> &'a PlanRow {
    let want = bale_core::paths::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    rows.iter()
        .find(|r| {
            r.path == path || bale_core::paths::canonicalize(&r.path).is_ok_and(|p| p == want)
        })
        .unwrap_or_else(|| panic!("no row for {}: {rows:#?}", path.display()))
}

#[test]
fn plan_skips_main_and_flags_cases() {
    let fx = Fixture::outside_temp();
    let (normal, gone, dirty) = three_worktrees(&fx);

    let rows = plan(&fx.home(), std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(rows.len(), 3, "{rows:#?}");
    assert!(rows.iter().all(|r| !r.path.ends_with("my repo é")));

    let n = row(&rows, &normal);
    assert_eq!(n.action, "bale");
    assert!(
        !n.dirty && !n.busy && !n.temp_location && !n.prunable,
        "{n:#?}"
    );
    assert!(n.regenerable_bytes >= 5 && n.total_bytes >= n.regenerable_bytes);

    let g = row(&rows, &gone);
    assert!(g.prunable);
    assert_eq!(g.action, "report (prunable)");

    let d = row(&rows, &dirty);
    assert!(d.dirty);
    assert_eq!(d.action, "bale");
}

#[test]
fn plan_from_linked_worktree_and_duplicate_repo_args() {
    let fx = Fixture::outside_temp();
    let (normal, _, _) = three_worktrees(&fx);
    let rows = plan(&fx.home(), &[normal.clone(), fx.repo.clone()]).unwrap();
    assert_eq!(rows.len(), 3, "{rows:#?}");
}

#[test]
fn dry_run_changes_nothing() {
    let fx = Fixture::outside_temp();
    let (normal, _, dirty) = three_worktrees(&fx);
    let rows = plan(&fx.home(), std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(rows.len(), 3);
    assert!(normal.join("app/build/out.bin").exists());
    assert!(dirty.join("app/build/out.bin").exists());
    assert!(!fx.home().0.exists(), "plan wrote under BALE_HOME");
    assert!(git(&fx.repo, &["worktree", "list", "--porcelain"])
        .lines()
        .all(|l| !l.starts_with("locked")));
}

#[test]
fn apply_bales_and_reports_free_space() {
    let fx = Fixture::outside_temp();
    let (normal, gone, dirty) = three_worktrees(&fx);
    let home = fx.home();
    let rows = plan(&home, std::slice::from_ref(&fx.repo)).unwrap();

    let report = apply(&home, &rows).unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.baled, 2);
    assert_eq!(report.reported, 1);
    assert_eq!(report.busy, 0);
    assert!(!normal.join("app/build").exists());
    assert!(!dirty.join("app/build").exists());
    assert_eq!(fs::read(dirty.join("f")).unwrap(), b"uncommitted\n");
    assert!(report.free_before > 0 && report.free_after > 0);

    let reg = Registry::open(home);
    assert_eq!(
        reg.get(&workspace_id(&normal)).unwrap().unwrap().level,
        Level::Baled
    );
    assert_eq!(
        reg.get(&workspace_id(&dirty)).unwrap().unwrap().level,
        Level::Baled
    );
    assert!(reg.get(&workspace_id(&gone)).unwrap().is_none());
}

#[test]
fn registered_non_hot_is_reported_and_left_alone() {
    let fx = Fixture::outside_temp();
    let (normal, _, _) = three_worktrees(&fx);
    let home = fx.home();
    let reg = Registry::open(home.clone());
    let mut rec = register(&home, &normal).unwrap();
    rec.level = Level::Baled;
    reg.put(&rec).unwrap();

    let rows = plan(&home, std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(row(&rows, &normal).action, "report (baled)");

    let report = apply(&home, &rows).unwrap();
    assert_eq!(report.baled, 1);
    assert_eq!(report.reported, 2);
    assert!(normal.join("app/build/out.bin").exists());
    assert_eq!(reg.get(&rec.id).unwrap().unwrap().level, Level::Baled);
}

#[test]
fn temp_location_is_reported_never_touched() {
    // Fixture::new lives under the system temp dir.
    let fx = Fixture::new();
    ignore_build(&fx);
    let wt = gradle_worktree(&fx, "tmp-tree");
    let home = fx.home();

    let rows = plan(&home, std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].temp_location);
    assert_eq!(rows[0].action, "report (temp)");

    let report = apply(&home, &rows).unwrap();
    assert_eq!((report.baled, report.reported), (0, 1));
    assert!(wt.join("app/build/out.bin").exists());
    assert!(Registry::open(home)
        .get(&workspace_id(&wt))
        .unwrap()
        .is_none());
}

#[cfg(unix)]
#[test]
fn busy_tree_is_registered_but_not_baled() {
    let fx = Fixture::outside_temp();
    ignore_build(&fx);
    let wt = gradle_worktree(&fx, "busy");
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(&wt)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let home = fx.home();

    let result = plan(&home, std::slice::from_ref(&fx.repo)).and_then(|rows| {
        let report = apply(&home, &rows)?;
        Ok((rows, report))
    });
    child.kill().unwrap();
    child.wait().unwrap();

    let (rows, report) = result.unwrap();
    assert!(rows[0].busy, "{rows:#?}");
    assert_eq!(rows[0].action, "register only (busy)");
    assert_eq!((report.baled, report.busy), (0, 1), "{report:?}");
    assert!(wt.join("app/build/out.bin").exists());
    assert_eq!(
        Registry::open(home)
            .get(&workspace_id(&wt))
            .unwrap()
            .unwrap()
            .level,
        Level::Hot
    );
}

#[test]
fn corrupt_record_gives_error_row_and_others_still_plan() {
    let fx = Fixture::outside_temp();
    let (normal, _, dirty) = three_worktrees(&fx);
    let home = fx.home();
    fs::create_dir_all(home.records_dir()).unwrap();
    fs::write(
        home.records_dir()
            .join(format!("{}.json", workspace_id(&normal))),
        "{not json",
    )
    .unwrap();

    let rows = plan(&home, std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(rows.len(), 3, "{rows:#?}");
    let n = row(&rows, &normal);
    assert!(n.action.starts_with("report (error: "), "{n:#?}");
    assert!(n.action.contains("corrupt record"), "{n:#?}");
    assert_eq!(row(&rows, &dirty).action, "bale");

    let report = apply(&home, &rows).unwrap();
    assert_eq!((report.baled, report.reported), (1, 2), "{report:?}");
    assert!(normal.join("app/build/out.bin").exists());
}

#[test]
fn bad_repo_arg_gives_error_row_and_other_repos_continue() {
    let fx = Fixture::outside_temp();
    three_worktrees(&fx);
    // Missing, so git cannot climb to an enclosing repo.
    let bad = fx.repo.parent().unwrap().join("no such repo");
    let rows = plan(&fx.home(), &[bad.clone(), fx.repo.clone()]).unwrap();
    assert_eq!(rows.len(), 4, "{rows:#?}");
    assert_eq!(rows[0].path, bad);
    assert!(rows[0].action.starts_with("report (error: "), "{rows:#?}");
}

#[test]
fn two_repos_give_rows_from_both() {
    let a = Fixture::outside_temp();
    let b = Fixture::outside_temp();
    let (a_normal, _, _) = three_worktrees(&a);
    ignore_build(&b);
    let b_tree = gradle_worktree(&b, "other");
    let rows = plan(&a.home(), &[a.repo.clone(), b.repo.clone()]).unwrap();
    assert_eq!(rows.len(), 4, "{rows:#?}");
    assert_eq!(row(&rows, &a_normal).action, "bale");
    assert_eq!(row(&rows, &b_tree).action, "bale");
    assert_eq!(row(&rows, &b_tree).repo, b.repo);
}

#[test]
fn registered_hot_plans_as_bale() {
    let fx = Fixture::outside_temp();
    let (normal, _, _) = three_worktrees(&fx);
    register(&fx.home(), &normal).unwrap();
    let rows = plan(&fx.home(), std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(row(&rows, &normal).action, "bale");
}

#[test]
fn user_locked_worktree_is_reported_never_baled() {
    let fx = Fixture::outside_temp();
    let (normal, _, _) = three_worktrees(&fx);
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "mine",
            normal.to_str().unwrap(),
        ],
    );
    let home = fx.home();
    let rows = plan(&home, std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(row(&rows, &normal).action, "report (locked: mine)");

    let report = apply(&home, &rows).unwrap();
    assert_eq!(report.baled, 1, "{report:?}");
    assert!(normal.join("app/build/out.bin").exists());
    assert!(Registry::open(home)
        .get(&workspace_id(&normal))
        .unwrap()
        .is_none());
}

#[test]
fn missing_dir_not_prunable_to_git_plans_as_prunable() {
    let fx = Fixture::outside_temp();
    ignore_build(&fx);
    let wt = gradle_worktree(&fx, "locked-gone");
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "baled by bale",
            wt.to_str().unwrap(),
        ],
    );
    fs::remove_dir_all(&wt).unwrap();
    let rows = plan(&fx.home(), std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert!(rows[0].prunable);
    assert_eq!(rows[0].action, "report (prunable)");
}

#[test]
fn prunable_temp_row_has_both_flags() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("tmp-gone", "tmp-gone");
    fs::remove_dir_all(&wt).unwrap();
    let rows = plan(&fx.home(), std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].prunable && rows[0].temp_location, "{rows:#?}");
    assert_eq!(rows[0].action, "report (prunable)");
}

#[test]
fn apply_records_row_error_and_continues() {
    let fx = Fixture::outside_temp();
    let (normal, _, dirty) = three_worktrees(&fx);
    let home = fx.home();
    let mut rows = plan(&home, std::slice::from_ref(&fx.repo)).unwrap();
    // System temp has no git repo above it, so register fails the same way
    // wherever this checkout lives.
    let plain_tmp = tempfile::TempDir::new().unwrap();
    let not_a_worktree = plain_tmp.path().join("plain");
    fs::create_dir_all(&not_a_worktree).unwrap();
    let mut bogus = rows[0].clone();
    bogus.path = not_a_worktree.clone();
    bogus.action = "bale".into();
    rows.insert(0, bogus);

    let report = apply(&home, &rows).unwrap();
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(report.errors[0].0, not_a_worktree);
    assert_eq!(report.baled, 2);
    assert!(!normal.join("app/build").exists());
    assert!(!dirty.join("app/build").exists());
}

#[test]
fn lock_without_reason_plans_as_locked() {
    let fx = Fixture::outside_temp();
    let (normal, _, _) = three_worktrees(&fx);
    git(&fx.repo, &["worktree", "lock", normal.to_str().unwrap()]);
    let rows = plan(&fx.home(), std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(row(&rows, &normal).action, "report (locked)");
}

#[test]
fn lock_added_between_plan_and_apply_is_respected() {
    let fx = Fixture::outside_temp();
    let (normal, _, dirty) = three_worktrees(&fx);
    let home = fx.home();
    let rows = plan(&home, std::slice::from_ref(&fx.repo)).unwrap();
    assert_eq!(row(&rows, &normal).action, "bale");
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "mine",
            normal.to_str().unwrap(),
        ],
    );

    let report = apply(&home, &rows).unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!((report.baled, report.reported), (1, 2), "{report:?}");
    assert!(normal.join("app/build/out.bin").exists());
    assert!(!dirty.join("app/build").exists());
    assert!(Registry::open(home)
        .get(&workspace_id(&normal))
        .unwrap()
        .is_none());
}

#[test]
fn repo_names_main_checkout_for_linked_path_arg() {
    let fx = Fixture::outside_temp();
    let (normal, _, _) = three_worktrees(&fx);
    let rows = plan(&fx.home(), std::slice::from_ref(&normal)).unwrap();
    assert_eq!(rows.len(), 3, "{rows:#?}");
    let main = bale_core::paths::canonicalize(&fx.repo).unwrap();
    for r in &rows {
        assert_eq!(
            bale_core::paths::canonicalize(&r.repo).unwrap(),
            main,
            "{r:#?}"
        );
    }
}
