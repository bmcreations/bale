//! Sweep's reaping of Xcode DerivedData whose project is gone: folders in
//! Xcode's shared DerivedData and in roots learned from `-derivedDataPath`.
//! Only macOS builds this; see `up`'s own DerivedData removal.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::busy::busy_holders;
use crate::derived_data::{folder_name, is_derived_data_root, workspace_key, DdDir};
use crate::ops::up::remove_entry;
use crate::BaleHome;

/// Sizing a folder before it is removed stops after this long; the bytes
/// counted so far are what the trace records.
const SIZING_LIMIT: Duration = Duration::from_secs(30);

/// `(folder, result)` for each folder reaped (or, on `dry_run`, that would
/// be) or that failed. `shared` is the listing of Xcode's shared DerivedData,
/// `trees` the paths of the trees that still hold DerivedData and `known`
/// every registered tree's path, both as recorded and canonical. The
/// learned roots in `<home>/dd-roots` that no longer exist are ignored; once
/// the file is older than `grace` they are dropped from it too, since a root
/// is learned before the build that creates it. A shared folder without a
/// usable `info.plist` is judged by the project its name matches (see
/// [`inferred_workspaces`]).
pub(crate) fn reap_orphans(
    home: &BaleHome,
    shared: &[DdDir],
    trees: &[PathBuf],
    known: &[PathBuf],
    grace: Duration,
    dry_run: bool,
) -> Vec<(String, String)> {
    let mut candidates: Vec<(PathBuf, PathBuf, bool)> = shared
        .iter()
        .filter_map(|dd| Some((dd.dir.clone(), dd.workspace()?.to_path_buf(), false)))
        .collect();
    candidates.extend(
        inferred_workspaces(home, shared, known)
            .into_iter()
            .map(|(dir, ws)| (dir, ws, true)),
    );
    for root in learned_roots(home, grace) {
        let is_dir = fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir());
        if !is_dir || !is_derived_data_root(&root) {
            continue;
        }
        if let Some(ws) = DdDir::read(root.clone()).workspace() {
            let ws = ws.to_path_buf();
            candidates.push((root, ws, false));
        }
    }
    let now = SystemTime::now();
    let mut rows = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for (dir, workspace, inferred) in candidates {
        if seen.contains(&dir) || !orphaned(&dir, &workspace, inferred, trees, grace, now) {
            continue;
        }
        let result = reap(home, &dir, inferred, dry_run);
        rows.push((dir.display().to_string(), result));
        seen.push(dir);
    }
    rows
}

