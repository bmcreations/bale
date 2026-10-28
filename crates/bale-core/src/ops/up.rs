use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::busy::{busy_holders, Holder};
use crate::config::load_config;
use crate::detect::{detect, regenerable_dirs};
use crate::git;
use crate::idle::last_source_change;
use crate::lock::WorkspaceLock;
use crate::registry::{workspace_id, Level, Record, Registry};
use crate::BaleHome;

pub(crate) const TRASH_PREFIX: &str = ".bale-trash-";
/// Skip reason for a record a `bale open` left half done.
pub(crate) const OPENING: &str = "opening; run `bale open` to finish";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const DD_NOTE: &str = "DerivedData not removed: ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(Level),
    Busy(Vec<Holder>),
    Skipped(String),
}

/// Creates a `Hot` record for a linked worktree, or refreshes an existing
/// record's path, branch, head and admin dir (keeping its level). Errors for
/// the main checkout and for paths that are not a worktree of their repo.
pub fn register(home: &BaleHome, path: &Path) -> Result<Record> {
    let root = fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let common = git::common_dir(&root)?;
    let info = git::worktrees(&root)?
        .into_iter()
        .find(|w| fs::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone()) == root)
        .with_context(|| {
            format!(
                "{} is not a worktree of {}",
                root.display(),
                common.display()
            )
        })?;
    if info.is_main {
        bail!(
            "{} is the main checkout; bale only manages linked worktrees",
            root.display()
        );
    }
    let admin = git::admin_dir(&root)?;
    let id = workspace_id(&root);
    let registry = Registry::open(home.clone());

    let Some(_lock) = WorkspaceLock::try_acquire(home, &id)? else {
        // Another bale process is working on it; use what it recorded.
        return registry
            .get(&id)?
            .with_context(|| format!("{} is locked by another bale process", root.display()));
    };

    let record = match registry.get(&id)? {
        Some(existing) => Record {
            repo_common_dir: common,
            path: root,
            admin_dir: admin,
            branch: info.branch,
            head: info.head,
            ..existing
        },
        None => {
            let last_activity = regenerable(home, &root)
                .and_then(|dirs| last_source_change(&root, &dirs))
                .unwrap_or_else(|_| SystemTime::now());
            Record {
                id,
                repo_common_dir: common,
                path: root,
                admin_dir: admin,
                branch: info.branch,
                head: info.head,
                level: Level::Hot,
                last_activity,
                note: None,
                trash: Vec::new(),
            }
        }
    };
    registry.put(&record)?;
    Ok(record)
}

/// Turns a CLI argument into a workspace id. An existing path whose record is
/// mid-transition or cold resolves to that record untouched (git run in such a
/// dir could climb to a parent repo or see a detached HEAD); any other
/// existing path is registered. Otherwise `arg` must be a registered id or
/// the path of a registered workspace that no longer exists.
pub fn resolve(home: &BaleHome, arg: &str) -> Result<String> {
    let path = Path::new(arg);
    let registry = Registry::open(home.clone());
    let absolute =
        std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))?;
    let by_path = registry.get(&workspace_id(&absolute))?;
    if fs::symlink_metadata(path).is_ok() {
        match by_path {
            Some(r) if !matches!(r.level, Level::Hot | Level::Baled) => return Ok(r.id),
            _ => return Ok(register(home, path)?.id),
        }
    }
    if let Some(r) = registry.get(arg)? {
        return Ok(r.id);
    }
    by_path
        .map(|r| r.id)
        .with_context(|| format!("{arg} is neither an existing path nor a registered workspace"))
}

/// What `up` did, plus the regenerable-named dirs it left in place because
/// they hold real content (see [`bale_able`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpReport {
    pub outcome: Outcome,
    pub kept: Vec<PathBuf>,
}

impl UpReport {
    fn plain(outcome: Outcome) -> UpReport {
        UpReport {
            outcome,
            kept: Vec::new(),
        }
    }
}

/// Removes the worktree's regenerable output in place and marks it `Baled`.
/// A record left `Baling` by a crash resumes at trash removal.
pub fn up(home: &BaleHome, id: &str) -> Result<Outcome> {
    up_report(home, id).map(|r| r.outcome)
}

/// `up`, also naming the regenerable-named dirs it kept.
pub fn up_report(home: &BaleHome, id: &str) -> Result<UpReport> {
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return Ok(UpReport::plain(Outcome::Skipped("locked".into())));
    };
    up_locked_report(home, id)
}

