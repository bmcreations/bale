//! `bale cold`: save a worktree's uncommitted work and ignored files, verify
//! the saved state, then lock git's entry and delete the checkout. The lock
//! keeps `git worktree prune` from erasing the entry while the dir is gone.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
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
use crate::ops::disk::{
    self, read_manifest, record_tree, restore_check, write_manifest, Manifest, Skip, Verdict,
};
use crate::ops::up::{
    bale_able_from, is_busy_error, remove_entry, up_locked, Outcome, SkipCode, SkipReason, OPENING,
    TRASH_PREFIX,
};
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
    cold_ignoring(home, id, &[])
}

/// `cold`, where processes in `ignore_pids` do not count as holding the
/// tree. A hook passes the processes that invoked it, which hold the tree
/// only because they are waiting on the hook. Every busy check honors it.
pub fn cold_ignoring(home: &BaleHome, id: &str, ignore_pids: &[u32]) -> Result<Outcome> {
    cold_with(home, id, false, ignore_pids)
}

/// `cold` for unattended callers: once the workspace lock is held, a tree
/// with uncommitted changes, or whose state git cannot report, is left
/// untouched as `Skipped("dirty")`. A `Cooling` record resumes regardless,
/// since its work is already being saved.
pub fn cold_if_clean(home: &BaleHome, id: &str) -> Result<Outcome> {
    cold_with(home, id, true, &[])
}

fn cold_with(home: &BaleHome, id: &str, require_clean: bool, ignore: &[u32]) -> Result<Outcome> {
    crate::trace::op(home, "cold", Some(id), || {
        cold_untraced(home, id, require_clean, ignore)
    })
}

fn cold_untraced(
    home: &BaleHome,
    id: &str,
    require_clean: bool,
    ignore: &[u32],
) -> Result<Outcome> {
    let Some(_lock) = WorkspaceLock::try_acquire(home, id)? else {
        return Ok(skip(SkipCode::Locked, "locked"));
    };
    let registry = Registry::open(home.clone());
    let Some(record) = registry.get(id)? else {
        bail!("no record for {id}; register the worktree first with `bale up <path>`");
    };
    match record.level {
        Level::Cold => return Ok(skip(SkipCode::Cold, "cold")),
        Level::Opening => return Ok(skip(SkipCode::Opening, OPENING)),
        Level::Cooling => return recover(home, &registry, record, ignore),
        Level::Hot | Level::Baling | Level::Baled => {}
    }

    let root = crate::paths::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    if let Some(reason) = not_checkout(&root, &record) {
        return Ok(Outcome::Skipped(reason));
    }
    if require_clean && git::is_dirty(&root).unwrap_or(true) {
        return Ok(skip(SkipCode::Dirty, "dirty"));
    }
    if let Some(reason) = refusal(&root, &exclusions(home, &root)?)? {
        return Ok(Outcome::Skipped(reason));
    }
    match up_locked(home, id, ignore)? {
        Outcome::Done(Level::Baled) => {}
        other => return Ok(other),
    }

    // `up` rewrote the record; build on its copy.
    let mut record = registry
        .get(id)?
        .with_context(|| format!("record for {id} vanished"))?;
    // The branch may have changed since registration; open restores this one.
    record.branch = git::current_branch(&root)?;
    // Lets a cold tree still describe itself; best effort, never a refusal.
    if let Ok(scope) = git::scope(&root) {
        record.scope = Some(scope);
    }
    git::lock_worktree(&root, &root)?;
    record.level = Level::Cooling;
    registry.put(&record)?;
    finish(home, &registry, record, &root, ignore)
}

/// What cold leaves out of the ignored-file save for a tree. Capture,
/// verify and recovery all build it with [`exclusions`], so they can't
/// disagree about which files belong in the save.
#[derive(Debug, Clone)]
pub struct Exclusions {
    /// Config `ignored_denylist_extra`: an entry without `/` matches any
    /// path component, one with `/` matches a path prefix.
    pub deny: Vec<String>,
    /// The regenerable dirs `up` would remove, relative to the tree root. A
    /// regenerable-named dir that `up` keeps is not here, so its ignored
    /// files are saved.
    pub regen: Vec<PathBuf>,
    /// Config `regenerable_extra` names, for submodules, which are checked
    /// by name (see `refusal`).
    pub regen_extra: Vec<String>,
}

/// [`Exclusions`] for the tree at `dir`, from the config and what
/// `bale_able` would remove there now.
pub fn exclusions(home: &BaleHome, dir: &Path) -> Result<Exclusions> {
    let cfg = load_config(home, Some(dir))?;
    let (removable, _) = bale_able_from(dir, regenerable_dirs(dir, &detect(dir), &cfg))?;
    let regen = removable
        .iter()
        .filter_map(|p| p.strip_prefix(dir).ok().map(Path::to_path_buf))
        .collect();
    Ok(Exclusions {
        deny: cfg.ignored_denylist_extra,
        regen,
        regen_extra: cfg.regenerable_extra,
    })
}

