mod support;

use bale_core::ops::cold::{exclusions, save_ignored, save_wip};
use bale_core::ops::sweep::{sweep, Action};
use bale_core::ops::up::register;
use bale_core::registry::{Level, Registry};
use bale_core::BaleHome;
use std::fs;
use std::path::{Path, PathBuf};
use support::{git, Fixture};

/// A linked Gradle worktree with build output and a source file.
fn gradle_worktree(fx: &Fixture, name: &str) -> PathBuf {
    let wt = fx.add_worktree(name, name);
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    fs::create_dir_all(wt.join("app/build")).unwrap();
    fs::write(wt.join("app/build/out.bin"), "regen").unwrap();
    wt
}

/// Commits `.gitignore` (so `settings.gradle.kts` and `app/build` don't make
/// worktrees dirty) on main before any worktree is added.
fn ignore_build(fx: &Fixture) {
    fs::write(
        fx.repo.join(".gitignore"),
        "app/build/\nsettings.gradle.kts\n",
    )
    .unwrap();
    git(&fx.repo, &["add", ".gitignore"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
}

fn global_config(home: &BaleHome, text: &str) {
    fs::create_dir_all(&home.0).unwrap();
    fs::write(home.0.join("config.toml"), text).unwrap();
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

fn row<'a>(rows: &'a [(String, Action, String)], id: &str) -> &'a (String, Action, String) {
    rows.iter()
        .find(|r| r.0 == id)
        .unwrap_or_else(|| panic!("no row for {id}: {rows:?}"))
}

fn break_gitfile(wt: &Path) {
    fs::write(wt.join(".git"), "gitdir: /nonexistent/bale-test\n").unwrap();
}

fn bale_refs(fx: &Fixture) -> String {
    git(&fx.repo, &["for-each-ref", "refs/bale/"])
}

fn registry_bytes(home: &BaleHome) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out: Vec<_> = fs::read_dir(home.records_dir())
        .unwrap()
        .flatten()
        .map(|e| (e.path(), fs::read(e.path()).unwrap()))
        .collect();
    out.sort();
    out
}

#[test]
fn missing_dir_is_reported_and_other_is_processed() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\n");
    let gone = gradle_worktree(&fx, "gone");
    let kept = gradle_worktree(&fx, "kept");
    let gone_id = register(&fx.home(), &gone).unwrap().id;
    let kept_id = register(&fx.home(), &kept).unwrap().id;
    fs::remove_dir_all(&gone).unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(row(&rows, &gone_id).1, Action::Report("missing".into()));
    assert_eq!(row(&rows, &kept_id).1, Action::Up);
    assert_eq!(level(&fx, &gone_id), Level::Hot);
    assert_eq!(level(&fx, &kept_id), Level::Baled);
    assert!(!kept.join("app/build").exists());
}

#[test]
fn fresh_worktree_is_kept() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx, "wt");
    let id = register(&fx.home(), &wt).unwrap().id;

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Keep);
    assert_eq!(level(&fx, &id), Level::Hot);
    assert!(wt.join("app/build/out.bin").exists());
}

#[test]
fn clean_idle_worktree_goes_cold() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\nidle_cold_days = 0\n");
    let wt = gradle_worktree(&fx, "wt");
    let id = register(&fx.home(), &wt).unwrap().id;

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Cold, "{rows:?}");
    assert_eq!(level(&fx, &id), Level::Cold);
    assert!(!wt.exists());

    // A cold record stays cold.
    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Keep);
    assert!(!wt.exists());
}

#[test]
fn dirty_idle_worktree_is_never_cold() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\nidle_cold_days = 0\n");
    let wt = gradle_worktree(&fx, "wt");
    fs::write(wt.join("f"), "edited\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Up, "{rows:?}");
    assert_eq!(level(&fx, &id), Level::Baled);

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Keep, "{rows:?}");
    assert_eq!(level(&fx, &id), Level::Baled);
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "edited\n");
    assert_eq!(bale_refs(&fx), "");
}

