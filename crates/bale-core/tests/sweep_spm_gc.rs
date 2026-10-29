//! Sweep's removal of shared packages folders (`<home>/spm/<key>`) nothing
//! links to. Own test binary: it points HOME at a temp dir.
#![cfg(target_os = "macos")]

mod support;

use bale_core::ops::sweep::{sweep, Action};
use bale_core::ops::up::register;
use bale_core::registry::{Registry, SpmLink};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};
use support::{write_plist, Fixture};

static HOME_LOCK: Mutex<()> = Mutex::new(());

struct Env {
    fx: Fixture,
    _guard: MutexGuard<'static, ()>,
    /// Xcode's shared DerivedData under the temp HOME.
    dd: PathBuf,
}

fn env() -> Env {
    let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::new();
    let user = fx.home().0.parent().unwrap().join("user");
    std::env::set_var("HOME", &user);
    let dd = user.join("Library/Developer/Xcode/DerivedData");
    fs::create_dir_all(&dd).unwrap();
    Env {
        fx,
        _guard: guard,
        dd,
    }
}

impl Env {
    fn spm(&self, key: &str) -> PathBuf {
        self.fx.home().spm_dir().join(key)
    }

    /// `spm/<key>` as a finished clone holds it, last changed `hours` ago.
    fn shared(&self, key: &str, hours: u64) -> PathBuf {
        let dir = self.spm(key);
        fs::create_dir_all(dir.join("checkouts/a")).unwrap();
        fs::write(dir.join("workspace-state.json"), "{}").unwrap();
        fs::write(dir.join("checkouts/a/f"), vec![0u8; 4096]).unwrap();
        age(&dir, hours);
        dir
    }

    fn learn(&self, roots: &[&Path]) {
        fs::create_dir_all(&self.fx.home().0).unwrap();
        let lines: Vec<String> = roots.iter().map(|p| p.display().to_string()).collect();
        fs::write(self.fx.home().dd_roots_file(), lines.join("\n")).unwrap();
    }

    fn rows(&self, dry_run: bool) -> Vec<(String, String)> {
        sweep(&self.fx.home(), dry_run)
            .unwrap()
            .into_iter()
            .filter(|r| r.1 == Action::Reap && r.0.contains("/spm/"))
            .map(|r| (r.0, r.2))
            .collect()
    }

    fn op_lines(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let Ok(dir) = fs::read_dir(self.fx.home().trace_dir()) else {
            return out;
        };
        for e in dir.flatten() {
            for line in fs::read_to_string(e.path()).unwrap().lines() {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                if v["kind"] == "op" && v["op"] == "spm_gc" {
                    out.push(v);
                }
            }
        }
        out
    }
}

/// Backdates `dir` and everything directly in it, and the files below.
fn age(dir: &Path, hours: u64) {
    let t =
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(hours * 3600));
    for e in walkdir::WalkDir::new(dir)
        .contents_first(true)
        .into_iter()
        .flatten()
    {
        filetime::set_symlink_file_times(e.path(), t, t).unwrap();
    }
}

/// `<root>/<name>/SourcePackages` linked to `target`; returns the link.
fn link_in(root: &Path, name: &str, target: &Path) -> PathBuf {
    let folder = root.join(name);
    fs::create_dir_all(&folder).unwrap();
    let sp = folder.join("SourcePackages");
    symlink(target, &sp).unwrap();
    sp
}

#[test]
fn unreferenced_old_folder_is_removed_and_traced() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let rows = e.rows(false);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].1.starts_with("removed"), "{rows:?}");
    assert!(!k.exists());
    let ops = e.op_lines();
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["outcome"], "ok");
    assert!(ops[0]["bytes_freed"].as_u64().unwrap() >= 4096);
    assert!(!ops[0].to_string().contains('/'), "no paths: {}", ops[0]);
}

#[test]
fn a_newer_folder_survives() {
    let e = env();
    let k = e.shared("aaaa", 0);
    assert!(e.rows(false).is_empty());
    assert!(k.exists());
}

