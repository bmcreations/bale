//! `bale cold`: save a worktree's uncommitted work and ignored files, verify
//! the saved state, then delete the checkout while git's entry stays locked.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::busy::{busy_holders, Holder};
use crate::config::{load_config, Config};
use crate::detect::{detect, regenerable_dirs};
use crate::git::{self, Git};
use crate::lock::WorkspaceLock;
use crate::ops::up::{is_busy_error, remove_entry, up_locked, Outcome, OPENING, TRASH_PREFIX};
use crate::registry::{Level, Record, Registry};
use crate::BaleHome;

pub(crate) const MANIFEST: &str = "manifest.json";
pub(crate) const FILES: &str = "files";
/// Names the trash dir `finish` verified and the wip commit it verified
/// against, so recovery can delete what is left of it without git.
const TRASH_MARKER: &str = "trash";

#[derive(Debug, Serialize, Deserialize)]
struct TrashMarker {
    name: String,
    wip: String,
}

/// One saved ignored file, relative to the worktree root. A symlink has
/// `symlink_target` set and `sha256` is the hash of the target path text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFile {
    pub rel: PathBuf,
    pub sha256: String,
    pub symlink_target: Option<PathBuf>,
}

/// Bales the worktree, then saves its work under `refs/bale/<id>/` and
/// `$BALE_HOME/<id>/`, verifies it, and removes the checkout. A record left
/// `Cooling` by a crash resumes from what is on disk.
pub fn cold(home: &BaleHome, id: &str) -> Result<Outcome> {
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return Ok(Outcome::Skipped("locked".into()));
    };
    let registry = Registry::open(home.clone());
    let Some(record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    match record.level {
        Level::Cold => return Ok(Outcome::Skipped("cold".into())),
        Level::Opening => return Ok(Outcome::Skipped(OPENING.into())),
        Level::Cooling => return recover(home, &registry, record),
        Level::Hot | Level::Baling | Level::Baled => {}
    }

    let root = fs::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    if let Some(reason) = refusal(&root, &deny_list(home, &root)?)? {
        return Ok(Outcome::Skipped(reason));
    }
    match up_locked(home, id)? {
        Outcome::Done(Level::Baled) => {}
        other => return Ok(other),
    }

    // `up` rewrote the record; build on its copy.
    let mut record = registry
        .get(id)?
        .with_context(|| format!("record for {id} vanished"))?;
    git::lock_worktree(&root, &root)?;
    record.level = Level::Cooling;
    registry.put(&record)?;
    finish(home, &registry, record, &root)
}

/// Config `ignored_denylist` plus `regenerable` names for the tree at `root`.
fn deny_list(home: &BaleHome, root: &Path) -> Result<Vec<String>> {
    let cfg = load_config(home, Some(root))?;
    let mut deny = cfg.ignored_denylist_extra;
    deny.extend(cfg.regenerable_extra);
    Ok(deny)
}

