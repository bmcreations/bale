//! Kept in its own test binary: it points HOME at a temp dir, which would
//! race the other sweep tests.
#![cfg(target_os = "macos")]

mod support;

use bale_core::ops::sweep::{sweep, Action};
use bale_core::ops::up::register;
use bale_core::registry::{Level, Registry};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use support::{git, write_plist, Fixture};

/// Every test here points HOME at its own dir, so they take turns.
static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Points HOME at a fresh dir beside the fixture's and returns the shared
/// DerivedData root under it.
fn user_home(fx: &Fixture) -> (MutexGuard<'static, ()>, PathBuf) {
    let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let user_home = fx.home().0.parent().unwrap().join("user");
    std::env::set_var("HOME", &user_home);
    let root = user_home.join("Library/Developer/Xcode/DerivedData");
    fs::create_dir_all(&root).unwrap();
    (guard, root)
}

/// A DerivedData folder `name` under `root` built from `workspace`, with
/// `Build/` and some output.
fn dd(root: &Path, name: &str, workspace: Option<&Path>) -> PathBuf {
    let dir = root.join(name);
    write_plist(&dir, workspace);
    fs::create_dir_all(dir.join("Build/Products")).unwrap();
    fs::write(dir.join("Build/Products/out"), vec![0u8; 8192]).unwrap();
    dir
}

/// Backdates the folder, its `info.plist` and `Build/` by `hours`.
fn age(dir: &Path, hours: u64) {
    let t = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(hours * 3600),
    );
    for p in [dir.to_path_buf(), dir.join("info.plist"), dir.join("Build")] {
        filetime::set_file_mtime(p, t).unwrap();
    }
}

fn gone(fx: &Fixture, name: &str) -> PathBuf {
    fx.repo.parent().unwrap().join(name)
}

fn reap_rows(rows: &[(String, Action, String)]) -> Vec<&(String, Action, String)> {
    rows.iter().filter(|r| r.1 == Action::Reap).collect()
}

fn op_lines(fx: &Fixture, op: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for e in fs::read_dir(fx.home().trace_dir()).unwrap().flatten() {
        for line in fs::read_to_string(e.path()).unwrap().lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v["kind"] == "op" && v["op"] == op {
                out.push(v);
            }
        }
    }
    out
}

#[test]
fn old_orphaned_derived_data_is_reaped() {
    let fx = Fixture::new();
    let (_g, root) = user_home(&fx);
    let orphan = dd(
        &root,
        "App-old",
        Some(&gone(&fx, "deleted-proj/App.xcodeproj")),
    );
    age(&orphan, 2);

    let rows = sweep(&fx.home(), false).unwrap();
    let reaped = reap_rows(&rows);
    assert_eq!(reaped.len(), 1, "{rows:?}");
    assert!(reaped[0].0.ends_with("App-old"), "{rows:?}");
    assert!(reaped[0].2.starts_with("reaped"), "{rows:?}");
    assert!(!orphan.exists());

    let ops = op_lines(&fx, "reap");
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["outcome"], "ok");
    assert_eq!(ops[0]["trigger"], "sweep");
    assert!(ops[0]["bytes_freed"].as_u64().unwrap() >= 8192, "{ops:?}");
}

#[test]
fn derived_data_inside_the_grace_period_is_kept() {
    let fx = Fixture::new();
    let (_g, root) = user_home(&fx);
    let fresh = dd(
        &root,
        "App-fresh",
        Some(&gone(&fx, "deleted-proj/App.xcodeproj")),
    );
    // The folder is old but its Build dir was just written.
    let busy = dd(
        &root,
        "App-building",
        Some(&gone(&fx, "other/App.xcodeproj")),
    );
    age(&busy, 2);
    filetime::set_file_mtime(busy.join("Build"), filetime::FileTime::now()).unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    assert!(reap_rows(&rows).is_empty(), "{rows:?}");
    assert!(fresh.exists() && busy.exists());

    fs::write(fx.home().0.join("config.toml"), "reap_grace_minutes = 0\n").unwrap();
    sweep(&fx.home(), false).unwrap();
    assert!(!fresh.exists() && !busy.exists());
}

