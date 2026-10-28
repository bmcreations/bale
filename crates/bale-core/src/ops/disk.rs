//! The saved state as `open` writes it to disk: the manifest of ignored
//! files, file modes and empty dirs, and the code that applies it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::git::Git;
use crate::ops::cold::{
    copy_file, copy_symlink, create_private, hex, regen_dirs, submodule_deny, submodule_git_dir,
    Exclusions, SavedFile, FILES, MANIFEST,
};
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
    /// Permission bits of every dir open writes (unix only). Checkout and
    /// the manifest copy create dirs with the umask's mode.
    #[serde(default)]
    pub dir_modes: Vec<SavedMode>,
    /// Permission bits of the tree's root dir (unix only).
    #[serde(default)]
    pub root_mode: Option<u32>,
    /// The sha256 of every regular file open writes (tracked and saved),
    /// taken from the tree's raw bytes by cold's check. Open compares the
    /// raw bytes it wrote against these, so a change in git config between
    /// cold and open fails instead of changing a file. Empty in saves made
    /// before these were recorded; open then skips the check.
    #[serde(default)]
    pub hashes: Vec<SavedHash>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHash {
    pub rel: PathBuf,
    pub sha256: String,
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
    /// Checked-out submodules the compare walks into, each with what is
    /// left out inside it, relative to it (as `submodule_change` judges
    /// it). Its `.git` is left out too.
    pub subs: BTreeMap<PathBuf, Vec<String>>,
}

impl Skip {
    /// The ignored-file rule `save_ignored` applies: under a regenerable dir,
    /// a `node_modules` or `.bale-trash-*` component, or a deny entry.
    pub(crate) fn denied(&self, rel: &Path) -> bool {
        denied(rel, &self.regen, &self.deny)
    }

    /// Whether the walk leaves out `rel` and everything under it.
    fn skips(&self, rel: &Path, depth: usize, name: &std::ffi::OsStr) -> bool {
        // The root's `.git`, and each walked submodule's.
        (name == ".git" && (depth == 1 || rel.parent().is_some_and(|p| self.subs.contains_key(p))))
            || ((self.denied(rel) || self.sub_denied(rel)) && !self.tracked.contains(rel))
    }

    /// Left out inside a submodule.
    fn sub_denied(&self, rel: &Path) -> bool {
        self.subs.iter().any(|(sub, deny)| {
            rel.strip_prefix(sub)
                .is_ok_and(|rest| !rest.as_os_str().is_empty() && denied(rest, &[], deny))
        })
    }

    /// Inside a submodule, which open does not write.
    fn in_sub(&self, rel: &Path) -> bool {
        self.subs
            .keys()
            .any(|sub| rel != sub && rel.starts_with(sub))
    }
}

