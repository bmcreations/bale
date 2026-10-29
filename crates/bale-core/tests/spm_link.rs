//! Linking trees to shared Swift packages. Kept in its own test binary: it
//! points HOME and DEVELOPER_DIR at temp dirs, which would race other tests.
#![cfg(target_os = "macos")]

mod support;

use bale_core::derived_data::folder_name;
use bale_core::ops::cold::cold;
use bale_core::ops::sweep::sweep;
use bale_core::ops::up::{register, up};
use bale_core::spm_link::{link_derived_data_path, relink_tree};
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

/// Registers a worktree with sharing off, then puts `build` into the folder
/// `up` will look at; returns (id, folder).
fn up_over(e: &Env, build: impl Fn(&Path, &Path)) -> PathBuf {
    let h = e.fx.home();
    make_shared(&h);
    disable(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let id = register(&h, &wt).unwrap().id;
    let folder = dd_of(e, &proj);
    fs::create_dir_all(&folder).unwrap();
    build(&folder, &shared(&h));
    up(&h, &id).unwrap();
    folder
}

#[test]
fn up_keeps_a_folder_xcode_has_started_to_fill() {
    let e = env();
    let folder = up_over(&e, |f, s| {
        std::os::unix::fs::symlink(s, f.join("SourcePackages")).unwrap();
        fs::create_dir_all(f.join("Build")).unwrap();
    });
    assert_eq!(names(&folder), ["Build", "SourcePackages"]);
}

#[test]
fn up_keeps_a_folder_with_a_real_source_packages() {
    let e = env();
    let folder = up_over(&e, |f, _| {
        real_packages(f);
    });
    assert!(folder.join("SourcePackages/marker").is_file());
}

#[test]
fn cold_removes_the_premade_folder() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let id = register(&h, &wt).unwrap().id;
    let folder = dd_of(&e, &proj);
    assert!(folder.exists());
    cold(&h, &id).unwrap();
    assert!(fs::symlink_metadata(&folder).is_err());
    assert!(shared(&h).join("marker").is_file());
    let rec = bale_core::registry::Registry::open(h)
        .get(&id)
        .unwrap()
        .unwrap();
    assert!(rec.spm.is_empty());
}

#[test]
fn sweep_removes_the_premade_folder_of_a_tree_that_is_gone() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let id = register(&h, &wt).unwrap().id;
    let folder = dd_of(&e, &proj);
    assert!(folder.exists());
    fs::remove_dir_all(&wt).unwrap();

    sweep(&h, true).unwrap();
    assert!(folder.exists(), "dry run removes nothing");
    sweep(&h, false).unwrap();
    assert!(fs::symlink_metadata(&folder).is_err());
    assert!(shared(&h).join("marker").is_file());
    let rec = bale_core::registry::Registry::open(h)
        .get(&id)
        .unwrap()
        .unwrap();
    assert!(rec.spm.is_empty());
}

#[test]
fn up_without_a_premade_folder_leaves_no_note() {
    let e = env();
    let h = e.fx.home();
    let wt = e.fx.add_worktree("wt", "f");
    project(&wt);
    let id = register(&h, &wt).unwrap().id;
    up(&h, &id).unwrap();
    let rec = bale_core::registry::Registry::open(h)
        .get(&id)
        .unwrap()
        .unwrap();
    assert_eq!(rec.note, None);
}

