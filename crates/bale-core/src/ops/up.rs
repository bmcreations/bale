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
    Skipped(SkipReason),
}

/// Why an operation left a tree alone, by kind. The usage trace records
/// only this, never the reason's text, which can name paths in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipCode {
    /// Another bale process holds the workspace lock.
    Locked,
    /// The tree is already cold.
    Cold,
    /// A `bale open` was left half done.
    Opening,
    /// The tree has uncommitted changes (`cold_if_clean`).
    Dirty,
    /// Someone other than bale locked the git worktree.
    WorktreeLocked,
    /// What is at the record's path is not the worktree's checkout.
    IdentityMismatch,
    SparseCheckout,
    /// Assume-unchanged or skip-worktree index entries.
    HiddenEntries,
    /// A submodule holds changes or files the save would not keep.
    Submodule,
    NestedRepo,
    /// A file's extended attributes, flags or ACL can't be saved.
    DirtyXattr,
    /// `open` would not give the tree back as it is.
    NotRestorable,
    /// Too little free space to check the save.
    FreeSpace,
    /// A dir the checks must read can't be read.
    Unreadable,
}

impl SkipCode {
    /// The code as the trace writes it.
    pub fn name(self) -> &'static str {
        match self {
            SkipCode::Locked => "locked",
            SkipCode::Cold => "cold",
            SkipCode::Opening => "opening",
            SkipCode::Dirty => "dirty",
            SkipCode::WorktreeLocked => "worktree_locked",
            SkipCode::IdentityMismatch => "identity_mismatch",
            SkipCode::SparseCheckout => "sparse_checkout",
            SkipCode::HiddenEntries => "hidden_entries",
            SkipCode::Submodule => "submodule",
            SkipCode::NestedRepo => "nested_repo",
            SkipCode::DirtyXattr => "dirty_xattr",
            SkipCode::NotRestorable => "not_restorable",
            SkipCode::FreeSpace => "free_space",
            SkipCode::Unreadable => "unreadable",
        }
    }
}

/// A skip's kind and the text shown to the user. Derefs to the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipReason {
    pub code: SkipCode,
    pub text: String,
}

impl SkipReason {
    pub fn new(code: SkipCode, text: impl Into<String>) -> SkipReason {
        SkipReason {
            code,
            text: text.into(),
        }
    }
}

impl std::ops::Deref for SkipReason {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq<str> for SkipReason {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

/// Creates a `Hot` record for a linked worktree, or refreshes an existing
/// record's path, branch, head and admin dir (keeping its level). Errors for
/// the main checkout and for paths that are not a worktree of their repo.
pub fn register(home: &BaleHome, path: &Path) -> Result<Record> {
    crate::trace::op(home, "adopt", None, || register_untraced(home, path))
}

/// `register` without an `op` line, for operations that register on the
/// way (`resolve`, `new`) and are traced as themselves.
pub(crate) fn register_untraced(home: &BaleHome, path: &Path) -> Result<Record> {
    let root = crate::paths::canonicalize(path)
        .with_context(|| format!("resolving {}", path.display()))?;
    // Without its own gitfile, git here climbs to an enclosing repo and the
    // record would be repointed at it, defeating every checkout check.
    if fs::symlink_metadata(root.join(".git")).is_err() {
        bail!(
            "{} has no .git, so it is not a worktree checkout; move it away",
            root.display()
        );
    }
    let common = git::common_dir(&root)?;
    let listed = git::worktrees(&root)?;
    let info = listed
        .iter()
        .find(|w| crate::paths::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone()) == root)
        .cloned()
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
    // A linked checkout's admin dir always sits under <common>/worktrees/. A
    // gitfile pointing anywhere else (the main git dir, say) is not this
    // worktree, whatever `git worktree list` still says about the path.
    let worktrees_dir =
        crate::paths::canonicalize(common.join("worktrees")).unwrap_or(common.join("worktrees"));
    let admin_canon = crate::paths::canonicalize(&admin).unwrap_or_else(|_| admin.clone());
    if admin_canon.parent() != Some(worktrees_dir.as_path()) {
        bail!(
            "{} uses git dir {}, which is not a linked worktree's; move it away",
            root.display(),
            admin.display()
        );
    }
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
                origin: None,
                pinned: false,
                scope: None,
                stash: None,
                restore_hook: false,
                spm: Vec::new(),
            }
        }
    };
    #[cfg(target_os = "macos")]
    let record = {
        let mut record = record;
        link_swift_packages(home, &mut record, &listed);
        record
    };
    registry.put(&record)?;
    Ok(record)
}

