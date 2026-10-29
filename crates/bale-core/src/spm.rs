//! Shared Swift packages: finding a tree's `Package.resolved` files and the
//! key that names the shared packages folder for one of them.

// Nothing calls these until seeding and the hooks land.
#![allow(dead_code)]

use crate::derived_data::is_derived_data_root;
use crate::detect::{GRADLE_DIRS, SWIFT_DIRS};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// How deep below the tree an `.xcodeproj` or `.xcworkspace` may sit.
const MAX_DEPTH: usize = 3;

/// A project's `Package.resolved` and the `.xcodeproj` or `.xcworkspace` that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub project: PathBuf,
    pub resolved: PathBuf,
}

/// The `Package.resolved` of each `.xcodeproj` and `.xcworkspace` up to three
/// levels below `tree`, sorted by project path. Regenerable dirs (built-in
/// names and DerivedData roots) and `.git` are not searched, and a project is
/// never descended into.
pub(crate) fn resolved_files(tree: &Path) -> Vec<Resolved> {
    let mut out = Vec::new();
    let mut it = WalkDir::new(tree)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = it.next() {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let resolved = if name.ends_with(".xcodeproj") {
            entry
                .path()
                .join("project.xcworkspace/xcshareddata/swiftpm/Package.resolved")
        } else if name.ends_with(".xcworkspace") {
            entry.path().join("xcshareddata/swiftpm/Package.resolved")
        } else {
            let skip = name == ".git"
                || GRADLE_DIRS.iter().chain(SWIFT_DIRS).any(|n| *n == name)
                || is_derived_data_root(entry.path());
            if skip {
                it.skip_current_dir();
            }
            continue;
        };
        it.skip_current_dir();
        if resolved.is_file() {
            out.push(Resolved {
                project: entry.into_path(),
                resolved,
            });
        }
    }
    out.sort_by(|a, b| a.project.cmp(&b.project));
    out
}

/// First 16 hex characters of the sha256 of the `Package.resolved` bytes and
/// the Xcode build version. Any byte change gives a new key.
pub(crate) fn key(resolved: &Path, xcode_build: &str) -> io::Result<String> {
    let bytes = std::fs::read(resolved)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hasher.update(xcode_build.as_bytes());
    Ok(hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// `ProductBuildVersion` of the active Xcode, read from `version.plist` beside
/// its developer dir (`$DEVELOPER_DIR`, else `xcode-select -p`) without running
/// `xcodebuild`. Cached per process. `None` off macOS or when it can't be read,
/// which turns shared packages off for the call.
pub(crate) fn xcode_build() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        static BUILD: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        BUILD
            .get_or_init(|| developer_dir().and_then(|d| build_version(&d)))
            .clone()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn developer_dir() -> Option<PathBuf> {
    match std::env::var_os("DEVELOPER_DIR") {
        Some(d) if !d.is_empty() => return Some(PathBuf::from(d)),
        _ => {}
    }
    let out = std::process::Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8(out.stdout).ok()?;
    let dir = dir.trim();
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// `ProductBuildVersion` from `<developer_dir>/../version.plist`.
fn build_version(developer_dir: &Path) -> Option<String> {
    let plist = plist::Value::from_file(developer_dir.parent()?.join("version.plist")).ok()?;
    let build = plist
        .as_dictionary()?
        .get("ProductBuildVersion")?
        .as_string()?;
    (!build.is_empty()).then(|| build.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn xcodeproj_resolved(project: &Path) -> PathBuf {
        project.join("project.xcworkspace/xcshareddata/swiftpm/Package.resolved")
    }

    #[test]
    fn finds_resolved_in_projects_and_workspaces() {
        let t = tempfile::tempdir().unwrap();
        let mac = t.path().join("apple/Mac/LoupeMac.xcodeproj");
        let ws = t.path().join("App.xcworkspace");
        write(&xcodeproj_resolved(&mac), "{}");
        write(&ws.join("xcshareddata/swiftpm/Package.resolved"), "{}");
        let found = resolved_files(t.path());
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].project, ws);
        assert_eq!(found[1].project, mac);
        assert_eq!(found[1].resolved, xcodeproj_resolved(&mac));
    }

    #[test]
    fn two_projects_are_both_returned() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("apple/Mac/LoupeMac.xcodeproj");
        let b = t.path().join("Other.xcodeproj");
        write(&xcodeproj_resolved(&a), "a");
        write(&xcodeproj_resolved(&b), "b");
        let projects: Vec<_> = resolved_files(t.path())
            .into_iter()
            .map(|r| r.project)
            .collect();
        assert!(projects.contains(&a) && projects.contains(&b));
    }

    #[test]
    fn project_without_resolved_is_skipped() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("A.xcodeproj/project.xcworkspace")).unwrap();
        assert!(resolved_files(t.path()).is_empty());
    }

    #[test]
    fn projects_deeper_than_three_levels_are_ignored() {
        let t = tempfile::tempdir().unwrap();
        write(
            &xcodeproj_resolved(&t.path().join("a/b/c/Deep.xcodeproj")),
            "{}",
        );
        assert!(resolved_files(t.path()).is_empty());
    }

    #[test]
    fn regenerable_dirs_and_git_are_skipped() {
        let t = tempfile::tempdir().unwrap();
        for dir in [".git", ".build", "build", "Elsewhere/App-abc"] {
            write(
                &xcodeproj_resolved(&t.path().join(dir).join("X.xcodeproj")),
                "{}",
            );
        }
        // A DerivedData root is recognised by info.plist (with WorkspacePath) and Build/.
        let dd = t.path().join("Elsewhere/App-abc");
        write(
            &dd.join("info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>WorkspacePath</key><string>/x/App.xcodeproj</string></dict></plist>"#,
        );
        fs::create_dir_all(dd.join("Build")).unwrap();
        assert_eq!(resolved_files(t.path()), Vec::new());
    }

    #[test]
    fn key_is_sixteen_hex_chars_and_stable() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("Package.resolved");
        write(&f, "{\"pins\":[]}");
        let k = key(&f, "27A9275").unwrap();
        assert_eq!(k.len(), 16);
        assert!(k
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(k, key(&f, "27A9275").unwrap());
    }

    #[test]
    fn key_changes_with_xcode_build_and_content() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("Package.resolved");
        write(&f, "{\"pins\":[]}");
        let a = key(&f, "27A9275").unwrap();
        assert_ne!(a, key(&f, "27A9276").unwrap());
        write(&f, "{\"pins\": []}");
        assert_ne!(a, key(&f, "27A9275").unwrap());
    }

    #[test]
    fn key_of_missing_file_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        assert!(key(&t.path().join("nope"), "x").is_err());
    }

    #[test]
    fn build_version_reads_version_plist_beside_developer_dir() {
        let t = tempfile::tempdir().unwrap();
        write(
            &t.path().join("Contents/version.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>ProductBuildVersion</key><string>27A9275</string></dict></plist>"#,
        );
        let dev = t.path().join("Contents/Developer");
        assert_eq!(build_version(&dev), Some("27A9275".to_string()));
    }

    #[test]
    fn build_version_is_none_when_unreadable() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(build_version(&t.path().join("Contents/Developer")), None);
        write(&t.path().join("Contents/version.plist"), "not a plist");
        assert_eq!(build_version(&t.path().join("Contents/Developer")), None);
    }

    #[test]
    fn xcode_build_is_none_off_macos() {
        if !cfg!(target_os = "macos") {
            assert_eq!(xcode_build(), None);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn xcode_build_is_nonempty_when_present() {
        if let Some(b) = xcode_build() {
            assert!(!b.is_empty());
        }
    }
}
