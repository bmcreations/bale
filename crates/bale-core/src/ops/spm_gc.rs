//! Sweep's removal of shared Swift packages folders (`<home>/spm/<key>`) that
//! nothing links to, and of leftover temp clones. Only macOS builds this.
//!
//! Deleting a shared folder under a live build breaks it, so every doubt
//! means "keep":
//!
//! * A folder is referenced when a `SourcePackages` symlink resolves into
//!   `spm/<key>`. Links are looked for in Xcode's shared DerivedData, in every
//!   learned `-derivedDataPath` root that exists (without asking whether it
//!   looks like DerivedData: a fresh one holds only the link), in each
//!   registered tree's own folders (`<tree>/<child>` as a root, and
//!   `<tree>/<child>/DerivedData` for `build/DerivedData`), and in the folders
//!   and keys the records name. Both `<root>/SourcePackages` and
//!   `<root>/<child>/SourcePackages` are checked.
//! * Keys are compared by file name under the canonical `spm/`, so `/var`
//!   and `/private/var` spellings of one folder agree.
//! * If any listing fails for a reason other than "not found", or a record
//!   can't be read, nothing is deleted this sweep and the row says why. The
//!   one exception is a permission error on a folder directly in a tree.
//! * A candidate must also be older than `grace` (itself and what is directly
//!   in it) and have no process holding it.
//! * Nothing follows a link: `remove_entry` and `remove_stub` remove links
//!   themselves, and spm entries that are not real directories are ignored.
//!
//! The race with hooks that link (`link_tree`, `link_derived_data_path`,
//! `relink_tree`) is closed by `spm/.lock` (see [`crate::spm_lock`]). A hook
//! holds it shared while it checks for `spm/<key>` and makes the link. The
//! sweep takes it exclusive without waiting, scans for references once more,
//! and renames each unreferenced `spm/<key>` to `spm/.gc-<key>-<pid>-<n>`;
//! then it lets go. A hook then either linked before the rename (and the
//! rescan saw the link) or finds no `spm/<key>`. The deletion of the renamed
//! folders, which takes seconds, happens outside the lock. A `.gc-*` left by a
//! sweep that died is deleted by the next one, with no grace period. If a hook
//! holds the lock, the sweep skips the cleanup. If the sweep holds it, the
//! hook links nothing and the tree builds unshared.
//!
//! The cleanup runs whether or not `share_swift_packages` is on, so turning
//! the feature off still lets the folders it made go away.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::busy::busy_holders;
use crate::ops::up::{is_stub, remove_entry, remove_stub};
use crate::registry::{Record, Registry};
use crate::BaleHome;

/// Sizing a folder before it is removed stops after this long.
const SIZING_LIMIT: Duration = Duration::from_secs(30);

/// Why the cleanup could not say what is still linked.
struct ScanError {
    /// For the trace; names no path.
    reason: String,
    /// For the sweep's row.
    detail: String,
}

type Scan<T> = Result<T, ScanError>;

impl From<io::Error> for ScanError {
    fn from(e: io::Error) -> Self {
        ScanError {
            reason: format!("{:?}", e.kind()),
            detail: format!("could not read a folder it needs to check: {e}"),
        }
    }
}

impl ScanError {
    fn new(reason: &str, detail: String) -> Self {
        ScanError {
            reason: reason.into(),
            detail,
        }
    }

    /// Writes the trace line and the row for a cleanup that did not run.
    fn skip(&self, home: &BaleHome, rows: &mut Vec<(String, String)>) {
        crate::trace::spm_gc_skipped(home, &self.reason);
        rows.push((
            home.spm_dir().display().to_string(),
            format!("skipped: {}", self.detail),
        ));
    }
}

/// `Ok(None)` for a missing path, the error for any other failure.
fn or_missing<T>(r: io::Result<T>) -> Scan<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

type Records = Vec<(String, anyhow::Result<Record>)>;