/// Why this tree must not go cold, if it must not. The main checkout is an
/// error; everything else is a skip reason.
fn refusal(root: &Path, deny: &[String]) -> Result<Option<String>> {
    let info = git::worktrees(root)?
        .into_iter()
        .find(|w| fs::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone()) == root)
        .with_context(|| format!("{} is not a git worktree", root.display()))?;
    if info.is_main {
        bail!(
            "{} is the main checkout; bale only colds linked worktrees",
            root.display()
        );
    }

    let g = Git::at(root);
    let sparse = g.run(&[
        "config",
        "--bool",
        "--default",
        "false",
        "--get",
        "core.sparseCheckout",
    ])?;
    if sparse.trim() == "true" {
        return Ok(Some("sparse checkout".into()));
    }
    // A skip-worktree entry's edits are invisible to `add -A`.
    let skipped: Vec<String> = ls_files_v(&g)?
        .into_iter()
        .filter(|(tag, _)| tag.eq_ignore_ascii_case(&'S'))
        .map(|(_, p)| p)
        .collect();
    if !skipped.is_empty() {
        return Ok(Some(format!(
            "sparse: skip-worktree entries ({})",
            skipped.join(", ")
        )));
    }

    let dirty = g.run(&[
        "submodule",
        "--quiet",
        "foreach",
        "--recursive",
        r#"test -z "$(git status --porcelain)" || echo "$displaypath""#,
    ])?;
    let dirty: Vec<&str> = dirty.lines().filter(|l| !l.is_empty()).collect();
    if !dirty.is_empty() {
        return Ok(Some(format!("dirty submodule: {}", dirty.join(", "))));
    }

    // Nothing in a submodule's checkout is saved except what its own commits
    // hold, so its ignored files (other than regenerable output) or an
    // embedded git dir would be lost.
    let subs = g.run(&[
        "submodule",
        "--quiet",
        "foreach",
        "--recursive",
        r#"echo "$displaypath""#,
    ])?;
    let subs: Vec<&str> = subs.lines().filter(|l| !l.is_empty()).collect();
    let root_regen = if subs.is_empty() {
        Vec::new()
    } else {
        regen_dirs(root)
    };
    let mut ignored = Vec::new();
    let mut embedded = Vec::new();
    for sub in subs {
        let dir = root.join(sub);
        if fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.is_dir()) {
            embedded.push(sub);
            continue;
        }
        let mut regen = regen_dirs(&dir);
        regen.extend(root_regen.iter().filter(|r| r.starts_with(&dir)).cloned());
        let scan = scan_ignored_with(&dir, &submodule_deny(deny, sub), &regen)?;
        if !scan.entries.is_empty() || !scan.nested.is_empty() {
            ignored.push(sub);
        }
    }
    if !embedded.is_empty() {
        return Ok(Some(format!(
            "submodule with its own .git dir: {}",
            embedded.join(", ")
        )));
    }
    if !ignored.is_empty() {
        return Ok(Some(format!(
            "submodule with ignored files: {}",
            ignored.join(", ")
        )));
    }

    // `add -A` records a nested repo in an untracked dir as a bare gitlink,
    // and copying one out of an ignored dir flattens it; either way its
    // history would be deleted.
    let untracked = g.run(&[
        "ls-files",
        "-z",
        "--others",
        "--exclude-standard",
        "--directory",
    ])?;
    let mut nested = Vec::new();
    for entry in untracked.split('\0').filter(|e| e.ends_with('/')) {
        let mut it = WalkDir::new(root.join(entry))
            .follow_links(false)
            .follow_root_links(false)
            .into_iter();
        while let Some(item) = it.next() {
            // An unreadable dir could hide a nested repo.
            let item = match item {
                Ok(item) => item,
                Err(e) => {
                    let path = e.path().unwrap_or(&root.join(entry)).to_path_buf();
                    let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                    return Ok(Some(format!("unreadable: {}", rel.display())));
                }
            };
            if item.depth() > 0 && item.file_name() == ".git" {
                if let Ok(rel) = item.path().strip_prefix(root) {
                    nested.push(rel.parent().unwrap_or(rel).to_path_buf());
                }
                if item.file_type().is_dir() {
                    it.skip_current_dir();
                }
            }
        }
    }
    nested.extend(scan_ignored(root, deny)?.nested);
    if !nested.is_empty() {
        return Ok(Some(format!("nested repository: {}", join_paths(&nested))));
    }
    Ok(None)
}