/// `up` for a caller that already holds the `WorkspaceLock` for `id`.
pub(crate) fn up_locked(home: &BaleHome, id: &str) -> Result<Outcome> {
    up_locked_report(home, id).map(|r| r.outcome)
}

fn up_locked_report(home: &BaleHome, id: &str) -> Result<UpReport> {
    let registry = Registry::open(home.clone());
    let Some(mut record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    let root = fs::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;

    let skip = |reason: &str| Ok(UpReport::plain(Outcome::Skipped(reason.into())));
    let mut kept = Vec::new();
    match record.level {
        Level::Cooling | Level::Cold => return skip("cold"),
        Level::Opening => return skip(OPENING),
        Level::Baling => {}
        Level::Hot | Level::Baled => {
            if let Some(reason) = git::find_worktree(&root, &root)?.foreign_lock() {
                return skip(&reason);
            }
            let holders = busy_holders(&root)?;
            if !holders.is_empty() {
                return Ok(UpReport::plain(Outcome::Busy(holders)));
            }
            let (dirs, real) = bale_able(home, &root)?;
            kept = real;
            let moves = trash_names(&dirs);
            record.trash = moves.iter().map(|(_, to)| to.clone()).collect();
            record.level = Level::Baling;
            registry.put(&record)?;

            if let Err(failure) = move_all(&moves, |from, to| fs::rename(from, to)) {
                record.trash.clear();
                record.level = Level::Hot;
                registry.put(&record)?;
                return rename_failure(failure).map(UpReport::plain);
            }
        }
    }

    remove_trash(&root, &record.trash)?;
    record.trash.clear();
    #[cfg(target_os = "macos")]
    {
        let failures = dirs::home_dir()
            .map(|h| remove_derived_data(&root, &h.join("Library/Developer/Xcode/DerivedData")))
            .unwrap_or_default();
        if !failures.is_empty() {
            record.note = Some(format!("{DD_NOTE}{}", failures.join("; ")));
        } else if record
            .note
            .as_deref()
            .is_some_and(|n| n.starts_with(DD_NOTE))
        {
            record.note = None;
        }
    }

    record.level = Level::Baled;
    registry.put(&record)?;
    Ok(UpReport {
        outcome: Outcome::Done(Level::Baled),
        kept,
    })
}

/// Turns a failed move into the outcome. Dirs stranded in trash are an error
/// naming each one, so the user can move them back; otherwise a permission or
/// sharing error means busy. The rename error stays the root cause.
fn rename_failure(failure: MoveFailure) -> Result<Outcome> {
    let MoveFailure { cause, stranded } = failure;
    let busy = is_busy_error(&cause);
    let err = anyhow::Error::from(cause).context("moving build output aside");
    if !stranded.is_empty() {
        let list: Vec<String> = stranded
            .iter()
            .map(|(trash, original)| format!("{} (was {})", trash.display(), original.display()))
            .collect();
        return Err(err.context(format!(
            "could not restore build output; move these back by hand: {}",
            list.join(", ")
        )));
    }
    if busy {
        return Ok(Outcome::Busy(Vec::new()));
    }
    Err(err)
}

pub(crate) fn regenerable(home: &BaleHome, root: &Path) -> Result<Vec<PathBuf>> {
    let cfg = load_config(home, Some(root))?;
    Ok(regenerable_dirs(root, &detect(root), &cfg))
}

pub(crate) fn is_busy_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || (cfg!(windows) && e.raw_os_error() == Some(32))
}

#[derive(Debug)]
struct MoveFailure {
    cause: io::Error,
    /// `(trash, original)` pairs whose restore rename failed.
    stranded: Vec<(PathBuf, PathBuf)>,
}

/// The regenerable dirs under `root` that `up` may remove, and those it must
/// keep: git tracks a file in them, sees an untracked file there that is not
/// ignored, or they sit in a submodule or nested repo (any dir from below
/// `root` down to the candidate has a `.git` entry), whose files the root's
/// git can't see. Both sorted; trash dirs are left out.
pub(crate) fn bale_able(home: &BaleHome, root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    bale_able_from(root, regenerable(home, root)?)
}

