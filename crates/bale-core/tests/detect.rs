mod support;

use bale_core::config::Config;
use bale_core::detect::{detect, regenerable_dirs, BuildSystem};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use support::{write_plist, write_plist_binary};

fn cfg(extra: &[&str]) -> Config {
    Config {
        idle_baled: Duration::from_secs(4 * 3600),
        idle_cold: Duration::from_secs(7 * 86400),
        idle_stash: Duration::from_secs(600),
        reap_grace: Duration::from_secs(3600),
        share_swift_packages: true,
        stash_min_bytes: 1 << 20,
        patch_max_depth: 4,
        regenerable_extra: extra.iter().map(|s| s.to_string()).collect(),
        ignored_denylist_extra: vec![],
        worktree_root: None,
    }
}

fn touch(root: &Path, rel: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, "x").unwrap();
}

fn set(v: Vec<PathBuf>) -> BTreeSet<PathBuf> {
    v.into_iter().collect()
}

#[test]
fn gradle_tree() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    for f in [
        "settings.gradle.kts",
        "app/build/x",
        "lib/.cxx/y",
        ".gradle/z",
        "src/build.rs",
    ] {
        touch(r, f);
    }
    let sys = detect(r);
    assert_eq!(sys, vec![BuildSystem::Gradle]);
    let got = set(regenerable_dirs(r, &sys, &cfg(&[])));
    let want = set(vec![
        r.join("app/build"),
        r.join("lib/.cxx"),
        r.join(".gradle"),
    ]);
    assert_eq!(got, want);
}

#[test]
fn nested_build_inside_build_is_pruned() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "settings.gradle");
    touch(r, "app/build/intermediates/build/f");
    let got = regenerable_dirs(r, &detect(r), &cfg(&[]));
    assert_eq!(got, vec![r.join("app/build")]);
}

#[test]
fn xcode_in_subfolder() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join("Ios/App.xcodeproj")).unwrap();
    touch(r, ".build/f");
    let sys = detect(r);
    assert!(sys.contains(&BuildSystem::Xcode));
    assert_eq!(regenerable_dirs(r, &sys, &cfg(&[])), vec![r.join(".build")]);
}

#[test]
fn config_extra_dir() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "Package.swift");
    touch(r, "out/f");
    let got = regenerable_dirs(r, &detect(r), &cfg(&["out"]));
    assert_eq!(got, vec![r.join("out")]);
}

#[test]
fn none_detected_lists_nothing() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "build/f");
    let sys = detect(r);
    assert!(sys.is_empty());
    assert!(regenerable_dirs(r, &sys, &cfg(&[])).is_empty());
}

#[test]
fn skips_git_and_returns_symlink_itself() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "settings.gradle");
    touch(r, ".git/build/f");
    touch(r, "real/f");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(r.join("real"), r.join("build")).unwrap();
        let got = regenerable_dirs(r, &detect(r), &cfg(&[]));
        assert_eq!(got, vec![r.join("build")]);
    }
    #[cfg(not(unix))]
    {
        let got = regenerable_dirs(r, &detect(r), &cfg(&[]));
        assert!(got.is_empty());
    }
}

#[test]
fn xcode_derived_data_roots_are_regenerable() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    fs::create_dir_all(r.join("App.xcodeproj")).unwrap();
    // Real DerivedData roots, whatever their name.
    for name in ["build/dd", "DerivedData"] {
        write_plist(&r.join(name), Some(Path::new("/src/App.xcworkspace")));
        fs::create_dir_all(r.join(name).join("Build")).unwrap();
    }
    // info.plist but no Build/.
    write_plist(&r.join("no-build"), Some(Path::new("/src/App.xcworkspace")));
    // Build/ but no plist.
    fs::create_dir_all(r.join("no-plist/Build")).unwrap();
    // Build/ with an unparseable plist.
    fs::create_dir_all(r.join("bad-plist/Build")).unwrap();
    fs::write(r.join("bad-plist/info.plist"), "not a plist").unwrap();
    // Build/ with a plist lacking WorkspacePath.
    write_plist(&r.join("no-key"), None);
    fs::create_dir_all(r.join("no-key/Build")).unwrap();

    let got = set(regenerable_dirs(r, &detect(r), &cfg(&[])));
    assert_eq!(got, set(vec![r.join("build/dd"), r.join("DerivedData")]));
}

#[test]
fn derived_data_is_ignored_without_xcode_or_swiftpm() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "settings.gradle.kts");
    write_plist(&r.join("dd"), Some(Path::new("/src/App.xcworkspace")));
    fs::create_dir_all(r.join("dd/Build")).unwrap();
    assert!(regenerable_dirs(r, &detect(r), &cfg(&[])).is_empty());
}

#[test]
fn derived_data_is_not_descended_into() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "Package.swift");
    write_plist(&r.join("dd"), Some(Path::new("/src/App.xcworkspace")));
    fs::create_dir_all(r.join("dd/Build")).unwrap();
    touch(r, "dd/SourcePackages/.build/x");
    assert_eq!(
        regenerable_dirs(r, &detect(r), &cfg(&[])),
        vec![r.join("dd")]
    );
}

#[test]
fn binary_info_plist_is_a_derived_data_root() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "Package.swift");
    write_plist_binary(&r.join("dd"), Some(Path::new("/src/App.xcworkspace")));
    fs::create_dir_all(r.join("dd/Build")).unwrap();
    assert_eq!(
        regenerable_dirs(r, &detect(r), &cfg(&[])),
        vec![r.join("dd")]
    );
}

/// A DerivedData-shaped dir outside the tree, for the symlink cases.
#[cfg(unix)]
fn outside_derived_data() -> tempfile::TempDir {
    let o = tempfile::tempdir().unwrap();
    write_plist(o.path(), Some(Path::new("/src/App.xcworkspace")));
    fs::create_dir_all(o.path().join("Build")).unwrap();
    o
}

#[cfg(unix)]
#[test]
fn symlink_to_a_derived_data_dir_is_not_a_root() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "Package.swift");
    let o = outside_derived_data();
    std::os::unix::fs::symlink(o.path(), r.join("dd-link")).unwrap();
    assert!(regenerable_dirs(r, &detect(r), &cfg(&[])).is_empty());
}

#[cfg(unix)]
#[test]
fn symlinked_info_plist_is_not_a_root() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "Package.swift");
    let o = outside_derived_data();
    fs::create_dir_all(r.join("dd/Build")).unwrap();
    std::os::unix::fs::symlink(o.path().join("info.plist"), r.join("dd/info.plist")).unwrap();
    assert!(regenerable_dirs(r, &detect(r), &cfg(&[])).is_empty());
}

#[cfg(unix)]
#[test]
fn symlinked_build_dir_is_not_a_root() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path();
    touch(r, "Package.swift");
    let o = outside_derived_data();
    write_plist(&r.join("dd"), Some(Path::new("/src/App.xcworkspace")));
    std::os::unix::fs::symlink(o.path().join("Build"), r.join("dd/Build")).unwrap();
    assert!(regenerable_dirs(r, &detect(r), &cfg(&[])).is_empty());
}