#[test]
fn dirty_probe_failure_is_reported_never_cold() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\nidle_cold_days = 0\n");
    let wt = gradle_worktree(&fx, "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    break_gitfile(&wt);

    let rows = sweep(&fx.home(), false).unwrap();
    let (_, action, result) = row(&rows, &id);
    assert!(matches!(action, Action::Report(_)), "{rows:?}");
    assert!(result.contains("status"), "{result}");
    assert_eq!(level(&fx, &id), Level::Hot);
    assert!(wt.join("f").exists());
    assert!(wt.join("app/build/out.bin").exists());
}

#[test]
fn skip_in_one_workspace_does_not_stop_the_next() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\n");
    let bad = gradle_worktree(&fx, "bad");
    let good = gradle_worktree(&fx, "good");
    let bad_id = register(&fx.home(), &bad).unwrap().id;
    let good_id = register(&fx.home(), &good).unwrap().id;
    break_gitfile(&bad);

    let rows = sweep(&fx.home(), false).unwrap();
    let (_, action, result) = row(&rows, &bad_id);
    assert_eq!(*action, Action::Up);
    // A broken gitfile fails the checkout check, so up skips it unchanged.
    assert!(result.contains("not the worktree's checkout"), "{result}");
    assert!(bad.join("app/build/out.bin").exists());
    assert_eq!(row(&rows, &good_id).1, Action::Up);
    assert_eq!(level(&fx, &good_id), Level::Baled);
    assert!(!good.join("app/build").exists());
}

#[test]
fn cooling_with_checkout_gone_resumes_cold() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("f"), "modified\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
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
    save_wip(&wt, &id).unwrap();
    save_ignored(
        &wt,
        &fx.home().saved_dir(&id),
        &exclusions(&fx.home(), &wt).unwrap(),
    )
    .unwrap();
    let trash = wt.with_file_name(".bale-trash-wt-123");
    fs::rename(&wt, &trash).unwrap();
    set_level(&fx, &id, Level::Cooling);

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Cold, "{rows:?}");
    assert_eq!(level(&fx, &id), Level::Cold);
    assert!(!trash.exists());
    assert!(!wt.exists());
}

#[test]
fn interrupted_open_is_reported_and_path_not_created() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;
    fs::remove_dir_all(&wt).unwrap();
    set_level(&fx, &id, Level::Opening);

    let rows = sweep(&fx.home(), false).unwrap();
    let (_, action, result) = row(&rows, &id);
    assert!(matches!(action, Action::Report(_)), "{rows:?}");
    assert!(result.contains(&format!("bale open {id}")), "{result}");
    assert!(!wt.exists());
    assert_eq!(level(&fx, &id), Level::Opening);
}

#[test]
fn dry_run_changes_nothing() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\nidle_cold_days = 0\n");
    let clean = gradle_worktree(&fx, "clean");
    let dirty = gradle_worktree(&fx, "dirty");
    fs::write(dirty.join("f"), "edited\n").unwrap();
    let clean_id = register(&fx.home(), &clean).unwrap().id;
    let dirty_id = register(&fx.home(), &dirty).unwrap().id;
    let before = registry_bytes(&fx.home());

    let rows = sweep(&fx.home(), true).unwrap();
    assert_eq!(row(&rows, &clean_id).1, Action::Cold, "{rows:?}");
    assert_eq!(row(&rows, &dirty_id).1, Action::Up, "{rows:?}");
    assert_eq!(registry_bytes(&fx.home()), before);
    assert!(clean.join("app/build/out.bin").exists());
    assert!(dirty.join("app/build/out.bin").exists());
    assert!(!fx.home().saved_dir(&clean_id).exists());
    assert_eq!(bale_refs(&fx), "");
    let locked = git(&fx.repo, &["worktree", "list", "--porcelain"]);
    assert!(!locked.contains("locked"), "{locked}");
}

#[test]
fn per_repo_config_overrides_global_threshold() {
    let fx = Fixture::new();
    fs::write(fx.repo.join(".bale.toml"), "idle_baled_hours = 0\n").unwrap();
    git(&fx.repo, &["add", ".bale.toml"]);
    git(&fx.repo, &["commit", "-m", "bale config"]);
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 100\n");
    let wt = gradle_worktree(&fx, "wt");
    let id = register(&fx.home(), &wt).unwrap().id;

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Up, "{rows:?}");
    assert_eq!(level(&fx, &id), Level::Baled);
}

