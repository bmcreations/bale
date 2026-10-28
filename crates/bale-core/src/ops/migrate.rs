use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::busy::busy_holders_many;
use crate::git::{self, WorktreeInfo, LOCK_REASON};
use crate::ops::up::{regenerable, register, up, Outcome};
use crate::registry::{canonical_or_ancestor, workspace_id, Level, Registry};
use crate::BaleHome;

pub const BALE: &str = "bale";
pub const REGISTER_ONLY_BUSY: &str = "register only (busy)";
pub const REPORT_PRUNABLE: &str = "report (prunable)";
pub const REPORT_TEMP: &str = "report (temp)";

/// One linked worktree and what `apply` would do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    /// The main checkout of the row's repo (the argument itself when it
    /// could not be listed); `apply` reads free space on the first row's.
    pub repo: PathBuf,
    pub path: PathBuf,
    pub total_bytes: u64,
    pub regenerable_bytes: u64,
    pub dirty: bool,
    pub busy: bool,
    pub temp_location: bool,
    pub prunable: bool,
    pub action: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrateReport {
    pub free_before: u64,
    pub free_after: u64,
    pub baled: usize,
    pub busy: usize,
    pub reported: usize,
    pub errors: Vec<(PathBuf, String)>,
}

fn error_action(e: &anyhow::Error) -> String {
    format!("report (error: {e:#})")
}

/// Lists the linked worktrees of each repo (a repo may be named by any of its
/// worktrees) and decides an action for each. Main checkouts are skipped and a
/// repo named twice is listed once. A repo or row that cannot be probed
/// becomes a `report (error: ...)` row; the rest still plan. Writes nothing.
pub fn plan(home: &BaleHome, repos: &[PathBuf]) -> Result<Vec<PlanRow>> {
    let registry = Registry::open(home.clone());
    let temp_roots = temp_roots();
    let mut seen_repos = HashSet::new();
    let mut rows = Vec::new();
    for repo in repos {
        let listed = git::common_dir(repo).and_then(|common| {
            let common = fs::canonicalize(&common).unwrap_or(common);
            if !seen_repos.insert(common) {
                return Ok(None);
            }
            git::worktrees(repo).map(Some)
        });
        match listed {
            Ok(None) => {}
            Ok(Some(list)) => {
                // git lists the main checkout first, even from a linked path.
                let main = list
                    .iter()
                    .find(|w| w.is_main)
                    .map_or_else(|| repo.clone(), |w| w.path.clone());
                rows.extend(
                    list.iter()
                        .filter(|w| !w.is_main)
                        .map(|w| plan_row(home, &registry, &temp_roots, &main, w)),
                );
            }
            Err(e) => {
                let mut row = empty_row(repo, repo);
                row.action = error_action(&e.context(format!("listing {}", repo.display())));
                rows.push(row);
            }
        }
    }
    probe_busy(&mut rows);
    Ok(rows)
}

fn empty_row(repo: &Path, path: &Path) -> PlanRow {
    PlanRow {
        repo: repo.to_path_buf(),
        path: path.to_path_buf(),
        total_bytes: 0,
        regenerable_bytes: 0,
        dirty: false,
        busy: false,
        temp_location: false,
        prunable: false,
        action: String::new(),
    }
}

/// Fills one row. The decision runs in `decide`; any error it hits becomes
/// the row's action instead of failing the plan.
fn plan_row(
    home: &BaleHome,
    registry: &Registry,
    temp_roots: &[PathBuf],
    repo: &Path,
    w: &WorktreeInfo,
) -> PlanRow {
    let mut row = empty_row(repo, &w.path);
    let canonical = canonical_or_ancestor(&w.path);
    row.temp_location = temp_roots.iter().any(|t| canonical.starts_with(t));
    row.prunable = w.prunable;
    row.action = match decide(home, registry, w, &canonical, &mut row) {
        Ok(action) => action,
        Err(e) => error_action(&e),
    };
    row
}

/// Decides in order: prunable, temp, registered non-hot, locked by someone
/// other than bale, missing dir, then sizes and dirtiness. A row that gets
/// through every check is `bale` until `probe_busy` settles it.
fn decide(
    home: &BaleHome,
    registry: &Registry,
    w: &WorktreeInfo,
    canonical: &Path,
    row: &mut PlanRow,
) -> Result<String> {
    if row.prunable {
        return Ok(REPORT_PRUNABLE.into());
    }
    if row.temp_location {
        return Ok(REPORT_TEMP.into());
    }
    if let Some(record) = registry.get(&workspace_id(&w.path))? {
        if record.level != Level::Hot {
            return Ok(format!("report ({})", record.level.name()));
        }
    }
    match w.locked.as_deref() {
        Some(LOCK_REASON) | None => {}
        Some("") => return Ok("report (locked)".into()),
        Some(reason) => return Ok(format!("report (locked: {reason})")),
    }
    if !w.path.is_dir() {
        // A locked worktree whose dir is gone is not prunable to git, but
        // there is nothing here to bale.
        row.prunable = true;
        return Ok(REPORT_PRUNABLE.into());
    }
    (row.total_bytes, row.regenerable_bytes) = sizes(home, canonical);
    row.dirty = git::is_dirty(&w.path).with_context(|| format!("checking {}", w.path.display()))?;
    Ok(BALE.into())
}

