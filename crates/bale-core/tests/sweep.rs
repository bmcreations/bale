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

/// A worktree made with plain `git worktree add`, which no hook saw.
fn plain_worktree(fx: &Fixture, name: &str) -> PathBuf {
    fx.add_worktree(name, name)
}

fn adopt_rows(rows: &[(String, Action, String)]) -> Vec<&(String, Action, String)> {
    rows.iter().filter(|r| r.1 == Action::Adopt).collect()
}

#[test]
fn sweep_adopts_worktrees_of_a_known_repo_that_were_never_registered() {
    let fx = Fixture::new();
    let known = plain_worktree(&fx, "known");
    let missed = plain_worktree(&fx, "missed");
    let known_id = register(&fx.home(), &known).unwrap().id;
    // idle_stash_minutes = 0 would stash any tree a restoring hook covers.
    global_config(&fx.home(), "idle_stash_minutes = 0\n");

    let rows = sweep(&fx.home(), false).unwrap();
    let adopted = adopt_rows(&rows);
    assert_eq!(adopted.len(), 1, "{rows:?}");
    let id = &adopted[0].0;
    assert_ne!(id, &known_id);
    let rec = Registry::open(fx.home()).get(id).unwrap().unwrap();
    assert_eq!(rec.path, bale_core::paths::canonicalize(&missed).unwrap());
    assert_eq!(rec.level, Level::Hot);
    assert!(!rec.restore_hook && rec.stash.is_none());
    // The main checkout stays out, and a second sweep adopts nothing more.
    assert_eq!(Registry::open(fx.home()).all_lenient().unwrap().len(), 2);
    let rows = sweep(&fx.home(), false).unwrap();
    assert!(adopt_rows(&rows).is_empty(), "{rows:?}");
    assert_ne!(row(&rows, id).1, Action::Stash);
    assert!(
        !Registry::open(fx.home())
            .get(id)
            .unwrap()
            .unwrap()
            .restore_hook
    );
}

#[test]
fn sweep_does_not_adopt_what_adopt_refuses_or_is_not_there() {
    let fx = Fixture::new();
    let known = plain_worktree(&fx, "known");
    register(&fx.home(), &known).unwrap();
    // A worktree whose directory is gone is prunable.
    let gone = plain_worktree(&fx, "gone");
    fs::remove_dir_all(&gone).unwrap();
    // A gitfile pointing at the main git dir is not a linked worktree's.
    let fake = plain_worktree(&fx, "fake");
    fs::write(
        fake.join(".git"),
        format!("gitdir: {}\n", fx.repo.join(".git").display()),
    )
    .unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    assert!(adopt_rows(&rows).is_empty(), "{rows:?}");
    // Only `known`: the main checkout is never adopted.
    assert_eq!(Registry::open(fx.home()).all_lenient().unwrap().len(), 1);
}

#[test]
fn dry_run_reports_missed_worktrees_without_registering() {
    let fx = Fixture::new();
    register(&fx.home(), &plain_worktree(&fx, "known")).unwrap();
    plain_worktree(&fx, "missed");

    let rows = sweep(&fx.home(), true).unwrap();
    let adopted = adopt_rows(&rows);
    assert_eq!(adopted.len(), 1, "{rows:?}");
    assert!(adopted[0].2.starts_with("would adopt"), "{rows:?}");
    assert_eq!(Registry::open(fx.home()).all_lenient().unwrap().len(), 1);
}

