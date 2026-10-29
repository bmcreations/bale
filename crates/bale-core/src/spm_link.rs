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
use crate::lock::WorkspaceLock;
use crate::registry::{Record, Registry, SpmLink};
use crate::spm::{finished, key, resolved_files, seed, xcode_build, Resolved};
use crate::spm_lock::{try_lock, Mode};
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
    // Held until the last link exists: the sweep must not remove a folder
    // between the check for it and the link to it.
    let Some(_spm_lock) = try_lock(home, Mode::Shared) else {
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

/// True when the `Package.resolved` `l` was linked for has a different mtime
/// or size now. One stat. A file that is gone counts as unchanged: there is
/// nothing to relink to.
fn changed(l: &SpmLink) -> bool {
    fs::metadata(&l.resolved).is_ok_and(|m| m.len() != l.size || m.modified().ok() != Some(l.mtime))
}

/// The `SourcePackages` link of `l` when it is a symlink into bale's `spm/`
/// folder. A real directory (Xcode's own, or one a user made) or a link
/// somewhere else is not bale's to change.
fn own_link(home: &BaleHome, l: &SpmLink) -> Option<PathBuf> {
    let sp = l.folder.join("SourcePackages");
    let is_link = fs::symlink_metadata(&sp).is_ok_and(|m| m.file_type().is_symlink());
    let into_spm = fs::read_link(&sp).is_ok_and(|t| t.starts_with(home.spm_dir()));
    (is_link && into_spm).then_some(sp)
}

/// Points `sp` at `target` by making a link under a temp name in the same
/// directory and renaming it over `sp`, so a build never sees it missing.
fn repoint(sp: &Path, target: &Path) -> io::Result<()> {
    let tmp = sp.with_file_name(format!(".SourcePackages.bale-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    fs::rename(&tmp, sp).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Brings `l` up to date with its `Package.resolved`; true when `l` changed.
fn relink_one(home: &BaleHome, l: &mut SpmLink, build: &str) -> bool {
    let (Ok(k2), Ok(meta)) = (key(&l.resolved, build), fs::metadata(&l.resolved)) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    if k2 != l.key {
        // Not bale's link any more: leave the folder alone and only refresh
        // mtime and size, so the edit isn't rehashed on every call.
        if let Some(sp) = own_link(home, l) {
            // A clone of the old folder, not an empty one: SPM then updates
            // the changed pins instead of fetching everything.
            let old = home.spm_dir().join(&l.key);
            let new = home.spm_dir().join(&k2);
            let target = if is_real_dir(&new) {
                Some(new)
            } else if is_real_dir(&old) {
                seed(home, &k2, &old).ok()
            } else {
                None
            };
            // Nothing to link to (old folder gone, or the seed failed) or
            // the rename failed: keep the old key and link, but record the
            // new mtime and size so later hooks take the fast path until
            // the next edit or sweep instead of retrying every call.
            if let Some(target) = target {
                if repoint(&sp, &target).is_ok() {
                    l.key = k2;
                }
            }
        }
    }
    l.mtime = mtime;
    l.size = meta.len();
    true
}

/// Pre-tool hook: relinks the tree `id`'s pre-made DerivedData folders whose
/// `Package.resolved` changed since they were linked.
///
/// The common case, nothing changed, reads the record and stats each
/// `Package.resolved` once: no hashing, no walk, no writes. On a change the
/// file is rehashed. An unchanged key only refreshes the recorded mtime and
/// size. A new key moves the link to `spm/<key>`, cloned from the old key's
/// folder when it doesn't exist. Only a symlink into `spm/` is repointed. The
/// record is written under the workspace lock; a locked tree is skipped and
/// retried by the next hook. Every failure is swallowed.
///
/// The K2 seed clone runs synchronously in the hook and can pause the tool
/// call for seconds, once per `Package.resolved` change. When a relink can't
/// proceed (no K2 and no K1 to clone, or the clone fails), the recorded mtime
/// and size are still refreshed so it isn't retried until the next edit.
pub fn relink_tree(home: &BaleHome, id: &str) {
    let registry = Registry::open(home.clone());
    match registry.get(id) {
        Ok(Some(r)) if r.spm.iter().any(changed) => {}
        _ => return,
    }
    if !enabled(home) {
        return;
    }
    let Some(build) = xcode_build() else { return };
    let Ok(Some(_lock)) = WorkspaceLock::try_acquire(home, id) else {
        return;
    };
    // Not taken above: a hook with nothing to relink never touches it. Without
    // it the relink waits for the next hook.
    let Some(_spm_lock) = try_lock(home, Mode::Shared) else {
        return;
    };
    let Ok(Some(mut record)) = registry.get(id) else {
        return;
    };
    let mut dirty = false;
    for l in record.spm.iter_mut().filter(|l| changed(l)) {
        dirty |= relink_one(home, l, &build);
    }
    if dirty {
        let _ = registry.put(&record);
    }
}

/// Pre-tool hook, for a Bash command with `-derivedDataPath root`: when
/// `root/SourcePackages` doesn't exist and tree `id` is linked to a shared
/// folder that exists, links it. An existing `root/SourcePackages`, of any
/// kind, is left alone. Never seeds: only an existing folder is linked. With
/// several projects of different keys, the first one's folder is used.
pub fn link_derived_data_path(home: &BaleHome, id: &str, root: &Path) {
    let sp = root.join("SourcePackages");
    if exists(&sp) {
        return;
    }
    let Ok(Some(record)) = Registry::open(home.clone()).get(id) else {
        return;
    };
    let keys: Vec<PathBuf> = record
        .spm
        .iter()
        .map(|l| home.spm_dir().join(&l.key))
        .collect();
    // Nothing to link to: leave `spm/` and its lock alone.
    if !keys.iter().any(|p| is_real_dir(p)) || !enabled(home) {
        return;
    }
    // Held until the link exists; see `link_tree`. The folder is looked for
    // again under it, since the sweep may have set it aside meanwhile.
    let Some(_spm_lock) = try_lock(home, Mode::Shared) else {
        return;
    };
    let Some(shared) = keys.into_iter().find(|p| is_real_dir(p)) else {
        return;
    };
    let made_root = !root.exists() && fs::create_dir_all(root).is_ok();
    if std::os::unix::fs::symlink(&shared, &sp).is_err() && made_root {
        let _ = fs::remove_dir(root);
    }
}