/// `(folder or key, result)` for each shared folder, temp clone or stub
/// removed (or, on `dry_run`, that would be) or that failed, and one row
/// naming `spm/` when the cleanup could not run.
pub(crate) fn collect(home: &BaleHome, grace: Duration, dry_run: bool) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let registry = Registry::open(home.clone());
    let now = SystemTime::now();
    match registry.all_lenient() {
        Ok(records) => drop_gone_stubs(home, &records, grace, now, dry_run, &mut rows),
        Err(e) => {
            ScanError::new("registry", format!("cannot list the records: {e:#}"))
                .skip(home, &mut rows);
            return rows;
        }
    }
    if let Err(e) = run(home, &registry, grace, now, dry_run, &mut rows) {
        e.skip(home, &mut rows);
    }
    rows
}

fn run(
    home: &BaleHome,
    registry: &Registry,
    grace: Duration,
    now: SystemTime,
    dry_run: bool,
    rows: &mut Vec<(String, String)>,
) -> Scan<()> {
    let Some(entries) = or_missing(fs::read_dir(home.spm_dir()))? else {
        return Ok(());
    };
    // Stubs may have been the last references; read the records afresh.
    let records = registry
        .all_lenient()
        .map_err(|e| ScanError::new("registry", format!("cannot list the records: {e:#}")))?;
    let mut keys: Vec<PathBuf> = Vec::new();
    let mut leftovers: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry?;
        // A symlink or file in spm/ is not ours to remove or follow.
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = newest_change(&entry.path()).is_some_and(|t| age(t, now) >= grace);
        if name.starts_with(".gc-") {
            leftovers.push(entry.path());
        } else if name.starts_with(".tmp-") {
            if old {
                leftovers.push(entry.path());
            }
        } else if old {
            keys.push(entry.path());
        }
    }
    keys.sort();
    // Nothing to decide: skip the scans and the lock.
    if keys.is_empty() {
        return remove_all(home, leftovers, dry_run, rows);
    }
    let first = References::scan(home, &records)?;
    keys.retain(|k| !first.contains(k));
    let held = idle(&keys)?;
    let held: Vec<PathBuf> = keys
        .into_iter()
        .zip(held)
        .filter_map(|(k, h)| h.then_some(k))
        .collect();
    let mut condemned = leftovers;
    if dry_run {
        condemned.extend(held);
    } else if !held.is_empty() {
        let Some(_lock) = crate::spm_lock::try_lock(home, crate::spm_lock::Mode::Exclusive) else {
            return Err(ScanError::new(
                "locked",
                "a hook is linking shared packages".into(),
            ));
        };
        // Records and DerivedData as they are now that no hook can link.
        let records = registry
            .all_lenient()
            .map_err(|e| ScanError::new("registry", format!("cannot list the records: {e:#}")))?;
        let second = References::scan(home, &records)?;
        for dir in held.into_iter().filter(|k| !second.contains(k)) {
            match condemn(&dir) {
                Ok(gc) => condemned.push(gc),
                Err(e) => rows.push((
                    dir.display().to_string(),
                    format!("error: renaming {}: {e}", dir.display()),
                )),
            }
        }
    }
    remove_all(home, condemned, dry_run, rows)
}

/// For each of `dirs`, whether no process holds it, from one scan.
fn idle(dirs: &[PathBuf]) -> Scan<Vec<bool>> {
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    let canon: Vec<PathBuf> = dirs
        .iter()
        .map(crate::paths::canonicalize)
        .collect::<io::Result<_>>()?;
    let holders = crate::busy::busy_holders_many(&canon, &[])
        .map_err(|e| ScanError::new("busy", format!("cannot tell which are in use: {e:#}")))?;
    Ok(holders.iter().map(|h| h.is_empty()).collect())
}