/// Steps 3-7 for a `Cooling` record whose checkout is at `root`. Any failure
/// before the checkout moves puts the record back to `Baled`.
fn finish(
    home: &BaleHome,
    registry: &Registry,
    mut record: Record,
    root: &Path,
) -> Result<Outcome> {
    let id = record.id.clone();
    let prepared = (|| -> Result<Vec<Holder>> {
        let dest = home.saved_dir(&id);
        save_wip(root, &id)?;
        let deny = deny_list(home, root)?;
        save_ignored(root, &dest, &deny)?;
        verify(root, &id, &dest, &deny)?;
        busy_holders(root)
    })();
    let holders = match prepared {
        Ok(h) => h,
        Err(e) => {
            record.level = Level::Baled;
            registry.put(&record)?;
            return Err(e);
        }
    };
    if !holders.is_empty() {
        record.level = Level::Baled;
        registry.put(&record)?;
        return Ok(Outcome::Busy(holders));
    }

    let trash = trash_path(root)?;
    if let Err(e) = write_trash_marker(&home.saved_dir(&id), root, &id, &trash) {
        record.level = Level::Baled;
        registry.put(&record)?;
        return Err(e);
    }
    if let Err(e) = fs::rename(root, &trash) {
        record.level = Level::Baled;
        registry.put(&record)?;
        if is_busy_error(&e) {
            return Ok(Outcome::Busy(Vec::new()));
        }
        return Err(e).with_context(|| format!("moving {} aside", root.display()));
    }
    // A failed removal leaves the record `Cooling`; the next run finishes it.
    remove_entry(&trash).with_context(|| format!("removing {}", trash.display()))?;
    record.level = Level::Cold;
    registry.put(&record)?;
    Ok(Outcome::Done(Level::Cold))
}

/// Resumes a `Cooling` record. A present checkout is saved again from
/// scratch; a missing one is only finished off (trash removed, `Cold`) when
/// the wip ref and the manifest both exist.
fn recover(home: &BaleHome, registry: &Registry, mut record: Record) -> Result<Outcome> {
    let path = record.path.clone();
    if fs::symlink_metadata(&path).is_ok() {
        let root =
            fs::canonicalize(&path).with_context(|| format!("resolving {}", path.display()))?;
        if let Some(reason) = refusal(&root, &deny_list(home, &root)?)? {
            record.level = Level::Baled;
            registry.put(&record)?;
            return Ok(Outcome::Skipped(reason));
        }
        git::lock_worktree(&root, &root)?;
        return finish(home, registry, record, &root);
    }

    let wip = format!("refs/bale/{}/wip", record.id);
    let resolved = Git::at(&record.repo_common_dir).run(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{wip}^{{commit}}"),
    ]);
    let Ok(wip_sha) = resolved else {
        bail!(
            "{} is gone and {wip} does not resolve in {}; leaving any .bale-trash dir beside it",
            path.display(),
            record.repo_common_dir.display()
        );
    };
    let manifest = home.saved_dir(&record.id).join(MANIFEST);
    if !manifest.is_file() {
        bail!(
            "{} is gone and {} is missing; leaving any .bale-trash dir beside it",
            path.display(),
            manifest.display()
        );
    }
    let dest = home.saved_dir(&record.id);
    let marker = read_trash_marker(&dest)?;
    for trash in trash_siblings(&path)? {
        // `finish` verified this trash against this wip just before moving
        // it; a removal that failed partway can't be verified again.
        let verified = marker.as_ref().is_some_and(|m| {
            trash.file_name().and_then(|n| n.to_str()) == Some(m.name.as_str())
                && m.wip == wip_sha.trim()
        });
        if !verified {
            // Without its `.git`, git would run in whatever repo encloses it.
            if fs::symlink_metadata(trash.join(".git")).is_err() {
                bail!(
                    "keeping {}: it has no .git, so it cannot be checked against {wip}",
                    trash.display()
                );
            }
            // Still a working checkout: prove the saved state covers it (and
            // is not stale) before deleting it.
            let deny = deny_list(home, &trash)?;
            verify(&trash, &record.id, &dest, &deny)
                .with_context(|| format!("keeping {}", trash.display()))?;
        }
        remove_entry(&trash).with_context(|| format!("removing {}", trash.display()))?;
    }
    record.level = Level::Cold;
    registry.put(&record)?;
    Ok(Outcome::Done(Level::Cold))
}

/// Writes `dest/trash` (tmp + rename, 0600 on unix) naming `trash` and the
/// current `refs/bale/<id>/wip` commit.
fn write_trash_marker(dest: &Path, root: &Path, id: &str, trash: &Path) -> Result<()> {
    let wip = Git::at(root).run(&[
        "rev-parse",
        "--verify",
        &format!("refs/bale/{id}/wip^{{commit}}"),
    ])?;
    let name = trash
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} is not valid UTF-8", trash.display()))?;
    let marker = TrashMarker {
        name: name.to_string(),
        wip: wip.trim().to_string(),
    };
    let path = dest.join(TRASH_MARKER);
    let tmp = dest.join(format!("{TRASH_MARKER}.tmp"));
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clearing {}", tmp.display())),
    }
    let mut f = create_private(&tmp)?;
    f.write_all(&serde_json::to_vec(&marker)?)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

