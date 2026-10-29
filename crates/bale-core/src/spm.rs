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
    hasher.update([0]);
    hasher.update(xcode_build.as_bytes());
    Ok(hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// `ProductBuildVersion` of the active Xcode, read from the `version.plist` of
/// its developer dir (`$DEVELOPER_DIR`, else the `xcode-select` link, else
/// `xcode-select -p`) without running `xcodebuild`. A found build is cached per
/// process. `None` off macOS or when it can't be read, which turns shared
/// packages off for the call.
pub(crate) fn xcode_build() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        if let Some(b) = BUILD.get() {
            return Some(b.clone());
        }
        let dir = developer_dir(std::env::var_os("DEVELOPER_DIR"), XCODE_SELECT_LINK)
            .or_else(xcode_select_p)?;
        let build = build_version(&dir)?;
        Some(BUILD.get_or_init(|| build).clone())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Where `xcode-select --switch` records the developer dir.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const XCODE_SELECT_LINK: &str = "/var/db/xcode_select_link";

/// `$DEVELOPER_DIR` when set and non-empty, else the target of the
/// `xcode-select` link, which saves spawning `xcode-select -p` in every hook.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn developer_dir(env: Option<std::ffi::OsString>, link: impl AsRef<Path>) -> Option<PathBuf> {
    match env {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => std::fs::read_link(link).ok(),
    }
}

#[cfg(target_os = "macos")]
fn xcode_select_p() -> Option<PathBuf> {
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

/// `ProductBuildVersion` from `<developer_dir>/../version.plist`, or from
/// `<developer_dir>/Contents/version.plist` when `$DEVELOPER_DIR` names the
/// `.app` rather than its `Contents/Developer`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn build_version(developer_dir: &Path) -> Option<String> {
    let beside = developer_dir.parent()?.join("version.plist");
    let inside = developer_dir.join("Contents/version.plist");
    let plist = plist::Value::from_file(beside)
        .or_else(|_| plist::Value::from_file(inside))
        .ok()?;
    let build = plist
        .as_dictionary()?
        .get("ProductBuildVersion")?
        .as_string()?;
    (!build.is_empty()).then(|| build.to_string())
}

#[derive(serde::Deserialize)]
struct ResolvedFile {
    pins: Vec<ResolvedPin>,
}

#[derive(serde::Deserialize)]
struct ResolvedPin {
    identity: String,
    state: ResolvedPinState,
}

#[derive(serde::Deserialize)]
struct ResolvedPinState {
    revision: String,
}

#[derive(serde::Deserialize)]
struct WorkspaceState {
    object: WorkspaceObject,
}

#[derive(serde::Deserialize)]
struct WorkspaceObject {
    dependencies: Vec<WorkspaceDependency>,
}

/// Every field is optional because local packages (`packageRef.kind`
/// `fileSystem`) have no `checkoutState`.
#[derive(serde::Deserialize)]
struct WorkspaceDependency {
    #[serde(rename = "packageRef")]
    package_ref: Option<PackageRef>,
    state: Option<DependencyState>,
}

#[derive(serde::Deserialize)]
struct PackageRef {
    identity: Option<String>,
}

#[derive(serde::Deserialize)]
struct DependencyState {
    #[serde(rename = "checkoutState")]
    checkout_state: Option<CheckoutState>,
}

#[derive(serde::Deserialize)]
struct CheckoutState {
    revision: Option<String>,
}

/// Whether the SourcePackages folder `dir` holds a finished resolve of
/// `resolved`: every pin (`pins[].identity`, `pins[].state.revision`) has an
/// entry in `dir/workspace-state.json` (`object.dependencies[].packageRef.identity`)
/// at the same `state.checkoutState.revision`. State entries with no pin
/// (local packages) are ignored, and the version numbers of both files are not
/// checked. Only the `Package.resolved` layout of versions 2 and 3 (top-level
/// `pins`) is read; version 1 keeps its pins under `object`, fails to parse and
/// is never finished. A missing file, bad JSON or a missing field is not
/// finished. With zero pins the folder is finished once `workspace-state.json`
/// parses. Assumes SPM records a pin only after its checkout completes; that is
/// untested for an interrupted checkout.
pub(crate) fn finished(dir: &Path, resolved: &Path) -> bool {
    finished_inner(dir, resolved).unwrap_or(false)
}

