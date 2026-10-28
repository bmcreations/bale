//! Build-system detection and the regenerable directories each one produces.

use crate::config::Config;
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
/// followed; a matching symlink is returned as the link itself. A match is never
/// descended into, and `.git` is skipped.
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
        if names.iter().any(|n| *n == name) {
            out.push(entry.into_path());
            if ft.is_dir() {
                it.skip_current_dir();
            }
        }
    }
    out
}