fn read_trash_marker(dest: &Path) -> Result<Option<TrashMarker>> {
    let path = dest.join(TRASH_MARKER);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let marker =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(marker))
}

fn trash_path(root: &Path) -> Result<PathBuf> {
    let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
        bail!("{} has no parent directory", root.display());
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(parent.join(format!("{TRASH_PREFIX}{}-{nanos}", name.to_string_lossy())))
}

/// `<parent>/.bale-trash-<name>-<digits>` entries for the checkout at `path`.
fn trash_siblings(path: &Path) -> Result<Vec<PathBuf>> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(Vec::new());
    };
    let parent =
        fs::canonicalize(parent).with_context(|| format!("resolving {}", parent.display()))?;
    let prefix = format!("{TRASH_PREFIX}{}-", name.to_string_lossy());
    let mut found = Vec::new();
    for entry in fs::read_dir(&parent).with_context(|| format!("reading {}", parent.display()))? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(rest) = file_name.strip_prefix(&prefix) else {
            continue;
        };
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            found.push(entry.path());
        }
    }
    Ok(found)
}

/// Writes `refs/bale/<id>/head` (HEAD) and `refs/bale/<id>/wip`: a commit
/// whose tree is the working tree as `add -A` sees it, parent HEAD. All work
/// happens in a temporary index; the real one is never written.
pub fn save_wip(dir: &Path, id: &str) -> Result<()> {
    let g = Git::at(dir);
    let head = g.run(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    let head = head.trim();
    let tree = snapshot_tree(dir)?;
    let commit = g
        .clone()
        .env("GIT_AUTHOR_NAME", "bale")
        .env("GIT_AUTHOR_EMAIL", "bale@localhost")
        .env("GIT_COMMITTER_NAME", "bale")
        .env("GIT_COMMITTER_EMAIL", "bale@localhost")
        .run(&[
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            &tree,
            "-p",
            head,
            "-m",
            "bale wip",
        ])?;
    let commit = commit.trim();
    g.run(&["update-ref", &format!("refs/bale/{id}/head"), head])?;
    g.run(&["update-ref", &format!("refs/bale/{id}/wip"), commit])?;
    Ok(())
}

/// The tree `add -A` would build, computed in a temporary copy of the index
/// (so force-added files stay), with assume-unchanged bits cleared (so their
/// edits are seen). Without a real index it starts from `read-tree HEAD`.
fn snapshot_tree(dir: &Path) -> Result<String> {
    let tmp = tempfile::TempDir::new().context("creating a temporary index dir")?;
    let index = tmp.path().join("index");
    let real = Git::at(dir).run(&["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let real = PathBuf::from(real.trim_end_matches(['\n', '\r']));
    let g = Git::at(dir).env("GIT_INDEX_FILE", index.as_os_str());
    if real.is_file() {
        fs::copy(&real, &index).with_context(|| format!("copying {}", real.display()))?;
    } else {
        g.run(&["read-tree", "HEAD"])?;
    }
    let assumed: Vec<String> = ls_files_v(&g)?
        .into_iter()
        .filter(|(tag, _)| tag.is_ascii_lowercase())
        .map(|(_, p)| p)
        .collect();
    for chunk in assumed.chunks(200) {
        let mut args = vec!["update-index", "--no-assume-unchanged", "--"];
        args.extend(chunk.iter().map(String::as_str));
        g.run(&args)?;
    }
    g.run(&["add", "-A"])?;
    Ok(g.run(&["write-tree"])?.trim().to_string())
}

/// `(tag, path)` for each entry of `git ls-files -v`.
fn ls_files_v(g: &Git) -> Result<Vec<(char, String)>> {
    let out = g.run(&["ls-files", "-v", "-z"])?;
    Ok(out
        .split('\0')
        .filter_map(|e| {
            let tag = e.chars().next()?;
            Some((tag, e.get(2..)?.to_string()))
        })
        .collect())
}

/// Copies the worktree's ignored files to `dest/files/` (dirs 0700, files
/// 0600 on unix) and writes `dest/manifest.json`. Regenerable dirs,
/// `node_modules` and `deny` are skipped: an entry without `/` matches any
/// path component, one with `/` matches a path prefix. Symlinks are saved as
/// symlinks and never followed. `dest` is cleared first.
pub fn save_ignored(dir: &Path, dest: &Path, deny: &[String]) -> Result<Vec<SavedFile>> {
    let scan = scan_ignored(dir, deny)?;
    if !scan.nested.is_empty() {
        bail!(
            "nested repository among ignored files: {}",
            join_paths(&scan.nested)
        );
    }

    match fs::remove_dir_all(dest) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clearing {}", dest.display())),
    }
    let files = dest.join(FILES);
    make_dirs(&files)?;

    let mut saved = Vec::new();
    for (rel, is_link) in scan.entries {
        let from = dir.join(&rel);
        let to = files.join(&rel);
        if is_link {
            saved.push(copy_symlink(&from, &to, rel)?);
            continue;
        }
        if let Some(parent) = to.parent() {
            make_dirs(parent)?;
        }
        let sha256 = copy_file(&from, &to)?;
        saved.push(SavedFile {
            rel,
            sha256,
            symlink_target: None,
        });
    }

    let manifest = dest.join(MANIFEST);
    let tmp = dest.join(format!("{MANIFEST}.tmp"));
    let mut f = create_private(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(&saved)?)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, &manifest).with_context(|| format!("writing {}", manifest.display()))?;
    Ok(saved)
}

/// What `save_ignored` would save: `(rel, is_symlink)` for every ignored
/// file and symlink outside the denylist, plus every nested repository
/// (a dir holding a `.git` entry) it met on the way.
struct IgnoredScan {
    entries: Vec<(PathBuf, bool)>,
    nested: Vec<PathBuf>,
}

fn scan_ignored(dir: &Path, deny: &[String]) -> Result<IgnoredScan> {
    scan_ignored_with(dir, deny, &regen_dirs(dir))
}

/// `deny` as it applies inside the submodule at `sub` (relative to the
/// worktree root): bare names as they are; a slashed entry only when it lies
/// under `sub/`, rebased onto the submodule root.
fn submodule_deny(deny: &[String], sub: &str) -> Vec<String> {
    let sub = sub.trim_end_matches('/');
    deny.iter()
        .filter_map(|d| {
            if !d.contains('/') {
                return Some(d.clone());
            }
            let d = d.trim_end_matches('/');
            if d == sub {
                // The whole submodule; "/" trims to "", a prefix of everything.
                return Some("/".to_string());
            }
            d.strip_prefix(sub)?
                .strip_prefix('/')
                .map(|rest| format!("{rest}/"))
        })
        .collect()
}

/// Regenerable dirs under `dir` for the build systems detected there.
fn regen_dirs(dir: &Path) -> Vec<PathBuf> {
    regenerable_dirs(dir, &detect(dir), &no_extras())
}

/// `scan_ignored` with the regenerable dirs (absolute) given by the caller.
fn scan_ignored_with(dir: &Path, deny: &[String], regen: &[PathBuf]) -> Result<IgnoredScan> {
    let regen: Vec<PathBuf> = regen
        .iter()
        .filter_map(|p| p.strip_prefix(dir).ok().map(Path::to_path_buf))
        .collect();
    let denied = |rel: &Path| -> bool {
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
    };

    let listed = Git::at(dir).run(&[
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ])?;
    let mut tops: Vec<PathBuf> = listed
        .split('\0')
        .filter(|e| !e.is_empty())
        .map(|e| PathBuf::from(e.trim_end_matches('/')))
        .filter(|rel| !denied(rel))
        .collect();
    tops.sort();
    tops.dedup();
    // git can list a dir and also a file inside it; the walk of the dir
    // covers the file.
    let mut kept: Vec<PathBuf> = Vec::new();
    for rel in tops {
        if !kept.iter().any(|k| rel.starts_with(k)) {
            kept.push(rel);
        }
    }

    let mut scan = IgnoredScan {
        entries: Vec::new(),
        nested: Vec::new(),
    };
    for top in kept {
        let from = dir.join(&top);
        let mut it = WalkDir::new(&from)
            .follow_links(false)
            .follow_root_links(false)
            .sort_by_file_name()
            .into_iter();
        while let Some(item) = it.next() {
            let item = item.with_context(|| format!("walking {}", from.display()))?;
            let rel = item
                .path()
                .strip_prefix(dir)
                .context("walk left the worktree")?
                .to_path_buf();
            let ft = item.file_type();
            if item.file_name() == ".git" {
                scan.nested
                    .push(rel.parent().map(Path::to_path_buf).unwrap_or_default());
                if ft.is_dir() {
                    it.skip_current_dir();
                }
                continue;
            }
            if denied(&rel) {
                if ft.is_dir() {
                    it.skip_current_dir();
                }
                continue;
            }
            if ft.is_symlink() {
                scan.entries.push((rel, true));
            } else if ft.is_file() {
                scan.entries.push((rel, false));
            }
        }
    }
    Ok(scan)
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A config with no extra regenerable names; callers pass those via `deny`.
fn no_extras() -> Config {
    Config {
        idle_baled: Duration::ZERO,
        idle_cold: Duration::ZERO,
        regenerable_extra: Vec::new(),
        ignored_denylist_extra: Vec::new(),
        worktree_root: None,
    }
}

fn make_dirs(path: &Path) -> Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(path)
        .with_context(|| format!("creating {}", path.display()))
}

pub(crate) fn create_private(path: &Path) -> Result<File> {
    let mut o = OpenOptions::new();
    // Never reuse an existing path: it could be a link out of `dest`.
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
        .with_context(|| format!("creating {}", path.display()))
}

/// Copies `from` to `to` with mode 0600 and returns the sha256 of the bytes.
pub(crate) fn copy_file(from: &Path, to: &Path) -> Result<String> {
    let mut src = File::open(from).with_context(|| format!("opening {}", from.display()))?;
    let mut dst = create_private(to)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = src
            .read(&mut buf)
            .with_context(|| format!("reading {}", from.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n])
            .with_context(|| format!("writing {}", to.display()))?;
    }
    dst.sync_all()?;
    Ok(hex(&hasher.finalize()))
}

