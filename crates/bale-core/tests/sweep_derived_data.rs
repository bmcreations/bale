//! Kept in its own test binary: it points HOME at a temp dir, which would
//! race the other sweep tests.
#![cfg(target_os = "macos")]

mod support;

use bale_core::ops::sweep::{sweep, sweep_with, Action};
use bale_core::ops::up::{register, up, Outcome};
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
fn scratch_build_folders_whose_project_is_gone_are_reaped() {
    let fx = Fixture::new();
    let (_g, _root) = user_home(&fx);
    let pad = fx
        .repo
        .parent()
        .unwrap()
        .join("scratch/proj/session/scratchpad");
    let orphan = dd(&pad, "dd-a", Some(&gone(&fx, "wt-gone/App.xcodeproj")));
    let nested = dd(
        &pad.join("wt-b"),
        ".dd",
        Some(&gone(&fx, "wt-gone-b/App.xcodeproj")),
    );
    // A build folder still inside the grace period, and an agent's notes.
    let fresh = dd(&pad, "dd-fresh", Some(&gone(&fx, "wt-gone/App.xcodeproj")));
    let notes = pad.join("notes");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("plan.md"), "keep me").unwrap();
    for d in [&orphan, &nested] {
        age(d, 2);
    }

    let rows = sweep_with(&fx.home(), false, |_| vec![pad.clone()]).unwrap();
    assert_eq!(reap_rows(&rows).len(), 2, "{rows:?}");
    assert!(!orphan.exists() && !nested.exists());
    assert!(fresh.exists() && notes.join("plan.md").exists());
    assert!(pad.join("wt-b").is_dir());
    // Learned, so a later sweep reaps `dd-fresh` once its grace is up.
    let learned = bale_core::derived_data::learned_roots(&fx.home());
    let fresh = bale_core::paths::canonicalize(&fresh).unwrap();
    assert!(learned.contains(&fresh), "{learned:?}");
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

#[cfg(unix)]
struct KillOnDrop(std::process::Child);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A worktree `name` holding an `App.xcodeproj`, so it detects as Xcode.
fn xcode_tree(fx: &Fixture, name: &str) -> PathBuf {
    let wt = fx.add_worktree(name, name);
    fs::create_dir_all(wt.join("App.xcodeproj")).unwrap();
    wt
}

/// A learned `-derivedDataPath` root `name` in a scratchpad beside the repo,
/// built from `tree`'s `App.xcodeproj`.
fn learned(fx: &Fixture, name: &str, tree: &Path) -> PathBuf {
    let pad = fx
        .repo
        .parent()
        .unwrap()
        .join("scratch/proj/session/scratchpad");
    let dir = dd(&pad, name, Some(&tree.join("App.xcodeproj")));
    bale_core::derived_data::learn_root(&fx.home(), &dir).unwrap();
    dir
}

#[test]
fn up_removes_learned_roots_built_from_the_tree_only() {
    let fx = Fixture::new();
    let (_g, _root) = user_home(&fx);
    let wt = xcode_tree(&fx, "wt");
    let other = xcode_tree(&fx, "other");
    let mine = learned(&fx, "dd-pin", &wt);
    let theirs = learned(&fx, "dd-other", &other);
    // Not a DerivedData root (no `Build/`), though its plist names the tree.
    let not_root = fx.repo.parent().unwrap().join("scratch/no-build");
    write_plist(&not_root, Some(&wt.join("App.xcodeproj")));
    bale_core::derived_data::learn_root(&fx.home(), &not_root).unwrap();

    let id = register(&fx.home(), &wt).unwrap().id;
    let out = up(&fx.home(), &id).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(!mine.exists(), "{}", mine.display());
    assert!(theirs.join("Build/Products/out").exists());
    assert!(not_root.join("info.plist").exists());
    // The line stays; reap drops dead lines.
    let lines = bale_core::derived_data::learned_roots(&fx.home());
    assert_eq!(lines.len(), 3, "{lines:?}");
}

#[cfg(unix)]
#[test]
fn up_keeps_and_reports_a_busy_learned_root() {
    let fx = Fixture::new();
    let (_g, _root) = user_home(&fx);
    let wt = xcode_tree(&fx, "wt");
    let busy = learned(&fx, "dd-busy", &wt);
    let id = register(&fx.home(), &wt).unwrap().id;
    let _child = KillOnDrop(
        std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&busy)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));

    let out = up(&fx.home(), &id).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(busy.join("Build/Products/out").exists());
    let note = Registry::open(fx.home())
        .get(&id)
        .unwrap()
        .unwrap()
        .note
        .unwrap_or_default();
    assert!(
        note.contains("dd-busy") && note.contains("in use"),
        "{note}"
    );
}

