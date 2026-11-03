//! Keeping binary-artifact paths in a shared packages folder's
//! `workspace-state.json` pointed at the shared folder itself (macOS only).
//!
//! SwiftPM records each binary target's absolute `path` under
//! `object.artifacts[]`, spelled through the `SourcePackages` it was handed:
//! for a tree linked to `spm/<key>` that is
//! `<DerivedData>/<folder>/SourcePackages/artifacts/...`, and a folder seeded
//! from a sibling's real `SourcePackages` inherits the sibling's spelling.
//! Every tree that shares the folder reads that path, so when the folder in it
//! is deleted (reaped, or a worktree removed), all of them fail with "There is
//! no XCFramework found at ...". bale can't change what SwiftPM writes, so it
//! rewrites such paths to `spm/<key>/artifacts/...` after the fact: when it
//! seeds or links a shared folder, before an `xcodebuild` in a linked tree, and
//! on every sweep.

// The sweep and the hooks are the other callers; off macOS only `seed` is.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const STATE: &str = "workspace-state.json";
/// What every path SwiftPM wrote through a `SourcePackages` contains.
const MARK: &str = "/SourcePackages/artifacts/";

#[derive(serde::Deserialize)]
struct State {
    object: Object,
}

#[derive(serde::Deserialize)]
struct Object {
    #[serde(default)]
    artifacts: Vec<Artifact>,
}

#[derive(serde::Deserialize)]
struct Artifact {
    #[serde(rename = "packageRef")]
    package_ref: Option<PackageRef>,
    path: Option<String>,
}

#[derive(serde::Deserialize)]
struct PackageRef {
    kind: Option<String>,
}

/// Where `path` should point inside `shared`, when it is an artifact path
/// spelled through some `SourcePackages` other than `shared` and the same
/// relative path exists in `shared`. A path whose target is missing from
/// `shared` is left as it is: rewriting it would not make it work.
fn repaired(shared: &Path, path: &str) -> Option<PathBuf> {
    if Path::new(path).starts_with(shared) {
        return None;
    }
    let i = path.rfind(MARK)?;
    let rest = &path[i + MARK.len()..];
    let new = shared.join("artifacts").join(rest);
    fs::symlink_metadata(&new).is_ok().then_some(new)
}

