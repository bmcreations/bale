//! The saved state as `open` writes it to disk: the manifest of ignored
//! files, file modes and empty dirs, and the code that applies it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::git::Git;
use crate::ops::cold::{copy_file, copy_symlink, create_private, SavedFile, FILES, MANIFEST};
use crate::ops::up::TRASH_PREFIX;

/// `dest/manifest.json`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Ignored files and symlinks copied to `dest/files/`.
    pub files: Vec<SavedFile>,
    /// Permission bits of every regular file in the tree that open writes
    /// (unix only; empty elsewhere). Checkout alone would write 0644 or
    /// 0755 and the saved copies are 0600.
    #[serde(default)]
    pub modes: Vec<SavedMode>,
    /// Dirs with nothing in them that open writes; git keeps no dirs.
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMode {
    pub rel: PathBuf,
    pub mode: u32,
}

/// Reads `dest/manifest.json`. A bare array (the format before modes and
/// dirs were recorded) reads as files only.
pub(crate) fn read_manifest(dest: &Path) -> Result<Manifest> {
    let path = dest.join(MANIFEST);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    if let Ok(files) = serde_json::from_str::<Vec<SavedFile>>(&text) {
        return Ok(Manifest {
            files,
            ..Manifest::default()
        });
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Writes `dest/manifest.json` (tmp + rename, 0600 on unix).
pub(crate) fn write_manifest(dest: &Path, manifest: &Manifest) -> Result<()> {
    let path = dest.join(MANIFEST);
    let tmp = dest.join(format!("{MANIFEST}.tmp"));
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clearing {}", tmp.display())),
    }
    let mut f = create_private(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(manifest)?)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

/// What the disk walks leave out, relative to the tree root: the root's own
/// `.git` (exact name only), and the subtrees bale deliberately does not
/// keep or that something else restores.
pub(crate) struct Skip {
    /// The regenerable dirs `up` removed.
    pub regen: Vec<PathBuf>,
    /// Config `ignored_denylist_extra`: bare names match any component,
    /// slashed entries match a path prefix.
    pub deny: Vec<String>,
    /// Submodule checkouts: open leaves each an empty dir, and their
    /// content is checked against the submodule's own git dir.
    pub gitlinks: BTreeSet<PathBuf>,
    /// Every path git saves, and each dir above one. A denied path in here
    /// is walked anyway: git restores it, so it has to match.
    pub tracked: BTreeSet<PathBuf>,
}

impl Skip {
    /// The ignored-file rule `save_ignored` applies: under a regenerable dir,
    /// a `node_modules` or `.bale-trash-*` component, or a deny entry.
    pub(crate) fn denied(&self, rel: &Path) -> bool {
        denied(rel, &self.regen, &self.deny)
    }

    /// Whether the walk leaves out `rel` and everything under it.
    fn skips(&self, rel: &Path, depth: usize, name: &std::ffi::OsStr) -> bool {
        (depth == 1 && name == ".git") || (self.denied(rel) && !self.tracked.contains(rel))
    }
}

/// `paths` (slash-separated, relative) and every dir above each.
pub(crate) fn with_ancestors<'a>(paths: impl Iterator<Item = &'a str>) -> BTreeSet<PathBuf> {
    let mut set = BTreeSet::new();
    for p in paths.filter(|p| !p.is_empty()) {
        let mut cur = PathBuf::from(p);
        loop {
            if !set.insert(cur.clone()) {
                break;
            }
            if !cur.pop() || cur.as_os_str().is_empty() {
                break;
            }
        }
    }
    set
}

pub(crate) fn denied(rel: &Path, regen: &[PathBuf], deny: &[String]) -> bool {
    regen.iter().any(|r| rel.starts_with(r))
        || rel.components().any(|c| {
            let Component::Normal(name) = c else {
                return false;
            };
            let name = name.to_string_lossy();
            name == "node_modules"
                || name.starts_with(TRASH_PREFIX)
                || deny.iter().any(|d| !d.contains('/') && *d == name)
        })
        || deny
            .iter()
            .filter(|d| d.contains('/'))
            .any(|d| rel.starts_with(d.trim_end_matches('/')))
}

/// The modes of the regular files and the empty dirs of the tree at `root`,
/// as the walk under `skip` sees it. A dir counts as empty when nothing the
/// walk keeps is inside it.
pub(crate) fn record_tree(root: &Path, skip: &Skip) -> Result<(Vec<SavedMode>, Vec<PathBuf>)> {
    let mut modes = Vec::new();
    let mut dirs = Vec::new();
    let mut parents = BTreeSet::new();
    let mut it = WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .follow_root_links(false)
        .sort_by_file_name()
        .into_iter();
    while let Some(item) = it.next() {
        let item = item.with_context(|| format!("walking {}", root.display()))?;
        let rel = item
            .path()
            .strip_prefix(root)
            .context("walk left the tree")?
            .to_path_buf();
        let ft = item.file_type();
        if skip.skips(&rel, item.depth(), item.file_name()) {
            if ft.is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        if let Some(parent) = rel.parent() {
            parents.insert(parent.to_path_buf());
        }
        if ft.is_dir() {
            if skip.gitlinks.contains(&rel) {
                it.skip_current_dir();
            } else {
                dirs.push(rel);
            }
        } else if ft.is_file() {
            if let Some(mode) = file_mode(&item.metadata()?) {
                modes.push(SavedMode { rel, mode });
            }
        }
    }
    dirs.retain(|d| !parents.contains(d));
    Ok((modes, dirs))
}

#[cfg(unix)]
pub(crate) fn file_mode(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
pub(crate) fn file_mode(_meta: &fs::Metadata) -> Option<u32> {
    None
}

/// Writes the manifest's part of a restore into `root`, after git checked
/// out the wip tree: the saved files from `files_dir` (one already present,
/// from an interrupted open, is left for the caller's check), the empty
/// dirs, then each recorded file mode.
pub(crate) fn apply_manifest(root: &Path, manifest: &Manifest, files_dir: &Path) -> Result<()> {
    for f in &manifest.files {
        let to = root.join(&f.rel);
        if fs::symlink_metadata(&to).is_ok() {
            continue;
        }
        if let Some(parent) = f.rel.parent() {
            make_parents(root, parent)?;
        }
        let from = files_dir.join(&f.rel);
        if f.symlink_target.is_some() {
            copy_symlink(&from, &to, f.rel.clone())?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    for d in &manifest.dirs {
        make_parents(root, d)?;
    }
    #[cfg(unix)]
    for m in &manifest.modes {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(&m.rel);
        // Never through a symlink; a file that isn't there is the caller's
        // check to report.
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
            fs::set_permissions(&path, fs::Permissions::from_mode(m.mode))
                .with_context(|| format!("setting the mode of {}", path.display()))?;
        }
    }
    Ok(())
}

/// Creates `rel` under `root` one component at a time, refusing to pass
/// through anything that is not a real directory.
pub(crate) fn make_parents(root: &Path, rel: &Path) -> Result<()> {
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{} is in the way of a saved file", cur.display()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&cur).with_context(|| format!("creating {}", cur.display()))?
            }
            Err(e) => return Err(e).with_context(|| format!("checking {}", cur.display())),
        }
    }
    Ok(())
}

/// What a disk entry is, as far as a restore can reproduce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink,
    /// A fifo, socket or device: neither git nor the manifest can hold it.
    Other,
}