#[test]
fn up_relinks_a_removed_learned_root_to_shared_packages() {
    let fx = Fixture::new();
    let (_g, root) = user_home(&fx);
    let wt = xcode_tree(&fx, "wt");
    let mine = learned(&fx, "dd-pin", &wt);
    let id = register(&fx.home(), &wt).unwrap().id;
    // The tree is linked to a shared packages folder, as `register` does.
    let shared = fx.home().spm_dir().join("k1");
    fs::create_dir_all(&shared).unwrap();
    let stub = root.join("App-stub");
    fs::create_dir_all(&stub).unwrap();
    std::os::unix::fs::symlink(&shared, stub.join("SourcePackages")).unwrap();
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(&id).unwrap().unwrap();
    rec.spm.push(bale_core::registry::SpmLink {
        project: wt.join("App.xcodeproj"),
        resolved: wt.join("App.xcodeproj/Package.resolved"),
        mtime: std::time::SystemTime::now(),
        size: 0,
        key: "k1".into(),
        folder: stub,
    });
    reg.put(&rec).unwrap();

    up(&fx.home(), &id).unwrap();
    assert!(!mine.join("Build").exists());
    assert!(!mine.join("info.plist").exists());
    assert_eq!(fs::read_link(mine.join("SourcePackages")).unwrap(), shared);
}

#[test]
fn up_removes_an_unignored_derived_data_root_two_levels_into_the_tree() {
    let fx = Fixture::new();
    let (_g, _root) = user_home(&fx);
    let wt = xcode_tree(&fx, "wt");
    let dot_dd = dd(&wt, ".dd", Some(&wt.join("App.xcodeproj")));
    let nested = dd(&wt.join("build"), "dd", Some(&wt.join("App.xcodeproj")));
    // Named like one, but no `info.plist`: real content, kept.
    let plain = wt.join(".dd-notes");
    fs::create_dir_all(plain.join("Build")).unwrap();
    fs::write(plain.join("Build/notes.md"), "keep").unwrap();
    // Three levels down: not covered, so git's rule still keeps it.
    let deep = dd(&wt.join("a/b"), "dd", Some(&wt.join("App.xcodeproj")));
    let id = register(&fx.home(), &wt).unwrap().id;

    let out = up(&fx.home(), &id).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(!dot_dd.exists() && !nested.exists());
    assert!(plain.join("Build/notes.md").exists());
    assert!(deep.join("Build/Products/out").exists());
}

#[test]
fn up_keeps_an_in_tree_derived_data_root_holding_a_tracked_file() {
    let fx = Fixture::new();
    let (_g, _root) = user_home(&fx);
    let wt = xcode_tree(&fx, "wt");
    let dot_dd = dd(&wt, ".dd", Some(&wt.join("App.xcodeproj")));
    fs::write(dot_dd.join("keep.txt"), "keep\n").unwrap();
    git(&wt, &["add", ".dd/keep.txt"]);
    git(&wt, &["commit", "-qm", "keep"]);
    let id = register(&fx.home(), &wt).unwrap().id;
    up(&fx.home(), &id).unwrap();
    assert!(dot_dd.join("Build/Products/out").exists());
}

#[test]
fn baled_tree_with_a_learned_root_is_baled_again() {
    let fx = Fixture::new();
    fs::create_dir_all(&fx.home().0).unwrap();
    fs::write(fx.home().0.join("config.toml"), "idle_baled_hours = 0\n").unwrap();
    let (_g, _root) = user_home(&fx);
    let wt = xcode_tree(&fx, "wt");
    let mine = learned(&fx, "dd-pin", &wt);
    let id = register(&fx.home(), &wt).unwrap().id;
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(&id).unwrap().unwrap();
    rec.level = Level::Baled;
    reg.put(&rec).unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(
        rows.iter().find(|r| r.0 == id).unwrap().1,
        Action::Up,
        "{rows:?}"
    );
    assert!(!mine.exists());
    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(
        rows.iter().find(|r| r.0 == id).unwrap().1,
        Action::Keep,
        "{rows:?}"
    );
}

#[test]
fn baled_tree_with_an_unignored_in_tree_root_is_baled_again() {
    let fx = Fixture::new();
    fs::create_dir_all(&fx.home().0).unwrap();
    fs::write(fx.home().0.join("config.toml"), "idle_baled_hours = 0\n").unwrap();
    let (_g, _root) = user_home(&fx);
    let wt = xcode_tree(&fx, "wt");
    let dot_dd = dd(&wt, ".dd", Some(&wt.join("App.xcodeproj")));
    let id = register(&fx.home(), &wt).unwrap().id;
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(&id).unwrap().unwrap();
    rec.level = Level::Baled;
    reg.put(&rec).unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(
        rows.iter().find(|r| r.0 == id).unwrap().1,
        Action::Up,
        "{rows:?}"
    );
    assert!(!dot_dd.exists());
}