/// Rewrites each `object.artifacts[].path` in `shared/workspace-state.json`
/// that points into a `SourcePackages` other than `shared` to the same path
/// under `shared`, and returns how many it rewrote. Local packages
/// (`packageRef.kind` `fileSystem`) are left alone: their artifacts live in
/// the checkout. Only the path strings change; the rest of the file keeps its
/// bytes. The new file is written to a temp name, fsynced and renamed over the
/// old one, and only if the old one hasn't changed since it was read; a
/// SwiftPM write in between wins and the next call retries. A missing file is
/// zero rewrites.
pub(crate) fn repair_artifact_paths(shared: &Path) -> io::Result<usize> {
    let file = shared.join(STATE);
    let text = match fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    // The common case: nothing spelled through a `SourcePackages`, no parse.
    if !text.contains(MARK) {
        return Ok(0);
    }
    let state: State = serde_json::from_str(&text)?;
    let mut out = text.clone();
    let mut n = 0;
    for a in &state.object.artifacts {
        let local = a
            .package_ref
            .as_ref()
            .and_then(|r| r.kind.as_deref())
            .is_some_and(|k| k == "fileSystem");
        let Some(path) = a.path.as_deref().filter(|_| !local) else {
            continue;
        };
        let Some(new) = repaired(shared, path) else {
            continue;
        };
        let Some(new) = new.to_str() else { continue };
        // SwiftPM writes slashes unescaped, as serde_json does.
        let (old, new) = (serde_json::to_string(path)?, serde_json::to_string(new)?);
        if out.contains(&old) {
            out = out.replace(&old, &new);
            n += 1;
        }
    }
    if n == 0 {
        return Ok(0);
    }
    let tmp = shared.join(format!(".{STATE}.bale-{}", std::process::id()));
    let written = (|| {
        let mut f = fs::File::create(&tmp)?;
        io::Write::write_all(&mut f, out.as_bytes())?;
        f.sync_all()?;
        if fs::read_to_string(&file)? != text {
            return Ok(false);
        }
        fs::rename(&tmp, &file)?;
        Ok(true)
    })();
    match written {
        Ok(true) => Ok(n),
        Ok(false) => {
            let _ = fs::remove_file(&tmp);
            Ok(0)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Repairs every shared folder `spm/<key>` (names starting with `.` are temp
/// or gc folders and are skipped). Errors are swallowed per folder.
pub(crate) fn repair_all(spm_dir: &Path) {
    let Ok(entries) = fs::read_dir(spm_dir) else {
        return;
    };
    for e in entries.flatten() {
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && !e.file_name().to_string_lossy().starts_with('.') {
            let _ = repair_artifact_paths(&e.path());
        }
    }
}

// SwiftPM writes POSIX paths, and the fixtures put temp paths into JSON
// unescaped, which a Windows path breaks.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A SwiftPM-formatted state file with one artifact per `(kind, path)`.
    fn state(artifacts: &[(&str, &str)]) -> String {
        let items: Vec<String> = artifacts
            .iter()
            .map(|(kind, path)| {
                format!(
                    "      {{\n        \"packageRef\" : {{\n          \"kind\" : \"{kind}\"\n        }},\n        \"path\" : \"{path}\"\n      }}"
                )
            })
            .collect();
        format!(
            "{{\n  \"object\" : {{\n    \"artifacts\" : [\n{}\n    ],\n    \"dependencies\" : [\n\n    ],\n    \"prebuilts\" : [\n\n    ]\n  }},\n  \"version\" : 7\n}}",
            items.join(",\n")
        )
    }

    fn artifact(shared: &Path, rel: &str) {
        fs::create_dir_all(shared.join("artifacts").join(rel)).unwrap();
    }

    #[test]
    fn rewrites_a_path_into_a_deleted_derived_data_folder_and_nothing_else() {
        let t = tempfile::tempdir().unwrap();
        let shared = t.path().join("spm/k");
        artifact(&shared, "pkg/A/A.xcframework");
        artifact(&shared, "pkg/B/B.xcframework");
        let s = shared.display();
        let gone = "/nowhere/DerivedData/App-gone/SourcePackages/artifacts/pkg/A/A.xcframework";
        let canonical = format!("{s}/artifacts/pkg/B/B.xcframework");
        let local = "/tree/Local/SourcePackages/artifacts/pkg/B/B.xcframework";
        let missing = "/nowhere/DerivedData/App-gone/SourcePackages/artifacts/pkg/C/C.xcframework";
        let before = state(&[
            ("remoteSourceControl", gone),
            ("remoteSourceControl", &canonical),
            ("fileSystem", local),
            ("remoteSourceControl", missing),
        ]);
        fs::write(shared.join(STATE), &before).unwrap();

        assert_eq!(repair_artifact_paths(&shared).unwrap(), 1);

        let after = fs::read_to_string(shared.join(STATE)).unwrap();
        let want = before.replace(gone, &format!("{s}/artifacts/pkg/A/A.xcframework"));
        assert_eq!(after, want);
        // Run again: nothing left to do.
        assert_eq!(repair_artifact_paths(&shared).unwrap(), 0);
    }

    #[test]
    fn rewrites_a_path_through_a_live_link_to_another_folder() {
        let t = tempfile::tempdir().unwrap();
        let shared = t.path().join("spm/k");
        artifact(&shared, "pkg/A/A.xcframework");
        let other = "/dd/App-x/SourcePackages/artifacts/pkg/A/A.xcframework";
        fs::write(shared.join(STATE), state(&[("remoteSourceControl", other)])).unwrap();
        assert_eq!(repair_artifact_paths(&shared).unwrap(), 1);
        let after = fs::read_to_string(shared.join(STATE)).unwrap();
        assert!(!after.contains("SourcePackages"), "{after}");
    }

    #[test]
    fn a_missing_state_file_or_one_without_artifact_paths_is_left_alone() {
        let t = tempfile::tempdir().unwrap();
        let shared = t.path().join("k");
        fs::create_dir_all(&shared).unwrap();
        assert_eq!(repair_artifact_paths(&shared).unwrap(), 0);
        fs::write(shared.join(STATE), "not json").unwrap();
        assert_eq!(repair_artifact_paths(&shared).unwrap(), 0);
    }

    #[test]
    fn repair_all_skips_dot_folders() {
        let t = tempfile::tempdir().unwrap();
        let spm = t.path().join("spm");
        let p = "/dd/X/SourcePackages/artifacts/pkg/A/A.xcframework";
        for name in ["k", ".gc-k-1-0"] {
            let shared = spm.join(name);
            artifact(&shared, "pkg/A/A.xcframework");
            fs::write(shared.join(STATE), state(&[("remoteSourceControl", p)])).unwrap();
        }
        repair_all(&spm);
        let read = |n: &str| fs::read_to_string(spm.join(n).join(STATE)).unwrap();
        assert!(!read("k").contains(p));
        assert!(read(".gc-k-1-0").contains(p));
    }
}
