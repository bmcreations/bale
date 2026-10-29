//! Sweep's reaping of Xcode DerivedData whose project is gone: folders in
//! Xcode's shared DerivedData and in roots learned from `-derivedDataPath`.
//! Only macOS builds this; see `up`'s own DerivedData removal.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::busy::busy_holders;
use crate::derived_data::{is_derived_data_root, DdDir};
use crate::ops::up::remove_entry;
use crate::BaleHome;

/// Sizing a folder before it is removed stops after this long; the bytes
/// counted so far are what the trace records.
const SIZING_LIMIT: Duration = Duration::from_secs(30);

/// `(folder, result)` for each folder reaped (or, on `dry_run`, that would
/// be) or that failed. `shared` is the listing of Xcode's shared DerivedData
/// and `trees` the registered trees' paths, as recorded and canonical. The
/// learned roots in `<home>/dd-roots` that no longer exist are ignored; once
/// the file is older than `grace` they are dropped from it too, since a root
/// is learned before the build that creates it.
pub(crate) fn reap_orphans(
    home: &BaleHome,
    shared: &[DdDir],
    trees: &[PathBuf],
    grace: Duration,
    dry_run: bool,
) -> Vec<(String, String)> {
    let mut candidates: Vec<DdDir> = Vec::new();
    for root in learned_roots(home, grace) {
        let is_dir = fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir());
        if is_dir && is_derived_data_root(&root) {
            candidates.push(DdDir::read(root));
        }
    }
    let now = SystemTime::now();
    let mut rows = Vec::new();
    let mut seen: Vec<&Path> = Vec::new();
    for dd in shared.iter().chain(&candidates) {
        if seen.contains(&dd.dir.as_path()) || !orphaned(dd, trees, grace, now) {
            continue;
        }
        seen.push(&dd.dir);
        rows.push((dd.dir.display().to_string(), reap(home, &dd.dir, dry_run)));
    }
    rows
}

/// The roots in `dd-roots` that exist. A file untouched for `grace` is
/// rewritten without the ones that do not.
fn learned_roots(home: &BaleHome, grace: Duration) -> Vec<PathBuf> {
    let all = crate::derived_data::learned_roots(home);
    let (live, dead): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|r| fs::symlink_metadata(r).is_ok());
    let stale = fs::metadata(home.dd_roots_file())
        .and_then(|m| m.modified())
        .is_ok_and(|t| age(t, SystemTime::now()) >= grace);
    if !dead.is_empty() && stale {
        let text: String = live.iter().map(|r| format!("{}\n", r.display())).collect();
        let _ = fs::write(home.dd_roots_file(), text);
    }
    live
}

/// Whether `dd` was built from a project that is gone for good: it has a
/// `WorkspacePath` that is missing from disk (not merely unreadable, nor on
/// a volume that is not mounted), no registered tree holds it, and nothing
/// in the folder changed within `grace`.
fn orphaned(dd: &DdDir, trees: &[PathBuf], grace: Duration, now: SystemTime) -> bool {
    let Some(workspace) = dd.workspace() else {
        return false;
    };
    fs::symlink_metadata(workspace).is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        && !on_unmounted_volume(workspace)
        && !{
            let resolved = resolve_missing(workspace);
            trees
                .iter()
                .any(|t| workspace.starts_with(t) || resolved.starts_with(t))
        }
        && newest_change(&dd.dir).is_some_and(|t| age(t, now) >= grace)
}

/// `path` with its deepest existing ancestor canonicalized, so a missing
/// project under `/var/..` still compares against a tree recorded as
/// `/private/var/..`.
fn resolve_missing(path: &Path) -> PathBuf {
    let mut rest = PathBuf::new();
    let mut base = path;
    while let Some(parent) = base.parent() {
        if let Ok(canon) = crate::paths::canonicalize(parent) {
            return canon
                .join(base.file_name().map(PathBuf::from).unwrap_or_default())
                .join(rest);
        }
        rest = base
            .file_name()
            .map(|n| Path::new(n).join(&rest))
            .unwrap_or(rest);
        base = parent;
    }
    path.to_path_buf()
}

/// True for a path under `/Volumes/<name>` when `<name>` is not mounted.
fn on_unmounted_volume(path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix("/Volumes") else {
        return false;
    };
    rest.components()
        .next()
        .is_some_and(|name| !Path::new("/Volumes").join(name).exists())
}

/// The newest mtime among the folder, its `info.plist` and `Build/`.
fn newest_change(dir: &Path) -> Option<SystemTime> {
    [dir.to_path_buf(), dir.join("info.plist"), dir.join("Build")]
        .iter()
        .filter_map(|p| fs::symlink_metadata(p).ok()?.modified().ok())
        .max()
}

/// `now` minus `t`; zero when `t` lies in the future.
fn age(t: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(t).unwrap_or(Duration::ZERO)
}

/// Removes `dir` unless a process holds it, and writes the `reap` op line.
fn reap(home: &BaleHome, dir: &Path, dry_run: bool) -> String {
    match busy_holders(dir, &[]) {
        Ok(h) if !h.is_empty() => return "skipped: in use".into(),
        Ok(_) => {}
        Err(e) => return format!("error: {e:#}"),
    }
    let start = Instant::now();
    let bytes = crate::trace::size_tree(dir, &[], start + SIZING_LIMIT).tree;
    let size = crate::human_bytes(bytes);
    if dry_run {
        return format!("would reap {size}: its project is gone");
    }
    let result = remove_entry(dir);
    crate::trace::reaped(home, bytes, result.is_ok(), start.elapsed());
    match result {
        Ok(()) => format!("reaped {size}: its project is gone"),
        Err(e) => format!("error: removing {}: {e}", dir.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_volume_is_unmounted() {
        assert!(on_unmounted_volume(Path::new(
            "/Volumes/bale-not-mounted/a"
        )));
        assert!(!on_unmounted_volume(Path::new("/Volumes")));
        assert!(!on_unmounted_volume(Path::new("/tmp/a")));
        assert!(!on_unmounted_volume(Path::new("/Users/x/Volumes/a")));
    }
}