/// Why this tree must not go cold, if it must not. The main checkout is an
/// error; everything else is a skip reason.
fn refusal(root: &Path, ex: &Exclusions) -> Result<Option<SkipReason>> {
    let info = git::worktrees(root)?
        .into_iter()
        .find(|w| crate::paths::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone()) == root)
        .with_context(|| format!("{} is not a git worktree", root.display()))?;
    if info.is_main {
        bail!(
            "{} is the main checkout; bale only colds linked worktrees",
            root.display()
        );
    }
    if let Some(reason) = info.foreign_lock() {
        return Ok(Some(SkipReason::new(SkipCode::WorktreeLocked, reason)));
    }
    if let Some(reason) = case_variant_git(root, ex) {
        return Ok(Some(SkipReason::new(SkipCode::NotRestorable, reason)));
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
        return Ok(Some(SkipReason::new(
            SkipCode::SparseCheckout,
            "sparse checkout",
        )));
    }
    let hidden = hidden_entries(&g)?;
    if !hidden.is_empty() {
        return Ok(Some(SkipReason::new(
            SkipCode::HiddenEntries,
            hidden_reason(&hidden),
        )));
    }

    let dirty = g.run(&[
        "submodule",
        "--quiet",
        "foreach",
        "--recursive",
        // The runner's -c overrides reach this status; the flag outranks a
        // nested `.gitmodules` ignore, which -c does not.
        r#"test -z "$(git status --porcelain --ignore-submodules=none)" || echo "$displaypath""#,
    ])?;
    let dirty: Vec<&str> = dirty.lines().filter(|l| !l.is_empty()).collect();
    if !dirty.is_empty() {
        return Ok(Some(SkipReason::new(
            SkipCode::Submodule,
            format!("dirty submodule: {}", dirty.join(", ")),
        )));
    }

    let moved = moved_gitlinks(&g.run(&[
        "diff-index",
        "--raw",
        "-z",
        "--no-renames",
        "--ignore-submodules=dirty",
        "HEAD",
    ])?);
    if !moved.is_empty() {
        return Ok(Some(SkipReason::new(
            SkipCode::Submodule,
            moved_gitlink_reason(&moved),
        )));
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
    // A gitlink whose dir has no `.git` is not checked out, so `foreach`
    // skips it and nothing would save what is in it.
    let mut gitlink_paths = gitlinks(&g)?;
    for sub in &subs {
        if submodule_git_dir(&root.join(sub))?.is_some() {
            let inner = gitlinks(&Git::at(root.join(sub)))?;
            gitlink_paths.extend(inner.into_iter().map(|p| format!("{sub}/{p}")));
        }
    }
    for path in gitlink_paths {
        let dir = root.join(&path);
        if submodule_git_dir(&dir)?.is_none() && !is_empty_or_missing(&dir)? {
            return Ok(Some(SkipReason::new(
                SkipCode::Submodule,
                format!("submodule {path} is not checked out but has files"),
            )));
        }
    }
    // Inside a submodule regenerable output is judged by name: none of it
    // is saved, and up leaves it in place (root's git can't see into it).
    let sub_deny: Vec<String> = ex.deny.iter().chain(&ex.regen_extra).cloned().collect();
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
        let hidden: Vec<String> = hidden_entries(&Git::at(&dir))?
            .into_iter()
            .map(|p| format!("{sub}/{p}"))
            .collect();
        if !hidden.is_empty() {
            return Ok(Some(SkipReason::new(
                SkipCode::HiddenEntries,
                hidden_reason(&hidden),
            )));
        }
        let mut regen = regen_dirs(&dir);
        regen.extend(root_regen.iter().filter(|r| r.starts_with(&dir)).cloned());
        let scan = scan_ignored_with(
            &Git::at(&dir),
            &dir,
            &submodule_deny(&sub_deny, sub),
            &regen,
        )?;
        if !scan.entries.is_empty() || !scan.nested.is_empty() {
            ignored.push(sub);
        }
    }
    if !embedded.is_empty() {
        return Ok(Some(SkipReason::new(
            SkipCode::Submodule,
            format!("submodule with its own .git dir: {}", embedded.join(", ")),
        )));
    }
    if !ignored.is_empty() {
        return Ok(Some(SkipReason::new(
            SkipCode::Submodule,
            format!("submodule with ignored files: {}", ignored.join(", ")),
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
                    return Ok(Some(SkipReason::new(
                        SkipCode::Unreadable,
                        format!("unreadable: {}", rel.display()),
                    )));
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
    nested.extend(scan_ignored(root, ex)?.nested);
    if !nested.is_empty() {
        return Ok(Some(SkipReason::new(
            SkipCode::NestedRepo,
            format!("nested repository: {}", join_paths(&nested)),
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
    ignore: &[u32],
) -> Result<Outcome> {
    let id = record.id.clone();
    let dest = home.saved_dir(&id);
    let prepared = (|| -> Result<(Vec<Holder>, Exclusions)> {
        save_wip(root, &id)?;
        let ex = exclusions(home, root)?;
        save_ignored(root, &dest, &ex)?;
        verify(root, &id, &dest, &ex)?;
        Ok((busy_holders(root, ignore)?, ex))
    })();
    let (holders, ex) = match prepared {
        Ok(h) => h,
        Err(e) => {
            back_to_baled(registry, &mut record, root)?;
            return Err(e);
        }
    };
    if !holders.is_empty() {
        back_to_baled(registry, &mut record, root)?;
        return Ok(Outcome::Busy(holders));
    }
    match no_room_to_check(root, &ex) {
        Ok(None) => {}
        Ok(Some(reason)) => {
            back_to_baled(registry, &mut record, root)?;
            return Ok(Outcome::Skipped(reason));
        }
        Err(e) => {
            back_to_baled(registry, &mut record, root)?;
            return Err(e);
        }
    }

    let trash = trash_path(root)?;
    #[cfg(test)]
    tests::run_seam(&tests::BEFORE_RENAME, root);
    if let Err(e) = fs::rename(root, &trash) {
        back_to_baled(registry, &mut record, root)?;
        if is_busy_error(&e) {
            return Ok(Outcome::Busy(Vec::new()));
        }
        return Err(e).with_context(|| format!("moving {} aside", root.display()));
    }
    #[cfg(test)]
    tests::run_seam(&tests::AFTER_RENAME, &trash);
    // A write between the checks above and the rename (lsof saw no holder,
    // but a process can open the tree after it ran) would be lost. Check the
    // moved tree again, and who holds it, and move it back unless both pass.
    // Until the marker is written, a crash leaves an unmarked trash, which
    // `recover` checks again before deleting it.
    let checked = (|| -> Result<Option<Outcome>> {
        // Git fails on such a path on some platforms and skips it on others.
        if let Some(reason) = case_variant_git(&trash, &ex) {
            return Ok(Some(Outcome::Skipped(not_restorable(&SkipReason::new(
                SkipCode::NotRestorable,
                reason,
            )))));
        }
        let wip = wip_commit(&trash, &id)?;
        // The same exclusions as the save: the trash is checked against
        // exactly what was captured.
        if verify_in(&trash, &wip, &dest, &ex, Place::Moved)?.is_some() {
            return Ok(Some(Outcome::Busy(Vec::new())));
        }
        let snapshot = match restore_check(&id, &trash, &wip, &dest, &ex)? {
            Verdict::Same(snapshot) => snapshot,
            Verdict::Differs(reason) => return Ok(Some(Outcome::Skipped(not_restorable(&reason)))),
        };
        #[cfg(test)]
        tests::run_seam(&tests::AFTER_COMPARE, &trash);
        let holders = busy_holders(&trash, ignore)?;
        if !holders.is_empty() {
            return Ok(Some(Outcome::Busy(holders)));
        }
        // A write after the compare read a file would not show in it.
        if snapshot.changed()?.is_some() {
            return Ok(Some(Outcome::Busy(Vec::new())));
        }
        record_check(&dest, *snapshot)?;
        write_trash_marker(&dest, &wip, &trash)?;
        Ok(None)
    })();
    match checked {
        Ok(None) => {}
        Ok(Some(busy)) => {
            move_back(registry, &mut record, &trash, root)?;
            return Ok(busy);
        }
        Err(e) => {
            move_back(registry, &mut record, &trash, root)?;
            return Err(e).with_context(|| format!("checking {} after moving it", trash.display()));
        }
    }
    // A failed removal leaves the record `Cooling`; the next run finishes it.
    remove_trash(&trash)?;
    // The build dirs are gone; the blobs are left to GC.
    record.stash = None;
    record.level = Level::Cold;
    registry.put(&record)?;
    Ok(Outcome::Done(Level::Cold))
}

/// The skip reason for a folder at the record's path that is not the
/// worktree's own checkout. Nothing may be saved from it, or run there: git
/// in a folder with no `.git` works on whatever repo encloses it.
fn not_checkout(root: &Path, record: &Record) -> Option<SkipReason> {
    disk::ensure_checkout(root, &record.admin_dir)
        .err()
        .map(|e| {
            SkipReason::new(SkipCode::IdentityMismatch, format!(
                "{} is not this worktree's checkout ({e:#}); leaving it and the saved state as they are",
                record.path.display()
            ))
        })
}

/// Room the restore check needs beyond its copy of the tree.
const CHECK_MARGIN: u64 = 1 << 30;

/// The skip reason when the volume holding `tree` lacks room for the
/// restore check: its copy of the tree (estimated as the bytes of every
/// file the save does not leave out) plus [`CHECK_MARGIN`].
fn no_room_to_check(tree: &Path, ex: &Exclusions) -> Result<Option<SkipReason>> {
    let mut size = 0u64;
    let mut it = WalkDir::new(tree)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter();
    while let Some(item) = it.next() {
        // Unreadable entries are reported by the checks that read them.
        let Ok(item) = item else { continue };
        let Ok(rel) = item.path().strip_prefix(tree) else {
            continue;
        };
        let skip = (item.depth() == 1 && item.file_name() == ".git")
            || (item.depth() > 0 && disk::denied(rel, &ex.regen, &ex.deny));
        if skip {
            if item.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        if item.file_type().is_file() {
            size += item.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    let needed = size.saturating_add(CHECK_MARGIN);
    let free = disk::free_space(tree)?;
    if free >= needed {
        return Ok(None);
    }
    Ok(Some(SkipReason::new(
        SkipCode::FreeSpace,
        format!(
            "not enough free space to check it: needs {} (a copy of the tree plus {}), {} free on {}",
            crate::human_bytes(needed),
            crate::human_bytes(CHECK_MARGIN),
            crate::human_bytes(free),
            tree.display()
        ),
    )))
}

/// The skip reason for a tree `open` would not give back as it is. Keeps a
/// more specific code (an attribute, a submodule) when the check found one.
fn not_restorable(reason: &SkipReason) -> SkipReason {
    let code = match reason.code {
        SkipCode::DirtyXattr | SkipCode::Submodule => reason.code,
        _ => SkipCode::NotRestorable,
    };
    SkipReason::new(
        code,
        format!("open would not restore it as it is: {}", reason.text),
    )
}

fn skip(code: SkipCode, text: &str) -> Outcome {
    Outcome::Skipped(SkipReason::new(code, text))
}

/// Renames the moved-aside checkout back to `root` and sets `Baled`. A failed
/// rename leaves the trash where it is and the record `Cooling`, and the
/// error names both paths.
fn move_back(registry: &Registry, record: &mut Record, trash: &Path, root: &Path) -> Result<()> {
    #[cfg(test)]
    tests::run_seam(&tests::BEFORE_MOVE_BACK, trash);
    fs::rename(trash, root)
        .with_context(|| format!("moving {} back to {}", trash.display(), root.display()))?;
    back_to_baled(registry, record, root)
}

/// Puts a record whose checkout is still at `root` back to `Baled`. The
/// checkout exists, so git needs no lock to keep its entry: bale's own lock
/// is released (a lock with any other reason stays). Releasing it is best
/// effort: a Baled tree that keeps bale's lock works as before, and the
/// caller's own error matters more.
fn back_to_baled(registry: &Registry, record: &mut Record, root: &Path) -> Result<()> {
    record.level = Level::Baled;
    registry.put(record)?;
    let _ = git::unlock_worktree_if_ours(root, root);
    Ok(())
}

/// Resumes a `Cooling` record. A present checkout is saved again from
/// scratch; a missing one is only finished off (trash removed, `Cold`) when
/// the wip ref and the manifest both exist. An unmarked trash that differs
/// from the save, or can't be checked, is moved back and set `Baled`.
fn recover(
    home: &BaleHome,
    registry: &Registry,
    mut record: Record,
    ignore: &[u32],
) -> Result<Outcome> {
    let path = record.path.clone();
    // A check the crash interrupted left its copy of the tree.
    disk::remove_stale_checks(&path, &record.id)?;
    if fs::symlink_metadata(&path).is_ok() {
        // The checkout was moved aside, so whatever is at the path now was
        // put there since; saving it would replace the save of the trash.
        let trash = trash_siblings(&path)?;
        if !trash.is_empty() {
            let names: Vec<String> = trash.iter().map(|t| t.display().to_string()).collect();
            return Ok(Outcome::Skipped(SkipReason::new(
                SkipCode::IdentityMismatch,
                format!(
                    "{} was recreated after the checkout was moved to {}; move {} away and run \
                     `bale cold` again to finish",
                    path.display(),
                    names.join(", "),
                    path.display()
                ),
            )));
        }
        let root = crate::paths::canonicalize(&path)
            .with_context(|| format!("resolving {}", path.display()))?;
        if let Some(reason) = not_checkout(&root, &record) {
            return Ok(Outcome::Skipped(reason));
        }
        if let Some(reason) = refusal(&root, &exclusions(home, &root)?)? {
            back_to_baled(registry, &mut record, &root)?;
            return Ok(Outcome::Skipped(reason));
        }
        record.branch = git::current_branch(&root)?;
        git::lock_worktree(&root, &root)?;
        return finish(home, registry, record, &root, ignore);
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
            // is not stale) before deleting it. If it holds work the save
            // does not, or can't be checked, it goes back to the recorded
            // path as a `Baled` tree; nothing is deleted.
            let checked = exclusions(home, &trash).and_then(|ex| {
                if let Some(reason) = case_variant_git(&trash, &ex) {
                    return Ok(Some(Outcome::Skipped(not_restorable(&SkipReason::new(
                        SkipCode::NotRestorable,
                        reason,
                    )))));
                }
                if let Some(reason) = no_room_to_check(&trash, &ex)? {
                    return Ok(Some(Outcome::Skipped(reason)));
                }
                let wip = wip_sha.trim();
                if verify_in(&trash, wip, &dest, &ex, Place::Moved)?.is_some() {
                    return Ok(Some(Outcome::Busy(Vec::new())));
                }
                let snapshot = match restore_check(&record.id, &trash, wip, &dest, &ex)? {
                    Verdict::Same(snapshot) => snapshot,
                    Verdict::Differs(reason) => {
                        return Ok(Some(Outcome::Skipped(not_restorable(&reason))))
                    }
                };
                // As `finish`: a process that has the tree open may still
                // write to it.
                let holders = busy_holders(&trash, ignore)?;
                if !holders.is_empty() {
                    return Ok(Some(Outcome::Busy(holders)));
                }
                if snapshot.changed()?.is_some() {
                    return Ok(Some(Outcome::Busy(Vec::new())));
                }
                record_check(&dest, *snapshot)?;
                // As `finish`: once proven, mark it, so a removal that
                // stops partway is finished on the next run, never moved
                // back half-removed.
                write_trash_marker(&dest, wip, &trash)?;
                Ok(None)
            });
            match checked {
                Ok(None) => {}
                Ok(Some(outcome)) => {
                    move_back(registry, &mut record, &trash, &path)?;
                    return Ok(outcome);
                }
                Err(e) => {
                    let e = e.context(format!("could not check {}", trash.display()));
                    return match move_back(registry, &mut record, &trash, &path) {
                        Ok(()) => Err(e),
                        Err(back) => Err(e.context(format!("{back:#}; keeping it"))),
                    };
                }
            }
        }
        remove_trash(&trash)?;
    }
    // The build dirs are gone; the blobs are left to GC.
    record.stash = None;
    record.level = Level::Cold;
    registry.put(&record)?;
    Ok(Outcome::Done(Level::Cold))
}

/// Removes the moved-aside checkout. `remove_dir_all` doesn't say which
/// entry it failed on, so after a failure the rest is removed one entry at
/// a time and the error names the first that can't be. The record stays
/// `Cooling`, so the next `bale cold` finishes the removal.
fn remove_trash(trash: &Path) -> Result<()> {
    if remove_entry(trash).is_ok() {
        return Ok(());
    }
    let hint = |path: &Path| {
        format!(
            "removing {}: clear what blocks it (a dir without write permission, an immutable \
             flag, an ACL that denies delete) and run `bale cold` again to finish",
            path.display()
        )
    };
    let walk = WalkDir::new(trash)
        .contents_first(true)
        .follow_links(false)
        .follow_root_links(false);
    for item in walk {
        let item = match item {
            Ok(item) => item,
            Err(e) => {
                let path = e.path().unwrap_or(trash).to_path_buf();
                return Err(anyhow::Error::new(e)).context(hint(&path));
            }
        };
        let path = item.path();
        let removed = if item.file_type().is_dir() {
            fs::remove_dir(path)
        } else {
            remove_entry(path)
        };
        removed.with_context(|| hint(path))?;
    }
    Ok(())
}

/// Adds the check's content hashes and symlink targets to `dest`'s
/// manifest, for open to check what it writes against.
fn record_check(dest: &Path, snapshot: disk::Snapshot) -> Result<()> {
    let mut manifest = read_manifest(dest)?;
    manifest.hashes = snapshot.hashes;
    manifest.links = snapshot.links;
    write_manifest(dest, &manifest)
}

/// Writes `dest/trash` (tmp + rename, 0600 on unix) naming `trash` and the
/// wip commit it was verified against.
fn write_trash_marker(dest: &Path, wip: &str, trash: &Path) -> Result<()> {
    let name = trash
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} is not valid UTF-8", trash.display()))?;
    let marker = TrashMarker {
        name: name.to_string(),
        wip: wip.to_string(),
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
    let parent = crate::paths::canonicalize(parent)
        .with_context(|| format!("resolving {}", parent.display()))?;
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
    snapshot_tree_in(dir, Place::InPlace)
}

/// Where the checkout `snapshot_tree_in` reads sits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// At the path git records for it.
    InPlace,
    /// Moved aside (a `.bale-trash-*` dir). A submodule's git dir names its
    /// checkout by a path relative to the old location, so git run inside
    /// the submodule without an explicit git dir and work tree fails (see
    /// `submodule_change`); `git add -A` runs one to probe a submodule for
    /// changes. Gitlinks are left out of `add -A` and re-read with
    /// `update-index --remove`, which takes the submodule's HEAD without
    /// entering it (and drops the entry when the dir is gone).
    Moved,
}

fn snapshot_tree_in(dir: &Path, place: Place) -> Result<String> {
    let tmp = tempfile::TempDir::new().context("creating a temporary index dir")?;
    let index = tmp.path().join("index");
    let real = Git::at(dir).run(&["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let real = PathBuf::from(real.trim_end_matches(['\n', '\r']));
    let g = Git::at(dir).env("GIT_INDEX_FILE", index.as_os_str());
    if real.is_file() {
        // Git re-reads an entry whose stat time is not older than the index
        // file's mtime ("racily clean"), so the copy keeps the real index's
        // mtime: a fresh one (as `fs::copy` gives on Linux) hides an edit
        // that kept the file's size within the same timestamp tick. Read
        // first, so an index rewritten during the copy only looks older.
        let mtime = fs::metadata(&real)
            .and_then(|m| m.modified())
            .with_context(|| format!("reading the mtime of {}", real.display()))?;
        fs::copy(&real, &index).with_context(|| format!("copying {}", real.display()))?;
        fs::OpenOptions::new()
            .write(true)
            .open(&index)
            .and_then(|f| f.set_modified(mtime))
            .with_context(|| format!("setting the mtime of {}", index.display()))?;
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
    if place == Place::InPlace {
        g.run(&["add", "-A"])?;
        return Ok(g.run(&["write-tree"])?.trim().to_string());
    }
    let gitlinks = gitlinks(&g)?;
    let excludes: Vec<String> = gitlinks
        .iter()
        .map(|p| format!(":(exclude,literal){p}"))
        .collect();
    let mut args = vec!["add", "-A", "--", "."];
    args.extend(excludes.iter().map(String::as_str));
    g.run(&args)?;
    for chunk in gitlinks.chunks(200) {
        // `--remove` drops the gitlink of a deleted submodule dir, as
        // `add -A` does in place.
        let mut args = vec!["update-index", "--remove", "--"];
        args.extend(chunk.iter().map(String::as_str));
        g.run(&args)?;
    }
    Ok(g.run(&["write-tree"])?.trim().to_string())
}

/// What changed inside a submodule of the checkout at `root` (nested ones
/// included), if anything did. The wip commit holds only each submodule's
/// HEAD, so an edit, an untracked file, or an ignored file outside the
/// regenerable output (judged as `refusal` judges it) would be deleted
/// unsaved. Each submodule's git dir is named explicitly, so this works on
/// a moved-aside tree too, where git can't enter a submodule on its own.
fn submodule_change(root: &Path, ex: &Exclusions) -> Result<Option<String>> {
    let sub_deny: Vec<String> = ex.deny.iter().chain(&ex.regen_extra).cloned().collect();
    let mut pending: Vec<String> = gitlinks(&Git::at(root))?;
    let root_regen = if pending.is_empty() {
        Vec::new()
    } else {
        regen_dirs(root)
    };
    while let Some(sub) = pending.pop() {
        let dir = root.join(&sub);
        let Some(git_dir) = submodule_git_dir(&dir)? else {
            // Not checked out: an empty or missing dir holds nothing.
            if is_empty_or_missing(&dir)? {
                continue;
            }
            return Ok(Some(format!("submodule {sub} has files but no .git")));
        };
        let g = Git::at(&dir)
            .env("GIT_DIR", git_dir.as_os_str())
            .env("GIT_WORK_TREE", dir.as_os_str());
        // `dirty` still reports a nested submodule's new commits; its
        // contents are checked when it is popped.
        let status = g.run(&[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "-z",
            "--ignore-submodules=dirty",
        ])?;
        let status: Vec<&str> = status.split('\0').filter(|e| !e.is_empty()).collect();
        if !status.is_empty() {
            return Ok(Some(format!(
                "submodule {sub} changed while saving: {}",
                status.join(", ")
            )));
        }
        let hidden = hidden_entries(&g)?;
        if !hidden.is_empty() {
            return Ok(Some(format!(
                "submodule {sub} has entries status can't see: {}",
                hidden.join(", ")
            )));
        }
        let mut regen = regen_dirs(&dir);
        regen.extend(root_regen.iter().filter(|r| r.starts_with(&dir)).cloned());
        let scan = scan_ignored_with(&g, &dir, &submodule_deny(&sub_deny, &sub), &regen)?;
        let found: Vec<PathBuf> = scan.entries.into_iter().map(|(rel, _)| rel).collect();
        if !found.is_empty() || !scan.nested.is_empty() {
            return Ok(Some(format!(
                "submodule {sub} has unsaved ignored files: {}",
                join_paths(&[found, scan.nested].concat())
            )));
        }
        pending.extend(gitlinks(&g)?.into_iter().map(|p| format!("{sub}/{p}")));
    }
    Ok(None)
}

fn is_empty_or_missing(dir: &Path) -> Result<bool> {
    match fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

/// The git dir of the submodule checkout at `dir`: its `.git` dir, or the
/// one its `.git` file names (relative to `dir`). `None` without a `.git`.
pub(crate) fn submodule_git_dir(dir: &Path) -> Result<Option<PathBuf>> {
    let dot_git = dir.join(".git");
    let meta = match fs::symlink_metadata(&dot_git) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dot_git.display())),
    };
    if meta.is_dir() {
        return Ok(Some(dot_git));
    }
    let text =
        fs::read_to_string(&dot_git).with_context(|| format!("reading {}", dot_git.display()))?;
    let Some(target) = text.trim_end_matches(['\n', '\r']).strip_prefix("gitdir: ") else {
        bail!("{} does not name a git dir", dot_git.display());
    };
    // Resolved here: on Windows `dir` is a verbatim path, where `..` is not
    // collapsed for git.
    let git_dir = dir.join(target);
    let git_dir = crate::paths::canonicalize(&git_dir)
        .with_context(|| format!("resolving {}", git_dir.display()))?;
    Ok(Some(git_dir))
}

/// Paths in raw `-z` diff output (`diff-tree`, `diff-index`) whose new
/// side is a submodule entry (mode 160000): a gitlink that moved or was
/// added. A deleted one is left out; open gives that back as it is.
pub(crate) fn moved_gitlinks(raw: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut fields = raw.split('\0');
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        if meta.split(' ').nth(1) == Some("160000") {
            found.push(path.to_string());
        }
    }
    found
}

/// The refusal for submodules whose commit is not the one HEAD records.
/// Open checks out HEAD's gitlink, so a commit made only in the submodule
/// would be left in its reflog.
pub(crate) fn moved_gitlink_reason(paths: &[String]) -> String {
    format!(
        "submodule {} is not at the commit HEAD records; commit the submodule change in the worktree first",
        paths.join(", ")
    )
}

/// Paths of the index's submodule entries (mode 160000).
fn gitlinks(g: &Git) -> Result<Vec<String>> {
    let out = g.run(&["ls-files", "-s", "-z"])?;
    Ok(out
        .split('\0')
        .filter_map(|e| {
            let (meta, path) = e.split_once('\t')?;
            meta.starts_with("160000 ").then(|| path.to_string())
        })
        .collect())
}

/// Index entries flagged assume-unchanged (lowercase tag) or skip-worktree
/// (`S`): git's status skips their edits, and `add -A` skips a
/// skip-worktree entry's.
fn hidden_entries(g: &Git) -> Result<Vec<String>> {
    Ok(ls_files_v(g)?
        .into_iter()
        .filter(|(tag, _)| tag.is_ascii_lowercase() || *tag == 'S')
        .map(|(_, p)| p)
        .collect())
}

fn hidden_reason(paths: &[String]) -> String {
    format!(
        "assume-unchanged or skip-worktree entries: {}",
        paths.join(", ")
    )
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
/// 0600 on unix) and writes `dest/manifest.json`, which also records every
/// file's mode and the empty dirs. `ex.regen`, `node_modules`
/// and `ex.deny` are skipped. Symlinks are saved as symlinks and never
/// followed. `dest` is cleared first.
pub fn save_ignored(dir: &Path, dest: &Path, ex: &Exclusions) -> Result<Vec<SavedFile>> {
    let scan = scan_ignored(dir, ex)?;
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

    // What `add -A` puts in the wip: the index plus untracked files that
    // are not ignored.
    let listed = Git::at(dir).run(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])?;
    let skip = Skip {
        regen: ex.regen.clone(),
        deny: ex.deny.clone(),
        gitlinks: gitlinks(&Git::at(dir))?
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        tracked: disk::with_ancestors(listed.split('\0')),
        subs: std::collections::BTreeMap::new(),
    };
    let tree = record_tree(dir, &skip)?;
    write_manifest(
        dest,
        &Manifest {
            files: saved.clone(),
            modes: tree.modes,
            dirs: tree.dirs,
            dir_modes: tree.dir_modes,
            root_mode: tree.root_mode,
            // Filled in by cold's check, from the moved tree.
            hashes: Vec::new(),
            links: Vec::new(),
        },
    )?;
    Ok(saved)
}

/// What `save_ignored` would save: `(rel, is_symlink)` for every ignored
/// file and symlink outside the denylist, plus every nested repository
/// (a dir holding a `.git` entry) it met on the way.
struct IgnoredScan {
    entries: Vec<(PathBuf, bool)>,
    nested: Vec<PathBuf>,
}

fn scan_ignored(dir: &Path, ex: &Exclusions) -> Result<IgnoredScan> {
    let regen: Vec<PathBuf> = ex.regen.iter().map(|r| dir.join(r)).collect();
    scan_ignored_with(&Git::at(dir), dir, &ex.deny, &regen)
}

/// `deny` as it applies inside the submodule at `sub` (relative to the
/// worktree root): bare names as they are; a slashed entry only when it lies
/// under `sub/`, rebased onto the submodule root.
pub(crate) fn submodule_deny(deny: &[String], sub: &str) -> Vec<String> {
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
pub(crate) fn regen_dirs(dir: &Path) -> Vec<PathBuf> {
    regenerable_dirs(dir, &detect(dir), &no_extras())
}

/// `scan_ignored` with the regenerable dirs (absolute) given by the caller.
/// `g` lists the ignored files of the checkout at `dir`.
fn scan_ignored_with(
    g: &Git,
    dir: &Path,
    deny: &[String],
    regen: &[PathBuf],
) -> Result<IgnoredScan> {
    let regen: Vec<PathBuf> = regen
        .iter()
        .filter_map(|p| p.strip_prefix(dir).ok().map(Path::to_path_buf))
        .collect();
    let denied = |rel: &Path| disk::denied(rel, &regen, deny);

    let listed = g.run(&[
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

/// The skip reason for entries under `dir` named like `.git` in another
/// case (`.GIT`, `.Git`, ...). Git refuses such a path on every platform,
/// so the save could not hold it and open could not put it back. Exact
/// `.git` entries are left to the submodule and nested-repo checks, and
/// regenerable dirs `up` removes are not searched.
fn case_variant_git(dir: &Path, ex: &Exclusions) -> Option<String> {
    let regen: Vec<PathBuf> = ex.regen.iter().map(|r| dir.join(r)).collect();
    let mut found = Vec::new();
    let mut it = WalkDir::new(dir)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter();
    while let Some(item) = it.next() {
        // An unreadable entry is reported by the checks that read it.
        let Ok(item) = item else { continue };
        if item.depth() == 0 {
            continue;
        }
        let name = item.file_name();
        if name == ".git" || regen.iter().any(|r| r == item.path()) {
            if item.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        if name.eq_ignore_ascii_case(".git") {
            if let Ok(rel) = item.path().strip_prefix(dir) {
                found.push(rel.to_path_buf());
            }
            if item.file_type().is_dir() {
                it.skip_current_dir();
            }
        }
    }
    if found.is_empty() {
        return None;
    }
    Some(format!(
        "git cannot store a path named like .git in another case: {}",
        join_paths(&found)
    ))
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
        idle_stash: Duration::ZERO,
        reap_grace: Duration::from_secs(3600),
        stash_min_bytes: 0,
        patch_max_depth: 0,
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
fn verify(root: &Path, id: &str, dest: &Path, ex: &Exclusions) -> Result<()> {
    let wip = wip_commit(root, id)?;
    match verify_in(root, &wip, dest, ex, Place::InPlace)? {
        None => Ok(()),
        Some(change) => bail!("{change}"),
    }
}

/// The commit `refs/bale/<id>/wip` names.
fn wip_commit(dir: &Path, id: &str) -> Result<String> {
    let wip = Git::at(dir).run(&[
        "rev-parse",
        "--verify",
        &format!("refs/bale/{id}/wip^{{commit}}"),
    ])?;
    Ok(wip.trim().to_string())
}

/// `verify` against the wip commit `wip`. `Ok(Some(change))` says what no
/// longer matches the saved state; `Err` means the check itself failed.
fn verify_in(
    root: &Path,
    wip: &str,
    dest: &Path,
    ex: &Exclusions,
    place: Place,
) -> Result<Option<String>> {
    let saved = read_manifest(dest)?.files;
    let mut want: Vec<PathBuf> = scan_ignored(root, ex)?
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
        return Ok(Some(format!(
            "ignored files changed while saving: unsaved [{}], gone [{}]",
            join_paths(&missing),
            join_paths(&extra)
        )));
    }
    for f in &saved {
        let src = root.join(&f.rel);
        let copy = dest.join(FILES).join(&f.rel);
        match &f.symlink_target {
            Some(target) => {
                for p in [&src, &copy] {
                    let is_link = fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
                    if !is_link || fs::read_link(p).ok().as_ref() != Some(target) {
                        return Ok(Some(format!(
                            "saved symlink {} does not match {}",
                            f.rel.display(),
                            p.display()
                        )));
                    }
                }
            }
            None => {
                for p in [&src, &copy] {
                    let is_file = fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file());
                    if !is_file || sha_file(p)? != f.sha256 {
                        return Ok(Some(format!(
                            "saved file {} does not match {}",
                            f.rel.display(),
                            p.display()
                        )));
                    }
                }
            }
        }
    }

    if let Some(change) = submodule_change(root, ex)? {
        return Ok(Some(change));
    }
    let rebuilt = snapshot_tree_in(root, place)?;
    let tree = Git::at(root).run(&["rev-parse", &format!("{wip}^{{tree}}")])?;
    if rebuilt != tree.trim() {
        return Ok(Some(format!(
            "working tree changed while saving: tree {rebuilt} is not wip {wip}'s {}",
            tree.trim()
        )));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::process::Command;
    use std::rc::Rc;
    use std::thread::LocalKey;

    type Seam = RefCell<Option<Box<dyn Fn(&Path)>>>;

    thread_local! {
        /// Test-only seam: runs in `finish` with the checkout's path after
        /// the tree is verified and before it is moved aside.
        pub(super) static BEFORE_RENAME: Seam = const { RefCell::new(None) };
        /// Test-only seam: runs in `finish` with the trash path right after
        /// the move, before the moved tree is checked.
        pub(super) static AFTER_RENAME: Seam = const { RefCell::new(None) };
        /// Test-only seam: runs in `move_back` with the trash path before
        /// it is renamed back.
        pub(super) static BEFORE_MOVE_BACK: Seam = const { RefCell::new(None) };
        /// Test-only seam: runs in `finish` with the trash path after the
        /// disk compare passed, before the holder check and the marker.
        pub(super) static AFTER_COMPARE: Seam = const { RefCell::new(None) };
    }

    pub(super) fn run_seam(seam: &'static LocalKey<Seam>, path: &Path) {
        seam.with(|h| {
            if let Some(f) = h.borrow().as_ref() {
                f(path)
            }
        });
    }

    /// Sets a seam until the guard drops.
    struct SeamGuard(&'static LocalKey<Seam>);

    impl Drop for SeamGuard {
        fn drop(&mut self) {
            self.0.with(|h| *h.borrow_mut() = None);
        }
    }

    fn set_seam(seam: &'static LocalKey<Seam>, f: impl Fn(&Path) + 'static) -> SeamGuard {
        seam.with(|h| *h.borrow_mut() = Some(Box::new(f)));
        SeamGuard(seam)
    }

    /// Fakes the free space `disk::free_space` reports until the guard drops.
    struct FreeGuard;

    impl Drop for FreeGuard {
        fn drop(&mut self) {
            disk::FAKE_FREE.with(|f| f.set(None));
        }
    }

    fn fake_free(bytes: u64) -> FreeGuard {
        disk::FAKE_FREE.with(|f| f.set(Some(bytes)));
        FreeGuard
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn none() -> Exclusions {
        Exclusions {
            deny: Vec::new(),
            regen: Vec::new(),
            regen_extra: Vec::new(),
        }
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = crate::paths::canonicalize(tmp.path()).unwrap().join("r");
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

    /// A write after `finish` verified the tree but before the rename would
    /// be lost; the check of the moved tree catches it and moves it back.
    #[test]
    fn write_before_rename_moves_the_tree_back() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let seam = set_seam(&BEFORE_RENAME, |root: &Path| {
            fs::write(root.join("late.txt"), "late\n").unwrap()
        });
        let out = cold(&home, &id);
        drop(seam);

        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert_eq!(fs::read_to_string(wt.join("late.txt")).unwrap(), "late\n");
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
        let entry = git::find_worktree(&repo, &wt).unwrap();
        assert_eq!(entry.locked, None);

        // Nothing changes on the next try, so it goes cold.
        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
        assert!(!wt.exists());
    }

    /// A write to the moved tree after the disk compare passed, before the
    /// marker, is seen by the stat re-check and moves the tree back.
    #[test]
    fn write_after_compare_moves_the_tree_back() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        fs::write(wt.join("work.txt"), "one\n").unwrap();
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let seam = set_seam(&AFTER_COMPARE, |trash: &Path| {
            let mut f = OpenOptions::new()
                .append(true)
                .open(trash.join("work.txt"))
                .unwrap();
            f.write_all(b"two\n").unwrap();
        });
        let out = cold(&home, &id);
        drop(seam);

        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert_eq!(
            fs::read_to_string(wt.join("work.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    /// A worktree of `repo` at `wt` with `repo2` added as submodule `lib`.
    fn with_submodule(tmp: &Path, repo: &Path) -> PathBuf {
        let sub = tmp.join("sub");
        fs::create_dir_all(&sub).unwrap();
        git(&sub, &["init", "-q", "-b", "main"]);
        git(&sub, &["config", "user.name", "t"]);
        git(&sub, &["config", "user.email", "t@t"]);
        // Gradle makes `build/` regenerable inside the submodule.
        fs::write(sub.join("a.txt"), "a\n").unwrap();
        fs::write(sub.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        fs::write(sub.join("settings.gradle.kts"), "").unwrap();
        git(&sub, &["add", "."]);
        git(
            &sub,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "s"],
        );
        let wt = tmp.join("wt");
        git(
            repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        git(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.to_str().unwrap(),
                "lib",
            ],
        );
        git(
            &wt,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "sub"],
        );
        wt
    }

    /// The moved tree's submodule can't run git (its checkout path is
    /// relative to the old location); the re-check still works there.
    #[test]
    fn write_before_rename_with_submodule_moves_the_tree_back() {
        let (tmp, repo) = repo();
        let wt = with_submodule(tmp.path(), &repo);
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let seam = set_seam(&BEFORE_RENAME, |root: &Path| {
            fs::write(root.join("late.txt"), "late\n").unwrap()
        });
        let out = cold(&home, &id);
        drop(seam);

        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert!(wt.join("late.txt").is_file());
        assert!(wt.join("lib/.git").exists());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
        assert!(!wt.exists());
    }

    /// Runs `cold` with `write` acting on the checkout just before the
    /// rename, for a worktree with submodule `lib`.
    fn cold_with_late_submodule_write(
        write: impl Fn(&Path) + 'static,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        BaleHome,
        String,
        Result<Outcome>,
    ) {
        let (tmp, repo) = repo();
        let wt = with_submodule(tmp.path(), &repo);
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;
        let seam = set_seam(&BEFORE_RENAME, write);
        let out = cold(&home, &id);
        drop(seam);
        (tmp, wt, home, id, out)
    }

    /// A late write inside the submodule is not in the wip commit (which
    /// holds only the submodule's HEAD); the tree must come back with it.
    fn assert_late_submodule_write_kept(rel: &str, text: &str) {
        let rel_in = rel.to_string();
        let text_in = text.to_string();
        let (_tmp, wt, home, id, out) = cold_with_late_submodule_write(move |root: &Path| {
            let path = root.join("lib").join(&rel_in);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &text_in).unwrap()
        });
        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()), "{rel}");
        assert_eq!(
            fs::read_to_string(wt.join("lib").join(rel)).unwrap(),
            text,
            "{rel}"
        );
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    #[test]
    fn tracked_edit_in_submodule_before_rename_moves_the_tree_back() {
        assert_late_submodule_write_kept("a.txt", "edited\n");
    }

    #[test]
    fn untracked_file_in_submodule_before_rename_moves_the_tree_back() {
        assert_late_submodule_write_kept("late.txt", "late\n");
    }

    #[test]
    fn ignored_file_in_submodule_before_rename_moves_the_tree_back() {
        assert_late_submodule_write_kept("late.log", "late\n");
    }

    /// A worktree with submodule `lib`, which has its own submodule `deep`.
    /// `setup_lib` runs in lib's source repo before its commit.
    fn nested_case(setup_lib: impl Fn(&Path)) -> (tempfile::TempDir, PathBuf, BaleHome, String) {
        let (tmp, repo) = repo();
        let inner = tmp.path().join("inner");
        fs::create_dir_all(&inner).unwrap();
        git(&inner, &["init", "-q", "-b", "main"]);
        git(&inner, &["config", "user.name", "t"]);
        git(&inner, &["config", "user.email", "t@t"]);
        fs::write(inner.join("i.txt"), "i\n").unwrap();
        git(&inner, &["add", "."]);
        git(
            &inner,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "i"],
        );
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        git(&sub, &["init", "-q", "-b", "main"]);
        git(&sub, &["config", "user.name", "t"]);
        git(&sub, &["config", "user.email", "t@t"]);
        git(
            &sub,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                inner.to_str().unwrap(),
                "deep",
            ],
        );
        setup_lib(&sub);
        git(
            &sub,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-am", "s"],
        );
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        git(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.to_str().unwrap(),
                "lib",
            ],
        );
        git(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "-q",
                "--init",
                "--recursive",
            ],
        );
        git(
            &wt,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "sub"],
        );
        assert!(wt.join("lib/deep/i.txt").is_file());
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;
        (tmp, wt, home, id)
    }

    /// A late write inside a submodule of the submodule is caught too.
    #[test]
    fn write_in_nested_submodule_before_rename_moves_the_tree_back() {
        let (_tmp, wt, home, id) = nested_case(|_| {});
        let seam = set_seam(&BEFORE_RENAME, |root: &Path| {
            fs::write(root.join("lib/deep/i.txt"), "edited\n").unwrap()
        });
        let out = cold(&home, &id);
        drop(seam);

        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert_eq!(
            fs::read_to_string(wt.join("lib/deep/i.txt")).unwrap(),
            "edited\n"
        );
        assert!(trash_siblings(&wt).unwrap().is_empty());
        // Unchanged since, it goes cold.
        fs::write(wt.join("lib/deep/i.txt"), "i\n").unwrap();
        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
    }

    /// lib's own `.gitmodules` says `ignore = all` for `deep`.
    fn ignore_deep(lib: &Path) {
        git(
            lib,
            &[
                "config",
                "-f",
                ".gitmodules",
                "submodule.deep.ignore",
                "all",
            ],
        );
    }

    fn commit_in_deep(root: &Path) {
        git(
            &root.join("lib/deep"),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "late",
            ],
        );
    }

    /// Pins `--ignore-submodules=none` on `is_dirty`: `-c
    /// diff.ignoreSubmodules` does not outrank `.gitmodules`.
    #[test]
    fn is_dirty_sees_a_submodule_edit_under_ignore_all() {
        let (_tmp, wt, _home, _id) = submodule_case(|wt| {
            ignore_all(wt);
            fs::write(wt.join("lib/a.txt"), "edited\n").unwrap();
        });
        assert!(git::is_dirty(&wt).unwrap());
    }

    /// Pins `--ignore-submodules=none` in the foreach status.
    #[test]
    fn nested_submodule_commit_under_ignore_all_is_refused() {
        let (_tmp, wt, home, id) = nested_case(ignore_deep);
        commit_in_deep(&wt);
        let out = cold(&home, &id).unwrap();
        assert_eq!(
            out,
            Outcome::Skipped(SkipReason::new(SkipCode::Submodule, "dirty submodule: lib"))
        );
    }

    /// Pins `--ignore-submodules=dirty` in `submodule_change`.
    #[test]
    fn late_nested_submodule_commit_under_ignore_all_moves_the_tree_back() {
        let (_tmp, wt, home, id) = nested_case(ignore_deep);
        let seam = set_seam(&BEFORE_RENAME, commit_in_deep);
        let out = cold(&home, &id);
        drop(seam);
        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert!(wt.join("lib/deep/.git").exists());
        assert!(trash_siblings(&wt).unwrap().is_empty());
    }

    /// Regenerable output written inside the submodule in the window is not
    /// a change: none of it is saved, and it goes with the tree.
    #[test]
    fn build_output_in_submodule_before_rename_still_goes_cold() {
        let (_tmp, wt, home, id, out) = cold_with_late_submodule_write(|root: &Path| {
            fs::create_dir_all(root.join("lib/build")).unwrap();
            fs::write(root.join("lib/build/out.class"), "x").unwrap()
        });
        assert_eq!(out.unwrap(), Outcome::Done(Level::Cold));
        assert!(!wt.exists());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Cold);
    }

    /// A commit inside the submodule in the window changes its gitlink.
    #[test]
    fn submodule_commit_before_rename_moves_the_tree_back() {
        let (tmp, repo) = repo();
        let wt = with_submodule(tmp.path(), &repo);
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let seam = set_seam(&BEFORE_RENAME, |root: &Path| {
            git(
                &root.join("lib"),
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "late",
                ],
            )
        });
        let out = cold(&home, &id);
        drop(seam);

        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert!(wt.join("lib/.git").exists());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    /// A worktree with submodule `lib`, `setup` run on it, registered.
    fn submodule_case(setup: impl Fn(&Path)) -> (tempfile::TempDir, PathBuf, BaleHome, String) {
        let (tmp, repo) = repo();
        let wt = with_submodule(tmp.path(), &repo);
        setup(&wt);
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;
        (tmp, wt, home, id)
    }

    /// `cold` on a `submodule_case`, with `write` run just before the rename.
    fn cold_late(
        setup: impl Fn(&Path),
        write: impl Fn(&Path) + 'static,
    ) -> (tempfile::TempDir, PathBuf, Result<Outcome>) {
        let (tmp, wt, home, id) = submodule_case(setup);
        let seam = set_seam(&BEFORE_RENAME, write);
        let out = cold(&home, &id);
        drop(seam);
        (tmp, wt, out)
    }

    fn hide_untracked(dir: &Path) {
        git(dir, &["config", "status.showUntrackedFiles", "no"]);
    }

    /// `.gitmodules` says `ignore = all` for `lib`, committed.
    fn ignore_all(wt: &Path) {
        git(
            wt,
            &["config", "-f", ".gitmodules", "submodule.lib.ignore", "all"],
        );
        git(
            wt,
            &["-c", "commit.gpgsign=false", "commit", "-qam", "ignore"],
        );
    }

    /// The user's `status.showUntrackedFiles=no` in the submodule must not
    /// hide an untracked file there from the refusal.
    #[test]
    fn untracked_file_hidden_by_config_in_submodule_is_refused() {
        let (_tmp, wt, home, id) = submodule_case(|wt| {
            hide_untracked(&wt.join("lib"));
            fs::write(wt.join("lib/pre.txt"), "pre\n").unwrap();
        });
        let out = cold(&home, &id).unwrap();
        assert_eq!(
            out,
            Outcome::Skipped(SkipReason::new(SkipCode::Submodule, "dirty submodule: lib"))
        );
        assert!(wt.join("lib/pre.txt").is_file());
    }

    #[test]
    fn late_write_hidden_by_config_in_submodule_moves_the_tree_back() {
        let (_tmp, wt, out) = cold_late(
            |wt| hide_untracked(&wt.join("lib")),
            |root| fs::write(root.join("lib/late.txt"), "late\n").unwrap(),
        );
        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert!(wt.join("lib/late.txt").is_file());
        assert!(trash_siblings(&wt).unwrap().is_empty());
    }

    #[test]
    fn late_write_hidden_by_config_at_root_moves_the_tree_back() {
        let (_tmp, wt, out) = cold_late(hide_untracked, |root| {
            fs::write(root.join("late.txt"), "late\n").unwrap()
        });
        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert!(wt.join("late.txt").is_file());
    }

    /// An untracked file at the root under `showUntrackedFiles=no` goes in
    /// the wip commit, so the tree may go cold.
    #[test]
    fn untracked_file_hidden_by_config_at_root_is_saved() {
        let (tmp, _wt, home, id) = submodule_case(|wt| {
            hide_untracked(wt);
            fs::write(wt.join("pre.txt"), "pre\n").unwrap();
        });
        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
        let blob = Git::at(tmp.path().join("r"))
            .run(&["show", &format!("refs/bale/{id}/wip:pre.txt")])
            .unwrap();
        assert_eq!(blob, "pre\n");
    }

    #[test]
    fn submodule_commit_under_ignore_all_moves_the_tree_back() {
        let (_tmp, wt, out) = cold_late(ignore_all, |root| {
            git(
                &root.join("lib"),
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "late",
                ],
            )
        });
        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert!(wt.join("lib/.git").exists());
    }

    #[test]
    fn submodule_edit_under_ignore_all_moves_the_tree_back() {
        let (_tmp, wt, out) = cold_late(ignore_all, |root| {
            fs::write(root.join("lib/a.txt"), "edited\n").unwrap()
        });
        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert_eq!(
            fs::read_to_string(wt.join("lib/a.txt")).unwrap(),
            "edited\n"
        );
    }

    #[test]
    fn submodule_edit_under_ignore_all_is_refused() {
        let (_tmp, wt, home, id) = submodule_case(|wt| {
            ignore_all(wt);
            fs::write(wt.join("lib/a.txt"), "edited\n").unwrap();
        });
        let out = cold(&home, &id).unwrap();
        assert_eq!(
            out,
            Outcome::Skipped(SkipReason::new(SkipCode::Submodule, "dirty submodule: lib"))
        );
        assert_eq!(
            fs::read_to_string(wt.join("lib/a.txt")).unwrap(),
            "edited\n"
        );
    }

    /// An assume-unchanged entry's edits are invisible to `status`; cold
    /// refuses the tree while one exists.
    #[test]
    fn assume_unchanged_at_root_is_refused() {
        let (_tmp, _wt, home, id) = submodule_case(|wt| {
            git(wt, &["update-index", "--assume-unchanged", ".gitignore"]);
        });
        let out = cold(&home, &id).unwrap();
        assert_eq!(
            out,
            Outcome::Skipped(SkipReason::new(
                SkipCode::HiddenEntries,
                "assume-unchanged or skip-worktree entries: .gitignore"
            ))
        );
    }

    #[test]
    fn assume_unchanged_in_submodule_is_refused() {
        let (_tmp, wt, home, id) = submodule_case(|wt| {
            git(
                &wt.join("lib"),
                &["update-index", "--assume-unchanged", "a.txt"],
            );
        });
        let out = cold(&home, &id).unwrap();
        assert_eq!(
            out,
            Outcome::Skipped(SkipReason::new(
                SkipCode::HiddenEntries,
                "assume-unchanged or skip-worktree entries: lib/a.txt"
            ))
        );
        assert!(wt.join("lib/a.txt").is_file());
    }

    #[test]
    fn skip_worktree_in_submodule_is_refused() {
        let (_tmp, _wt, home, id) = submodule_case(|wt| {
            git(
                &wt.join("lib"),
                &["update-index", "--skip-worktree", "a.txt"],
            );
        });
        let out = cold(&home, &id).unwrap();
        assert_eq!(
            out,
            Outcome::Skipped(SkipReason::new(
                SkipCode::HiddenEntries,
                "assume-unchanged or skip-worktree entries: lib/a.txt"
            ))
        );
    }

    /// Set in the window, the flag would hide the edit from the re-check.
    #[test]
    fn late_assume_unchanged_edit_in_submodule_moves_the_tree_back() {
        let (_tmp, wt, out) = cold_late(
            |_| {},
            |root| {
                let lib = root.join("lib");
                git(&lib, &["update-index", "--assume-unchanged", "a.txt"]);
                fs::write(lib.join("a.txt"), "edited\n").unwrap();
            },
        );
        assert_eq!(out.unwrap(), Outcome::Busy(Vec::new()));
        assert_eq!(
            fs::read_to_string(wt.join("lib/a.txt")).unwrap(),
            "edited\n"
        );
    }

    /// Files in a submodule dir that is not checked out are in no save;
    /// cold refuses before saving anything.
    #[test]
    fn uninitialized_submodule_with_files_is_refused() {
        let (tmp, wt, home, id) = submodule_case(|wt| {
            git(wt, &["submodule", "deinit", "-q", "-f", "lib"]);
            fs::write(wt.join("lib/x.txt"), "x\n").unwrap();
        });
        let out = cold(&home, &id).unwrap();
        assert_eq!(
            out,
            Outcome::Skipped(SkipReason::new(
                SkipCode::Submodule,
                "submodule lib is not checked out but has files"
            ))
        );
        assert!(wt.join("lib/x.txt").is_file());
        let wip = Git::at(tmp.path().join("r")).run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/bale/{id}/wip"),
        ]);
        assert!(wip.is_err(), "nothing is saved");
    }

    /// An empty, not checked out submodule dir holds nothing to lose.
    #[test]
    fn uninitialized_empty_submodule_goes_cold() {
        let (_tmp, _wt, home, id) = submodule_case(|wt| {
            git(wt, &["submodule", "deinit", "-q", "-f", "lib"]);
        });
        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
    }

    /// The in-place verify looks inside submodules: the wip commit holds
    /// only `lib`'s HEAD, so the root tree can't show this file.
    #[test]
    fn verify_catches_untracked_file_in_submodule() {
        let (tmp, wt, _home, _id) = submodule_case(|_| {});
        let dest = tmp.path().join("dest");
        save_wip(&wt, "x").unwrap();
        save_ignored(&wt, &dest, &none()).unwrap();
        verify(&wt, "x", &dest, &none()).unwrap();
        fs::write(wt.join("lib/late.txt"), "late\n").unwrap();
        let err = verify(&wt, "x", &dest, &none()).unwrap_err();
        assert!(
            format!("{err:#}").contains("submodule lib changed while saving"),
            "{err:#}"
        );
    }

    /// The moved-tree check refuses a wip whose gitlink is not HEAD's, even
    /// when the refusal before the save missed it: open's `reset` would put
    /// HEAD's gitlink back.
    #[test]
    fn restore_check_refuses_moved_gitlink() {
        let (tmp, wt, _home, _id) = submodule_case(|wt| {
            git(
                &wt.join("lib"),
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "moved",
                ],
            )
        });
        let dest = tmp.path().join("dest");
        save_wip(&wt, "x").unwrap();
        save_ignored(&wt, &dest, &none()).unwrap();
        let wip = wip_commit(&wt, "x").unwrap();
        match restore_check("x", &wt, &wip, &dest, &none()).unwrap() {
            Verdict::Differs(reason) => assert!(reason.contains("submodule lib"), "{reason}"),
            Verdict::Same(_) => panic!("a moved gitlink passed the check"),
        }
    }

    /// `recover` checks an unmarked trash inside its submodules too.
    #[test]
    fn crash_after_rename_keeps_a_late_write_in_a_submodule() {
        let (_tmp, wt, home, id) = submodule_case(|_| {});
        let before = set_seam(&BEFORE_RENAME, |root: &Path| {
            fs::write(root.join("lib/late.txt"), "late\n").unwrap()
        });
        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop((before, after));
        assert!(crashed.is_err());
        assert_eq!(trash_siblings(&wt).unwrap().len(), 1);

        assert_eq!(cold(&home, &id).unwrap(), Outcome::Busy(Vec::new()));
        assert_eq!(
            fs::read_to_string(wt.join("lib/late.txt")).unwrap(),
            "late\n"
        );
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    /// The trash marker lets `recover` delete the trash without checking
    /// it, so it must not exist until the moved tree has been checked.
    #[test]
    fn no_trash_marker_before_the_moved_tree_is_checked() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;
        let marker = home.saved_dir(&id).join(TRASH_MARKER);

        let seen = Rc::new(Cell::new(None));
        let seen_in = seen.clone();
        let seam = set_seam(&AFTER_RENAME, move |_: &Path| {
            seen_in.set(Some(marker.exists()))
        });
        let out = cold(&home, &id);
        drop(seam);

        assert_eq!(out.unwrap(), Outcome::Done(Level::Cold));
        assert_eq!(seen.get(), Some(false), "marker written before the check");
    }

    /// A crash right after the move, with a write that landed before it:
    /// the next run must check the trash, find the write and keep it.
    /// Recovery checks an unmarked trash for holders before deleting it, as
    /// `finish` does: a process that has the moved tree open moves it back.
    /// Unix only: holders are not listed on Windows (`busy_holders`).
    #[cfg(unix)]
    #[test]
    fn recovery_keeps_an_unmarked_trash_a_process_holds() {
        struct KillOnDrop(std::process::Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop(after);
        assert!(crashed.is_err());
        let trash = trash_siblings(&wt).unwrap();
        assert_eq!(trash.len(), 1);

        let holder = KillOnDrop(
            Command::new("sleep")
                .arg("30")
                .current_dir(&trash[0])
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(Duration::from_millis(300));
        let out = cold(&home, &id);
        drop(holder);

        assert!(
            matches!(&out, Ok(Outcome::Busy(h)) if !h.is_empty()),
            "{out:?}"
        );
        assert!(wt.join(".gitignore").is_file());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    /// Recovery that has checked an unmarked trash and then fails partway
    /// through removing it must finish the removal on the next run. Moving
    /// the half-removed tree back would let a later cold save its losses
    /// over the good save.
    #[cfg(unix)]
    #[test]
    fn recovery_removal_that_stops_partway_finishes_on_retry() {
        use std::os::unix::fs::PermissionsExt;
        if Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|o| o.stdout.starts_with(b"0\n"))
        {
            return; // root ignores the permission that blocks removal
        }
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        for i in 0..30 {
            fs::write(wt.join(format!("u{i}.txt")), "u").unwrap();
        }
        fs::create_dir(wt.join("mm")).unwrap();
        fs::write(wt.join("mm/f"), "x").unwrap();
        fs::set_permissions(wt.join("mm"), fs::Permissions::from_mode(0o500)).unwrap();
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop(after);
        assert!(crashed.is_err());
        let trash = trash_siblings(&wt).unwrap();
        assert_eq!(trash.len(), 1);

        let first = cold(&home, &id);
        assert!(first.is_err(), "{first:?}");
        fs::set_permissions(trash[0].join("mm"), fs::Permissions::from_mode(0o700)).unwrap();

        let second = cold(&home, &id);
        assert!(
            matches!(second, Ok(Outcome::Done(Level::Cold))),
            "{second:?}"
        );
        assert!(!wt.exists());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        crate::ops::open::open(&home, &id).unwrap();
        assert_eq!(fs::read_to_string(wt.join("u7.txt")).unwrap(), "u");
        assert_eq!(fs::read_to_string(wt.join("mm/f")).unwrap(), "x");
    }

    /// A removal that fails partway names the entry it failed on, so the
    /// user can see what blocks it.
    #[cfg(unix)]
    #[test]
    fn failed_trash_removal_names_the_path() {
        use std::os::unix::fs::PermissionsExt;
        if Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|o| o.stdout.starts_with(b"0\n"))
        {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let trash = tmp.path().join(".bale-trash-wt-1");
        fs::create_dir_all(trash.join("stuck")).unwrap();
        fs::write(trash.join("stuck/held.txt"), "x").unwrap();
        fs::set_permissions(trash.join("stuck"), fs::Permissions::from_mode(0o500)).unwrap();
        let err = remove_trash(&trash).unwrap_err();
        fs::set_permissions(trash.join("stuck"), fs::Permissions::from_mode(0o700)).unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("stuck/held.txt"), "{msg}");
        assert!(msg.contains("bale cold"), "{msg}");
    }

    #[test]
    fn crash_after_rename_keeps_a_late_write() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let before = set_seam(&BEFORE_RENAME, |root: &Path| {
            fs::write(root.join("late.txt"), "late\n").unwrap()
        });
        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop((before, after));
        assert!(crashed.is_err());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Cooling);
        let trash = trash_siblings(&wt).unwrap();
        assert_eq!(trash.len(), 1);

        assert_eq!(cold(&home, &id).unwrap(), Outcome::Busy(Vec::new()));
        assert_eq!(fs::read_to_string(wt.join("late.txt")).unwrap(), "late\n");
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
        assert_eq!(git::find_worktree(&repo, &wt).unwrap().locked, None);
    }

    /// A worktree at `r/.claude/worktrees/w` (inside the main checkout, which
    /// does not ignore `.claude/`) with untracked `work.txt` and ignored
    /// `.env`, registered.
    fn nested_worktree() -> (tempfile::TempDir, PathBuf, PathBuf, BaleHome, String) {
        let (tmp, repo) = repo();
        // Joined part by part: on Windows a `/` would stay in the path and
        // never match the canonical path bale reports.
        let wt = repo.join(".claude").join("worktrees").join("w");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        fs::write(wt.join("work.txt"), "work\n").unwrap();
        fs::write(wt.join(".env"), "SECRET=1\n").unwrap();
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;
        (tmp, repo, wt, home, id)
    }

    fn wip_ref(repo: &Path, id: &str) -> String {
        Git::at(repo)
            .run(&["rev-parse", &format!("refs/bale/{id}/wip")])
            .unwrap()
    }

    /// What an editor leaves when it recreates the path: a dir with no
    /// `.git`, so git in it climbs into the enclosing repo.
    fn recreate_as_stray(wt: &Path) {
        fs::create_dir_all(wt.join(".idea")).unwrap();
        fs::write(wt.join(".idea/workspace.xml"), "<x/>\n").unwrap();
    }

    /// The audit's F1: after a crash past the rename, a recreated folder at
    /// the path is not re-saved over the record. Once it is moved away,
    /// recovery finishes and open gives the work back.
    #[test]
    fn recovery_refuses_a_recreated_folder_beside_the_trash() {
        let (_tmp, repo, wt, home, id) = nested_worktree();
        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop(after);
        assert!(crashed.is_err());
        let trash = trash_siblings(&wt).unwrap();
        assert_eq!(trash.len(), 1);
        let wip = wip_ref(&repo, &id);
        let saved_env = home.saved_dir(&id).join(FILES).join(".env");
        assert_eq!(fs::read_to_string(&saved_env).unwrap(), "SECRET=1\n");

        recreate_as_stray(&wt);
        let out = cold(&home, &id).unwrap();
        let Outcome::Skipped(reason) = &out else {
            panic!("{out:?}");
        };
        assert!(reason.contains(&wt.display().to_string()), "{reason}");
        assert!(reason.contains(&trash[0].display().to_string()), "{reason}");
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Cooling);
        assert_eq!(wip_ref(&repo, &id), wip);
        assert_eq!(fs::read_to_string(&saved_env).unwrap(), "SECRET=1\n");
        assert_eq!(trash_siblings(&wt).unwrap(), trash);
        assert_eq!(
            fs::read_to_string(trash[0].join("work.txt")).unwrap(),
            "work\n"
        );
        assert_eq!(
            fs::read_to_string(trash[0].join(".env")).unwrap(),
            "SECRET=1\n"
        );
        assert_eq!(
            git::find_worktree(&repo, &wt).unwrap().locked.as_deref(),
            Some(git::LOCK_REASON)
        );

        fs::remove_dir_all(&wt).unwrap();
        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
        assert!(trash_siblings(&wt).unwrap().is_empty());
        crate::ops::open::open(&home, &id).unwrap();
        assert_eq!(fs::read(wt.join("work.txt")).unwrap(), b"work\n");
        assert_eq!(fs::read(wt.join(".env")).unwrap(), b"SECRET=1\n");
    }

    /// A crash before the rename leaves the checkout at the path. If it is
    /// then replaced by a folder that is not this worktree's checkout,
    /// recovery refuses it and writes nothing.
    #[test]
    fn recovery_refuses_a_folder_that_is_not_the_checkout() {
        let (_tmp, repo, wt, home, id) = nested_worktree();
        let before = set_seam(&BEFORE_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop(before);
        assert!(crashed.is_err());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let wip = wip_ref(&repo, &id);
        let saved_env = home.saved_dir(&id).join(FILES).join(".env");

        fs::remove_dir_all(&wt).unwrap();
        recreate_as_stray(&wt);
        let out = cold(&home, &id).unwrap();
        let Outcome::Skipped(reason) = &out else {
            panic!("{out:?}");
        };
        assert!(reason.contains(&wt.display().to_string()), "{reason}");
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Cooling);
        assert_eq!(wip_ref(&repo, &id), wip);
        assert_eq!(fs::read_to_string(&saved_env).unwrap(), "SECRET=1\n");
        assert!(wt.join(".idea/workspace.xml").is_file());
        assert_eq!(
            git::find_worktree(&repo, &wt).unwrap().locked.as_deref(),
            Some(git::LOCK_REASON)
        );
    }

    /// The checkout check accepts the real checkout: a crash before the
    /// rename resumes and goes cold.
    #[test]
    fn recovery_of_the_checkout_itself_goes_cold() {
        let (_tmp, _repo, wt, home, id) = nested_worktree();
        let before = set_seam(&BEFORE_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop(before);
        assert!(crashed.is_err());

        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
        assert!(!wt.exists());
        crate::ops::open::open(&home, &id).unwrap();
        assert_eq!(fs::read(wt.join("work.txt")).unwrap(), b"work\n");
        assert_eq!(fs::read(wt.join(".env")).unwrap(), b"SECRET=1\n");
    }

    /// `add -A` drops a `.GIT` dir, so the git-side check of the moved
    /// tree passes; the restore comparison does not.
    #[test]
    fn case_variant_git_dir_after_rename_moves_the_tree_back() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let seam = set_seam(&AFTER_RENAME, |trash: &Path| {
            fs::create_dir_all(trash.join("d/.GIT")).unwrap();
            fs::write(trash.join("d/.GIT/keep"), "k").unwrap();
        });
        let out = cold(&home, &id).unwrap();
        drop(seam);

        let named = Path::new("d").join(".GIT").display().to_string();
        assert!(
            matches!(&out, Outcome::Skipped(r) if r.contains(&named)),
            "{out:?}"
        );
        assert_eq!(fs::read_to_string(wt.join("d/.GIT/keep")).unwrap(), "k");
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    /// Git refuses any case variant of `.git` as a path, so open could not
    /// restore one. Cold refuses before `up` removes anything or the tree
    /// moves, on every platform.
    #[test]
    fn case_variant_git_dir_is_refused_before_anything_changes() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        fs::write(wt.join(".gitignore"), ".env\nbuild/\n").unwrap();
        fs::write(wt.join("settings.gradle.kts"), "").unwrap();
        fs::create_dir_all(wt.join("build")).unwrap();
        fs::write(wt.join("build/out.bin"), "o").unwrap();
        fs::create_dir_all(wt.join("d/.Git")).unwrap();
        fs::write(wt.join("d/.Git/keep"), "k").unwrap();
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let seam = set_seam(&BEFORE_RENAME, |_: &Path| panic!("reached the rename"));
        let out = cold(&home, &id).unwrap();
        drop(seam);

        let named = Path::new("d").join(".Git").display().to_string();
        assert!(
            matches!(&out, Outcome::Skipped(r) if r.contains(&named)),
            "{out:?}"
        );
        assert_eq!(fs::read_to_string(wt.join("d/.Git/keep")).unwrap(), "k");
        assert!(wt.join("build/out.bin").is_file());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_ne!(record.level, Level::Cooling);
    }

    /// The restore check writes a copy of the tree beside it. Without room
    /// for that copy and a margin, cold stops before the rename.
    #[test]
    fn no_room_for_the_check_skips_before_the_rename() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let free = fake_free(1024);
        let seam = set_seam(&BEFORE_RENAME, |_: &Path| panic!("reached the rename"));
        let out = cold(&home, &id).unwrap();
        drop((seam, free));

        assert!(
            matches!(&out, Outcome::Skipped(r) if r.contains("free space") && r.contains("1.0 KiB free")),
            "{out:?}"
        );
        assert!(wt.join(".gitignore").is_file());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
    }

    /// Recovery's check of an unmarked trash needs the same room; without
    /// it the trash goes back to the path.
    #[test]
    fn no_room_for_the_check_in_recovery_moves_the_trash_back() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;
        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop(after);
        assert!(crashed.is_err());
        assert_eq!(trash_siblings(&wt).unwrap().len(), 1);

        let free = fake_free(0);
        let out = cold(&home, &id).unwrap();
        drop(free);

        assert!(
            matches!(&out, Outcome::Skipped(r) if r.contains("free space") && r.contains("0 B free")),
            "{out:?}"
        );
        assert!(wt.join(".gitignore").is_file());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    /// Recovery checks an unmarked trash with the same restore comparison.
    #[test]
    fn crash_after_rename_keeps_a_case_variant_git_dir() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let before = set_seam(&BEFORE_RENAME, |root: &Path| {
            fs::create_dir_all(root.join("d/.GIT")).unwrap();
            fs::write(root.join("d/.GIT/keep"), "k").unwrap();
        });
        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop((before, after));
        assert!(crashed.is_err());

        let out = cold(&home, &id).unwrap();
        let named = Path::new("d").join(".GIT").display().to_string();
        assert!(
            matches!(&out, Outcome::Skipped(r) if r.contains(&named)),
            "{out:?}"
        );
        assert_eq!(fs::read_to_string(wt.join("d/.GIT/keep")).unwrap(), "k");
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    /// When the move back fails (something took the path), the trash stays
    /// where it is, the record stays `Cooling`, and the error names both.
    #[test]
    fn failed_move_back_in_recovery_keeps_the_trash() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;
        let before = set_seam(&BEFORE_RENAME, |root: &Path| {
            fs::write(root.join("late.txt"), "late\n").unwrap()
        });
        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop((before, after));
        assert!(crashed.is_err());
        let trash = trash_siblings(&wt).unwrap();

        let taken = wt.clone();
        let seam = set_seam(&BEFORE_MOVE_BACK, move |_: &Path| {
            fs::create_dir_all(taken.join("other")).unwrap()
        });
        let err = cold(&home, &id).unwrap_err();
        drop(seam);

        let text = format!("{err:#}");
        // bale reports the canonical path (no 8.3 short names on Windows).
        let wt = crate::paths::canonicalize(&wt).unwrap();
        assert!(text.contains(&trash[0].display().to_string()), "{text}");
        assert!(text.contains(&wt.display().to_string()), "{text}");
        assert!(trash[0].join("late.txt").is_file());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Cooling);
    }

    /// recover lists regenerable dirs with git in the moved tree; a
    /// submodule there (whose git dir points at the old path) must not
    /// break that.
    #[test]
    fn crash_after_rename_with_submodule_keeps_a_late_write() {
        let (tmp, repo) = repo();
        let wt = with_submodule(tmp.path(), &repo);
        fs::write(wt.join("settings.gradle.kts"), "").unwrap();
        fs::create_dir_all(wt.join("build")).unwrap();
        fs::write(wt.join("build/k.txt"), "k\n").unwrap();
        git(&wt, &["add", "settings.gradle.kts", "build/k.txt"]);
        git(
            &wt,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "gradle"],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let before = set_seam(&BEFORE_RENAME, |root: &Path| {
            fs::write(root.join("late.txt"), "late\n").unwrap()
        });
        let after = set_seam(&AFTER_RENAME, |_: &Path| panic!("crash"));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cold(&home, &id)));
        drop((before, after));
        assert!(crashed.is_err());
        let trash = trash_siblings(&wt).unwrap();
        assert_eq!(trash.len(), 1);

        assert_eq!(cold(&home, &id).unwrap(), Outcome::Busy(Vec::new()));
        assert!(wt.join("late.txt").is_file());
        assert!(wt.join("lib/.git").exists());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    /// A git failure while checking the moved tree is an error, not a busy
    /// tree: the tree goes back and the cause is reported.
    #[test]
    fn git_failure_after_rename_moves_the_tree_back_with_an_error() {
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let wip = format!("refs/bale/{id}/wip");
        let seam = set_seam(&AFTER_RENAME, move |_: &Path| {
            git(&repo, &["update-ref", "-d", &wip])
        });
        let out = cold(&home, &id);
        drop(seam);

        let err = out.unwrap_err();
        assert!(format!("{err:#}").contains("after moving it"), "{err:#}");
        assert!(wt.join(".gitignore").is_file());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
        assert_eq!(cold(&home, &id).unwrap(), Outcome::Done(Level::Cold));
    }

    /// A process that opens the tree after the first busy check and before
    /// the move still holds the moved tree; the second check finds it.
    #[cfg(unix)]
    #[test]
    fn holder_that_arrives_before_rename_moves_the_tree_back() {
        struct KillOnDrop(std::process::Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let (tmp, repo) = repo();
        let wt = tmp.path().join("wt");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        let home = BaleHome(tmp.path().join("home"));
        let id = crate::ops::up::register(&home, &wt).unwrap().id;

        let child: Rc<RefCell<Option<KillOnDrop>>> = Rc::new(RefCell::new(None));
        let child_in = child.clone();
        let seam = set_seam(&BEFORE_RENAME, move |root: &Path| {
            let c = Command::new("sleep")
                .arg("30")
                .current_dir(root)
                .spawn()
                .unwrap();
            *child_in.borrow_mut() = Some(KillOnDrop(c));
            std::thread::sleep(Duration::from_millis(300));
        });
        let out = cold(&home, &id);
        drop(seam);
        drop(child.borrow_mut().take());

        assert!(
            matches!(&out, Ok(Outcome::Busy(h)) if !h.is_empty()),
            "{out:?}"
        );
        assert!(wt.join(".gitignore").is_file());
        assert!(trash_siblings(&wt).unwrap().is_empty());
        let record = Registry::open(home.clone()).get(&id).unwrap().unwrap();
        assert_eq!(record.level, Level::Baled);
    }

    #[test]
    fn verify_catches_changed_ignored_file() {
        let (tmp, root) = repo();
        fs::write(root.join(".env"), "A=1\n").unwrap();
        let dest = tmp.path().join("dest");
        save_wip(&root, "x").unwrap();
        save_ignored(&root, &dest, &none()).unwrap();
        verify(&root, "x", &dest, &none()).unwrap();
        fs::write(root.join(".env"), "A=2\n").unwrap();
        let err = verify(&root, "x", &dest, &none()).unwrap_err();
        assert!(format!("{err:#}").contains(".env"), "{err:#}");
    }

    #[test]
    fn verify_catches_new_ignored_file() {
        let (tmp, root) = repo();
        let dest = tmp.path().join("dest");
        save_wip(&root, "x").unwrap();
        save_ignored(&root, &dest, &none()).unwrap();
        fs::write(root.join(".env"), "late\n").unwrap();
        let err = verify(&root, "x", &dest, &none()).unwrap_err();
        assert!(format!("{err:#}").contains("unsaved [.env]"), "{err:#}");
    }

    #[test]
    fn verify_catches_edit_after_wip() {
        let (tmp, root) = repo();
        let dest = tmp.path().join("dest");
        save_wip(&root, "x").unwrap();
        save_ignored(&root, &dest, &none()).unwrap();
        fs::write(root.join("late.txt"), "late\n").unwrap();
        let err = verify(&root, "x", &dest, &none()).unwrap_err();
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
        let parent = crate::paths::canonicalize(tmp.path()).unwrap();
        assert_eq!(found, vec![parent.join(".bale-trash-a-1")]);
    }
}