/// `bale_able` over regenerable dirs the caller already listed.
pub(crate) fn bale_able_from(
    root: &Path,
    mut dirs: Vec<PathBuf>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    dirs.retain(|d| !in_trash(root, d));
    dirs.sort();
    let rels: Vec<PathBuf> = dirs
        .iter()
        .map(|d| d.strip_prefix(root).unwrap_or(d).to_path_buf())
        .collect();
    let mut real: Vec<bool> = rels.iter().map(|rel| in_nested_repo(root, rel)).collect();
    let asked: Vec<usize> = (0..rels.len()).filter(|&i| !real[i]).collect();
    for chunk in asked.chunks(100) {
        let mut args: Vec<String> = [
            "--no-optional-locks",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ]
        .map(String::from)
        .to_vec();
        args.extend(
            chunk
                .iter()
                .map(|&i| format!(":(literal){}", slashed(&rels[i]))),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = git::Git::at(root).run(&args)?;
        for listed in out.split('\0').filter(|p| !p.is_empty()) {
            let listed = Path::new(listed);
            // Candidates never nest, so a path lies under at most one.
            if let Some(&i) = chunk.iter().find(|&&i| listed.starts_with(&rels[i])) {
                real[i] = true;
            }
        }
    }
    let (kept, trash): (Vec<_>, Vec<_>) = dirs.into_iter().zip(real).partition(|(_, r)| *r);
    Ok((
        trash.into_iter().map(|(d, _)| d).collect(),
        kept.into_iter().map(|(d, _)| d).collect(),
    ))
}

/// Whether a dir between `root` (exclusive) and `root/rel` (inclusive) has a
/// `.git` entry of any kind.
fn in_nested_repo(root: &Path, rel: &Path) -> bool {
    let mut dir = root.to_path_buf();
    rel.components().any(|c| {
        dir.push(c);
        fs::symlink_metadata(dir.join(".git")).is_ok()
    })
}

/// `rel` with `/` between components, as git pathspecs want.
fn slashed(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `(dir, trash)` for each dir: `<parent>/.bale-trash-<name>-<nanos>`.
fn trash_names(dirs: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dirs.iter()
        .filter_map(|dir| {
            let (parent, name) = (dir.parent()?, dir.file_name()?);
            let trash = parent.join(format!("{TRASH_PREFIX}{}-{nanos}", name.to_string_lossy()));
            Some((dir.clone(), trash))
        })
        .collect()
}

/// Whether `name` is a trash name bale makes: `.bale-trash-<name>-<digits>`.
pub(crate) fn is_trash_name(name: &str) -> bool {
    name.strip_prefix(TRASH_PREFIX)
        .and_then(|rest| rest.rsplit_once('-'))
        .is_some_and(|(orig, digits)| {
            !orig.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Moves each dir to its trash name with `rename`. On the first failure
/// every dir already moved is renamed back; restores that fail are reported.
fn move_all(
    moves: &[(PathBuf, PathBuf)],
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<(), MoveFailure> {
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (dir, trash) in moves {
        let trash = trash.clone();
        if let Err(cause) = rename(dir, &trash) {
            let stranded = moved
                .into_iter()
                .rev()
                .filter(|(from, to)| rename(to, from).is_err())
                .map(|(from, to)| (to, from))
                .collect();
            return Err(MoveFailure { cause, stranded });
        }
        moved.push((dir.clone(), trash));
    }
    Ok(())
}

fn in_trash(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with(TRASH_PREFIX))
    })
}

/// Removes the trash entries `up` recorded for the tree at `root`, and
/// nothing else. Each must have bale's trash name, sit under `root` outside
/// any nested repo, and hold no `.git`; one that does not is an error and
/// stays. One already gone is fine. Symlinks are never followed: a trash
/// entry that is a link has only the link removed.
fn remove_trash(root: &Path, recorded: &[PathBuf]) -> Result<()> {
    for path in recorded {
        if fs::symlink_metadata(path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy());
        let rel = path.strip_prefix(root).ok();
        let parent_rel = rel.and_then(Path::parent);
        let ok = name.is_some_and(|n| is_trash_name(&n))
            && parent_rel.is_some_and(|p| !in_nested_repo(root, p))
            && fs::symlink_metadata(path.join(".git")).is_err();
        if !ok {
            bail!(
                "keeping {}: it is not trash bale made in {}",
                path.display(),
                root.display()
            );
        }
        remove_entry(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn remove_entry(path: &Path) -> io::Result<()> {
    let ft = fs::symlink_metadata(path)?.file_type();
    if ft.is_symlink() {
        // A Windows directory symlink or junction needs remove_dir; neither
        // call follows the link.
        return fs::remove_file(path).or_else(|e| {
            if cfg!(windows) {
                fs::remove_dir(path)
            } else {
                Err(e)
            }
        });
    }
    if ft.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Removes DerivedData folders under `dd_root` built from this worktree and
/// returns a message per folder that could not be probed or removed. A folder
/// a process holds is left alone. Missing `dd_root` is not a failure.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn remove_derived_data(root: &Path, dd_root: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    if !dd_root.is_dir() {
        return failures;
    }
    for dir in crate::derived_data::derived_data_for(root, dd_root) {
        match busy_holders(&dir) {
            Ok(h) if !h.is_empty() => continue,
            Ok(_) => {}
            Err(e) => {
                failures.push(format!("{}: {e:#}", dir.display()));
                continue;
            }
        }
        if let Err(e) = remove_entry(&dir) {
            failures.push(format!("{}: {e}", dir.display()));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn dirs_in(tmp: &Path, names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| {
                let d = tmp.join(n);
                fs::create_dir_all(&d).unwrap();
                fs::write(d.join("f"), n).unwrap();
                d
            })
            .collect()
    }

    fn trash_in(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(TRASH_PREFIX)
            })
            .collect()
    }

    #[test]
    fn second_rename_failure_restores_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dirs = dirs_in(tmp.path(), &["one", "two"]);
        let calls = Cell::new(0);
        let failure = move_all(&trash_names(&dirs), |from, to| {
            calls.set(calls.get() + 1);
            if from == dirs[1] {
                // The first dir really moved before this rename fails.
                assert!(!dirs[0].exists());
                assert_eq!(trash_in(tmp.path()).len(), 1);
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            fs::rename(from, to)
        })
        .unwrap_err();
        // Move one, fail two, restore one.
        assert_eq!(calls.get(), 3);
        assert_eq!(failure.cause.kind(), io::ErrorKind::PermissionDenied);
        assert!(failure.stranded.is_empty());
        assert_eq!(fs::read_to_string(dirs[0].join("f")).unwrap(), "one");
        assert!(trash_in(tmp.path()).is_empty());
    }

    #[test]
    fn failed_restore_is_reported_as_stranded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dirs = dirs_in(tmp.path(), &["one", "two"]);
        let failure = move_all(&trash_names(&dirs), |from, to| {
            let is_restore = from
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(TRASH_PREFIX);
            if from == dirs[1] || is_restore {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            fs::rename(from, to)
        })
        .unwrap_err();
        let trash = trash_in(tmp.path());
        assert_eq!(failure.stranded, vec![(trash[0].clone(), dirs[0].clone())]);

        let err = rename_failure(failure).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&trash[0].display().to_string()), "{msg}");
        assert!(msg.contains(&dirs[0].display().to_string()), "{msg}");
        assert_eq!(
            err.root_cause()
                .downcast_ref::<io::Error>()
                .map(|e| e.kind()),
            Some(io::ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn busy_rename_with_clean_restore_is_busy() {
        let failure = MoveFailure {
            cause: io::Error::from(io::ErrorKind::PermissionDenied),
            stranded: Vec::new(),
        };
        assert_eq!(rename_failure(failure).unwrap(), Outcome::Busy(Vec::new()));
    }

    #[test]
    fn missing_derived_data_root_is_not_a_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(remove_derived_data(tmp.path(), &tmp.path().join("nope")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn derived_data_removal_failure_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(ws.join("App.xcworkspace")).unwrap();
        let folder = tmp.path().join("dd/App-abc");
        fs::create_dir_all(folder.join("Build/locked")).unwrap();
        fs::write(folder.join("Build/locked/x"), "x").unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "WorkspacePath".into(),
            plist::Value::String(ws.join("App.xcworkspace").to_string_lossy().into_owned()),
        );
        plist::Value::Dictionary(dict)
            .to_file_xml(folder.join("info.plist"))
            .unwrap();
        let locked = folder.join("Build/locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

        let failures = remove_derived_data(&ws, &tmp.path().join("dd"));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("App-abc"), "{failures:?}");
    }
}