#[test]
fn a_recent_change_inside_keeps_the_folder() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let t = filetime::FileTime::now();
    filetime::set_file_mtime(k.join("workspace-state.json"), t).unwrap();
    assert!(e.rows(false).is_empty());
    assert!(k.exists());
}

#[test]
fn linked_from_global_derived_data_survives() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let other = e.shared("bbbb", 2);
    link_in(&e.dd, "App-abc", &k);
    let rows = e.rows(false);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(k.exists() && !other.exists());
}

#[test]
fn linked_from_a_learned_root_survives() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let root = e.fx.repo.parent().unwrap().join("custom-dd");
    write_plist(&root, Some(&e.fx.repo.join("X.xcodeproj")));
    fs::create_dir_all(root.join("Build")).unwrap();
    symlink(&k, root.join("SourcePackages")).unwrap();
    e.learn(&[&root]);
    assert!(e.rows(false).is_empty());
    assert!(k.exists());
}

#[test]
fn a_fresh_learned_root_holding_only_the_link_survives() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let root = e.fx.repo.parent().unwrap().join("fresh-dd");
    fs::create_dir_all(&root).unwrap();
    symlink(&k, root.join("SourcePackages")).unwrap();
    e.learn(&[&root]);
    assert!(e.rows(false).is_empty());
    assert!(k.exists());
}

#[test]
fn a_link_one_level_below_a_learned_root_survives() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let root = e.fx.repo.parent().unwrap().join("fc-dd");
    link_in(&root, "App-abc", &k);
    e.learn(&[&root]);
    assert!(e.rows(false).is_empty());
    assert!(k.exists());
}

#[test]
fn linked_from_a_registered_trees_dd_folder_survives() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let wt = e.fx.add_worktree("wt", "wt");
    register(&e.fx.home(), &wt).unwrap();
    link_in(&wt.join(".dd"), "App-abc", &k);
    assert!(e.rows(false).is_empty());
    assert!(k.exists());
}

#[test]
fn referenced_only_by_a_record_survives() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let wt = e.fx.add_worktree("wt", "wt");
    let id = register(&e.fx.home(), &wt).unwrap().id;
    // The folder sits where no scan looks; the project exists, so it is no stub.
    let folder = e.fx.repo.parent().unwrap().join("elsewhere/App-x");
    fs::create_dir_all(&folder).unwrap();
    symlink(&k, folder.join("SourcePackages")).unwrap();
    fs::write(folder.join("note"), "x").unwrap();
    add_spm(&e, &id, &wt.join("App.xcodeproj"), "aaaa", &folder);
    assert!(e.rows(false).is_empty());
    assert!(k.exists());
}

fn add_spm(e: &Env, id: &str, project: &Path, key: &str, folder: &Path) {
    let reg = Registry::open(e.fx.home());
    let mut rec = reg.get(id).unwrap().unwrap();
    rec.spm.push(SpmLink {
        project: project.to_path_buf(),
        resolved: project.join("Package.resolved"),
        mtime: SystemTime::now(),
        size: 1,
        key: key.into(),
        folder: folder.to_path_buf(),
    });
    reg.put(&rec).unwrap();
}

#[test]
fn a_textually_different_target_still_counts() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let canon = fs::canonicalize(&k).unwrap();
    // /var/... and /private/var/... are the same folder.
    let alt = if k == canon {
        PathBuf::from(format!("/private{}", k.display()))
    } else {
        canon
    };
    assert_ne!(alt, k);
    link_in(&e.dd, "App-abc", &alt);
    assert!(e.rows(false).is_empty());
    assert!(k.exists());
}

#[test]
fn old_tmp_folders_are_removed_and_new_ones_kept() {
    let e = env();
    let old = e.shared(".tmp-aaaa-1-0", 2);
    let new = e.shared(".tmp-bbbb-1-0", 0);
    let rows = e.rows(false);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(!old.exists() && new.exists());
}

