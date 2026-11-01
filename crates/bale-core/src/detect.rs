//! Build-system detection and the regenerable directories each one produces.

use crate::config::Config;
use crate::derived_data::{global_owned, is_derived_data_root, Trees};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// A build system bale knows. Each one's knowledge is its [`Spec`]; to add a
/// system, add a variant here, list it in [`BuildSystem::ALL`] and give it a
/// spec in [`BuildSystem::spec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildSystem {
    Gradle,
    Xcode,
    SwiftPm,
}

/// A file or directory name that shows a build system is in use.
#[derive(Debug, Clone, Copy)]
pub enum Marker {
    /// The whole name, such as `settings.gradle`.
    Name(&'static str),
    /// A name ending, such as `.xcodeproj`.
    Suffix(&'static str),
}

impl Marker {
    fn matches(self, name: &str) -> bool {
        match self {
            Marker::Name(n) => name == n,
            Marker::Suffix(s) => name.ends_with(s),
        }
    }
}

/// What bale knows about one build system's output in a tree.
#[derive(Debug)]
pub struct Spec {
    /// Names that mark a tree as using this system, at its root or one level
    /// down.
    pub markers: &'static [Marker],
    /// Directory names, anywhere in the tree, that the build recreates. `up`
    /// deletes them (unless they hold tracked or unignored files), and idle
    /// time ignores changes inside them.
    pub regenerable: &'static [&'static str],
    /// The names in `regenerable` whose large files `stash` may replace with
    /// empty stubs. Only list a directory that holds nothing but the build's
    /// own output: a build that reads an emptied input builds the wrong thing
    /// rather than failing. Leave it empty until a stash and restore of a real
    /// build has been measured.
    pub stash: &'static [&'static str],
    /// Whether an Xcode DerivedData root, by any name, counts as regenerable,
    /// with its `Build/` stashable.
    pub derived_data: bool,
}

const GRADLE: Spec = Spec {
    markers: &[
        Marker::Name("settings.gradle"),
        Marker::Name("settings.gradle.kts"),
    ],
    regenerable: &[
        "build",
        ".gradle",
        ".kotlin",
        ".cxx",
        ".externalNativeBuild",
    ],
    stash: &["build"],
    derived_data: false,
};

const XCODE: Spec = Spec {
    markers: &[Marker::Suffix(".xcodeproj"), Marker::Suffix(".xcworkspace")],
    regenerable: &[".build"],
    stash: &[],
    derived_data: true,
};

const SWIFT_PM: Spec = Spec {
    markers: &[Marker::Name("Package.swift")],
    regenerable: &[".build"],
    stash: &[],
    derived_data: true,
};

impl BuildSystem {
    /// Every system, in the order `detect` checks markers.
    pub const ALL: &'static [BuildSystem] = &[
        BuildSystem::Gradle,
        BuildSystem::Xcode,
        BuildSystem::SwiftPm,
    ];

    pub fn spec(self) -> &'static Spec {
        match self {
            BuildSystem::Gradle => &GRADLE,
            BuildSystem::Xcode => &XCODE,
            BuildSystem::SwiftPm => &SWIFT_PM,
        }
    }
}

/// Whether `name` is a regenerable directory name of any system.
pub(crate) fn is_any_regenerable_name(name: &str) -> bool {
    BuildSystem::ALL
        .iter()
        .any(|s| s.spec().regenerable.contains(&name))
}

fn marker_system(name: &str) -> Option<BuildSystem> {
    BuildSystem::ALL
        .iter()
        .copied()
        .find(|s| s.spec().markers.iter().any(|m| m.matches(name)))
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
        names.extend(sys.spec().regenerable);
    }
    names.extend(cfg.regenerable_extra.iter().map(String::as_str));
    let derived_data = systems.iter().any(|s| s.spec().derived_data);

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
        let dd_root = derived_data && ft.is_dir() && is_derived_data_root(entry.path());
        if dd_root || names.iter().any(|n| *n == name) {
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
/// root (not `SourcePackages/` or the module caches, which hold inputs);
/// otherwise the dirs named in each system's [`Spec::stash`], which for
/// Gradle is each `build/` dir (not `.gradle/`, `.kotlin/` or `.cxx/`).
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
    let names: Vec<&str> = systems
        .iter()
        .flat_map(|s| s.spec().stash)
        .copied()
        .collect();
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
        } else if dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| names.contains(&n))
        {
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
            keep_paths: None,
            keep_max_file_bytes: 1 << 20,
            keep_max_total_bytes: 10 << 20,
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

    #[test]
    fn every_spec_is_complete() {
        for sys in BuildSystem::ALL {
            let spec = sys.spec();
            assert!(!spec.markers.is_empty(), "{sys:?} has no markers");
            for name in spec.stash {
                assert!(
                    spec.regenerable.contains(name),
                    "{sys:?} stashes {name}, which is not regenerable"
                );
            }
        }
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