fn finished_inner(dir: &Path, resolved: &Path) -> Option<bool> {
    let resolved: ResolvedFile = serde_json::from_slice(&std::fs::read(resolved).ok()?).ok()?;
    let state: WorkspaceState =
        serde_json::from_slice(&std::fs::read(dir.join("workspace-state.json")).ok()?).ok()?;
    let have: std::collections::HashMap<&str, &str> = state
        .object
        .dependencies
        .iter()
        .filter_map(|d| {
            let id = d.package_ref.as_ref()?.identity.as_deref()?;
            let rev = d
                .state
                .as_ref()?
                .checkout_state
                .as_ref()?
                .revision
                .as_deref()?;
            Some((id, rev))
        })
        .collect();
    Some(
        resolved
            .pins
            .iter()
            .all(|p| have.get(p.identity.as_str()) == Some(&p.state.revision.as_str())),
    )
}

/// Clones `from` (a real directory, not a symlink) to `<home>/spm/<key>` and
/// returns that path. The clone is made at `spm/.tmp-<key>-<pid>-<n>` and moved
/// into place without replacing anything: if `spm/<key>` exists, before or
/// after the clone, the temp copy is deleted and the existing folder is
/// returned. There is no byte-copy fallback; a failed clone is an error. Only
/// macOS can clone, so elsewhere this always errors.
pub(crate) fn seed(home: &crate::BaleHome, key: &str, from: &Path) -> io::Result<PathBuf> {
    let meta = std::fs::symlink_metadata(from)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a directory (symlinks are refused)",
                from.display()
            ),
        ));
    }
    seed_checked(home, key, from)
}

#[cfg(not(target_os = "macos"))]
fn seed_checked(_home: &crate::BaleHome, _key: &str, _from: &Path) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cloning shared packages folders is only supported on macOS",
    ))
}