#[test]
fn a_stub_whose_project_is_gone_is_removed_with_its_folder() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let wt = e.fx.add_worktree("wt", "wt");
    let id = register(&e.fx.home(), &wt).unwrap().id;
    let folder = e.dd.join("App-gone");
    fs::create_dir_all(&folder).unwrap();
    symlink(&k, folder.join("SourcePackages")).unwrap();
    age(&folder, 2);
    add_spm(&e, &id, &wt.join("App.xcodeproj"), "aaaa", &folder);
    e.rows(false);
    assert!(!folder.exists());
    assert!(!k.exists(), "nothing links to it any more");
    let rec = Registry::open(e.fx.home()).get(&id).unwrap().unwrap();
    assert!(rec.spm.is_empty());
}

#[test]
fn a_stub_whose_project_exists_stays() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let wt = e.fx.add_worktree("wt", "wt");
    fs::create_dir_all(wt.join("App.xcodeproj")).unwrap();
    let id = register(&e.fx.home(), &wt).unwrap().id;
    let folder = e.dd.join("App-live");
    fs::create_dir_all(&folder).unwrap();
    symlink(&k, folder.join("SourcePackages")).unwrap();
    age(&folder, 2);
    add_spm(&e, &id, &wt.join("App.xcodeproj"), "aaaa", &folder);
    assert!(e.rows(false).is_empty());
    assert!(folder.exists() && k.exists());
}

#[test]
fn a_dangling_link_is_not_followed() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let outside = e.fx.repo.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep"), "x").unwrap();
    let gone = outside.join("missing");
    let sp = link_in(&e.dd, "App-abc", &gone);
    // A link in spm/ itself is not a folder to delete or follow either.
    symlink(&outside, e.fx.home().spm_dir().join("sneaky")).unwrap();
    let rows = e.rows(false);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(!k.exists());
    assert!(fs::symlink_metadata(&sp).is_ok());
    assert!(outside.join("keep").exists());
    assert!(fs::symlink_metadata(e.fx.home().spm_dir().join("sneaky")).is_ok());
}

#[test]
fn an_unreadable_root_stops_the_sweep_from_deleting() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let root = e.fx.repo.parent().unwrap().join("locked-dd");
    fs::create_dir_all(&root).unwrap();
    e.learn(&[&root]);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    let rows = e.rows(false);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(rows.is_empty(), "{rows:?}");
    assert!(k.exists());
    let ops = e.op_lines();
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["outcome"], "skipped");
    assert_eq!(ops[0]["reason"], "PermissionDenied");
}

#[test]
fn an_unreadable_dd_roots_file_stops_the_sweep_from_deleting() {
    let e = env();
    let k = e.shared("aaaa", 2);
    e.learn(&[]);
    let f = e.fx.home().dd_roots_file();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
    let rows = e.rows(false);
    fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(rows.is_empty(), "{rows:?}");
    assert!(k.exists());
}

#[test]
fn a_dry_run_reports_and_deletes_nothing() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let tmp = e.shared(".tmp-bbbb-1-0", 2);
    let rows = e.rows(true);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter().all(|r| r.1.starts_with("would remove")),
        "{rows:?}"
    );
    assert!(k.exists() && tmp.exists());
    assert!(e.op_lines().is_empty());
}

#[test]
fn cleanup_runs_even_when_sharing_is_off() {
    let e = env();
    fs::create_dir_all(&e.fx.home().0).unwrap();
    fs::write(
        e.fx.home().0.join("config.toml"),
        "share_swift_packages = false\n",
    )
    .unwrap();
    let k = e.shared("aaaa", 2);
    assert_eq!(e.rows(false).len(), 1);
    assert!(!k.exists());
}

#[test]
fn a_folder_a_process_holds_is_left() {
    let e = env();
    let k = e.shared("aaaa", 2);
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(&k)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let rows = e.rows(false);
    let _ = child.kill();
    let _ = child.wait();
    assert!(k.exists(), "{rows:?}");
}
