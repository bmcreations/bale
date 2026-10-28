use std::fs;
use std::path::{Path, PathBuf};

/// DerivedData folders directly under `dd_root` whose `info.plist`
/// `WorkspacePath` lies inside `workspace` (compared component-wise). A
/// folder with a missing or unparseable plist, or no `WorkspacePath`, is
/// never matched.
pub fn derived_data_for(workspace: &Path, dd_root: &Path) -> Vec<PathBuf> {
    let ws = workspace_key(workspace);
    index(dd_root)
        .into_iter()
        .filter(|d| d.built_from(&ws))
        .map(|d| d.dir)
        .collect()
}

/// One folder directly under a DerivedData root, read once so several
/// trees can be matched against it.
#[derive(Debug)]
pub(crate) struct DdDir {
    pub(crate) dir: PathBuf,
    /// `WorkspacePath` as recorded and canonicalised (when it still
    /// exists); both `None` without a usable plist.
    workspace: Option<PathBuf>,
    canonical: Option<PathBuf>,
}

impl DdDir {
    /// True when the project this folder was built from lies inside the
    /// tree whose [`workspace_key`] is `ws`.
    pub(crate) fn built_from(&self, ws: &Path) -> bool {
        self.workspace.as_ref().is_some_and(|wp| wp.starts_with(ws))
            || self.canonical.as_ref().is_some_and(|c| c.starts_with(ws))
    }
}

/// The form of a tree path [`DdDir::built_from`] compares against: the
/// canonical path, or the path as given when it cannot be resolved.
pub(crate) fn workspace_key(workspace: &Path) -> PathBuf {
    crate::paths::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// Every folder directly under `dd_root`, sorted, with its `WorkspacePath`.
/// Folders without one (such as `ModuleCache.noindex`) are listed too.
/// Missing or unreadable `dd_root` lists nothing.
pub(crate) fn index(dd_root: &Path) -> Vec<DdDir> {
    let Ok(entries) = fs::read_dir(dd_root) else {
        return Vec::new();
    };
    let mut out: Vec<DdDir> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let workspace = workspace_path(&e.path().join("info.plist"));
            let canonical = workspace
                .as_ref()
                .and_then(|wp| crate::paths::canonicalize(wp).ok());
            DdDir {
                dir: e.path(),
                workspace,
                canonical,
            }
        })
        .collect();
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

pub(crate) fn workspace_path(plist_path: &Path) -> Option<PathBuf> {
    let value = plist::Value::from_file(plist_path).ok()?;
    let wp = value.as_dictionary()?.get("WorkspacePath")?.as_string()?;
    if wp.is_empty() {
        return None;
    }
    Some(PathBuf::from(wp))
}

/// True when `dir` is an Xcode DerivedData root: a regular `info.plist` with a
/// parseable `WorkspacePath`, plus a `Build` subdirectory. The name is free.
///
/// `info.plist` and `Build` are looked up by joining the names, so on a
/// case-insensitive filesystem (APFS by default) `Info.plist` and `build` also
/// match. The `WorkspacePath` requirement and the keep rule, which spares any
/// tracked or unignored file, still apply.
pub(crate) fn is_derived_data_root(dir: &Path) -> bool {
    let plist = dir.join("info.plist");
    fs::symlink_metadata(&plist).is_ok_and(|m| m.file_type().is_file())
        && fs::symlink_metadata(dir.join("Build")).is_ok_and(|m| m.file_type().is_dir())
        && workspace_path(&plist).is_some()
}

/// Xcode's shared DerivedData dir, `~/Library/Developer/Xcode/DerivedData`.
pub(crate) fn global_root() -> Option<PathBuf> {
    crate::paths::home_dir().map(|h| h.join("Library/Developer/Xcode/DerivedData"))
}

/// The folders in Xcode's shared DerivedData built from the tree at `root`.
pub(crate) fn global_for(root: &Path) -> Vec<PathBuf> {
    global_root()
        .map(|g| derived_data_for(root, &g))
        .unwrap_or_default()
}

/// Every Xcode DerivedData root that belongs to the tree at `root`: the
/// in-tree ones among its regenerable `regen` dirs, then the shared ones.
pub(crate) fn all_for(root: &Path, regen: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = regen
        .iter()
        .filter(|d| is_derived_data_root(d))
        .cloned()
        .collect();
    roots.extend(global_for(root));
    roots
}