impl Kind {
    fn of(ft: fs::FileType) -> Kind {
        if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Dir => "dir",
            Kind::File => "file",
            Kind::Symlink => "symlink",
            Kind::Other => "special file",
        }
    }
}

/// What [`restore_check`] found.
pub(crate) enum Verdict {
    /// Open would give the tree back as it is. Holds what the walk saw, so
    /// the caller can tell whether anything changed since.
    Same(Snapshot),
    /// Why open would not give it back.
    Differs(String),
}

/// The tree as the compare walked it: every kept entry's stat signature.
pub(crate) struct Snapshot {
    tree: PathBuf,
    skip: Skip,
    seen: BTreeMap<PathBuf, (Kind, Stamp)>,
    root: Stamp,
}

impl Snapshot {
    /// The first path that was added, removed or changed (by its stat
    /// signature) since the compare, if any.
    pub(crate) fn changed(&self) -> Result<Option<PathBuf>> {
        if stamp(&fs::symlink_metadata(&self.tree)?) != self.root {
            return Ok(Some(PathBuf::new()));
        }
        let now = entries(&self.tree, &self.skip)?;
        if let Some(rel) = now.keys().find(|r| !self.seen.contains_key(*r)) {
            return Ok(Some(rel.clone()));
        }
        Ok(self
            .seen
            .iter()
            .find(|(rel, was)| now.get(*rel) != Some(*was))
            .map(|(rel, _)| rel.clone()))
    }
}

