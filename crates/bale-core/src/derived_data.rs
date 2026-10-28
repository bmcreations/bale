use std::fs;
use std::path::{Path, PathBuf};

/// DerivedData folders directly under `dd_root` whose `info.plist`
/// `WorkspacePath` lies inside `workspace` (compared component-wise). A
/// folder with a missing or unparseable plist, or no `WorkspacePath`, is
/// never matched.
pub fn derived_data_for(workspace: &Path, dd_root: &Path) -> Vec<PathBuf> {
    let ws = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
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
        let inside = wp.starts_with(&ws) || fs::canonicalize(&wp).is_ok_and(|c| c.starts_with(&ws));
        if inside {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

fn workspace_path(plist_path: &Path) -> Option<PathBuf> {
    let value = plist::Value::from_file(plist_path).ok()?;
    let wp = value.as_dictionary()?.get("WorkspacePath")?.as_string()?;
    if wp.is_empty() {
        return None;
    }
    Some(PathBuf::from(wp))
}
