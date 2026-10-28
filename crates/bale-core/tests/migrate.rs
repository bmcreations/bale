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
    let want = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    rows.iter()
        .find(|r| r.path == path || fs::canonicalize(&r.path).is_ok_and(|p| p == want))
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
