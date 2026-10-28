use bale_core::config::Config;
use bale_core::detect::{detect, regenerable_dirs, BuildSystem};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn cfg(extra: &[&str]) -> Config {
    Config {
        idle_baled: Duration::from_secs(4 * 3600),
        idle_cold: Duration::from_secs(7 * 86400),
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