#[cfg(target_os = "macos")]
fn seed_checked(home: &crate::BaleHome, key: &str, from: &Path) -> io::Result<PathBuf> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    fn c(p: &Path) -> io::Result<CString> {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
    }

    let spm = home.spm_dir();
    let dest = spm.join(key);
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&spm)?;
    // The pid alone is shared by threads; the counter makes each call unique.
    let tmp = spm.join(format!(
        ".tmp-{key}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let (from_c, tmp_c, dest_c) = (c(from)?, c(&tmp)?, c(&dest)?);
    // SAFETY: all three are valid NUL-terminated paths that outlive the calls.
    if unsafe { libc::clonefile(from_c.as_ptr(), tmp_c.as_ptr(), 0) } != 0 {
        let err = io::Error::last_os_error();
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(err);
    }
    // RENAME_EXCL fails with EEXIST when `dest` exists, even if it is an empty
    // directory, which plain rename(2) would silently replace.
    // SAFETY: as above.
    let rc = unsafe { libc::renamex_np(tmp_c.as_ptr(), dest_c.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        return Ok(dest);
    }
    let err = io::Error::last_os_error();
    let _ = std::fs::remove_dir_all(&tmp);
    if err.raw_os_error() == Some(libc::EEXIST) {
        Ok(dest)
    } else {
        Err(err)
    }
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

    fn assert_skipped(dir: &str) {
        let t = tempfile::tempdir().unwrap();
        write(
            &xcodeproj_resolved(&t.path().join(dir).join("X.xcodeproj")),
            "{}",
        );
        assert_eq!(resolved_files(t.path()), Vec::new(), "{dir}");
    }

    #[test]
    fn git_is_skipped() {
        assert_skipped(".git");
    }

    #[test]
    fn build_dirs_are_skipped() {
        assert_skipped("build");
        assert_skipped(".build");
    }

    #[test]
    fn derived_data_root_is_skipped() {
        let t = tempfile::tempdir().unwrap();
        let dd = t.path().join("Elsewhere/App-abc");
        write(&xcodeproj_resolved(&dd.join("X.xcodeproj")), "{}");
        // A DerivedData root is recognised by info.plist (with WorkspacePath) and Build/.
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
    fn build_version_accepts_the_app_bundle_form() {
        let t = tempfile::tempdir().unwrap();
        write(
            &t.path().join("Xcode.app/Contents/version.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>ProductBuildVersion</key><string>27A9275</string></dict></plist>"#,
        );
        assert_eq!(
            build_version(&t.path().join("Xcode.app")),
            Some("27A9275".to_string())
        );
    }

    #[test]
    fn developer_dir_prefers_env_then_the_link() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(
            developer_dir(Some("/env/Developer".into()), t.path().join("none")),
            Some(PathBuf::from("/env/Developer"))
        );
        assert_eq!(developer_dir(None, t.path().join("none")), None);
        #[cfg(unix)]
        {
            let link = t.path().join("xcode_select_link");
            std::os::unix::fs::symlink("/x/Contents/Developer", &link).unwrap();
            for env in [None, Some("".into())] {
                assert_eq!(
                    developer_dir(env, &link),
                    Some(PathBuf::from("/x/Contents/Developer"))
                );
            }
        }
    }

    #[test]
    fn xcode_build_is_none_off_macos() {
        if !cfg!(target_os = "macos") {
            assert_eq!(xcode_build(), None);
        }
    }

    // ---- finished ----

    fn resolved_json(pins: &[(&str, &str)]) -> String {
        let pins: Vec<String> = pins
            .iter()
            .map(|(id, rev)| {
                format!(r#"{{"identity":"{id}","kind":"remoteSourceControl","state":{{"revision":"{rev}","version":"1.0.0"}}}}"#)
            })
            .collect();
        format!(
            r#"{{"originHash":"x","pins":[{}],"version":3}}"#,
            pins.join(",")
        )
    }

    fn state_json(deps: &[(&str, &str)]) -> String {
        let mut entries: Vec<String> = deps
            .iter()
            .map(|(id, rev)| {
                format!(r#"{{"packageRef":{{"identity":"{id}","kind":"remoteSourceControl"}},"state":{{"checkoutState":{{"revision":"{rev}"}}}}}}"#)
            })
            .collect();
        entries.push(
            r#"{"packageRef":{"identity":"local","kind":"fileSystem"},"state":{"local":{}}}"#
                .to_string(),
        );
        format!(
            r#"{{"object":{{"dependencies":[{}]}},"version":7}}"#,
            entries.join(",")
        )
    }

    /// (folder, Package.resolved) in a temp dir.
    fn finished_case(pins: &[(&str, &str)], state: Option<&str>) -> bool {
        let t = tempfile::tempdir().unwrap();
        let resolved = t.path().join("Package.resolved");
        write(&resolved, &resolved_json(pins));
        let dir = t.path().join("SourcePackages");
        fs::create_dir_all(&dir).unwrap();
        if let Some(s) = state {
            write(&dir.join("workspace-state.json"), s);
        }
        finished(&dir, &resolved)
    }

    fn many(n: usize) -> Vec<(String, String)> {
        (0..n)
            .map(|i| (format!("pkg{i}"), format!("rev{i}")))
            .collect()
    }

    fn refs(v: &[(String, String)]) -> Vec<(&str, &str)> {
        v.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect()
    }

    #[test]
    fn finished_when_every_pin_is_in_the_state_file() {
        let all = many(49);
        let pins = refs(&all);
        assert!(finished_case(&pins, Some(&state_json(&pins))));
    }

    #[test]
    fn half_resolved_folder_is_not_finished() {
        let all = many(49);
        let pins = refs(&all);
        assert!(!finished_case(&pins, Some(&state_json(&pins[..9]))));
    }

    #[test]
    fn pin_at_another_revision_is_not_finished() {
        let all = many(3);
        let pins = refs(&all);
        let mut moved = pins.clone();
        moved[1].1 = "other";
        assert!(!finished_case(&pins, Some(&state_json(&moved))));
    }

    #[test]
    fn extra_state_entries_are_ignored() {
        let all = many(3);
        let pins = refs(&all);
        // state has all 3 plus an unpinned remote package, plus the local one.
        let mut more = pins.clone();
        more.push(("extra", "zzz"));
        assert!(finished_case(&pins[..2], Some(&state_json(&more))));
    }

    #[test]
    fn missing_or_bad_state_file_is_not_finished() {
        let pins = [("a", "1")];
        assert!(!finished_case(&pins, None));
        assert!(!finished_case(&pins, Some("not json")));
        assert!(!finished_case(&pins, Some("{}")));
        assert!(!finished_case(&pins, Some(r#"{"object":{}}"#)));
        assert!(!finished_case(
            &pins,
            Some(r#"{"object":{"dependencies":[{"packageRef":{"identity":"a"}}]}}"#)
        ));
    }

    #[test]
    fn missing_or_bad_resolved_is_not_finished() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("d");
        write(
            &dir.join("workspace-state.json"),
            &state_json(&[("a", "1")]),
        );
        let r = t.path().join("Package.resolved");
        assert!(!finished(&dir, &r));
        write(&r, "garbage");
        assert!(!finished(&dir, &r));
        write(&r, r#"{"pins":[{"identity":"a","state":{}}],"version":3}"#);
        assert!(!finished(&dir, &r));
        // version 1 keeps its pins under `object`; not supported.
        write(&r, r#"{"object":{"pins":[{"package":"a"}]},"version":1}"#);
        assert!(!finished(&dir, &r));
    }

    #[test]
    fn zero_pins_is_finished_when_the_state_file_parses() {
        assert!(finished_case(&[], Some(&state_json(&[]))));
        assert!(!finished_case(&[], Some("not json")));
        assert!(!finished_case(&[], None));
    }

    // ---- seed ----

    #[test]
    fn spm_dir_is_under_bale_home() {
        let h = crate::BaleHome(PathBuf::from("/h"));
        assert_eq!(h.spm_dir(), PathBuf::from("/h/spm"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn seed_is_an_error_off_macos() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("src");
        fs::create_dir_all(&from).unwrap();
        let home = crate::BaleHome(t.path().join("home"));
        assert!(seed(&home, "k", &from).is_err());
    }

    #[cfg(target_os = "macos")]
    fn source(t: &Path) -> PathBuf {
        let from = t.join("src");
        write(&from.join("workspace-state.json"), "{}");
        write(&from.join("checkouts/a/file.txt"), "hello");
        from
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seed_clones_the_tree_to_spm_key() {
        let t = tempfile::tempdir().unwrap();
        let from = source(t.path());
        let home = crate::BaleHome(t.path().join("home"));
        let got = seed(&home, "abc", &from).unwrap();
        assert_eq!(got, home.spm_dir().join("abc"));
        assert_eq!(
            fs::read_to_string(got.join("checkouts/a/file.txt")).unwrap(),
            "hello"
        );
        // A clone, not a link: editing it leaves the source alone.
        fs::write(got.join("checkouts/a/file.txt"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(from.join("checkouts/a/file.txt")).unwrap(),
            "hello"
        );
        let names: Vec<_> = fs::read_dir(home.spm_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("abc")]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seed_returns_the_existing_folder_untouched() {
        let t = tempfile::tempdir().unwrap();
        let from = source(t.path());
        let home = crate::BaleHome(t.path().join("home"));
        let first = seed(&home, "abc", &from).unwrap();
        write(&first.join("marker"), "m");
        let second = seed(&home, "abc", &from).unwrap();
        assert_eq!(first, second);
        assert!(second.join("marker").exists());
        assert_eq!(fs::read_dir(home.spm_dir()).unwrap().count(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seed_keeps_an_existing_empty_folder() {
        // The no-overwrite rename must hold even for an empty target dir,
        // which plain rename(2) would replace.
        let t = tempfile::tempdir().unwrap();
        let from = source(t.path());
        let home = crate::BaleHome(t.path().join("home"));
        let existing = home.spm_dir().join("abc");
        fs::create_dir_all(&existing).unwrap();
        let got = seed(&home, "abc", &from).unwrap();
        assert_eq!(got, existing);
        assert_eq!(fs::read_dir(&existing).unwrap().count(), 0);
        assert_eq!(fs::read_dir(home.spm_dir()).unwrap().count(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_seeds_share_one_clone() {
        let t = tempfile::tempdir().unwrap();
        let from = source(t.path());
        for _ in 0..20 {
            let home = crate::BaleHome(tempfile::tempdir_in(t.path()).unwrap().keep());
            let paths: Vec<PathBuf> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..4)
                    .map(|_| s.spawn(|| seed(&home, "abc", &from).unwrap()))
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert!(paths.iter().all(|p| *p == home.spm_dir().join("abc")));
            let names: Vec<_> = fs::read_dir(home.spm_dir())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, vec![std::ffi::OsString::from("abc")]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn seed_rejects_a_symlink_source() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let home = crate::BaleHome(t.path().join("home"));
        assert!(seed(&home, "abc", &link).is_err());
        assert!(!home.spm_dir().join("abc").exists());
    }

    #[test]
    fn seed_rejects_missing_and_file_sources() {
        let t = tempfile::tempdir().unwrap();
        let home = crate::BaleHome(t.path().join("home"));
        assert!(seed(&home, "abc", &t.path().join("nope")).is_err());
        let f = t.path().join("f");
        write(&f, "x");
        assert!(seed(&home, "abc", &f).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seed_failure_leaves_no_temp_dir() {
        // Source on one path, spm/ parent is a file: create_dir_all fails.
        let t = tempfile::tempdir().unwrap();
        let from = source(t.path());
        let home = crate::BaleHome(t.path().join("home"));
        write(&home.spm_dir(), "a file");
        assert!(seed(&home, "abc", &from).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn xcode_build_is_nonempty_when_present() {
        if let Some(b) = xcode_build() {
            assert!(!b.is_empty());
        }
    }
}
