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
use crate::ops::up::{is_busy_error, remove_entry, up_locked, Outcome, TRASH_PREFIX};
use crate::registry::{Level, Record, Registry};
use crate::BaleHome;

const MANIFEST: &str = "manifest.json";
const FILES: &str = "files";

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
        Level::Cooling => return recover(home, &registry, record),
        Level::Hot | Level::Baling | Level::Baled => {}
    }

    let root = fs::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    if let Some(reason) = refusal(&root)? {
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

/// Why this tree must not go cold, if it must not. The main checkout is an
/// error; everything else is a skip reason.
fn refusal(root: &Path) -> Result<Option<String>> {
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

    // `add -A` would record an untracked nested repo as a bare gitlink and
    // lose its contents.
    let untracked = g.run(&[
        "ls-files",
        "-z",
        "--others",
        "--exclude-standard",
        "--directory",
    ])?;
    let nested: Vec<&str> = untracked
        .split('\0')
        .filter(|e| e.ends_with('/') && root.join(e).join(".git").exists())
        .collect();
    if !nested.is_empty() {
        return Ok(Some(format!(
            "untracked nested repository: {}",
            nested.join(", ")
        )));
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
        let cfg = load_config(home, Some(root))?;
        let mut deny = cfg.ignored_denylist_extra.clone();
        deny.extend(cfg.regenerable_extra.iter().cloned());
        save_ignored(root, &dest, &deny)?;
        verify(root, &id, &dest)?;
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
    if resolved.is_err() {
        bail!(
            "{} is gone and {wip} does not resolve in {}; leaving any .bale-trash dir beside it",
            path.display(),
            record.repo_common_dir.display()
        );
    }
    let manifest = home.saved_dir(&record.id).join(MANIFEST);
    if !manifest.is_file() {
        bail!(
            "{} is gone and {} is missing; leaving any .bale-trash dir beside it",
            path.display(),
            manifest.display()
        );
    }
    for trash in trash_siblings(&path)? {
        remove_entry(&trash).with_context(|| format!("removing {}", trash.display()))?;
    }
    record.level = Level::Cold;
    registry.put(&record)?;
    Ok(Outcome::Done(Level::Cold))
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
    let prefix = format!("{TRASH_PREFIX}{}-", name.to_string_lossy());
    let mut found = Vec::new();
    for entry in fs::read_dir(parent).with_context(|| format!("reading {}", parent.display()))? {
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
    let regen: Vec<PathBuf> = regenerable_dirs(dir, &detect(dir), &no_extras())
        .into_iter()
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

    match fs::remove_dir_all(dest) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clearing {}", dest.display())),
    }
    let files = dest.join(FILES);
    make_dirs(&files)?;

    let listed = Git::at(dir).run(&[
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ])?;
    let mut saved = Vec::new();
    for entry in listed.split('\0').filter(|e| !e.is_empty()) {
        let rel = PathBuf::from(entry.trim_end_matches('/'));
        if denied(&rel) {
            continue;
        }
        let from = dir.join(&rel);
        let meta =
            fs::symlink_metadata(&from).with_context(|| format!("reading {}", from.display()))?;
        if meta.file_type().is_symlink() {
            saved.push(copy_symlink(&from, &files.join(&rel), rel)?);
            continue;
        }
        let mut it = WalkDir::new(&from)
            .follow_links(false)
            .follow_root_links(false)
            .sort_by_file_name()
            .into_iter();
        while let Some(item) = it.next() {
            let item = item.with_context(|| format!("walking {}", dir.join(&rel).display()))?;
            let rel = item
                .path()
                .strip_prefix(dir)
                .context("walk left the worktree")?
                .to_path_buf();
            let ft = item.file_type();
            if denied(&rel) {
                if ft.is_dir() {
                    it.skip_current_dir();
                }
                continue;
            }
            let to = files.join(&rel);
            if ft.is_dir() {
                make_dirs(&to)?;
            } else if ft.is_symlink() {
                saved.push(copy_symlink(item.path(), &to, rel)?);
            } else if ft.is_file() {
                if let Some(parent) = to.parent() {
                    make_dirs(parent)?;
                }
                let sha256 = copy_file(item.path(), &to)?;
                saved.push(SavedFile {
                    rel,
                    sha256,
                    symlink_target: None,
                });
            }
        }
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

fn create_private(path: &Path) -> Result<File> {
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
fn copy_file(from: &Path, to: &Path) -> Result<String> {
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

fn copy_symlink(from: &Path, to: &Path, rel: PathBuf) -> Result<SavedFile> {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha_file(path: &Path) -> Result<String> {
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
fn verify(root: &Path, id: &str, dest: &Path) -> Result<()> {
    let manifest = dest.join(MANIFEST);
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let saved: Vec<SavedFile> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
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
        verify(&root, "x", &dest).unwrap();
        fs::write(root.join(".env"), "A=2\n").unwrap();
        let err = verify(&root, "x", &dest).unwrap_err();
        assert!(format!("{err:#}").contains(".env"), "{err:#}");
    }

    #[test]
    fn verify_catches_edit_after_wip() {
        let (tmp, root) = repo();
        let dest = tmp.path().join("dest");
        save_wip(&root, "x").unwrap();
        save_ignored(&root, &dest, &[]).unwrap();
        fs::write(root.join("late.txt"), "late\n").unwrap();
        let err = verify(&root, "x", &dest).unwrap_err();
        assert!(
            format!("{err:#}").contains("changed while saving"),
            "{err:#}"
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
        let found = trash_siblings(&tmp.path().join("a")).unwrap();
        assert_eq!(found, vec![tmp.path().join(".bale-trash-a-1")]);
    }
}
