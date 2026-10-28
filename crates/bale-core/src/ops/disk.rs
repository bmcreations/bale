//! The saved state as `open` writes it to disk: the manifest of ignored
//! files, file modes and empty dirs, and the code that applies it.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::ops::cold::{copy_file, copy_symlink, create_private, SavedFile, MANIFEST};
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
}

impl Skip {
    /// The ignored-file rule `save_ignored` applies: under a regenerable dir,
    /// a `node_modules` or `.bale-trash-*` component, or a deny entry.
    pub(crate) fn denied(&self, rel: &Path) -> bool {
        denied(rel, &self.regen, &self.deny)
    }

    /// Whether the walk leaves out `rel` and everything under it.
    fn skips(&self, rel: &Path, depth: usize, name: &std::ffi::OsStr) -> bool {
        (depth == 1 && name == ".git") || self.denied(rel)
    }
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