#[test]
fn global_threshold_applies_without_repo_config() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 100\n");
    let wt = gradle_worktree(&fx, "wt");
    let id = register(&fx.home(), &wt).unwrap().id;

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Keep, "{rows:?}");
    assert_eq!(level(&fx, &id), Level::Hot);
}

#[test]
fn corrupt_record_is_reported_and_others_processed() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx, "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    fs::write(fx.home().records_dir().join("garbage.json"), "not json").unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (_, action, result) = row(&rows, "garbage");
    assert!(
        matches!(action, Action::Report(t) if t.starts_with("corrupt record: ")),
        "{rows:?}"
    );
    assert!(result.starts_with("corrupt record: "), "{result}");
    assert_eq!(row(&rows, &id).1, Action::Keep);
}

#[test]
fn malformed_config_does_not_hide_level_only_actions() {
    let fx = Fixture::new();
    let opening = fx.add_worktree("opening", "opening");
    let cold_wt = fx.add_worktree("cold", "cold");
    let opening_id = register(&fx.home(), &opening).unwrap().id;
    let cold_id = register(&fx.home(), &cold_wt).unwrap().id;
    fs::remove_dir_all(&opening).unwrap();
    set_level(&fx, &opening_id, Level::Opening);
    set_level(&fx, &cold_id, Level::Cold);
    global_config(&fx.home(), "idle_baled_hours = 'x'\n");

    let rows = sweep(&fx.home(), false).unwrap();
    let (_, _, result) = row(&rows, &opening_id);
    assert!(
        result.contains(&format!("run `bale open {opening_id}`")),
        "{result}"
    );
    assert_eq!(row(&rows, &cold_id).1, Action::Keep);
}

#[test]
fn up_row_names_a_build_dir_it_kept() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\n");
    let wt = gradle_worktree(&fx, "wt");
    fs::create_dir_all(wt.join("build")).unwrap();
    fs::write(wt.join("build/notes.md"), "notes\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;

    let rows = sweep(&fx.home(), false).unwrap();
    let (_, action, result) = row(&rows, &id);
    assert_eq!(*action, Action::Up);
    assert!(result.starts_with("baled; kept "), "{result}");
    assert!(result.contains("build"), "{result}");
    assert!(wt.join("build/notes.md").exists());
    assert!(!wt.join("app/build").exists());
}

#[test]
fn baled_tree_with_regrown_output_is_baled_again() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\n");
    let wt = gradle_worktree(&fx, "wt");
    // Dirty, which must not stop a re-bale.
    fs::write(wt.join("f"), "edited\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    set_level(&fx, &id, Level::Baled);

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Up, "{rows:?}");
    assert!(!wt.join("app/build").exists());
    assert_eq!(level(&fx, &id), Level::Baled);
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "edited\n");

    // Nothing regrew, and a dir with real content does not count.
    fs::create_dir_all(wt.join("build")).unwrap();
    fs::write(wt.join("build/notes.md"), "notes\n").unwrap();
    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(row(&rows, &id).1, Action::Keep, "{rows:?}");
}

#[test]
fn up_error_in_one_workspace_does_not_stop_the_next() {
    let fx = Fixture::new();
    ignore_build(&fx);
    global_config(&fx.home(), "idle_baled_hours = 0\n");
    let bad = gradle_worktree(&fx, "bad");
    let good = gradle_worktree(&fx, "good");
    let bad_rec = register(&fx.home(), &bad).unwrap();
    let good_id = register(&fx.home(), &good).unwrap().id;
    // The checkout check passes; up's git read of the index then fails.
    fs::write(bad_rec.admin_dir.join("index"), "x").unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    let (_, action, result) = row(&rows, &bad_rec.id);
    assert_eq!(*action, Action::Up);
    assert!(result.starts_with("error: "), "{result}");
    assert!(bad.join("app/build/out.bin").exists());
    assert_eq!(row(&rows, &good_id).1, Action::Up);
    assert_eq!(level(&fx, &good_id), Level::Baled);
    assert!(!good.join("app/build").exists());
}
