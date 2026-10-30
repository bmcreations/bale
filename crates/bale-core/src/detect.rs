//! Build-system detection and the regenerable directories each one produces.

use crate::config::Config;
use crate::derived_data::{global_owned, is_derived_data_root, Trees};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildSystem {
    Gradle,
    Xcode,
    SwiftPm,
}

pub(crate) const GRADLE_DIRS: &[&str] = &[
    "build",
    ".gradle",
    ".kotlin",
    ".cxx",
    ".externalNativeBuild",
];
pub(crate) const SWIFT_DIRS: &[&str] = &[".build"];

fn marker_system(name: &str) -> Option<BuildSystem> {
    match name {
        "settings.gradle" | "settings.gradle.kts" => Some(BuildSystem::Gradle),
        "Package.swift" => Some(BuildSystem::SwiftPm),
        n if n.ends_with(".xcodeproj") || n.ends_with(".xcworkspace") => Some(BuildSystem::Xcode),
        _ => None,
    }
}

fn scan_markers(dir: &Path, found: &mut Vec<BuildSystem>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(sys) = entry.file_name().to_str().and_then(marker_system) {
            if !found.contains(&sys) {
                found.push(sys);
            }
        }
    }
}

/// Detects build systems from marker files in `root` and its immediate subdirectories.
pub fn detect(root: &Path) -> Vec<BuildSystem> {
    let mut found = Vec::new();
    scan_markers(root, &mut found);
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && entry.file_name() != ".git" {
                scan_markers(&entry.path(), &mut found);
            }
        }
    }
    found
}