#[test]
fn a_broken_repo_does_not_stop_adoption_elsewhere() {
    let broken = Fixture::new();
    let broken_wt = plain_worktree(&broken, "wt");
    let fx = Fixture::new();
    let wt = plain_worktree(&fx, "known");
    let home = fx.home();
    let broken_id = register(&home, &broken_wt).unwrap().id;
    register(&home, &wt).unwrap();
    plain_worktree(&fx, "missed");
    // The first tree's record names a repo that is not there.
    let reg = Registry::open(home.clone());
    let mut rec = reg.get(&broken_id).unwrap().unwrap();
    rec.repo_common_dir = broken.repo.join("no-such-git-dir");
    reg.put(&rec).unwrap();

    let rows = sweep(&home, false).unwrap();
    assert_eq!(adopt_rows(&rows).len(), 1, "{rows:?}");
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

// ---- stash policy ----

mod warm {
    use super::*;
    use bale_core::config::Config;
    use bale_core::ops::cold::cold;
    use bale_core::ops::stash::{stash, StashOutcome};
    use bale_core::ops::sweep::decide;
    use bale_core::ops::unstash::unstash;
    use bale_core::ops::up::up;
    use std::time::Duration;
    use support::write_plist;

    const MIB: usize = 1 << 20;
    const BIG: &str = ".dd/Build/Products/Debug/a.bin";

    fn cfg() -> Config {
        Config {
            idle_baled: Duration::from_secs(4 * 3600),
            idle_cold: Duration::from_secs(7 * 86400),
            idle_stash: Duration::from_secs(600),
            reap_grace: Duration::from_secs(3600),
            stash_min_bytes: 1 << 20,
            patch_max_depth: 4,
            regenerable_extra: Vec::new(),
            ignored_denylist_extra: Vec::new(),
            worktree_root: None,
        }
    }

    fn mins(n: u64) -> Duration {
        Duration::from_secs(n * 60)
    }

    fn hours(n: u64) -> Duration {
        Duration::from_secs(n * 3600)
    }

    #[test]
    fn decide_stashes_after_idle_stash() {
        for level in [Level::Hot, Level::Baled] {
            assert_eq!(
                decide(level, mins(11), false, false, false, true, &cfg()),
                Action::Stash
            );
            assert_eq!(
                decide(level, mins(9), false, false, false, true, &cfg()),
                Action::Keep
            );
        }
    }

    #[test]
    fn decide_never_stashes_when_not_allowed() {
        for level in [Level::Hot, Level::Baled] {
            assert_eq!(
                decide(level, mins(11), false, false, false, false, &cfg()),
                Action::Keep
            );
            assert_eq!(
                decide(level, hours(5), false, true, false, false, &cfg()),
                Action::Up
            );
        }
        assert_eq!(
            decide(
                Level::Hot,
                Duration::from_secs(8 * 86400),
                false,
                true,
                false,
                false,
                &cfg()
            ),
            Action::Cold
        );
    }

    #[test]
    fn decide_ups_a_stashed_tree_at_idle_baled() {
        assert_eq!(
            decide(Level::Hot, hours(5), false, true, true, true, &cfg()),
            Action::Up
        );
        // Already stashed: nothing more to do until up is due.
        assert_eq!(
            decide(Level::Hot, mins(30), false, true, true, true, &cfg()),
            Action::Keep
        );
    }

    #[test]
    fn decide_up_beats_stash() {
        assert_eq!(
            decide(Level::Hot, hours(5), false, true, false, true, &cfg()),
            Action::Up
        );
    }

    #[test]
    fn decide_cold_beats_stash() {
        assert_eq!(
            decide(
                Level::Hot,
                Duration::from_secs(8 * 86400),
                false,
                true,
                false,
                true,
                &cfg()
            ),
            Action::Cold
        );
    }

    /// An Xcode worktree whose ignored `.dd` holds one large build file.
    /// `.gitignore` is committed first, so the tree is clean.
    fn xcode_worktree(fx: &Fixture) -> PathBuf {
        fs::write(fx.repo.join(".gitignore"), ".dd/\n").unwrap();
        git(&fx.repo, &["add", ".gitignore"]);
        git(&fx.repo, &["commit", "-m", "ignore"]);
        let wt = fx.add_worktree("wt", "feature");
        fs::create_dir_all(wt.join("App.xcodeproj")).unwrap();
        write_plist(&wt.join(".dd"), Some(&wt.join("App.xcodeproj")));
        let p = wt.join(BIG);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let bytes: Vec<u8> = (0..2 * MIB)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        fs::write(p, bytes).unwrap();
        wt
    }

    fn stashed_tree() -> (Fixture, PathBuf, String) {
        let fx = Fixture::new();
        let wt = xcode_worktree(&fx);
        let id = register(&fx.home(), &wt).unwrap().id;
        let out = stash(&fx.home(), &id, &[]).unwrap();
        assert!(matches!(out, StashOutcome::Stashed(_)), "{out:?}");
        assert_eq!(fs::metadata(wt.join(BIG)).unwrap().len(), 0);
        (fx, wt, id)
    }

    fn record(fx: &Fixture, id: &str) -> bale_core::registry::Record {
        Registry::open(fx.home()).get(id).unwrap().unwrap()
    }

    fn mark_restoring(fx: &Fixture, id: &str) {
        let reg = Registry::open(fx.home());
        let mut rec = reg.get(id).unwrap().unwrap();
        rec.stash.as_mut().unwrap().restoring = true;
        reg.put(&rec).unwrap();
    }

    fn allow_stash(fx: &Fixture, id: &str) {
        let reg = Registry::open(fx.home());
        let mut rec = reg.get(id).unwrap().unwrap();
        rec.restore_hook = true;
        reg.put(&rec).unwrap();
    }

    fn trace_lines(home: &BaleHome, kind: &str) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for entry in fs::read_dir(home.trace_dir()).into_iter().flatten() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            for line in fs::read_to_string(path).unwrap().lines() {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                if v["kind"] == kind {
                    out.push(v);
                }
            }
        }
        out
    }

    #[test]
    fn sweep_up_clears_stash() {
        let (fx, wt, id) = stashed_tree();
        global_config(&fx.home(), "idle_baled_hours = 0\n");

        let rows = sweep(&fx.home(), false).unwrap();
        assert_eq!(row(&rows, &id).1, Action::Up, "{rows:?}");
        assert!(!wt.join(".dd").exists());
        assert!(record(&fx, &id).stash.is_none());
    }

    #[test]
    fn sweep_cold_clears_stash() {
        let (fx, wt, id) = stashed_tree();
        global_config(&fx.home(), "idle_baled_hours = 0\nidle_cold_days = 0\n");

        let rows = sweep(&fx.home(), false).unwrap();
        assert_eq!(row(&rows, &id).1, Action::Cold, "{rows:?}");
        assert!(!wt.exists());
        assert!(record(&fx, &id).stash.is_none());
    }

    #[test]
    fn manual_up_clears_stash_and_unstash_is_a_noop() {
        let (fx, wt, id) = stashed_tree();
        up(&fx.home(), &id).unwrap();
        assert!(!wt.join(".dd").exists());
        assert!(record(&fx, &id).stash.is_none());
        assert!(unstash(&fx.home(), &id).unwrap().is_none());
        assert!(!wt.join(".dd").exists(), "unstash must not recreate dirs");
    }

    #[test]
    fn manual_up_clears_a_stash_that_is_restoring() {
        let (fx, wt, id) = stashed_tree();
        mark_restoring(&fx, &id);
        up(&fx.home(), &id).unwrap();
        assert!(!wt.join(".dd").exists());
        assert!(record(&fx, &id).stash.is_none());
        assert!(unstash(&fx.home(), &id).unwrap().is_none());
    }

    #[test]
    fn manual_cold_clears_stash() {
        let (fx, wt, id) = stashed_tree();
        cold(&fx.home(), &id).unwrap();
        assert!(!wt.exists());
        assert_eq!(level(&fx, &id), Level::Cold);
        assert!(record(&fx, &id).stash.is_none());
    }

    #[test]
    fn manual_cold_clears_a_stash_that_is_restoring() {
        let (fx, wt, id) = stashed_tree();
        mark_restoring(&fx, &id);
        cold(&fx.home(), &id).unwrap();
        assert!(!wt.exists());
        assert!(record(&fx, &id).stash.is_none());
    }

    #[test]
    fn unstash_ignores_a_cold_tree_with_a_leftover_stash() {
        let (fx, _wt, id) = stashed_tree();
        let reg = Registry::open(fx.home());
        let mut rec = reg.get(&id).unwrap().unwrap();
        rec.level = Level::Cold;
        reg.put(&rec).unwrap();
        assert!(unstash(&fx.home(), &id).unwrap().is_none());
    }

    #[test]
    fn sweep_stashes_and_traces() {
        let fx = Fixture::new();
        let wt = xcode_worktree(&fx);
        let id = register(&fx.home(), &wt).unwrap().id;
        allow_stash(&fx, &id);
        global_config(&fx.home(), "idle_stash_minutes = 0\n");

        let rows = sweep(&fx.home(), false).unwrap();
        assert_eq!(row(&rows, &id).1, Action::Stash, "{rows:?}");
        assert_eq!(fs::metadata(wt.join(BIG)).unwrap().len(), 0);
        assert!(record(&fx, &id).stash.is_some());

        let lines = trace_lines(&fx.home(), "stash");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["id"], id.as_str());
        assert_eq!(lines[0]["files"], 1);
        assert_eq!(lines[0]["bytes"], 2 * MIB as u64);
        assert!(lines[0]["stored_bytes"].as_u64().unwrap() > 0);
        assert!(lines[0]["secs"].is_number());
        let trees = trace_lines(&fx.home(), "tree");
        assert_eq!(trees[0]["action"], "stash");

        // Stashed now: the next sweep leaves it alone.
        let rows = sweep(&fx.home(), false).unwrap();
        assert_eq!(row(&rows, &id).1, Action::Keep, "{rows:?}");
        assert_eq!(trace_lines(&fx.home(), "stash").len(), 1);
    }

    #[test]
    fn sweep_keeps_an_idle_tree_no_restore_hook_covers() {
        let fx = Fixture::new();
        let wt = xcode_worktree(&fx);
        let id = register(&fx.home(), &wt).unwrap().id;
        global_config(&fx.home(), "idle_stash_minutes = 0\n");

        let rows = sweep(&fx.home(), false).unwrap();
        assert_eq!(row(&rows, &id).1, Action::Keep, "{rows:?}");
        assert_eq!(fs::metadata(wt.join(BIG)).unwrap().len(), (2 * MIB) as u64);
        let rec = record(&fx, &id);
        assert!(rec.stash.is_none() && !rec.restore_hook);
    }

    #[test]
    fn restored_tree_is_stashed_again() {
        let fx = Fixture::new();
        let wt = xcode_worktree(&fx);
        let id = register(&fx.home(), &wt).unwrap().id;
        allow_stash(&fx, &id);
        global_config(&fx.home(), "idle_stash_minutes = 0\n");
        sweep(&fx.home(), false).unwrap();
        let restored = unstash(&fx.home(), &id).unwrap().unwrap();
        assert_eq!(restored.files, 1);
        assert_eq!(trace_lines(&fx.home(), "unstash").len(), 1);

        let rows = sweep(&fx.home(), false).unwrap();
        assert_eq!(row(&rows, &id).1, Action::Stash, "{rows:?}");
        assert_eq!(fs::metadata(wt.join(BIG)).unwrap().len(), 0);
    }

    #[test]
    fn skipped_stash_is_not_an_error_and_is_traced_with_a_reason() {
        let fx = Fixture::new();
        let wt = xcode_worktree(&fx);
        let id = register(&fx.home(), &wt).unwrap().id;
        // Nothing reaches the size floor.
        allow_stash(&fx, &id);
        global_config(
            &fx.home(),
            "idle_stash_minutes = 0\nstash_min_bytes = 1073741824\n",
        );

        let rows = sweep(&fx.home(), false).unwrap();
        let (_, action, result) = row(&rows, &id);
        assert_eq!(*action, Action::Stash);
        assert!(result.starts_with("skipped: "), "{result}");
        assert_eq!(fs::metadata(wt.join(BIG)).unwrap().len(), (2 * MIB) as u64);
        let trees = trace_lines(&fx.home(), "tree");
        assert_eq!(trees[0]["action"], "stash");
        assert!(trees[0]["reason"].is_string(), "{trees:?}");
        assert!(trace_lines(&fx.home(), "stash").is_empty());
    }
}