/// Links the tree's new Swift projects to shared packages, seeding a shared
/// folder from the repo's other worktrees (the list `register` already read)
/// when the key has none. Best effort: a failure leaves the record as it was.
#[cfg(target_os = "macos")]
fn link_swift_packages(home: &BaleHome, record: &mut Record, listed: &[git::WorktreeInfo]) {
    let Some(dd_root) = crate::derived_data::global_root() else {
        return;
    };
    let siblings: Vec<PathBuf> = listed
        .iter()
        .filter(|w| !w.bare && !w.prunable)
        .filter_map(|w| crate::paths::canonicalize(&w.path).ok())
        .filter(|p| *p != record.path)
        .collect();
    let made = crate::spm_link::link_tree(home, &record.path, &dd_root, &siblings);
    crate::spm_link::merge_links(record, made);
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
            _ => return Ok(register_untraced(home, path)?.id),
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
    crate::trace::op(home, "up", Some(id), || {
        let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
            return Ok(UpReport::plain(Outcome::Skipped(SkipReason::new(
                SkipCode::Locked,
                "locked",
            ))));
        };
        up_locked_report(home, id, &[])
    })
}

/// `up` for a caller that already holds the `WorkspaceLock` for `id`.
/// Processes in `ignore` do not count as holding the tree.
pub(crate) fn up_locked(home: &BaleHome, id: &str, ignore: &[u32]) -> Result<Outcome> {
    up_locked_report(home, id, ignore).map(|r| r.outcome)
}