#[test]
fn gc_removes_the_premade_folder_of_a_vanished_tree() {
    let e = env();
    let h = e.fx.home();
    make_shared(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let id = register(&h, &wt).unwrap().id;
    let folder = dd_of(&e, &proj);
    assert!(folder.exists());
    support::git(
        &e.fx.repo,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );

    let report = bale_core::ops::gc::gc(&h, false).unwrap();
    assert_eq!(report.removed, vec![id]);
    assert!(fs::symlink_metadata(&folder).is_err());
    assert!(shared(&h).join("marker").is_file());
}

// Pre-tool hook: relink on change and -derivedDataPath.

const RESOLVED2: &str = r#"{"pins":[{"identity":"a","state":{"revision":"r2"}}],"version":3}"#;

fn key_of(resolved: &str) -> String {
    let mut h = Sha256::new();
    h.update(resolved.as_bytes());
    h.update([0]);
    h.update(BUILD.as_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn resolved_of(proj: &Path) -> PathBuf {
    proj.join("project.xcworkspace/xcshareddata/swiftpm/Package.resolved")
}

/// A registered tree linked to the K1 shared folder; returns (id, project, folder).
fn linked_tree(e: &Env) -> (String, PathBuf, PathBuf) {
    let h = e.fx.home();
    make_shared(&h);
    let wt = e.fx.add_worktree("wt", "f");
    let proj = project(&wt);
    let id = register(&h, &wt).unwrap().id;
    let folder = dd_of(e, &proj);
    assert!(is_link_to(&folder.join("SourcePackages"), &shared(&h)));
    (id, proj, folder)
}

fn record(h: &BaleHome, id: &str) -> bale_core::registry::Record {
    bale_core::registry::Registry::open(h.clone())
        .get(id)
        .unwrap()
        .unwrap()
}

#[test]
fn relink_repoints_to_an_existing_shared_folder_for_the_new_key() {
    let e = env();
    let h = e.fx.home();
    let (id, proj, folder) = linked_tree(&e);
    let k2 = h.spm_dir().join(key_of(RESOLVED2));
    fs::create_dir_all(&k2).unwrap();
    fs::write(k2.join("other"), "o").unwrap();
    fs::write(resolved_of(&proj), RESOLVED2).unwrap();

    relink_tree(&h, &id);

    assert!(is_link_to(&folder.join("SourcePackages"), &k2));
    assert_eq!(names(&k2), ["other"], "an existing K2 is not touched");
    let l = &record(&h, &id).spm[0];
    assert_eq!(l.key, key_of(RESOLVED2));
    assert_eq!(l.size, RESOLVED2.len() as u64);
    assert_eq!(
        l.mtime,
        fs::metadata(resolved_of(&proj))
            .unwrap()
            .modified()
            .unwrap()
    );
    // No temp link left behind, K1 kept for other trees.
    assert_eq!(names(&folder), ["SourcePackages"]);
    assert!(shared(&h).join("marker").is_file());
}

#[test]
fn relink_seeds_a_clone_of_the_old_folder_when_the_new_key_has_none() {
    let e = env();
    let h = e.fx.home();
    let (id, proj, folder) = linked_tree(&e);
    fs::write(resolved_of(&proj), RESOLVED2).unwrap();

    relink_tree(&h, &id);

    let k2 = h.spm_dir().join(key_of(RESOLVED2));
    assert!(is_link_to(&folder.join("SourcePackages"), &k2));
    assert_eq!(fs::read_to_string(k2.join("marker")).unwrap(), "m");
    assert!(shared(&h).join("marker").is_file(), "K1 untouched");
    assert_eq!(record(&h, &id).spm[0].key, key_of(RESOLVED2));
}

#[test]
fn relink_with_an_unchanged_key_only_refreshes_mtime_and_size() {
    let e = env();
    let h = e.fx.home();
    let (id, proj, folder) = linked_tree(&e);
    let before = fs::symlink_metadata(folder.join("SourcePackages"))
        .unwrap()
        .modified()
        .unwrap();
    // Same bytes, later mtime (as `git checkout` rewrites a file).
    let f = fs::File::options()
        .write(true)
        .open(resolved_of(&proj))
        .unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    f.set_modified(later).unwrap();
    drop(f);

    relink_tree(&h, &id);

    let l = &record(&h, &id).spm[0];
    assert_eq!(l.key, key());
    assert_eq!(l.mtime, later);
    assert_eq!(names(&h.spm_dir()), [key()], "nothing seeded");
    let after = fs::symlink_metadata(folder.join("SourcePackages"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after, "link not recreated");
}

#[test]
fn relink_never_touches_a_real_source_packages_dir() {
    let e = env();
    let h = e.fx.home();
    let (id, proj, folder) = linked_tree(&e);
    // Xcode (or the user) replaced the link with a real folder.
    fs::remove_file(folder.join("SourcePackages")).unwrap();
    let sp = real_packages(&folder);
    fs::write(resolved_of(&proj), RESOLVED2).unwrap();

    relink_tree(&h, &id);

    assert!(fs::symlink_metadata(&sp).unwrap().is_dir());
    assert!(sp.join("marker").is_file());
    assert_eq!(names(&folder), ["SourcePackages"]);
    assert_eq!(names(&h.spm_dir()), [key()], "nothing seeded for it");
}

#[test]
fn relink_leaves_a_link_to_somewhere_else_alone() {
    let e = env();
    let h = e.fx.home();
    let (id, proj, folder) = linked_tree(&e);
    let elsewhere = e.dd.parent().unwrap().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::remove_file(folder.join("SourcePackages")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, folder.join("SourcePackages")).unwrap();
    fs::write(resolved_of(&proj), RESOLVED2).unwrap();

    relink_tree(&h, &id);

    assert!(is_link_to(&folder.join("SourcePackages"), &elsewhere));
}

#[test]
fn relink_does_nothing_when_sharing_is_disabled() {
    let e = env();
    let h = e.fx.home();
    let (id, proj, folder) = linked_tree(&e);
    disable(&h);
    fs::write(resolved_of(&proj), RESOLVED2).unwrap();

    relink_tree(&h, &id);

    assert!(is_link_to(&folder.join("SourcePackages"), &shared(&h)));
    assert_eq!(record(&h, &id).spm[0].key, key());
}

#[test]
fn relink_skips_a_locked_tree_and_retries_later() {
    let e = env();
    let h = e.fx.home();
    let (id, proj, folder) = linked_tree(&e);
    fs::write(resolved_of(&proj), RESOLVED2).unwrap();
    {
        let _held = bale_core::lock::WorkspaceLock::try_acquire(&h, &id)
            .unwrap()
            .unwrap();
        relink_tree(&h, &id);
        assert!(is_link_to(&folder.join("SourcePackages"), &shared(&h)));
    }
    relink_tree(&h, &id);
    assert!(is_link_to(
        &folder.join("SourcePackages"),
        &h.spm_dir().join(key_of(RESOLVED2))
    ));
}

#[test]
fn relink_of_an_unchanged_tree_writes_nothing_and_is_fast() {
    let e = env();
    let h = e.fx.home();
    let (id, _proj, folder) = linked_tree(&e);
    let rec_file = h.records_dir().join(format!("{id}.json"));
    let stamp = |p: &Path| fs::symlink_metadata(p).unwrap().modified().unwrap();
    let (r0, l0, d0) = (
        stamp(&rec_file),
        stamp(&folder.join("SourcePackages")),
        stamp(&folder),
    );
    let before = (names(&h.spm_dir()), fs::read(&rec_file).unwrap());

    const N: u32 = 1000;
    let start = std::time::Instant::now();
    for _ in 0..N {
        relink_tree(&h, &id);
    }
    let mean = start.elapsed() / N;
    eprintln!("relink_tree, unchanged: {mean:?} per call over {N} calls");

    assert_eq!(
        (
            stamp(&rec_file),
            stamp(&folder.join("SourcePackages")),
            stamp(&folder)
        ),
        (r0, l0, d0)
    );
    assert_eq!(before, (names(&h.spm_dir()), fs::read(&rec_file).unwrap()));
    assert!(mean < std::time::Duration::from_millis(5), "{mean:?}");
}

#[test]
fn derived_data_path_without_source_packages_gets_the_shared_link() {
    let e = env();
    let h = e.fx.home();
    let (id, _proj, _folder) = linked_tree(&e);
    let root = e.dd.parent().unwrap().join("my dd");

    link_derived_data_path(&h, &id, &root);
    assert!(is_link_to(&root.join("SourcePackages"), &shared(&h)));

    // An existing folder without SourcePackages gets the link too.
    let root2 = e.dd.parent().unwrap().join("dd2");
    fs::create_dir_all(root2.join("Build")).unwrap();
    link_derived_data_path(&h, &id, &root2);
    assert!(is_link_to(&root2.join("SourcePackages"), &shared(&h)));
    assert!(root2.join("Build").is_dir());
}

#[test]
fn derived_data_path_with_source_packages_is_untouched() {
    let e = env();
    let h = e.fx.home();
    let (id, _proj, _folder) = linked_tree(&e);
    let root = e.dd.parent().unwrap().join("dd");
    let sp = real_packages(&root);
    link_derived_data_path(&h, &id, &root);
    assert!(fs::symlink_metadata(&sp).unwrap().is_dir());
    assert!(sp.join("marker").is_file());

    let root2 = e.dd.parent().unwrap().join("dd2");
    fs::create_dir_all(&root2).unwrap();
    std::os::unix::fs::symlink("/nonexistent", root2.join("SourcePackages")).unwrap();
    link_derived_data_path(&h, &id, &root2);
    assert!(is_link_to(
        &root2.join("SourcePackages"),
        Path::new("/nonexistent")
    ));
}

#[test]
fn derived_data_path_never_seeds_or_links_without_a_shared_folder() {
    let e = env();
    let h = e.fx.home();
    let (id, _proj, _folder) = linked_tree(&e);
    fs::remove_dir_all(shared(&h)).unwrap();
    let root = e.dd.parent().unwrap().join("dd");
    link_derived_data_path(&h, &id, &root);
    assert!(fs::symlink_metadata(&root).is_err(), "root not created");
    assert!(!h.spm_dir().exists() || names(&h.spm_dir()).is_empty());
}

#[test]
fn derived_data_path_does_nothing_when_sharing_is_disabled() {
    let e = env();
    let h = e.fx.home();
    let (id, _proj, _folder) = linked_tree(&e);
    disable(&h);
    let root = e.dd.parent().unwrap().join("dd");
    link_derived_data_path(&h, &id, &root);
    assert!(fs::symlink_metadata(&root).is_err());
}

#[test]
fn relink_that_cannot_proceed_is_not_retried_until_the_next_edit() {
    let e = env();
    let h = e.fx.home();
    let (id, proj, folder) = linked_tree(&e);
    fs::remove_dir_all(shared(&h)).unwrap();
    fs::write(resolved_of(&proj), RESOLVED2).unwrap();

    relink_tree(&h, &id);

    let l = record(&h, &id).spm[0].clone();
    assert_eq!(l.key, key(), "old key kept");
    assert_eq!(l.size, RESOLVED2.len() as u64);
    assert_eq!(
        l.mtime,
        fs::metadata(resolved_of(&proj))
            .unwrap()
            .modified()
            .unwrap()
    );
    assert!(fs::symlink_metadata(folder.join("SourcePackages")).is_ok());
    // Second call: fast path, so the record file is not rewritten.
    let rec_file = h.records_dir().join(format!("{id}.json"));
    let before = fs::read(&rec_file).unwrap();
    // A held lock would make a retry bail out silently, so check the record
    // is unchanged and the fast-path predicate is false via an unchanged stamp.
    relink_tree(&h, &id);
    assert_eq!(fs::read(&rec_file).unwrap(), before);
    assert!(!h.spm_dir().join(key_of(RESOLVED2)).exists());
}
