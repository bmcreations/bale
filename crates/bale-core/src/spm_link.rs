//! Linking a tree's Swift projects to shared packages folders (macOS only).
//!
//! Xcode keeps a project's packages in `<DerivedData>/<folder_name>/SourcePackages`.
//! For a project whose DerivedData folder does not exist yet, bale creates the
//! folder and a `SourcePackages` symlink to `<bale home>/spm/<key>`, so the
//! first build uses the shared copy. A folder that exists is never touched:
//! turning a real `SourcePackages` into a link made Xcode recompile everything.
//!
//! Nothing here fails its caller. A project that can't be linked or seeded is
//! skipped, so hooks and sweep behave as they did before this feature.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::derived_data::folder_name;
use crate::registry::{Record, SpmLink};
use crate::spm::{finished, key, resolved_files, seed, xcode_build, Resolved};
use crate::BaleHome;

/// `share_swift_packages` from the machine config; off when it can't be read.
fn enabled(home: &BaleHome) -> bool {
    crate::config::load_config(home, None).is_ok_and(|c| c.share_swift_packages)
}

fn exists(path: &Path) -> bool {
    !fs::symlink_metadata(path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

/// The real, finished `SourcePackages` that Xcode built for `project`, if any.
fn finished_packages(dd_root: &Path, project: &Path, resolved: &Path) -> Option<PathBuf> {
    let dir = dd_root.join(folder_name(project)).join("SourcePackages");
    (is_real_dir(&dir) && finished(&dir, resolved)).then_some(dir)
}

/// Seeds `spm/<key>` from the first of `siblings` whose copy of the project
/// has a finished real `SourcePackages` for `r.resolved`. A sibling's project
/// sits at the same path below its root as `r.project` does below `tree`.
fn seed_from_siblings(
    home: &BaleHome,
    k: &str,
    r: &Resolved,
    tree: &Path,
    dd_root: &Path,
    siblings: &[PathBuf],
) -> Option<PathBuf> {
    let rel = r.project.strip_prefix(tree).ok()?;
    siblings
        .iter()
        .filter(|s| s.as_path() != tree)
        .filter_map(|s| finished_packages(dd_root, &s.join(rel), &r.resolved))
        .find_map(|from| seed(home, k, &from).ok())
}

/// Creates `folder` and its `SourcePackages` link to `shared`, or nothing.
/// `folder` must not exist; its parent is not created.
fn make_link(shared: &Path, folder: &Path) -> bool {
    if fs::create_dir(folder).is_err() {
        return false;
    }
    if std::os::unix::fs::symlink(shared, folder.join("SourcePackages")).is_err() {
        let _ = fs::remove_dir(folder);
        return false;
    }
    true
}

/// For each Swift project in `tree` whose DerivedData folder under `dd_root`
/// does not exist, links a new folder to the shared packages for its
/// `Package.resolved`, seeding that from one of `siblings` (other checkouts of
/// the repo) when no shared folder exists yet. Returns the links made.
pub(crate) fn link_tree(
    home: &BaleHome,
    tree: &Path,
    dd_root: &Path,
    siblings: &[PathBuf],
) -> Vec<SpmLink> {
    if !enabled(home) || !dd_root.is_dir() {
        return Vec::new();
    }
    let files = resolved_files(tree);
    let Some(build) = (!files.is_empty()).then(xcode_build).flatten() else {
        return Vec::new();
    };
    let mut made = Vec::new();
    for r in files {
        let folder = dd_root.join(folder_name(&r.project));
        if exists(&folder) {
            continue;
        }
        let (Ok(k), Ok(meta)) = (key(&r.resolved, &build), fs::metadata(&r.resolved)) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else { continue };
        let shared = home.spm_dir().join(&k);
        if !is_real_dir(&shared)
            && seed_from_siblings(home, &k, &r, tree, dd_root, siblings).is_none()
        {
            continue;
        }
        if make_link(&shared, &folder) {
            made.push(SpmLink {
                project: r.project,
                resolved: r.resolved,
                mtime,
                size: meta.len(),
                key: k,
                folder,
            });
        }
    }
    made
}

/// Replaces `record.spm` entries for the projects in `made`, and drops the
/// entries whose folder is gone.
pub(crate) fn merge_links(record: &mut Record, made: Vec<SpmLink>) {
    record.spm.retain(|l| exists(&l.folder));
    for l in made {
        record.spm.retain(|e| e.project != l.project);
        record.spm.push(l);
    }
}

/// For each tree in `trees`, seeds `spm/<key>` from the tree's own real,
/// finished `SourcePackages` for each project whose key has no shared folder.
/// The tree keeps its folder.
pub(crate) fn seed_from_trees(home: &BaleHome, trees: &[PathBuf], dd_root: &Path) {
    if !enabled(home) || !dd_root.is_dir() {
        return;
    }
    for tree in trees {
        let files = resolved_files(tree);
        let Some(build) = (!files.is_empty()).then(xcode_build).flatten() else {
            continue;
        };
        for r in files {
            let Ok(k) = key(&r.resolved, &build) else {
                continue;
            };
            if exists(&home.spm_dir().join(&k)) {
                continue;
            }
            if let Some(from) = finished_packages(dd_root, &r.project, &r.resolved) {
                let _ = seed(home, &k, &from);
            }
        }
    }
}

/// Removes the pre-made folders `record.spm` names that are still untouched
/// stubs, and forgets the entries whose folder is gone. For a tree that is
/// going cold or whose checkout no longer exists, where `resolved_files` has
/// nothing to read. Returns whether `record.spm` changed.
pub(crate) fn drop_stubs(record: &mut Record) -> bool {
    let before = record.spm.len();
    for l in &record.spm {
        let _ = crate::ops::up::remove_stub(&l.folder);
    }
    record.spm.retain(|l| exists(&l.folder));
    record.spm.len() != before
}
