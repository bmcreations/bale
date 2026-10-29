//! Build-system detection and the regenerable directories each one produces.

use crate::config::Config;
use crate::derived_data::{global_for, is_derived_data_root};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildSystem {
    Gradle,
    Xcode,
    SwiftPm,
}

const GRADLE_DIRS: &[&str] = &[
    "build",
    ".gradle",
    ".kotlin",
    ".cxx",
    ".externalNativeBuild",
];
const SWIFT_DIRS: &[&str] = &[".build"];

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
/// canonical path outside `root`.
pub fn stash_dirs(root: &Path, systems: &[BuildSystem], cfg: &Config) -> Vec<PathBuf> {
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
        for dd in global_for(root) {
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