/// Lists regenerable directories under `root` for the given build systems plus
/// `cfg.regenerable_extra`. Paths are absolute when `root` is. Symlinks are not
/// followed; a matching symlink is returned as the link itself. On Xcode and
/// SwiftPM trees an Xcode DerivedData root (any name) also matches. A match is
/// never descended into, and `.git` is skipped.
pub fn regenerable_dirs(root: &Path, systems: &[BuildSystem], cfg: &Config) -> Vec<PathBuf> {
    if systems.is_empty() {
        return Vec::new();
    }
    let mut names: Vec<&str> = Vec::new();
    for sys in systems {
        match sys {
            BuildSystem::Gradle => names.extend(GRADLE_DIRS),
            BuildSystem::Xcode | BuildSystem::SwiftPm => names.extend(SWIFT_DIRS),
        }
    }
    names.extend(cfg.regenerable_extra.iter().map(String::as_str));
    let xcode = systems
        .iter()
        .any(|s| matches!(s, BuildSystem::Xcode | BuildSystem::SwiftPm));

    let mut out = Vec::new();
    let mut it = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = it.next() {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let ft = entry.file_type();
        if !(ft.is_dir() || ft.is_symlink()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if ft.is_dir() && name == ".git" {
            it.skip_current_dir();
            continue;
        }
        let derived_data = xcode && ft.is_dir() && is_derived_data_root(entry.path());
        if derived_data || names.iter().any(|n| *n == name) {
            out.push(entry.into_path());
            if ft.is_dir() {
                it.skip_current_dir();
            }
        }
    }
    out
}

/// The dirs whose large files `stash` may replace with empty stubs: a subset
/// of [`regenerable_dirs`] holding only what the build treats as its own
/// output, since a build that reads an empty input builds the wrong thing
/// rather than failing. For Xcode that is `Build/` inside each DerivedData
/// root (not `SourcePackages/` or the module caches, which hold inputs); for
/// Gradle each `build/` dir (not `.gradle/`, `.kotlin/` or `.cxx/`).
/// `regenerable_extra` dirs are left out: bale can't tell what they hold.
/// Symlinked dirs are left out too. On macOS the `Build/` of each DerivedData
/// folder in Xcode's shared DerivedData built from this tree comes last, as a
/// canonical path outside `root`. Of Xcode's shared DerivedData it takes only
/// the folders the tree owns given the registered `trees` (see
/// [`crate::derived_data::DdDir::owned_by`]), as `up` does; there is no
/// variant without `trees`, since prefix-only ownership would take a nested
/// tree's folders.
pub(crate) fn stash_dirs(
    root: &Path,
    systems: &[BuildSystem],
    cfg: &Config,
    trees: &Trees,
) -> Vec<PathBuf> {
    let gradle = systems.contains(&BuildSystem::Gradle);
    let real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    let mut out = Vec::new();
    for dir in regenerable_dirs(root, systems, cfg) {
        if !real_dir(&dir) {
            continue;
        }
        if is_derived_data_root(&dir) {
            let build = dir.join("Build");
            if real_dir(&build) {
                out.push(build);
            }
        } else if gradle && dir.file_name().is_some_and(|n| n == "build") {
            out.push(dir);
        }
    }
    if cfg!(target_os = "macos") {
        for dd in global_owned(root, trees) {
            let build = dd.join("Build");
            if !real_dir(&dd) || !real_dir(&build) {
                continue;
            }
            // Canonical, so a symlinked HOME doesn't hide the folder from
            // the later ownership check.
            if let Ok(build) = crate::paths::canonicalize(&build) {
                if !out.contains(&build) {
                    out.push(build);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::time::Duration;

    fn cfg(extra: &[&str]) -> Config {
        Config {
            idle_baled: Duration::from_secs(4 * 3600),
            idle_cold: Duration::from_secs(7 * 86400),
            idle_stash: Duration::from_secs(600),
            reap_grace: Duration::from_secs(3600),
            share_swift_packages: true,
            stash_min_bytes: 1 << 20,
            patch_max_depth: 4,
            share_checkouts: true,
            clone_min_bytes: 1 << 20,
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

    fn write_plist(dir: &Path, workspace: &Path) {
        fs::create_dir_all(dir).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "WorkspacePath".into(),
            plist::Value::String(workspace.to_string_lossy().into_owned()),
        );
        plist::Value::Dictionary(dict)
            .to_file_xml(dir.join("info.plist"))
            .unwrap();
    }

    #[test]
    fn stash_dirs_are_xcode_build_only() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        fs::create_dir_all(r.join("App.xcodeproj")).unwrap();
        write_plist(&r.join(".dd"), &r.join("App.xcodeproj"));
        for f in [
            ".dd/Build/Products/x",
            ".dd/SourcePackages/checkouts/y",
            ".dd/SDKExplicitPrecompiledModules/z",
            ".dd/ModuleCache.noindex/w",
            ".build/v",
            "out/u",
        ] {
            touch(r, f);
        }
        let sys = detect(r);
        let got = set(stash_dirs(r, &sys, &cfg(&["out"]), &Trees::default()));
        assert_eq!(got, set(vec![r.join(".dd/Build")]));
    }

    #[test]
    fn stash_dirs_are_gradle_build_only() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for f in [
            "settings.gradle.kts",
            "app/build/x",
            "lib/feature/build/y",
            ".gradle/z",
            ".kotlin/k",
            "lib/.cxx/c",
            "src/build.rs",
        ] {
            touch(r, f);
        }
        let sys = detect(r);
        let got = set(stash_dirs(r, &sys, &cfg(&[]), &Trees::default()));
        assert_eq!(
            got,
            set(vec![r.join("app/build"), r.join("lib/feature/build")])
        );
    }

    #[cfg(unix)]
    #[test]
    fn stash_dirs_skip_symlinked_build() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let o = tempfile::tempdir().unwrap();
        touch(r, "settings.gradle.kts");
        fs::create_dir_all(r.join("app")).unwrap();
        std::os::unix::fs::symlink(o.path(), r.join("app/build")).unwrap();
        assert!(stash_dirs(r, &detect(r), &cfg(&[]), &Trees::default()).is_empty());
    }
}