/// `(folder, project)` for each folder in `shared` with no usable
/// `info.plist` whose name is Xcode's [`folder_name`] for a project in a
/// known tree: a registered one (`known`) or one named in the kept trace.
/// A resolve that never builds leaves such a folder (`Logs/` and
/// `SourcePackages/` only). Candidate projects are each tree joined with
/// the project paths, relative to their tree, of the folders that do have
/// a plist. The trace is read only when some folder needs a match.
fn inferred_workspaces(
    home: &BaleHome,
    shared: &[DdDir],
    known: &[PathBuf],
) -> Vec<(PathBuf, PathBuf)> {
    let unmatched: Vec<(&Path, &str)> = shared
        .iter()
        .filter(|dd| dd.workspace().is_none())
        .filter_map(|dd| Some((dd.dir.as_path(), dd.dir.file_name()?.to_str()?)))
        .filter(|(_, name)| looks_like_folder_name(name))
        .collect();
    if unmatched.is_empty() {
        return Vec::new();
    }
    let mut roots: Vec<PathBuf> = known.to_vec();
    for path in crate::trace::tree_paths(home) {
        roots.push(workspace_key(&path));
        roots.push(path);
    }
    roots.sort();
    roots.dedup();
    let mut relative: Vec<PathBuf> = Vec::new();
    for ws in shared.iter().filter_map(DdDir::workspace) {
        // As recorded and resolved, since a tree may be recorded under
        // `/private/var/..` and its project under `/var/..`. The deepest
        // tree holding the project wins, so a nested worktree's project is
        // not taken as a path inside its parent repo.
        let resolved = resolve_missing(ws);
        let rel = roots
            .iter()
            .flat_map(|t| [ws.strip_prefix(t).ok(), resolved.strip_prefix(t).ok()])
            .flatten()
            .min_by_key(|rel| rel.components().count());
        if let Some(rel) = rel.filter(|r| r.components().count() > 0) {
            if !relative.iter().any(|r| r == rel) {
                relative.push(rel.to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    for (dir, name) in unmatched {
        let project = roots
            .iter()
            .flat_map(|t| relative.iter().map(move |r| t.join(r)))
            .find(|p| folder_name(p) == name);
        if let Some(project) = project {
            out.push((dir.to_path_buf(), project));
        }
    }
    out
}

/// True for `<stem>-<28 letters a-z>`, the shape of a [`folder_name`];
/// `ModuleCache.noindex` and other shared caches never match.
fn looks_like_folder_name(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(stem, hash)| {
        !stem.is_empty() && hash.len() == 28 && hash.bytes().all(|b| b.is_ascii_lowercase())
    })
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

/// Whether the folder `dir` was built from `workspace`, a project that is
/// gone for good: it is missing from disk (not merely unreadable, nor on a
/// volume that is not mounted), no registered tree holds it, and nothing in
/// the folder changed within `grace`. For an `inferred` workspace the
/// folder's immediate entries count as changes too, since such a folder
/// has no `info.plist` or `Build/` to go by.
fn orphaned(
    dir: &Path,
    workspace: &Path,
    inferred: bool,
    trees: &[PathBuf],
    grace: Duration,
    now: SystemTime,
) -> bool {
    fs::symlink_metadata(workspace).is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        && !on_unmounted_volume(workspace)
        && !{
            let resolved = resolve_missing(workspace);
            trees
                .iter()
                .any(|t| workspace.starts_with(t) || resolved.starts_with(t))
        }
        && newest_change(dir, inferred).is_some_and(|t| age(t, now) >= grace)
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

/// The newest mtime among the folder, its `info.plist` and `Build/`, and
/// with `entries` every entry directly inside it.
fn newest_change(dir: &Path, entries: bool) -> Option<SystemTime> {
    let mut paths = vec![dir.to_path_buf(), dir.join("info.plist"), dir.join("Build")];
    if entries {
        if let Ok(read) = fs::read_dir(dir) {
            paths.extend(read.flatten().map(|e| e.path()));
        }
    }
    paths
        .iter()
        .filter_map(|p| fs::symlink_metadata(p).ok()?.modified().ok())
        .max()
}

/// `now` minus `t`; zero when `t` lies in the future.
fn age(t: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(t).unwrap_or(Duration::ZERO)
}

/// Removes `dir` unless a process holds it, and writes the `reap` op line.
/// `inferred` says the project was matched by the folder's name.
fn reap(home: &BaleHome, dir: &Path, inferred: bool, dry_run: bool) -> String {
    let why = if inferred {
        "the project its name matches is gone"
    } else {
        "its project is gone"
    };
    match busy_holders(dir, &[]) {
        Ok(h) if !h.is_empty() => return "skipped: in use".into(),
        Ok(_) => {}
        Err(e) => return format!("error: {e:#}"),
    }
    let start = Instant::now();
    let bytes = crate::trace::size_tree(dir, &[], start + SIZING_LIMIT).tree;
    let size = crate::human_bytes(bytes);
    if dry_run {
        return format!("would reap {size}: {why}");
    }
    let result = remove_entry(dir);
    crate::trace::reaped(home, bytes, result.is_ok(), start.elapsed());
    match result {
        Ok(()) => format!("reaped {size}: {why}"),
        Err(e) => format!("error: removing {}: {e}", dir.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_xcode_folder_names_are_matched_by_name() {
        assert!(looks_like_folder_name("Code-fpmmyqjstrjggxfllpgquvlunhkk"));
        assert!(looks_like_folder_name("a-b.c-dmnmyroyujtqlrdmydpdvhpiidhk"));
        for name in [
            "ModuleCache.noindex",
            "SDKExplicitPrecompiledModules",
            "-fpmmyqjstrjggxfllpgquvlunhkk",
            "Code-fpmmyqjstrjggxfllpgquvlunhk",
            "Code-Fpmmyqjstrjggxfllpgquvlunhkk",
        ] {
            assert!(!looks_like_folder_name(name), "{name}");
        }
    }

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