#[test]
fn derived_data_is_kept_unless_its_project_is_provably_gone() {
    let fx = Fixture::new();
    let (_g, root) = user_home(&fx);
    let live_proj = fx.repo.join("Live.xcodeproj");
    fs::create_dir_all(&live_proj).unwrap();
    let live = dd(&root, "Live-a", Some(&live_proj));
    let no_key = dd(&root, "NoKey-b", None);
    let garbled = dd(&root, "Garbled-c", None);
    fs::write(garbled.join("info.plist"), "not a plist").unwrap();
    let cache = root.join("ModuleCache.noindex");
    fs::create_dir_all(&cache).unwrap();
    // A registered tree that is cold still owns paths below it.
    let wt = fx.add_worktree("wt", "wt");
    let id = register(&fx.home(), &wt).unwrap().id;
    let owned = dd(&root, "Owned-d", Some(&wt.join("Gone.xcodeproj")));
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(&id).unwrap().unwrap();
    rec.level = Level::Cold;
    reg.put(&rec).unwrap();
    // Not mounted: /Volumes/<name> is absent, so the path may just be offline.
    let offline = dd(
        &root,
        "Offline-e",
        Some(Path::new("/Volumes/bale-test-not-mounted/App.xcodeproj")),
    );
    for d in [&live, &no_key, &garbled, &owned, &offline] {
        age(d, 48);
    }

    let rows = sweep(&fx.home(), false).unwrap();
    assert!(reap_rows(&rows).is_empty(), "{rows:?}");
    for d in [&live, &no_key, &garbled, &owned, &offline, &cache] {
        assert!(d.exists(), "{}", d.display());
    }
}

#[test]
fn dry_run_reports_without_deleting() {
    let fx = Fixture::new();
    let (_g, root) = user_home(&fx);
    let orphan = dd(
        &root,
        "App-old",
        Some(&gone(&fx, "deleted-proj/App.xcodeproj")),
    );
    age(&orphan, 2);

    let rows = sweep(&fx.home(), true).unwrap();
    let reaped = reap_rows(&rows);
    assert_eq!(reaped.len(), 1, "{rows:?}");
    assert!(reaped[0].2.starts_with("would reap"), "{rows:?}");
    assert!(orphan.exists());
    assert!(op_lines(&fx, "reap").is_empty());
}

#[test]
fn learned_roots_are_reaped_by_the_same_rule() {
    let fx = Fixture::new();
    let (_g, _root) = user_home(&fx);
    let custom = fx.repo.parent().unwrap().join("fc-1-dd");
    let orphan = dd(
        custom.parent().unwrap(),
        "fc-1-dd",
        Some(&gone(&fx, "wt-gone/App.xcodeproj")),
    );
    let live_proj = fx.repo.join("Keep.xcodeproj");
    fs::create_dir_all(&live_proj).unwrap();
    let live = dd(fx.repo.parent().unwrap(), "fc-2-dd", Some(&live_proj));
    // Not a DerivedData root (no info.plist), and a line for a folder that is gone.
    let not_root = fx.repo.parent().unwrap().join("fc-3-dd");
    fs::create_dir_all(not_root.join("Build")).unwrap();
    let missing = fx.repo.parent().unwrap().join("fc-4-dd");
    for d in [&orphan, &live] {
        age(d, 2);
    }
    fs::create_dir_all(&fx.home().0).unwrap();
    let lines: Vec<String> = [&orphan, &live, &not_root, &missing, &orphan]
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    fs::write(fx.home().dd_roots_file(), lines.join("\n")).unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(reap_rows(&rows).len(), 1, "{rows:?}");
    assert!(!orphan.exists());
    assert!(live.exists() && not_root.exists());
}

#[test]
fn baled_tree_with_regrown_shared_derived_data_is_baled_again() {
    let fx = Fixture::new();
    fs::write(fx.repo.join(".gitignore"), "App.xcworkspace/\n").unwrap();
    git(&fx.repo, &["add", ".gitignore"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
    fs::create_dir_all(&fx.home().0).unwrap();
    fs::write(fx.home().0.join("config.toml"), "idle_baled_hours = 0\n").unwrap();
    let wt = fx.add_worktree("wt", "wt");
    fs::create_dir_all(wt.join("App.xcworkspace")).unwrap();

    let (_g, root) = user_home(&fx);
    let dd = self::dd(&root, "App-abc", Some(&wt.join("App.xcworkspace")));

    let id = register(&fx.home(), &wt).unwrap().id;
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(&id).unwrap().unwrap();
    rec.level = Level::Baled;
    reg.put(&rec).unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    let row = rows.iter().find(|r| r.0 == id).unwrap();
    assert_eq!(row.1, Action::Up, "{rows:?}");
    assert!(!dd.exists());
    assert_eq!(reg.get(&id).unwrap().unwrap().level, Level::Baled);

    // With the shared folder gone, the next sweep keeps the tree.
    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(rows.iter().find(|r| r.0 == id).unwrap().1, Action::Keep);
}