pub(crate) fn copy_symlink(from: &Path, to: &Path, rel: PathBuf) -> Result<SavedFile> {
    let target = fs::read_link(from).with_context(|| format!("reading {}", from.display()))?;
    if let Some(parent) = to.parent() {
        make_dirs(parent)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, to)
        .with_context(|| format!("creating {}", to.display()))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        let is_dir = fs::symlink_metadata(from)?.file_type().is_symlink_dir();
        if is_dir {
            std::os::windows::fs::symlink_dir(&target, to)
        } else {
            std::os::windows::fs::symlink_file(&target, to)
        }
        .with_context(|| format!("creating {}", to.display()))?;
    }
    Ok(SavedFile {
        rel,
        sha256: hex(&Sha256::digest(target.to_string_lossy().as_bytes())),
        symlink_target: Some(target),
    })
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn sha_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Checks the saved state before anything is deleted: each manifest entry
/// matches both the source and the copy, and the working tree still builds
/// exactly the wip commit's tree.
fn verify(root: &Path, id: &str, dest: &Path, deny: &[String]) -> Result<()> {
    let manifest = dest.join(MANIFEST);
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let saved: Vec<SavedFile> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
    let mut want: Vec<PathBuf> = scan_ignored(root, deny)?
        .entries
        .into_iter()
        .map(|(rel, _)| rel)
        .collect();
    let mut have: Vec<PathBuf> = saved.iter().map(|f| f.rel.clone()).collect();
    want.sort();
    have.sort();
    if want != have {
        let missing: Vec<PathBuf> = want.iter().filter(|w| !have.contains(w)).cloned().collect();
        let extra: Vec<PathBuf> = have.iter().filter(|h| !want.contains(h)).cloned().collect();
        bail!(
            "ignored files changed while saving: unsaved [{}], gone [{}]",
            join_paths(&missing),
            join_paths(&extra)
        );
    }
    for f in &saved {
        let src = root.join(&f.rel);
        let copy = dest.join(FILES).join(&f.rel);
        match &f.symlink_target {
            Some(target) => {
                for p in [&src, &copy] {
                    let is_link = fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
                    if !is_link || fs::read_link(p).ok().as_ref() != Some(target) {
                        bail!(
                            "saved symlink {} does not match {}",
                            f.rel.display(),
                            p.display()
                        );
                    }
                }
            }
            None => {
                for p in [&src, &copy] {
                    let is_file = fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file());
                    if !is_file || sha_file(p)? != f.sha256 {
                        bail!(
                            "saved file {} does not match {}",
                            f.rel.display(),
                            p.display()
                        );
                    }
                }
            }
        }
    }

    let rebuilt = snapshot_tree(root)?;
    let wip = Git::at(root).run(&["rev-parse", &format!("refs/bale/{id}/wip^{{tree}}")])?;
    if rebuilt != wip.trim() {
        bail!(
            "working tree changed while saving: tree {rebuilt} is not refs/bale/{id}/wip's {}",
            wip.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap().join("r");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "t"]);
        git(&root, &["config", "user.email", "t@t"]);
        fs::write(root.join(".gitignore"), ".env\n").unwrap();
        git(&root, &["add", ".gitignore"]);
        git(
            &root,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "i"],
        );
        (tmp, root)
    }

    #[test]
    fn verify_catches_changed_ignored_file() {
        let (tmp, root) = repo();
        fs::write(root.join(".env"), "A=1\n").unwrap();
        let dest = tmp.path().join("dest");
        save_wip(&root, "x").unwrap();
        save_ignored(&root, &dest, &[]).unwrap();
        verify(&root, "x", &dest, &[]).unwrap();
        fs::write(root.join(".env"), "A=2\n").unwrap();
        let err = verify(&root, "x", &dest, &[]).unwrap_err();
        assert!(format!("{err:#}").contains(".env"), "{err:#}");
    }

    #[test]
    fn verify_catches_new_ignored_file() {
        let (tmp, root) = repo();
        let dest = tmp.path().join("dest");
        save_wip(&root, "x").unwrap();
        save_ignored(&root, &dest, &[]).unwrap();
        fs::write(root.join(".env"), "late\n").unwrap();
        let err = verify(&root, "x", &dest, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("unsaved [.env]"), "{err:#}");
    }

    #[test]
    fn verify_catches_edit_after_wip() {
        let (tmp, root) = repo();
        let dest = tmp.path().join("dest");
        save_wip(&root, "x").unwrap();
        save_ignored(&root, &dest, &[]).unwrap();
        fs::write(root.join("late.txt"), "late\n").unwrap();
        let err = verify(&root, "x", &dest, &[]).unwrap_err();
        assert!(
            format!("{err:#}").contains("changed while saving"),
            "{err:#}"
        );
    }

    #[test]
    fn submodule_deny_rebases_only_entries_under_the_submodule() {
        let deny: Vec<String> = [
            "node",
            "cache/x",
            "lib/cache",
            "lib",
            "lib/",
            "libx/a",
            "lib/sub/y/",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            submodule_deny(&deny, "lib"),
            ["node", "cache/", "lib", "/", "sub/y/"]
                .map(String::from)
                .to_vec()
        );
    }

    #[test]
    fn trash_siblings_match_only_this_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        for n in [
            ".bale-trash-a-1",
            ".bale-trash-a-b-2",
            ".bale-trash-a-x",
            "a",
        ] {
            fs::create_dir_all(tmp.path().join(n)).unwrap();
        }
        // Found under the canonical parent (macOS temp dirs sit behind /var -> /private/var).
        let found = trash_siblings(&tmp.path().join("a")).unwrap();
        let parent = fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(found, vec![parent.join(".bale-trash-a-1")]);
    }
}