/// What a stat says about an entry that a write would change: on unix the
/// device, inode, size, mode, mtime and ctime; elsewhere size, mtime and
/// creation time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    unix: [i64; 7],
    #[cfg(not(unix))]
    created: Option<std::time::SystemTime>,
}

fn stamp(meta: &fs::Metadata) -> Stamp {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Stamp {
            len: meta.len(),
            modified: meta.modified().ok(),
            unix: [
                meta.dev() as i64,
                meta.ino() as i64,
                i64::from(meta.mode()),
                meta.mtime(),
                meta.mtime_nsec(),
                meta.ctime(),
                meta.ctime_nsec(),
            ],
        }
    }
    #[cfg(not(unix))]
    {
        Stamp {
            len: meta.len(),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
        }
    }
}

/// Whether `open` would give back the tree at `tree` byte for byte. Runs open's own restore (checkout of `wip` through git's
/// filters and eol rules, then the manifest in `dest`) into a temporary dir
/// beside `tree`, and compares the two with plain file reads: the paths both
/// ways, file types, file bytes, modes (unix) and symlink targets. A fifo,
/// socket or device in the tree is a mismatch. `Err` means the check itself
/// failed. On a match the verdict carries the stat of every entry walked.
pub(crate) fn restore_check(
    tree: &Path,
    wip: &str,
    dest: &Path,
    regen: &[PathBuf],
    deny: &[String],
) -> Result<Verdict> {
    let manifest = read_manifest(dest)?;
    let g = Git::at(tree);
    let git_dir = g.run(&["rev-parse", "--absolute-git-dir"])?;
    let git_dir = PathBuf::from(git_dir.trim_end_matches(['\n', '\r']));
    let listing = g.run(&["ls-tree", "-r", "-z", wip])?;
    let listed: Vec<(&str, &str)> = listing
        .split('\0')
        .filter_map(|e| e.split_once('\t'))
        .collect();
    let gitlinks = listed
        .iter()
        .filter(|(meta, _)| meta.starts_with("160000 "))
        .map(|(_, path)| PathBuf::from(path))
        .collect();
    let skip = Skip {
        regen: regen.to_vec(),
        deny: deny.to_vec(),
        gitlinks,
        tracked: with_ancestors(listed.iter().map(|(_, path)| *path)),
    };

    // Beside the tree, so it is on the same filesystem (case folding, exec
    // bits and symlinks behave the same).
    let parent = tree
        .parent()
        .with_context(|| format!("{} has no parent directory", tree.display()))?;
    let tmp = tempfile::Builder::new()
        .prefix(".bale-check-")
        .tempdir_in(parent)
        .with_context(|| format!("creating a check dir in {}", parent.display()))?;
    let work = tmp.path().join("tree");
    fs::create_dir(&work).with_context(|| format!("creating {}", work.display()))?;
    // A private index: the tree's own git dir is shared with the real one.
    Git::at(&work)
        .env("GIT_DIR", git_dir.as_os_str())
        .env("GIT_WORK_TREE", work.as_os_str())
        .env("GIT_INDEX_FILE", tmp.path().join("index").as_os_str())
        .run(&["read-tree", "-u", "--reset", wip])?;
    apply_manifest(&work, &manifest, &dest.join(FILES))?;
    let root = stamp(&fs::symlink_metadata(tree)?);
    let seen = entries(tree, &skip)?;
    if let Some(reason) = compare(tree, &work, &skip, &seen)? {
        return Ok(Verdict::Differs(reason));
    }
    Ok(Verdict::Same(Snapshot {
        tree: tree.to_path_buf(),
        skip,
        seen,
        root,
    }))
}