/// Renames `spm/<key>` to `spm/.gc-<key>-<pid>-<n>` and returns the new path.
fn condemn(dir: &Path) -> io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    let gc = dir.with_file_name(format!(
        ".gc-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::rename(dir, &gc)?;
    Ok(gc)
}

/// Removes the folders in `dirs` that no process holds, one row and one trace
/// line each; on `dry_run` only the rows.
fn remove_all(
    home: &BaleHome,
    dirs: Vec<PathBuf>,
    dry_run: bool,
    rows: &mut Vec<(String, String)>,
) -> Scan<()> {
    let idle = idle(&dirs)?;
    for (dir, idle) in dirs.into_iter().zip(idle) {
        if !idle {
            continue;
        }
        let start = Instant::now();
        let bytes = crate::trace::size_tree(&dir, &[], start + SIZING_LIMIT).tree;
        let size = crate::human_bytes(bytes);
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        let why = if name.starts_with(".tmp-") {
            "left over from a clone"
        } else if name.starts_with(".gc-") {
            "set aside for removal"
        } else {
            "nothing links to it"
        };
        if dry_run {
            rows.push((
                dir.display().to_string(),
                format!("would remove {size}: {why}"),
            ));
            continue;
        }
        let result = remove_entry(&dir);
        crate::trace::spm_gc(home, bytes, result.is_ok(), start.elapsed());
        rows.push((
            dir.display().to_string(),
            match result {
                Ok(()) => format!("removed {size}: {why}"),
                Err(e) => format!("error: removing {}: {e}", dir.display()),
            },
        ));
    }
    Ok(())
}

/// Removes the pre-made folders the records name that are still untouched
/// stubs (`is_stub`, via `remove_stub`) whose project is gone, once nothing
/// has changed in them for `grace`, and forgets them in the record.
fn drop_gone_stubs(
    home: &BaleHome,
    records: &Records,
    grace: Duration,
    now: SystemTime,
    dry_run: bool,
    rows: &mut Vec<(String, String)>,
) {
    for record in records.iter().filter_map(|(_, r)| r.as_ref().ok()) {
        let mut removed = false;
        for l in &record.spm {
            let project_gone = fs::symlink_metadata(&l.project)
                .is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
            let old = fs::symlink_metadata(&l.folder)
                .and_then(|m| m.modified())
                .is_ok_and(|t| age(t, now) >= grace);
            if !project_gone || !old || !is_stub(&l.folder) {
                continue;
            }
            if !matches!(busy_holders(&l.folder, &[]), Ok(h) if h.is_empty()) {
                continue;
            }
            if dry_run {
                rows.push((
                    l.folder.display().to_string(),
                    "would remove: its project is gone".into(),
                ));
            } else if let Ok(true) = remove_stub(&l.folder) {
                removed = true;
                rows.push((
                    l.folder.display().to_string(),
                    "removed: its project is gone".into(),
                ));
            }
        }
        if removed {
            forget_missing_folders(home, &record.id);
        }
    }
}

/// Drops `record.spm` entries whose folder is gone, under the tree's lock;
/// a locked tree keeps them until the next hook or sweep prunes them.
fn forget_missing_folders(home: &BaleHome, id: &str) {
    let Ok(Some(_lock)) = crate::lock::WorkspaceLock::try_acquire(home, id) else {
        return;
    };
    let registry = Registry::open(home.clone());
    let Ok(Some(mut record)) = registry.get(id) else {
        return;
    };
    let before = record.spm.len();
    record
        .spm
        .retain(|l| fs::symlink_metadata(&l.folder).is_ok());
    if record.spm.len() != before {
        let _ = registry.put(&record);
    }
}

/// The newest mtime among `dir` and the entries directly in it (such as
/// `workspace-state.json`, `checkouts`, `repositories`). `None` when `dir`
/// cannot be read, which keeps it.
fn newest_change(dir: &Path) -> Option<SystemTime> {
    let mut newest = fs::symlink_metadata(dir).ok()?.modified().ok()?;
    for e in fs::read_dir(dir).ok()? {
        let t = fs::symlink_metadata(e.ok()?.path()).ok()?.modified().ok()?;
        newest = newest.max(t);
    }
    Some(newest)
}

/// `now` minus `t`; zero when `t` lies in the future.
fn age(t: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(t).unwrap_or(Duration::ZERO)
}

/// The keys (names under `spm/`) that something links to.
struct References {
    keys: HashSet<OsString>,
    spm: Vec<PathBuf>,
}

impl References {
    /// Whether something links to the folder `dir` directly under `spm/`.
    fn contains(&self, dir: &Path) -> bool {
        dir.file_name().is_some_and(|n| self.keys.contains(n))
    }

    /// Every source of links; an error when any of them cannot be read.
    fn scan(home: &BaleHome, records: &Records) -> Scan<References> {
        let spm = home.spm_dir();
        let mut spms = vec![spm.clone()];
        spms.extend(crate::paths::canonicalize(&spm).ok());
        let mut refs = References {
            keys: HashSet::new(),
            spm: spms,
        };
        if let Some(global) = crate::derived_data::global_root() {
            refs.scan_root(&global)?;
        }
        // Not `learned_roots`: it hides an unreadable file as an empty one.
        let text = or_missing(fs::read_to_string(home.dd_roots_file()))?.unwrap_or_default();
        for line in text.lines().filter(|l| !l.is_empty()) {
            refs.scan_root(Path::new(line))?;
        }
        for (id, record) in records {
            // A record that can't be read may hold any reference.
            let record = record.as_ref().map_err(|e| {
                ScanError::new(
                    "corrupt_record",
                    format!("corrupt record {id}: {:#}", e.root_cause()),
                )
            })?;
            refs.scan_tree(&record.path)?;
            for l in &record.spm {
                // A folder that is gone no longer keeps its key.
                if or_missing(fs::symlink_metadata(&l.folder))?.is_some() {
                    refs.keys.insert(OsString::from(&l.key));
                    refs.scan_link(&l.folder.join("SourcePackages"))?;
                }
            }
        }
        Ok(refs)
    }

    /// `root/SourcePackages` and `root/<child>/SourcePackages`. A missing
    /// root has nothing; one that exists but can't be listed is an error.
    fn scan_root(&mut self, root: &Path) -> Scan<()> {
        let Some(meta) = or_missing(fs::symlink_metadata(root))? else {
            return Ok(());
        };
        if !meta.is_dir() {
            return Ok(());
        }
        self.scan_link(&root.join("SourcePackages"))?;
        let Some(entries) = or_missing(fs::read_dir(root))? else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                self.scan_link(&entry.path().join("SourcePackages"))?;
            }
        }
        Ok(())
    }

    /// A tree's own folders as DerivedData roots: `<tree>`, each
    /// `<tree>/<child>` (such as `.dd`), and `<tree>/<child>/DerivedData`
    /// (such as `build/DerivedData`). A child the user can't read is skipped:
    /// a tree holds many folders, and one of them being locked must not stop
    /// the cleanup. Anything deeper that fails still does.
    fn scan_tree(&mut self, tree: &Path) -> Scan<()> {
        self.scan_link(&tree.join("SourcePackages"))?;
        let Some(entries) = or_missing(fs::read_dir(tree))? else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let child = entry.path();
            match self.scan_root(&child) {
                Err(ScanError { reason, .. }) if reason == "PermissionDenied" => continue,
                other => other?,
            }
            let derived = child.join("DerivedData");
            if or_missing(fs::symlink_metadata(&derived))?.is_some_and(|m| m.is_dir()) {
                self.scan_root(&derived)?;
            }
        }
        Ok(())
    }

    /// Notes the key `link` points into, if it is a symlink into `spm/`.
    /// The target is never opened.
    fn scan_link(&mut self, link: &Path) -> Scan<()> {
        let Some(meta) = or_missing(fs::symlink_metadata(link))? else {
            return Ok(());
        };
        if !meta.file_type().is_symlink() {
            return Ok(());
        }
        let Some(target) = or_missing(fs::read_link(link))? else {
            return Ok(());
        };
        let target = link.parent().map_or(target.clone(), |p| p.join(&target));
        // Textual, then with the target's folder canonicalized (a dangling
        // target still has a parent), then fully canonical.
        let mut forms = vec![target.clone()];
        if let (Some(parent), Some(name)) = (target.parent(), target.file_name()) {
            forms.extend(
                crate::paths::canonicalize(parent)
                    .ok()
                    .map(|p| p.join(name)),
            );
        }
        forms.extend(crate::paths::canonicalize(&target).ok());
        for form in forms {
            for spm in &self.spm {
                if let Some(key) = form
                    .strip_prefix(spm)
                    .ok()
                    .and_then(|rest| rest.components().next())
                {
                    self.keys.insert(key.as_os_str().to_owned());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condemning_moves_the_key_out_of_sight_with_its_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("abcd");
        fs::create_dir_all(dir.join("checkouts")).unwrap();
        fs::write(dir.join("checkouts/f"), "x").unwrap();
        let gc = condemn(&dir).unwrap();
        assert!(fs::symlink_metadata(&dir).is_err(), "spm/<key> is gone");
        let name = gc.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".gc-abcd-"), "{name}");
        assert_eq!(gc.parent(), dir.parent());
        assert!(gc.join("checkouts/f").is_file());
    }
}
