use std::fs;
use std::path::{Path, PathBuf};

/// DerivedData folders directly under `dd_root` whose `info.plist`
/// `WorkspacePath` lies inside `workspace` (compared component-wise). A
/// folder with a missing or unparseable plist, or no `WorkspacePath`, is
/// never matched.
pub fn derived_data_for(workspace: &Path, dd_root: &Path) -> Vec<PathBuf> {
    let ws = crate::paths::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let Ok(entries) = fs::read_dir(dd_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(wp) = workspace_path(&entry.path().join("info.plist")) else {
            continue;
        };
        let inside = wp.starts_with(&ws)
            || crate::paths::canonicalize(&wp).is_ok_and(|c| c.starts_with(&ws));
        if inside {
            out.push(entry.path());
        }
    }
    out.sort();
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
pub(crate) fn is_derived_data_root(dir: &Path) -> bool {
    let plist = dir.join("info.plist");
    let is_file = fs::symlink_metadata(&plist).is_ok_and(|m| m.file_type().is_file());
    let has_build = fs::symlink_metadata(dir.join("Build")).is_ok_and(|m| m.file_type().is_dir());
    is_file && has_build && workspace_path(&plist).is_some()
}
