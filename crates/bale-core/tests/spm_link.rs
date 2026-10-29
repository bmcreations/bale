//! Linking trees to shared Swift packages. Kept in its own test binary: it
//! points HOME and DEVELOPER_DIR at temp dirs, which would race other tests.
#![cfg(target_os = "macos")]

mod support;

use bale_core::derived_data::folder_name;
use bale_core::ops::sweep::sweep;
use bale_core::ops::up::{register, up};
use bale_core::BaleHome;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use support::{write_plist, Fixture};

static HOME_LOCK: Mutex<()> = Mutex::new(());

const BUILD: &str = "TESTBUILD1";
const RESOLVED: &str = r#"{"pins":[{"identity":"a","state":{"revision":"r1"}}],"version":3}"#;
const STATE: &str = r#"{"object":{"dependencies":[{"packageRef":{"identity":"a"},"state":{"checkoutState":{"revision":"r1"}}}]},"version":6}"#;

struct Env {
    fx: Fixture,
    _guard: MutexGuard<'static, ()>,
    dd: PathBuf,
}

/// A fixture with HOME and DEVELOPER_DIR pointing at temp dirs.
fn env() -> Env {
    let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fixture::new();
    let tmp = fx.home().0.parent().unwrap().to_path_buf();
    let user = tmp.join("user");
    std::env::set_var("HOME", &user);
    let dd = user.join("Library/Developer/Xcode/DerivedData");
    fs::create_dir_all(&dd).unwrap();
    // xcode_build() caches the first build it reads, so every test must agree.
    static XCODE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let x = XCODE.get_or_init(|| {
        let x = tempfile::tempdir().unwrap();
        fs::create_dir_all(x.path().join("Developer")).unwrap();
        fs::write(
            x.path().join("version.plist"),
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>ProductBuildVersion</key><string>{BUILD}</string></dict></plist>"),
        )
        .unwrap();
        x
    });
    std::env::set_var("DEVELOPER_DIR", x.path().join("Developer"));
    Env {
        fx,
        _guard: guard,
        dd,
    }
}

fn key() -> String {
    let mut h = Sha256::new();
    h.update(RESOLVED.as_bytes());
    h.update([0]);
    h.update(BUILD.as_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An Xcode project with a `Package.resolved` in `tree`; returns the project.
fn project(tree: &Path) -> PathBuf {
    let p = tree.join("App.xcodeproj");
    let dir = p.join("project.xcworkspace/xcshareddata/swiftpm");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("Package.resolved"), RESOLVED).unwrap();
    p
}

/// DerivedData folder of `project`, as bale and Xcode name it.
fn dd_of(e: &Env, project: &Path) -> PathBuf {
    let canon = bale_core::paths::canonicalize(project).unwrap();
    e.dd.join(folder_name(&canon))
}

/// A finished real `SourcePackages` in `folder`, holding a `marker`.
fn real_packages(folder: &Path) -> PathBuf {
    let sp = folder.join("SourcePackages");
    fs::create_dir_all(&sp).unwrap();
    fs::write(sp.join("workspace-state.json"), STATE).unwrap();
    fs::write(sp.join("marker"), "m").unwrap();
    sp
}

fn disable(h: &BaleHome) {
    fs::create_dir_all(&h.0).unwrap();
    fs::write(h.0.join("config.toml"), "share_swift_packages = false\n").unwrap();
}

fn shared(h: &BaleHome) -> PathBuf {
    h.spm_dir().join(key())
}

fn make_shared(h: &BaleHome) {
    let s = shared(h);
    fs::create_dir_all(&s).unwrap();
    fs::write(s.join("marker"), "m").unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn is_link_to(p: &Path, target: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
        && fs::read_link(p).unwrap() == target
}

#[test]
fn register_links_a_new_tree_to_the_shared_folder() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let rec = register(&h, &wt).unwrap();
    let folder = dd_of(&e, &proj);
    assert_eq!(names(&folder), ["SourcePackages"]);
    assert!(is_link_to(&folder.join("SourcePackages"), &shared(&h)));
    // The record says what was linked, for the hook that relinks on change.
    assert_eq!(rec.spm.len(), 1);
    let l = &rec.spm[0];
    assert_eq!((l.key.as_str(), &l.folder), (key().as_str(), &folder));
    assert_eq!(l.size, RESOLVED.len() as u64);
    assert!(l.resolved.ends_with("swiftpm/Package.resolved"));
    // Registering again keeps one entry.
    assert_eq!(register(&h, &wt).unwrap().spm.len(), 1);
}

#[test]
fn register_leaves_an_existing_folder_alone() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let folder = dd_of(&e, &proj);
    real_packages(&folder);
    register(&h, &wt).unwrap();
    let sp = folder.join("SourcePackages");
    assert!(fs::symlink_metadata(&sp).unwrap().is_dir());
    assert!(sp.join("marker").is_file());
}

#[test]
fn register_creates_nothing_without_a_shared_folder() {
    let e = env();
    let h = e.fx.home();
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    register(&h, &wt).unwrap();
    assert!(fs::symlink_metadata(dd_of(&e, &proj)).is_err());
    assert!(!h.spm_dir().join(key()).exists());
}

