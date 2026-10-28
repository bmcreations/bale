mod support;

use bale_core::derived_data::derived_data_for;
use bale_core::git::worktrees;
use bale_core::ops::up::{register, up, Outcome};
use bale_core::registry::{Level, Registry};
use std::fs;
use std::path::{Path, PathBuf};
use support::Fixture;

/// A linked Gradle worktree with `app/build/big.bin` (1 MB) and `src/Main.kt`.
fn gradle_worktree(fx: &Fixture) -> PathBuf {
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    fs::create_dir_all(wt.join("app/build")).unwrap();
    fs::write(wt.join("app/build/big.bin"), vec![0u8; 1024 * 1024]).unwrap();
    fs::create_dir_all(wt.join("src")).unwrap();
    fs::write(wt.join("src/Main.kt"), "fun main() {}\n").unwrap();
    wt
}

fn linked_lock(fx: &Fixture) -> Option<String> {
    worktrees(&fx.repo)
        .unwrap()
        .into_iter()
        .find(|w| !w.is_main)
        .unwrap()
        .locked
}

fn level(fx: &Fixture, id: &str) -> Level {
    Registry::open(fx.home()).get(id).unwrap().unwrap().level
}

/// Any `.bale-trash-*` entry under `root`, `.git` skipped.
fn trash_left(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".bale-trash-"))
        .map(|e| e.into_path())
        .collect()
}

#[test]
fn bales_gradle_worktree() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    let rec = register(&fx.home(), &wt).unwrap();
    assert_eq!(rec.level, Level::Hot);

    let out = up(&fx.home(), &rec.id).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(!wt.join("app/build").exists());
    assert!(wt.join("src/Main.kt").exists());
    assert!(trash_left(&wt).is_empty());
    // The checkout is still there, so git needs no lock to keep its entry.
    assert_eq!(linked_lock(&fx), None);
    assert_eq!(level(&fx, &rec.id), Level::Baled);
}

#[test]
fn up_on_baled_reruns() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    up(&fx.home(), &id).unwrap();
    fs::create_dir_all(wt.join("app/build")).unwrap();
    fs::write(wt.join("app/build/again.bin"), "x").unwrap();

    let out = up(&fx.home(), &id).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(!wt.join("app/build").exists());
}

#[test]
fn up_without_record_is_an_error() {
    let fx = Fixture::new();
    let err = up(&fx.home(), "000000000000").unwrap_err();
    assert!(format!("{err:#}").contains("register"), "{err:#}");
}

#[cfg(unix)]
#[test]
fn symlinked_build_dir_removes_link_only() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    fs::create_dir_all(wt.join("app")).unwrap();
    let outside = fx.home().0.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, wt.join("app/build")).unwrap();

    let id = register(&fx.home(), &wt).unwrap().id;
    let out = up(&fx.home(), &id).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(fs::symlink_metadata(wt.join("app/build")).is_err());
    assert!(outside.join("keep.txt").exists());
    assert!(trash_left(&wt).is_empty());
}

#[cfg(unix)]
#[test]
fn trash_symlink_is_unlinked_not_followed() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    let outside = fx.home().0.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, wt.join("app/.bale-trash-build-1")).unwrap();

    let mut rec = register(&fx.home(), &wt).unwrap();
    rec.level = Level::Baling;
    Registry::open(fx.home()).put(&rec).unwrap();
    up(&fx.home(), &rec.id).unwrap();
    assert!(fs::symlink_metadata(wt.join("app/.bale-trash-build-1")).is_err());
    assert!(outside.join("keep.txt").exists());
}

#[test]
fn recovers_from_crash() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    let mut rec = register(&fx.home(), &wt).unwrap();
    rec.level = Level::Baling;
    Registry::open(fx.home()).put(&rec).unwrap();
    fs::create_dir_all(wt.join("app/.bale-trash-build-1")).unwrap();
    fs::write(wt.join("app/.bale-trash-build-1/x"), "x").unwrap();

    let out = up(&fx.home(), &rec.id).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(!wt.join("app/.bale-trash-build-1").exists());
    assert_eq!(level(&fx, &rec.id), Level::Baled);
}

fn lock(fx: &Fixture, wt: &Path, reason: Option<&str>) {
    let mut args = vec!["worktree", "lock"];
    if let Some(r) = reason {
        args.extend(["--reason", r]);
    }
    args.push(wt.to_str().unwrap());
    support::git(&fx.repo, &args);
}

#[test]
fn user_locked_tree_is_skipped_untouched() {
    for (reason, want) in [
        (Some("mine"), "locked: mine"),
        (None, "locked: no reason given"),
    ] {
        let fx = Fixture::new();
        let wt = gradle_worktree(&fx);
        let id = register(&fx.home(), &wt).unwrap().id;
        lock(&fx, &wt, reason);
        let out = up(&fx.home(), &id).unwrap();
        assert_eq!(out, Outcome::Skipped(want.into()));
        assert!(wt.join("app/build/big.bin").exists());
        assert_eq!(level(&fx, &id), Level::Hot);
        assert_eq!(linked_lock(&fx).as_deref(), Some(reason.unwrap_or("")));
    }
}

