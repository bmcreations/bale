use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::BaleHome;

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
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn read(dir: PathBuf) -> DdDir {
        let workspace = workspace_path(&dir.join("info.plist"));
        let canonical = workspace
            .as_ref()
            .and_then(|wp| crate::paths::canonicalize(wp).ok());
        DdDir {
            dir,
            workspace,
            canonical,
        }
    }

    /// The `WorkspacePath` as recorded, if the folder has a usable plist.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

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
        .map(|e| DdDir::read(e.path()))
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

/// True when `dir` is a regular directory (not a symlink) whose `info.plist`
/// `WorkspacePath` lies inside the tree at `root`. Unlike
/// [`is_derived_data_root`] it does not need `Build` to exist.
pub(crate) fn belongs_to_tree(dir: &Path, root: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && fs::symlink_metadata(dir.join("info.plist")).is_ok_and(|m| m.file_type().is_file())
        && DdDir::read(dir.to_path_buf()).built_from(&workspace_key(root))
}

/// Appends `root` to `<bale home>/dd-roots` unless a line already names it.
/// Each line is a DerivedData root itself, the folder holding `info.plist`.
pub fn learn_root(home: &BaleHome, root: &Path) -> io::Result<()> {
    let line = root.to_string_lossy();
    if line.contains('\n') || learned_roots(home).iter().any(|r| r == root) {
        return Ok(());
    }
    fs::create_dir_all(&home.0)?;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(home.dd_roots_file())?;
    writeln!(file, "{line}")
}