#[test]
fn disabled_in_config_means_no_link() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    disable(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    register(&h, &wt).unwrap();
    assert!(fs::symlink_metadata(dd_of(&e, &proj)).is_err());
}

#[test]
fn disabled_in_config_means_no_seeding_from_a_sibling() {
    let e = env();
    let h = e.fx.home();
    disable(&h);
    let main = project(&e.fx.repo);
    real_packages(&dd_of(&e, &main));
    let wt = e.fx.add_worktree("wt", "f");
    project(&wt);
    register(&h, &wt).unwrap();
    assert!(!shared(&h).exists());
}

#[test]
fn up_relinks_after_removing_build_output_and_keeps_the_shared_folder() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let id = register(&h, &wt).unwrap().id;
    // The build Xcode would do into the pre-made folder.
    let folder = dd_of(&e, &proj);
    write_plist(&folder, Some(&proj));
    fs::create_dir_all(folder.join("Build/Products")).unwrap();
    fs::write(folder.join("Build/Products/out"), "x").unwrap();

    up(&h, &id).unwrap();
    assert_eq!(names(&folder), ["SourcePackages"], "fresh pre-made folder");
    assert!(is_link_to(&folder.join("SourcePackages"), &shared(&h)));
    assert_eq!(fs::read_to_string(shared(&h).join("marker")).unwrap(), "m");
}

#[test]
fn up_removes_the_link_without_following_it() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    disable(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let id = register(&h, &wt).unwrap().id;
    let folder = dd_of(&e, &proj);
    write_plist(&folder, Some(&proj));
    fs::create_dir_all(folder.join("Build")).unwrap();
    std::os::unix::fs::symlink(shared(&h), folder.join("SourcePackages")).unwrap();

    up(&h, &id).unwrap();
    assert!(fs::symlink_metadata(&folder).is_err());
    assert_eq!(fs::read_to_string(shared(&h).join("marker")).unwrap(), "m");
}

#[test]
fn up_removes_a_premade_folder_that_never_got_an_info_plist() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    disable(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let id = register(&h, &wt).unwrap().id;
    let folder = dd_of(&e, &proj);
    fs::create_dir_all(&folder).unwrap();
    std::os::unix::fs::symlink(shared(&h), folder.join("SourcePackages")).unwrap();

    up(&h, &id).unwrap();
    assert!(fs::symlink_metadata(&folder).is_err());
    assert_eq!(fs::read_to_string(shared(&h).join("marker")).unwrap(), "m");
}

#[test]
fn register_seeds_from_a_finished_sibling_and_links() {
    let e = env();
    let h = e.fx.home();
    let main = project(&e.fx.repo);
    let main_sp = real_packages(&dd_of(&e, &main));
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    register(&h, &wt).unwrap();

    assert_eq!(fs::read_to_string(shared(&h).join("marker")).unwrap(), "m");
    let folder = dd_of(&e, &proj);
    assert_eq!(names(&folder), ["SourcePackages"]);
    assert!(is_link_to(&folder.join("SourcePackages"), &shared(&h)));
    // The main checkout keeps its real folder.
    assert!(fs::symlink_metadata(&main_sp).unwrap().is_dir());
    assert!(main_sp.join("marker").is_file());
}

#[test]
fn register_does_not_seed_from_an_unfinished_sibling() {
    let e = env();
    let h = e.fx.home();
    let main = project(&e.fx.repo);
    let main_sp = real_packages(&dd_of(&e, &main));
    fs::write(
        main_sp.join("workspace-state.json"),
        r#"{"object":{"dependencies":[]}}"#,
    )
    .unwrap();
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    register(&h, &wt).unwrap();
    assert!(!shared(&h).exists());
    assert!(fs::symlink_metadata(dd_of(&e, &proj)).is_err());
}

#[test]
fn sweep_seeds_from_a_tree_with_a_finished_real_folder() {
    let e = env();
    let h = e.fx.home();
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    register(&h, &wt).unwrap();
    assert!(!shared(&h).exists());
    // The tree builds on its own and finishes resolving.
    let folder = dd_of(&e, &proj);
    write_plist(&folder, Some(&proj));
    let sp = real_packages(&folder);

    sweep(&h, true).unwrap();
    assert!(!shared(&h).exists(), "dry run seeds nothing");
    sweep(&h, false).unwrap();
    assert_eq!(fs::read_to_string(shared(&h).join("marker")).unwrap(), "m");
    assert!(
        fs::symlink_metadata(&sp).unwrap().is_dir(),
        "tree keeps its folder"
    );
    assert!(sp.join("marker").is_file());
}

#[test]
fn sweep_seeds_from_the_main_checkout_of_a_known_repo() {
    let e = env();
    let h = e.fx.home();
    let wt = e.fx.add_worktree("wt", "f");
    register(&h, &wt).unwrap();
    let main = project(&e.fx.repo);
    let main_sp = real_packages(&dd_of(&e, &main));
    write_plist(&dd_of(&e, &main), Some(&main));

    sweep(&h, false).unwrap();
    assert_eq!(fs::read_to_string(shared(&h).join("marker")).unwrap(), "m");
    assert!(fs::symlink_metadata(&main_sp).unwrap().is_dir());
}