#[test]
fn held_lock_skips() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    let _held = bale_core::lock::WorkspaceLock::try_acquire(&fx.home(), &id)
        .unwrap()
        .unwrap();
    let out = up(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r == "locked"),
        "{out:?}"
    );
    assert!(wt.join("app/build").exists());
}

#[test]
fn cold_skips() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    let mut rec = register(&fx.home(), &wt).unwrap();
    rec.level = Level::Cold;
    Registry::open(fx.home()).put(&rec).unwrap();
    let out = up(&fx.home(), &rec.id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r == "cold"),
        "{out:?}"
    );
    assert!(wt.join("app/build").exists());
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

#[cfg(unix)]
#[test]
fn busy_tree_untouched() {
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    let _child = KillOnDrop(
        std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&wt)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));

    let out = up(&fx.home(), &id).unwrap();
    assert!(matches!(&out, Outcome::Busy(h) if !h.is_empty()), "{out:?}");
    assert!(wt.join("app/build/big.bin").exists());
    assert_eq!(level(&fx, &id), Level::Hot);
    assert_eq!(linked_lock(&fx), None);
}

#[cfg(unix)]
struct RestoreMode(PathBuf);

#[cfg(unix)]
impl Drop for RestoreMode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[cfg(unix)]
#[test]
fn rename_failure_rolls_back() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    fs::create_dir_all(wt.join(".gradle")).unwrap();
    fs::write(wt.join(".gradle/cache.bin"), "c").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    fs::set_permissions(wt.join("app"), fs::Permissions::from_mode(0o555)).unwrap();
    let _guard = RestoreMode(wt.join("app"));

    let out = up(&fx.home(), &id).unwrap();
    assert!(matches!(&out, Outcome::Busy(h) if h.is_empty()), "{out:?}");
    assert!(wt.join("app/build/big.bin").exists());
    assert!(wt.join(".gradle/cache.bin").exists());
    assert!(trash_left(&wt).is_empty(), "{:?}", trash_left(&wt));
    assert_eq!(level(&fx, &id), Level::Hot);
    assert_eq!(linked_lock(&fx), None);
}

#[test]
fn main_checkout_refused() {
    let fx = Fixture::new();
    let err = register(&fx.home(), &fx.repo).unwrap_err();
    assert!(format!("{err:#}").contains("main checkout"), "{err:#}");
}

#[test]
fn non_worktree_refused() {
    let fx = Fixture::new();
    let dir = fx.home().0.parent().unwrap().join("plain");
    fs::create_dir_all(&dir).unwrap();
    assert!(register(&fx.home(), &dir).is_err());
}

fn write_plist(dir: &Path, workspace_path: &Path) {
    fs::create_dir_all(dir).unwrap();
    let mut dict = plist::Dictionary::new();
    dict.insert(
        "WorkspacePath".into(),
        plist::Value::String(workspace_path.to_string_lossy().into_owned()),
    );
    plist::Value::Dictionary(dict)
        .to_file_xml(dir.join("info.plist"))
        .unwrap();
}

#[test]
fn derived_data_matching() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let other = tmp.path().join("other");
    fs::create_dir_all(ws.join("App.xcworkspace")).unwrap();
    fs::create_dir_all(&other).unwrap();
    let dd = tmp.path().join("dd");
    write_plist(&dd.join("A"), &ws.join("App.xcworkspace"));
    write_plist(&dd.join("B"), &other);
    fs::create_dir_all(dd.join("C")).unwrap();
    // A sibling whose name extends the workspace's must not match.
    write_plist(&dd.join("D"), &tmp.path().join("ws2/App.xcworkspace"));
    fs::create_dir_all(dd.join("E")).unwrap();
    fs::write(dd.join("E/info.plist"), "not a plist").unwrap();

    let found = derived_data_for(&ws, &dd);
    let names: Vec<_> = found
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["A"]);
}

#[cfg(unix)]
#[test]
fn unreadable_unrelated_dir_does_not_block_baling() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let wt = gradle_worktree(&fx);
    fs::create_dir_all(wt.join("secret/inner")).unwrap();
    fs::write(wt.join("secret/inner/s.txt"), "s").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    fs::set_permissions(wt.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
    let guard = RestoreMode(wt.join("secret"));

    let out = up(&fx.home(), &id).unwrap();
    assert!(matches!(out, Outcome::Done(Level::Baled)), "{out:?}");
    assert!(!wt.join("app/build").exists());
    assert_eq!(level(&fx, &id), Level::Baled);
    drop(guard);
    assert!(wt.join("secret/inner/s.txt").exists());
    assert!(trash_left(&wt).is_empty());
}