/// The roots in `<bale home>/dd-roots`, deduplicated; none when the file is
/// missing or unreadable.
pub fn learned_roots(home: &BaleHome) -> Vec<PathBuf> {
    let text = fs::read_to_string(home.dd_roots_file()).unwrap_or_default();
    let mut roots: Vec<PathBuf> = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let root = PathBuf::from(line);
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
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

/// Xcode's DerivedData folder name for a project or workspace path:
/// `<stem with spaces as _>-<28 letters>`. The letters come from the MD5 of
/// the NFD-normalised path (a leading `/private` dropped before `/tmp/`,
/// `/var/` or `/etc/`), each big-endian u64 half written as 14 base-26
/// digits `a`-`z`. Pure: reads no files.
pub fn folder_name(project: &Path) -> String {
    use md5::{Digest, Md5};
    use unicode_normalization::UnicodeNormalization;

    let full = project.to_string_lossy();
    let path = match full.strip_prefix("/private") {
        Some(rest)
            if rest.starts_with("/tmp/")
                || rest.starts_with("/var/")
                || rest.starts_with("/etc/") =>
        {
            rest
        }
        _ => &full,
    };
    let nfd: String = path.nfd().collect();
    let digest = Md5::digest(nfd.as_bytes());

    let mut hash = String::with_capacity(28);
    for half in digest.chunks_exact(8) {
        let mut n = u64::from_be_bytes(half.try_into().expect("8-byte chunk"));
        let mut letters = [b'a'; 14];
        for slot in letters.iter_mut().rev() {
            *slot = b'a' + (n % 26) as u8;
            n /= 26;
        }
        hash.extend(letters.iter().map(|&b| b as char));
    }

    let stem = Path::new(nfd.as_str())
        .file_stem()
        .map(|s| s.to_string_lossy().replace(' ', "_"))
        .unwrap_or_default();
    format!("{stem}-{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "/tmp/claude-501/-Users-bmc--t3-desktop-scratch-2026-10-28-worktrees-are-antiquated-dont-compress-300b5ddc/de2aa02c-f01d-4f79-8a6e-1aa14293a224/scratchpad";

    fn name_of(rel: &str) -> String {
        folder_name(Path::new(&format!("{P}/{rel}")))
    }

    #[test]
    fn folder_name_matches_recorded_xcode_names() {
        assert_eq!(
            name_of("t0/ios/Code.xcodeproj"),
            "Code-akujzjaeriyzdfecyhmsqsaxboch"
        );
        assert_eq!(
            name_of("t12/gen/with space dir/Demo.xcodeproj"),
            "Demo-dmnmyroyujtqlrdmydpdvhpiidhk"
        );
        assert_eq!(
            name_of("t12/gen/stemspace/Demo App.xcodeproj"),
            "Demo_App-cmbsorhiyxauyidmwlwxnybpjmxs"
        );
        assert_eq!(
            name_of("t12/gen/wsp/Outer.xcworkspace"),
            "Outer-atnnlkfqebpatwcgsyerokqdshud"
        );
    }

    #[test]
    fn folder_name_hashes_the_nfd_path() {
        // Stem is NFD on disk: e + U+0301.
        assert_eq!(
            name_of("t12/gen/odd/a-b.c e\u{301}+d(1).xcodeproj"),
            "a-b.c_e\u{301}+d(1)-ezlayyundjcdkzghoipzfouyojcv"
        );
        // NFC input (U+00E9) gives the NFD stem and NFD hash.
        assert_eq!(
            name_of("t12/gen/nfc/caf\u{e9}.xcodeproj"),
            "cafe\u{301}-glisvtzkzqzaosggenyztsoanxjr"
        );
    }

    #[test]
    fn folder_name_strips_private_only_before_tmp_var_etc() {
        let a = folder_name(Path::new("/private/tmp/x/A.xcodeproj"));
        assert_eq!(a, folder_name(Path::new("/tmp/x/A.xcodeproj")));
        let v = folder_name(Path::new("/private/var/x/A.xcodeproj"));
        assert_eq!(v, folder_name(Path::new("/var/x/A.xcodeproj")));
        let e = folder_name(Path::new("/private/etc/x/A.xcodeproj"));
        assert_eq!(e, folder_name(Path::new("/etc/x/A.xcodeproj")));
        for p in ["/privatefoo/x/A.xcodeproj", "/private/Users/x/A.xcodeproj"] {
            assert_ne!(folder_name(Path::new(p)), a, "{p}");
        }
        assert_ne!(
            folder_name(Path::new("/private/Users/x/A.xcodeproj")),
            folder_name(Path::new("/Users/x/A.xcodeproj"))
        );
        assert_ne!(
            folder_name(Path::new("/privatefoo/x/A.xcodeproj")),
            folder_name(Path::new("/foo/x/A.xcodeproj"))
        );
    }

    /// Reads (never writes) the real DerivedData folders and checks each
    /// folder's name against the one computed from its `WorkspacePath`.
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn folder_name_matches_live_derived_data() {
        let root = dirs::home_dir()
            .unwrap()
            .join("Library/Developer/Xcode/DerivedData");
        let (mut ok, mut skipped) = (0, 0);
        let mut bad = Vec::new();
        for entry in fs::read_dir(&root).unwrap().flatten() {
            let dir = entry.path();
            let Some(ws) = DdDir::read(dir.clone()).workspace().map(Path::to_path_buf) else {
                skipped += 1;
                continue;
            };
            let got = folder_name(&ws);
            let want = entry.file_name().to_string_lossy().into_owned();
            if got == want {
                ok += 1;
            } else {
                bad.push(format!("{want} (WorkspacePath {})", ws.display()));
            }
        }
        eprintln!(
            "live check: {ok} matched, {skipped} skipped, {} mismatched",
            bad.len()
        );
        assert!(bad.is_empty(), "mismatches: {bad:#?}");
    }

    #[test]
    fn learn_root_appends_once_per_path() {
        let h = tempfile::tempdir().unwrap();
        let home = BaleHome(h.path().join("bale"));
        assert!(learned_roots(&home).is_empty());
        learn_root(&home, Path::new("/tmp/a-dd")).unwrap();
        learn_root(&home, Path::new("/tmp/b-dd")).unwrap();
        learn_root(&home, Path::new("/tmp/a-dd")).unwrap();
        assert_eq!(
            fs::read_to_string(home.dd_roots_file()).unwrap(),
            "/tmp/a-dd\n/tmp/b-dd\n"
        );
        assert_eq!(learned_roots(&home).len(), 2);
    }

    #[test]
    fn learned_roots_skips_blank_and_repeated_lines() {
        let h = tempfile::tempdir().unwrap();
        let home = BaleHome(h.path().to_path_buf());
        fs::write(home.dd_roots_file(), "/x\n\n/y\n/x\n").unwrap();
        assert_eq!(
            learned_roots(&home),
            vec![PathBuf::from("/x"), PathBuf::from("/y")]
        );
    }
}