/// Marks `bale` rows busy from one scan of the process table. A row whose
/// path no longer resolves becomes an error row on its own; a failed scan
/// turns every remaining candidate into an error row.
fn probe_busy(rows: &mut [PlanRow]) {
    let mut candidates = Vec::new();
    let mut paths = Vec::new();
    for (i, row) in rows.iter_mut().enumerate() {
        if row.action != BALE {
            continue;
        }
        match fs::canonicalize(&row.path) {
            Ok(p) => {
                candidates.push(i);
                paths.push(p);
            }
            Err(e) => {
                let e = anyhow::Error::from(e)
                    .context(format!("canonicalizing {}", row.path.display()));
                row.action = error_action(&e);
            }
        }
    }
    match busy_holders_many(&paths) {
        Ok(lists) => {
            for (&i, holders) in candidates.iter().zip(lists) {
                if !holders.is_empty() {
                    rows[i].busy = true;
                    rows[i].action = REGISTER_ONLY_BUSY.into();
                }
            }
        }
        Err(e) => {
            let action = error_action(&e.context("probing for busy processes"));
            for &i in &candidates {
                rows[i].action = action.clone();
            }
        }
    }
}

/// Registers and bales each `bale` or `register only (busy)` row; `up`'s own
/// busy check decides at apply time. Report rows are counted and left alone.
/// A row's error, or failing to read free space afterwards, is recorded and
/// the report still returned. Never colds.
pub fn apply(home: &BaleHome, rows: &[PlanRow]) -> Result<MigrateReport> {
    let volume = rows.first().map(|r| existing_ancestor(&r.repo));
    let mut report = MigrateReport {
        free_before: free_space(volume.as_deref())?,
        ..MigrateReport::default()
    };
    for row in rows {
        if row.action != BALE && row.action != REGISTER_ONLY_BUSY {
            report.reported += 1;
            continue;
        }
        match foreign_lock(row) {
            Ok(true) => {
                report.reported += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                report.errors.push((row.path.clone(), format!("{e:#}")));
                continue;
            }
        }
        match register(home, &row.path).and_then(|rec| up(home, &rec.id)) {
            Ok(Outcome::Done(_)) => report.baled += 1,
            Ok(Outcome::Busy(_)) => report.busy += 1,
            Ok(Outcome::Skipped(_)) => report.reported += 1,
            Err(e) => report.errors.push((row.path.clone(), format!("{e:#}"))),
        }
    }
    match free_space(volume.as_deref()) {
        Ok(bytes) => report.free_after = bytes,
        Err(e) => report
            .errors
            .push((volume.unwrap_or_default(), format!("{e:#}"))),
    }
    Ok(report)
}

/// True when git now shows the row's worktree locked with a reason other
/// than bale's, for example a lock the user added after `plan`.
fn foreign_lock(row: &PlanRow) -> Result<bool> {
    let want = fs::canonicalize(&row.path).unwrap_or_else(|_| row.path.clone());
    let list = git::worktrees(&row.repo)
        .with_context(|| format!("re-reading locks in {}", row.repo.display()))?;
    Ok(list
        .iter()
        .find(|w| fs::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone()) == want)
        .and_then(|w| w.locked.as_deref())
        .is_some_and(|reason| reason != LOCK_REASON))
}

/// Free bytes on the volume holding `path`; 0 when there is no path.
fn free_space(path: Option<&Path>) -> Result<u64> {
    match path {
        Some(p) => fs4::available_space(p)
            .with_context(|| format!("reading free space on {}", p.display())),
        None => Ok(0),
    }
}

/// Total and regenerable file bytes under `root`, for display. Symlinks are
/// not followed and unreadable entries are skipped.
fn sizes(home: &BaleHome, root: &Path) -> (u64, u64) {
    let regen = regenerable(home, root).unwrap_or_default();
    let (mut total, mut regenerable_bytes) = (0, 0);
    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        total += meta.len();
        if regen.iter().any(|d| entry.path().starts_with(d)) {
            regenerable_bytes += meta.len();
        }
    }
    (total, regenerable_bytes)
}

fn temp_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for p in [
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
    ] {
        if let Ok(c) = fs::canonicalize(&p) {
            roots.push(c);
        }
        roots.push(p);
    }
    roots
}

/// `path` made absolute, then its nearest existing ancestor.
fn existing_ancestor(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(&absolute)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vanished_dir_fails_only_its_own_busy_probe() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kept = tmp.path().join("kept");
        let gone = tmp.path().join("gone");
        fs::create_dir_all(&kept).unwrap();
        let mut rows = vec![empty_row(tmp.path(), &kept), empty_row(tmp.path(), &gone)];
        for r in &mut rows {
            r.action = BALE.into();
        }
        probe_busy(&mut rows);
        assert_eq!(rows[0].action, BALE, "{rows:#?}");
        assert!(rows[1].action.starts_with("report (error: "), "{rows:#?}");
    }
}