#[test]
fn up_leaves_derived_data_of_a_registered_tree_nested_inside() {
    let fx = Fixture::new();
    let (_g, root) = user_home(&fx);
    let a = xcode_tree(&fx, "a");
    let b = a.join(".claude/worktrees/b");
    git(
        &fx.repo,
        &["worktree", "add", "-q", "-b", "b", b.to_str().unwrap()],
    );
    fs::create_dir_all(b.join("App.xcodeproj")).unwrap();
    let id_a = register(&fx.home(), &a).unwrap().id;
    let id_b = register(&fx.home(), &b).unwrap().id;
    let learned_b = learned(&fx, "dd-b", &b);
    let global_b = dd(&root, "App-b", Some(&b.join("App.xcodeproj")));

    let out = up(&fx.home(), &id_a).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(learned_b.join("Build/Products/out").exists());
    assert!(global_b.join("Build/Products/out").exists());

    let out = up(&fx.home(), &id_b).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(!learned_b.exists() && !global_b.exists());
}

#[test]
fn up_leaves_a_learned_root_inside_another_registered_tree() {
    let fx = Fixture::new();
    let (_g, _root) = user_home(&fx);
    let a = xcode_tree(&fx, "a");
    let c = xcode_tree(&fx, "c");
    let id_a = register(&fx.home(), &a).unwrap().id;
    register(&fx.home(), &c).unwrap();
    // Built from A's project, but it lives in C.
    let in_c = dd(&c, ".dd-a", Some(&a.join("App.xcodeproj")));
    bale_core::derived_data::learn_root(&fx.home(), &in_c).unwrap();

    up(&fx.home(), &id_a).unwrap();
    assert!(in_c.join("Build/Products/out").exists());
}

#[test]
fn up_with_an_unreadable_record_leaves_derived_data_of_a_nested_checkout() {
    let fx = Fixture::new();
    let (_g, root) = user_home(&fx);
    let a = xcode_tree(&fx, "a");
    let b = a.join(".claude/worktrees/b");
    git(
        &fx.repo,
        &["worktree", "add", "-q", "-b", "b", b.to_str().unwrap()],
    );
    fs::create_dir_all(b.join("App.xcodeproj")).unwrap();
    let id_a = register(&fx.home(), &a).unwrap().id;
    let id_b = register(&fx.home(), &b).unwrap().id;
    // B's record no longer parses, so bale can't tell B is registered.
    fs::write(
        fx.home().0.join(format!("registry/{id_b}.json")),
        "{ not json",
    )
    .unwrap();
    let learned_b = learned(&fx, "dd-b", &b);
    let global_b = dd(&root, "App-b", Some(&b.join("App.xcodeproj")));
    let global_a = dd(&root, "App-a", Some(&a.join("App.xcodeproj")));

    let out = up(&fx.home(), &id_a).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(learned_b.join("Build/Products/out").exists());
    assert!(global_b.join("Build/Products/out").exists());
    // A's own folder still goes.
    assert!(!global_a.exists());
}

#[test]
fn baled_tree_is_not_baled_again_for_derived_data_of_a_nested_tree() {
    let fx = Fixture::new();
    fs::create_dir_all(&fx.home().0).unwrap();
    fs::write(fx.home().0.join("config.toml"), "idle_baled_hours = 0\n").unwrap();
    let (_g, root) = user_home(&fx);
    let a = xcode_tree(&fx, "a");
    let b = a.join(".claude/worktrees/b");
    git(
        &fx.repo,
        &["worktree", "add", "-q", "-b", "b", b.to_str().unwrap()],
    );
    fs::create_dir_all(b.join("App.xcodeproj")).unwrap();
    let id_a = register(&fx.home(), &a).unwrap().id;
    register(&fx.home(), &b).unwrap();
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(&id_a).unwrap().unwrap();
    rec.level = Level::Baled;
    reg.put(&rec).unwrap();
    // Both folders are B's: the default root and a learned root.
    let learned_b = learned(&fx, "dd-b", &b);
    let global_b = dd(&root, "App-b", Some(&b.join("App.xcodeproj")));

    let rows = sweep(&fx.home(), true).unwrap();
    assert_eq!(
        rows.iter().find(|r| r.0 == id_a).unwrap().1,
        Action::Keep,
        "{rows:?}"
    );
    assert!(learned_b.exists() && global_b.exists());
}