/// A checked-out submodule as cold's check restores it: the commit the wip
/// tree records for it, from its own git dir.
struct SubCheckout {
    rel: PathBuf,
    commit: String,
    git_dir: PathBuf,
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

/// What the manifest records about the tree at `root` beside the saved
/// files: as the walk under `skip` sees it, the modes of the regular files
/// and the dirs (and the root), and the empty dirs. A dir counts as empty
/// when nothing the walk keeps is inside it.
pub(crate) struct TreeRecord {
    pub modes: Vec<SavedMode>,
    pub dirs: Vec<PathBuf>,
    pub dir_modes: Vec<SavedMode>,
    pub root_mode: Option<u32>,
}

pub(crate) fn record_tree(root: &Path, skip: &Skip) -> Result<TreeRecord> {
    let mut modes = Vec::new();
    let mut dirs = Vec::new();
    let mut dir_modes = Vec::new();
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
            if let Some(mode) = file_mode(&item.metadata()?) {
                dir_modes.push(SavedMode {
                    rel: rel.clone(),
                    mode,
                });
            }
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
    let root_mode = file_mode(&fs::symlink_metadata(root)?);
    Ok(TreeRecord {
        modes,
        dirs,
        dir_modes,
        root_mode,
    })
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
/// dirs, then each recorded file mode, then the dir modes from the deepest
/// up (a dir without write or search permission set first would block what
/// goes under it).
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut dir_modes: Vec<&SavedMode> = manifest.dir_modes.iter().collect();
        dir_modes.sort_by(|a, b| b.rel.cmp(&a.rel));
        let root_mode = manifest.root_mode.map(|mode| (Path::new(""), mode));
        for (rel, mode) in dir_modes
            .iter()
            .map(|m| (m.rel.as_path(), m.mode))
            .chain(root_mode)
        {
            let path = root.join(rel);
            if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                    .with_context(|| format!("setting the mode of {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// Writes the wip tree into `g`'s work tree through git's filters and eol
/// rules. Cold's check and open both check out with this, with the git
/// dir's HEAD where cold found it, so they run under the same config
/// (`includeIf "onbranch:..."` reads HEAD).
pub(crate) fn checkout_wip(g: &Git, wip: &str) -> Result<()> {
    g.run(&["read-tree", "-u", "--reset", wip])?;
    Ok(())
}

/// Gives the owner full access to every dir under `root` (and `root`), so a
/// check dir that copied a read-only dir mode can be removed.
fn loosen(root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut todo = vec![root.to_path_buf()];
        while let Some(dir) = todo.pop() {
            let Ok(meta) = fs::symlink_metadata(&dir) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let mode = meta.permissions().mode() | 0o700;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(mode));
            if let Ok(rd) = fs::read_dir(&dir) {
                todo.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = root;
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
    Same(Box<Snapshot>),
    /// Why open would not give it back.
    Differs(String),
}

/// The tree as the compare walked it: every kept entry's stat signature.
pub(crate) struct Snapshot {
    tree: PathBuf,
    skip: Skip,
    seen: BTreeMap<PathBuf, (Kind, Stamp)>,
    root: Stamp,
    /// The sha256 of every regular file the compare read.
    pub(crate) hashes: Vec<SavedHash>,
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
    ex: &Exclusions,
) -> Result<Verdict> {
    let manifest = read_manifest(dest)?;
    let g = Git::at(tree);
    let git_dir = g.run(&["rev-parse", "--absolute-git-dir"])?;
    let git_dir = PathBuf::from(git_dir.trim_end_matches(['\n', '\r']));
    let listing = g.run(&["ls-tree", "-r", "-z", wip])?;
    let mut paths: Vec<String> = Vec::new();
    let mut gitlinks = BTreeSet::new();
    let mut pending = Vec::new();
    let mut list = |prefix: &str, listing: &str, pending: &mut Vec<(String, String)>| {
        for (meta, path) in listing.split('\0').filter_map(|e| e.split_once('\t')) {
            let path = format!("{prefix}{path}");
            if meta.starts_with("160000 ") {
                let commit = meta.rsplit(' ').next().unwrap_or_default().to_string();
                gitlinks.insert(PathBuf::from(&path));
                pending.push((path.clone(), commit));
            }
            paths.push(path);
        }
    };
    list("", &listing, &mut pending);
    // Each checked-out submodule (nested ones too) is restored and compared
    // like the tree; open leaves it to `git submodule update`, which checks
    // out the same commit through the submodule's own config.
    let sub_deny: Vec<String> = ex.deny.iter().chain(&ex.regen_extra).cloned().collect();
    let mut subs = BTreeMap::new();
    let mut checkouts = Vec::new();
    let mut root_regen: Option<Vec<PathBuf>> = None;
    while let Some((sub, commit)) = pending.pop() {
        let dir = tree.join(&sub);
        let Some(git_dir) = submodule_git_dir(&dir)? else {
            continue;
        };
        // Its `core.worktree` names the old path once the tree is moved.
        let sg = Git::at(&dir)
            .env("GIT_DIR", git_dir.as_os_str())
            .env("GIT_WORK_TREE", dir.as_os_str());
        let listing = sg.run(&["ls-tree", "-r", "-z", &commit])?;
        list(&format!("{sub}/"), &listing, &mut pending);
        let mut deny = submodule_deny(&sub_deny, &sub);
        // As `submodule_change`: the submodule's own regenerable dirs and
        // the root's that fall inside it.
        let root_regen = root_regen.get_or_insert_with(|| regen_dirs(tree));
        let inside = root_regen.iter().filter(|r| r.starts_with(&dir)).cloned();
        for r in regen_dirs(&dir).into_iter().chain(inside) {
            if let Ok(rest) = r.strip_prefix(&dir) {
                deny.push(format!("{}/", rest.to_string_lossy().replace('\\', "/")));
            }
        }
        subs.insert(PathBuf::from(&sub), deny);
        checkouts.push(SubCheckout {
            rel: PathBuf::from(&sub),
            commit,
            git_dir,
        });
    }
    let skip = Skip {
        regen: ex.regen.clone(),
        deny: ex.deny.clone(),
        gitlinks,
        tracked: with_ancestors(paths.iter().map(String::as_str)),
        subs,
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
    let verdict = check_in(
        tree,
        wip,
        dest,
        &manifest,
        &git_dir,
        tmp.path(),
        &work,
        skip,
        &checkouts,
    );
    loosen(&work);
    verdict
}

/// [`restore_check`]'s restore into `work` (with its index in `tmp`) and
/// compare.
#[allow(clippy::too_many_arguments)]
fn check_in(
    tree: &Path,
    wip: &str,
    dest: &Path,
    manifest: &Manifest,
    git_dir: &Path,
    tmp: &Path,
    work: &Path,
    skip: Skip,
    subs: &[SubCheckout],
) -> Result<Verdict> {
    // A private index: the tree's own git dir is shared with the real one.
    let g = Git::at(work)
        .env("GIT_DIR", git_dir.as_os_str())
        .env("GIT_WORK_TREE", work.as_os_str())
        .env("GIT_INDEX_FILE", tmp.join("index").as_os_str());
    checkout_wip(&g, wip)?;
    apply_manifest(work, manifest, &dest.join(FILES))?;
    // Outer submodules first: a nested one's dir comes from its parent.
    for (i, sub) in subs.iter().enumerate() {
        let dir = work.join(&sub.rel);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let sg = Git::at(&dir)
            .env("GIT_DIR", sub.git_dir.as_os_str())
            .env("GIT_WORK_TREE", dir.as_os_str())
            .env("GIT_INDEX_FILE", tmp.join(format!("index-{i}")).as_os_str());
        checkout_wip(&sg, &sub.commit)?;
    }
    let root_meta = fs::symlink_metadata(tree)?;
    let root = stamp(&root_meta);
    let seen = entries(tree, &skip)?;
    if let (Some(x), Some(y)) = (
        file_mode(&root_meta),
        file_mode(&fs::symlink_metadata(work)?),
    ) {
        if x != y {
            return Ok(Verdict::Differs(format!(
                "the tree's root would be restored with mode {y:o}, not {x:o}"
            )));
        }
    }
    let mut hashes = Vec::new();
    if let Some(reason) = compare(tree, work, &skip, &seen, &mut hashes)? {
        return Ok(Verdict::Differs(reason));
    }
    Ok(Verdict::Same(Box::new(Snapshot {
        tree: tree.to_path_buf(),
        skip,
        seen,
        root,
        hashes,
    })))
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
        if kind == Kind::Dir && skip.gitlinks.contains(&rel) && !skip.subs.contains_key(&rel) {
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
    hashes: &mut Vec<SavedHash>,
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
        if let Some(name) = user_xattrs(&a)?.into_iter().next() {
            return Ok(Some(format!(
                "{} has extended attribute {name}, which can't be saved",
                rel.display()
            )));
        }
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
            Kind::Dir => {
                let (ma, mb) = (fs::symlink_metadata(&a)?, fs::symlink_metadata(&b)?);
                if let (Some(x), Some(y)) = (file_mode(&ma), file_mode(&mb)) {
                    if x != y {
                        return Ok(Some(format!(
                            "{} would be restored with mode {y:o}, not {x:o}",
                            rel.display()
                        )));
                    }
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
                match (ma.len() == mb.len()).then(|| same_bytes(&a, &b)) {
                    Some(Ok(Some(_))) if skip.in_sub(rel) => {}
                    Some(Ok(Some(sha256))) => hashes.push(SavedHash {
                        rel: (*rel).clone(),
                        sha256,
                    }),
                    Some(Err(e)) => return Err(e),
                    None | Some(Ok(None)) => {
                        return Ok(Some(format!(
                            "{} would be restored with different bytes",
                            rel.display()
                        )))
                    }
                }
            }
            Kind::Other => {}
        }
    }
    Ok(None)
}

/// The sha256 of `a`'s bytes if `b` holds the same bytes, from one read of
/// each, so the hash is of exactly the bytes compared.
fn same_bytes(a: &Path, b: &Path) -> Result<Option<String>> {
    let mut fa = fs::File::open(a).with_context(|| format!("opening {}", a.display()))?;
    let mut fb = fs::File::open(b).with_context(|| format!("opening {}", b.display()))?;
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    let mut hasher = Sha256::new();
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(None);
        }
        if na == 0 {
            return Ok(Some(hex(&hasher.finalize())));
        }
        hasher.update(&ba[..na]);
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

/// Extended attributes the OS sets on its own, which nothing restores and
/// no one misses: Gatekeeper's provenance and quarantine marks, the
/// last-opened date, the TCC sandbox list (macOS), and SELinux labels
/// (Linux, set again by policy). Anything else, Finder tags and colours
/// included, is the user's.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const OS_XATTRS: &[&str] = &[
    "com.apple.provenance",
    "com.apple.quarantine",
    "com.apple.lastuseddate#PS",
    "com.apple.macl",
    "security.selinux",
];

/// The extended attributes on `path` (not following a symlink) other than
/// [`OS_XATTRS`]. None where the filesystem has no xattrs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn user_xattrs(path: &Path) -> Result<Vec<String>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("{} has a NUL in its path", path.display()))?;
    let list = |buf: &mut [u8]| -> isize {
        let (ptr, len) = if buf.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (buf.as_mut_ptr().cast(), buf.len())
        };
        // SAFETY: `c` is NUL-terminated and `ptr` is null or valid for `len`.
        #[cfg(target_os = "macos")]
        let n = unsafe { libc::listxattr(c.as_ptr(), ptr, len, libc::XATTR_NOFOLLOW) };
        #[cfg(target_os = "linux")]
        let n = unsafe { libc::llistxattr(c.as_ptr(), ptr, len) };
        n
    };
    loop {
        let size = list(&mut []);
        if size < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOTSUP) {
                return Ok(Vec::new());
            }
            return Err(e).with_context(|| format!("listing xattrs of {}", path.display()));
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; size as usize];
        let n = list(&mut buf);
        if n < 0 {
            let e = io::Error::last_os_error();
            // Grew since the size call.
            if e.raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return Err(e).with_context(|| format!("listing xattrs of {}", path.display()));
        }
        buf.truncate(n as usize);
        return Ok(buf
            .split(|b| *b == 0)
            .filter(|n| !n.is_empty())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .filter(|n| !OS_XATTRS.contains(&n.as_str()))
            .collect());
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn user_xattrs(_path: &Path) -> Result<Vec<String>> {
    Ok(Vec::new())
}
