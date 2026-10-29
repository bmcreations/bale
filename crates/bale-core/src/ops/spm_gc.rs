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
//!   registered tree's own top-level folders, and in the folders and keys the
//!   records name. Both `<root>/SourcePackages` and
//!   `<root>/<child>/SourcePackages` are checked.
//! * Keys are compared by file name under the canonical `spm/`, so `/var`
//!   and `/private/var` spellings of one folder agree.
//! * If any listing fails for a reason other than "not found", nothing is
//!   deleted this sweep.
//! * A candidate must also be older than `grace` (itself and what is directly
//!   in it), have no process holding it, and show no reference in a second
//!   scan made right before it is removed.
//! * Nothing follows a link: `remove_entry` and `remove_stub` remove links
//!   themselves, and spm entries that are not real directories are ignored.
//!
//! There is no lock shared with hooks that link (`link_tree`,
//! `link_derived_data_path`, `relink_tree`): the second scan and the grace
//! period narrow that race but do not close it. The window left is a hook
//! linking an old, unreferenced folder in the instant between the second scan
//! and the removal.
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

/// Why a scan could not say what is linked: the error kind, for the trace.
struct ScanError(io::ErrorKind);

type Scan<T> = Result<T, ScanError>;

impl From<io::Error> for ScanError {
    fn from(e: io::Error) -> Self {
        ScanError(e.kind())
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

/// `(folder or key, result)` for each shared folder, temp clone or stub
/// removed (or, on `dry_run`, that would be) or that failed.
pub(crate) fn collect(home: &BaleHome, grace: Duration, dry_run: bool) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let registry = Registry::open(home.clone());
    let Ok(records) = registry.all_lenient() else {
        crate::trace::spm_gc_skipped(home, "registry");
        return rows;
    };
    let now = SystemTime::now();
    drop_gone_stubs(home, &records, grace, now, dry_run, &mut rows);
    // Stubs may have been the last references; read the records again.
    let Ok(records) = registry.all_lenient() else {
        crate::trace::spm_gc_skipped(home, "registry");
        return rows;
    };
    let spm = home.spm_dir();
    let entries = match or_missing(fs::read_dir(&spm)) {
        Ok(Some(e)) => e,
        Ok(None) => return rows,
        Err(ScanError(kind)) => {
            crate::trace::spm_gc_skipped(home, &format!("{kind:?}"));
            return rows;
        }
    };
    let mut candidates: Vec<(PathBuf, bool)> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            crate::trace::spm_gc_skipped(home, "read_dir");
            return rows;
        };
        // A symlink or file in spm/ is not ours to remove or follow.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let tmp = entry.file_name().to_string_lossy().starts_with(".tmp-");
        candidates.push((entry.path(), tmp));
    }
    candidates.sort();
    // Cheap checks first; the reference scan only when something is old enough.
    candidates.retain(|(dir, _)| newest_change(dir).is_some_and(|t| age(t, now) >= grace));
    if candidates.is_empty() {
        return rows;
    }
    let refs = match References::scan(home, &records) {
        Ok(r) => r,
        Err(ScanError(kind)) => {
            crate::trace::spm_gc_skipped(home, &format!("{kind:?}"));
            return rows;
        }
    };
    for (dir, tmp) in candidates {
        let name = dir.file_name().map(OsString::from).unwrap_or_default();
        if !tmp && refs.keys.contains(&name) {
            continue;
        }
        if let Some(row) = remove_candidate(home, &records, &dir, tmp, &name, dry_run) {
            rows.push((dir.display().to_string(), row));
        }
    }
    rows
}

/// Removes `dir` after the last checks; `None` when it is kept.
fn remove_candidate(
    home: &BaleHome,
    records: &[(String, anyhow::Result<Record>)],
    dir: &Path,
    tmp: bool,
    name: &OsString,
    dry_run: bool,
) -> Option<String> {
    match busy_holders(dir, &[]) {
        Ok(h) if !h.is_empty() => return None,
        Ok(_) => {}
        Err(e) => return Some(format!("error: {e:#}")),
    }
    let start = Instant::now();
    let bytes = crate::trace::size_tree(dir, &[], start + SIZING_LIMIT).tree;
    let size = crate::human_bytes(bytes);
    let why = if tmp {
        "left over from a clone"
    } else {
        "nothing links to it"
    };
    if dry_run {
        return Some(format!("would remove {size}: {why}"));
    }
    // A hook may have linked it while the checks ran.
    if !tmp {
        match References::scan(home, records) {
            Ok(r) if !r.keys.contains(name) => {}
            _ => return None,
        }
    }
    let result = remove_entry(dir);
    crate::trace::spm_gc(home, bytes, result.is_ok(), start.elapsed());
    Some(match result {
        Ok(()) => format!("removed {size}: {why}"),
        Err(e) => format!("error: removing {}: {e}", dir.display()),
    })
}

/// Removes the pre-made folders the records name that are still untouched
/// stubs (`is_stub`, via `remove_stub`) whose project is gone, once nothing
/// has changed in them for `grace`, and forgets them in the record.
fn drop_gone_stubs(
    home: &BaleHome,
    records: &[(String, anyhow::Result<Record>)],
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
    /// Every source of links; an error when any of them cannot be read.
    fn scan(home: &BaleHome, records: &[(String, anyhow::Result<Record>)]) -> Scan<References> {
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
        for (_, record) in records {
            // A record that can't be read may hold any reference.
            let Ok(record) = record else {
                return Err(ScanError(io::ErrorKind::InvalidData));
            };
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

    /// A tree's own top-level folders as DerivedData roots: `<tree>/.dd`,
    /// `<tree>/derived`, whatever its builds were pointed at.
    fn scan_tree(&mut self, tree: &Path) -> Scan<()> {
        self.scan_root(tree)?;
        let Some(entries) = or_missing(fs::read_dir(tree))? else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                self.scan_root(&entry.path())?;
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