/// Every entry under `root` the walk keeps, by exact name, with its stat.
fn entries(root: &Path, skip: &Skip) -> Result<BTreeMap<PathBuf, (Kind, Stamp)>> {
    let mut found = BTreeMap::new();
    let mut it = WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .follow_root_links(false)
        .sort_by_file_name()
        .into_iter();
    while let Some(item) = it.next() {
        let item = item.with_context(|| format!("walking {}", root.display()))?;
        let rel = item
            .path()
            .strip_prefix(root)
            .context("walk left the tree")?
            .to_path_buf();
        let kind = Kind::of(item.file_type());
        if skip.skips(&rel, item.depth(), item.file_name()) {
            if kind == Kind::Dir {
                it.skip_current_dir();
            }
            continue;
        }
        if kind == Kind::Dir && skip.gitlinks.contains(&rel) {
            it.skip_current_dir();
        }
        let meta = item
            .metadata()
            .with_context(|| format!("reading {}", item.path().display()))?;
        found.insert(rel, (kind, stamp(&meta)));
    }
    Ok(found)
}

fn compare(
    tree: &Path,
    restored: &Path,
    skip: &Skip,
    seen: &BTreeMap<PathBuf, (Kind, Stamp)>,
) -> Result<Option<String>> {
    let have: BTreeMap<&PathBuf, Kind> = seen.iter().map(|(r, (k, _))| (r, *k)).collect();
    let want: BTreeMap<PathBuf, Kind> = entries(restored, skip)?
        .into_iter()
        .map(|(r, (k, _))| (r, k))
        .collect();
    if let Some((rel, _)) = have.iter().find(|(_, k)| **k == Kind::Other) {
        return Ok(Some(format!(
            "{}: a fifo, socket or device can't be saved",
            rel.display()
        )));
    }
    if let Some(rel) = have.keys().find(|r| !want.contains_key(**r)) {
        return Ok(Some(format!("{} would not be restored", rel.display())));
    }
    if let Some(rel) = want.keys().find(|r| !have.contains_key(r)) {
        return Ok(Some(format!(
            "{} would be restored but is not in the tree",
            rel.display()
        )));
    }
    for (rel, kind) in &have {
        let (a, b) = (tree.join(rel), restored.join(rel));
        let other = want[*rel];
        if *kind != other {
            return Ok(Some(format!(
                "{} would be restored as a {}, not a {}",
                rel.display(),
                other.name(),
                kind.name()
            )));
        }
        match kind {
            Kind::Symlink => {
                let (ta, tb) = (fs::read_link(&a)?, fs::read_link(&b)?);
                if ta != tb {
                    return Ok(Some(format!(
                        "{} would point at {}, not {}",
                        rel.display(),
                        tb.display(),
                        ta.display()
                    )));
                }
            }
            Kind::File => {
                let (ma, mb) = (fs::symlink_metadata(&a)?, fs::symlink_metadata(&b)?);
                if let (Some(x), Some(y)) = (file_mode(&ma), file_mode(&mb)) {
                    if x != y {
                        return Ok(Some(format!(
                            "{} would be restored with mode {y:o}, not {x:o}",
                            rel.display()
                        )));
                    }
                }
                if ma.len() != mb.len() || !same_bytes(&a, &b)? {
                    return Ok(Some(format!(
                        "{} would be restored with different bytes",
                        rel.display()
                    )));
                }
            }
            Kind::Dir | Kind::Other => {}
        }
    }
    Ok(None)
}

fn same_bytes(a: &Path, b: &Path) -> Result<bool> {
    let mut fa = fs::File::open(a).with_context(|| format!("opening {}", a.display()))?;
    let mut fb = fs::File::open(b).with_context(|| format!("opening {}", b.display()))?;
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

/// Reads until `buf` is full or the file ends.
fn read_full(f: &mut fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