fn up_locked_report(home: &BaleHome, id: &str, ignore: &[u32]) -> Result<UpReport> {
    let registry = Registry::open(home.clone());
    let Some(mut record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    let root = crate::paths::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;

    let skip = |code: SkipCode, reason: &str| {
        Ok(UpReport::plain(Outcome::Skipped(SkipReason::new(
            code, reason,
        ))))
    };
    let mut kept = Vec::new();
    // A DerivedData folder belongs to the deepest registered tree holding
    // its project, so `up` leaves those of a tree nested in this one.
    let trees = crate::derived_data::Trees::registered(home);
    match record.level {
        Level::Cooling | Level::Cold => return skip(SkipCode::Cold, "cold"),
        Level::Opening => return skip(SkipCode::Opening, OPENING),
        Level::Baling => {}
        Level::Hot | Level::Baled => {
            // A folder recreated at the path is not this worktree: git there
            // may fall back to an enclosing repo, so remove nothing from it.
            if let Err(e) = super::disk::ensure_checkout(&root, &record.admin_dir) {
                return skip(
                    SkipCode::IdentityMismatch,
                    &format!("{} is not the worktree's checkout: {e:#}", root.display()),
                );
            }
            if let Some(reason) = git::find_worktree(&root, &root)?.foreign_lock() {
                return skip(SkipCode::WorktreeLocked, &reason);
            }
            let holders = busy_holders(&root, ignore)?;
            if !holders.is_empty() {
                return Ok(UpReport::plain(Outcome::Busy(holders)));
            }
            let (dirs, real) = bale_able(home, &root)?;
            kept = real;
            // Xcode's build logs go with its DerivedData; record them first.
            #[allow(unused_mut)]
            let mut built = crate::derived_data::all_for(&root, &dirs, &trees);
            #[cfg(target_os = "macos")]
            built.extend(crate::derived_data::learned_for(home, &root, &trees));
            crate::trace::harvest(home, id, &built);
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
    let removed = {
        let dd_root = crate::derived_data::global_root();
        let mut failures = dd_root
            .as_deref()
            .map(|dd| remove_derived_data(&root, dd, &trees, ignore))
            .unwrap_or_default();
        let (removed, learned_failures) = remove_learned(home, &root, &trees, ignore);
        failures.extend(learned_failures);
        // The tree's next build starts from a fresh folder, so link it now.
        if let Some(dd) = &dd_root {
            let made = crate::spm_link::link_tree(home, &root, dd, &[]);
            crate::spm_link::merge_links(&mut record, made);
        }
        if !failures.is_empty() {
            record.note = Some(format!("{DD_NOTE}{}", failures.join("; ")));
        } else if record
            .note
            .as_deref()
            .is_some_and(|n| n.starts_with(DD_NOTE))
        {
            record.note = None;
        }
        removed
    };

    // The stubs went with the dirs; a later unstash must not refill them.
    // The blobs are left to GC.
    record.stash = None;
    record.level = Level::Baled;
    registry.put(&record)?;
    // As for the shared root above: the next build in a removed
    // `-derivedDataPath` root starts from the shared packages. This reads
    // the links from the record just written.
    #[cfg(target_os = "macos")]
    for dir in &removed {
        crate::spm_link::link_derived_data_path(home, id, dir);
    }
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
///
/// On macOS an Xcode DerivedData root at most two levels below `root` (such
/// as `.dd` or `build/dd`, built with `-derivedDataPath`) is build output
/// even when git does not ignore it: only a tracked file or a nested repo
/// keeps it.
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
    let (tracked_only, asked): (Vec<usize>, Vec<usize>) = (0..rels.len())
        .filter(|&i| !real[i])
        .partition(|&i| cfg!(target_os = "macos") && shallow_derived_data(&dirs[i], &rels[i]));
    for (set, untracked_too) in [(asked, true), (tracked_only, false)] {
        for chunk in set.chunks(100) {
            let mut args: Vec<&str> = vec!["--no-optional-locks", "ls-files", "-z", "--cached"];
            if untracked_too {
                args.extend(["--others", "--exclude-standard"]);
            }
            args.push("--");
            let specs: Vec<String> = chunk
                .iter()
                .map(|&i| format!(":(literal){}", slashed(&rels[i])))
                .collect();
            args.extend(specs.iter().map(String::as_str));
            let out = git::Git::at(root).run(&args)?;
            for listed in out.split('\0').filter(|p| !p.is_empty()) {
                let listed = Path::new(listed);
                // Candidates never nest, so a path lies under at most one.
                if let Some(&i) = chunk.iter().find(|&&i| listed.starts_with(&rels[i])) {
                    real[i] = true;
                }
            }
        }
    }
    let (kept, trash): (Vec<_>, Vec<_>) = dirs.into_iter().zip(real).partition(|(_, r)| *r);
    Ok((
        trash.into_iter().map(|(d, _)| d).collect(),
        kept.into_iter().map(|(d, _)| d).collect(),
    ))
}

/// Whether `dir`, at `rel` below the tree, is a real (not linked) Xcode
/// DerivedData root at most two levels down.
fn shallow_derived_data(dir: &Path, rel: &Path) -> bool {
    rel.components().count() <= 2
        && fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && crate::derived_data::is_derived_data_root(dir)
}

/// Whether a dir between `root` (exclusive) and `root/rel` (inclusive) has a
/// `.git` entry of any kind.
pub(crate) fn in_nested_repo(root: &Path, rel: &Path) -> bool {
    let mut dir = root.to_path_buf();
    rel.components().any(|c| {
        dir.push(c);
        fs::symlink_metadata(dir.join(".git")).is_ok()
    })
}

/// `rel` with `/` between components, as git pathspecs want.
pub(crate) fn slashed(rel: &Path) -> String {
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

/// Removes `dir` if it is a folder bale pre-made: a real directory holding
/// exactly one entry, `SourcePackages`, which is a symlink. Returns whether it
/// did. Anything else, such as a folder Xcode has started to fill, is left
/// alone. The link is removed with `remove_file` and the folder with
/// `remove_dir`, so a file Xcode adds in between makes the removal fail
/// instead of deleting it, and the shared folder is never followed.
pub(crate) fn remove_stub(dir: &Path) -> io::Result<bool> {
    if !is_stub(dir) {
        return Ok(false);
    }
    let link = dir.join("SourcePackages");
    fs::remove_file(link)?;
    fs::remove_dir(dir)?;
    Ok(true)
}

/// Whether `dir` has the shape of a folder bale pre-made; see [`remove_stub`].
/// A missing or unreadable `dir` is not one.
pub(crate) fn is_stub(dir: &Path) -> bool {
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir()) {
        return false;
    }
    let Ok(mut entries) = fs::read_dir(dir) else {
        return false;
    };
    let Some(Ok(only)) = entries.next() else {
        return false;
    };
    entries.next().is_none()
        && only.file_name() == "SourcePackages"
        && fs::symlink_metadata(only.path()).is_ok_and(|m| m.file_type().is_symlink())
}

/// `Ok` when no process (outside `ignore`) holds `dir`; otherwise the
/// message `up` records for it: who holds it, or why that is unknown.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn idle(dir: &Path, ignore: &[u32]) -> Result<(), String> {
    match busy_holders(dir, ignore) {
        Ok(h) if h.is_empty() => Ok(()),
        Ok(h) => {
            let who: Vec<String> = h
                .iter()
                .map(|h| format!("pid {} {}", h.pid, h.command))
                .collect();
            Err(format!("{}: in use by {}", dir.display(), who.join(", ")))
        }
        Err(e) => Err(format!("{}: {e:#}", dir.display())),
    }
}

/// Removes the learned `-derivedDataPath` roots built from the tree at
/// `root` (see [`crate::derived_data::learned_for`]), returning those removed
/// and a message per root that a process holds or that could not be probed
/// or removed. Their `dd-roots` lines stay; reap drops dead ones.
#[cfg(target_os = "macos")]
fn remove_learned(
    home: &BaleHome,
    root: &Path,
    trees: &crate::derived_data::Trees,
    ignore: &[u32],
) -> (Vec<PathBuf>, Vec<String>) {
    let mut removed = Vec::new();
    let mut failures = Vec::new();
    for dir in crate::derived_data::learned_for(home, root, trees) {
        if let Err(why) = idle(&dir, ignore) {
            failures.push(why);
            continue;
        }
        match remove_entry(&dir) {
            Ok(()) => removed.push(dir),
            Err(e) => failures.push(format!("{}: {e}", dir.display())),
        }
    }
    (removed, failures)
}

/// Removes DerivedData folders under `dd_root` built from this worktree and
/// returns a message per folder that could not be probed or removed, or that
/// a process holds (such a folder is left alone). Missing `dd_root` is not a
/// failure.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn remove_derived_data(
    root: &Path,
    dd_root: &Path,
    trees: &crate::derived_data::Trees,
    ignore: &[u32],
) -> Vec<String> {
    let mut failures = Vec::new();
    if !dd_root.is_dir() {
        return failures;
    }
    for dir in crate::derived_data::owned_under(root, dd_root, trees) {
        if let Err(why) = idle(&dir, ignore) {
            failures.push(why);
            continue;
        }
        if let Err(e) = remove_entry(&dir) {
            failures.push(format!("{}: {e}", dir.display()));
        }
    }
    // A folder bale pre-made for a project that never built has no
    // `info.plist`, so the match above can't see it; find it by name. This
    // pass deliberately runs even when `share_swift_packages` is false: stubs
    // made while it was on still need cleaning up.
    for r in crate::spm::resolved_files(root) {
        let dir = dd_root.join(crate::derived_data::folder_name(&r.project));
        // Most projects have no pre-made folder; don't probe those.
        if !is_stub(&dir) {
            continue;
        }
        if let Err(why) = idle(&dir, ignore) {
            failures.push(why);
            continue;
        }
        if let Err(e) = remove_stub(&dir) {
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
        assert!(remove_derived_data(
            tmp.path(),
            &tmp.path().join("nope"),
            &Default::default(),
            &[],
        )
        .is_empty());
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

        let failures = remove_derived_data(&ws, &tmp.path().join("dd"), &Default::default(), &[]);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("App-abc"), "{failures:?}");
    }
}
