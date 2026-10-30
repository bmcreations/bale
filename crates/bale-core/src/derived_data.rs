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
/// A `root` that exists is stored canonical, and a line that resolves to it
/// counts as naming it, so `/tmp/x` and `/private/tmp/x` make one line.
pub fn learn_root(home: &BaleHome, root: &Path) -> io::Result<()> {
    let root = crate::paths::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let line = root.to_string_lossy();
    let named = |r: &PathBuf| *r == root || crate::paths::canonicalize(r).is_ok_and(|c| c == root);
    if line.contains('\n') || learned_roots(home).iter().any(named) {
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

/// The DerivedData roots among the plain folders at most two levels under
/// each of `dirs`: `dir/<a>` or, when `<a>` is not a root, `dir/<a>/<b>`. A
/// root is not looked inside, and no symlink is followed, `dirs` themselves
/// included. Listing stops at `deadline`, keeping what it found so far.
#[cfg(target_os = "macos")]
pub fn roots_in(dirs: &[PathBuf], deadline: std::time::Instant) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for dir in dirs {
        if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        for child in crate::paths::plain_subdirs(dir, deadline) {
            if is_derived_data_root(&child) {
                roots.push(child);
                continue;
            }
            for grandchild in crate::paths::plain_subdirs(&child, deadline) {
                if is_derived_data_root(&grandchild) {
                    roots.push(grandchild);
                }
            }
        }
    }
    roots
}

/// The learned roots (see [`learn_root`]) that are DerivedData built from the
/// tree at `root`: each passes [`is_derived_data_root`] and
/// [`belongs_to_tree`], and neither lies inside the other, so a root inside
/// the tree is left to `up`'s in-tree pass and no root holding the tree is
/// ever named.
#[cfg(target_os = "macos")]
pub(crate) fn learned_for(home: &BaleHome, root: &Path) -> Vec<PathBuf> {
    let tree = workspace_key(root);
    learned_roots(home)
        .into_iter()
        .filter(|dir| {
            let key = workspace_key(dir);
            !key.starts_with(&tree)
                && !tree.starts_with(&key)
                && is_derived_data_root(dir)
                && belongs_to_tree(dir, root)
        })
        .collect()
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
    #[cfg(unix)]
    fn learn_root_stores_one_line_for_aliases_of_a_folder() {
        let t = tempfile::tempdir().unwrap();
        let home = BaleHome(t.path().join("bale"));
        let dir = t.path().join("dd");
        fs::create_dir_all(&dir).unwrap();
        let link = t.path().join("dd-link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let canonical = crate::paths::canonicalize(&dir).unwrap();
        // As typed (`/var/..` on macOS), through a link, then canonical.
        learn_root(&home, &dir).unwrap();
        learn_root(&home, &link).unwrap();
        learn_root(&home, &canonical).unwrap();
        assert_eq!(learned_roots(&home), vec![canonical]);
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

    /// A DerivedData root at `dir`: an `info.plist` naming `/gone/App.xcodeproj`, plus `Build/`.
    #[cfg(target_os = "macos")]
    fn fake_root(dir: &Path) {
        fs::create_dir_all(dir.join("Build")).unwrap();
        fs::write(
            dir.join("info.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
             <key>WorkspacePath</key><string>/gone/App.xcodeproj</string></dict></plist>\n",
        )
        .unwrap();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn roots_in_finds_roots_one_and_two_levels_down() {
        let t = tempfile::tempdir().unwrap();
        let pad = t.path().join("scratchpad");
        fake_root(&pad.join("dd-ios"));
        fake_root(&pad.join("wt-ios/.dd"));
        fs::create_dir_all(pad.join("notes")).unwrap();
        // Three levels down is past the walk.
        fake_root(&pad.join("a/b/dd"));
        let mut found = roots_in(
            std::slice::from_ref(&pad),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        );
        found.sort();
        assert_eq!(found, vec![pad.join("dd-ios"), pad.join("wt-ios/.dd")]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn roots_in_does_not_descend_into_a_root() {
        let t = tempfile::tempdir().unwrap();
        let pad = t.path().join("scratchpad");
        fake_root(&pad.join("dd"));
        fake_root(&pad.join("dd/inner"));
        let found = roots_in(
            std::slice::from_ref(&pad),
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        );
        assert_eq!(found, vec![pad.join("dd")]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn roots_in_follows_no_symlink() {
        let t = tempfile::tempdir().unwrap();
        let elsewhere = t.path().join("elsewhere");
        fake_root(&elsewhere.join("dd"));
        let pad = t.path().join("scratchpad");
        fs::create_dir_all(&pad).unwrap();
        // A link to a root, and a link to a folder holding one.
        std::os::unix::fs::symlink(elsewhere.join("dd"), pad.join("dd-link")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, pad.join("wt-link")).unwrap();
        // A scratchpad that is itself a link is not walked either.
        let linked_pad = t.path().join("linked-pad");
        std::os::unix::fs::symlink(&elsewhere, &linked_pad).unwrap();
        let found = roots_in(
            &[pad, linked_pad],
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn roots_in_stops_at_the_deadline() {
        let t = tempfile::tempdir().unwrap();
        let pad = t.path().join("scratchpad");
        fake_root(&pad.join("dd"));
        let past = std::time::Instant::now() - std::time::Duration::from_millis(1);
        assert!(roots_in(&[pad], past).is_empty());
    }
}
