use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::busy::busy_holders;
use crate::git;
use crate::ops::up::{regenerable, register, up, Outcome};
use crate::registry::{workspace_id, Level, Registry};
use crate::BaleHome;

pub const BALE: &str = "bale";
pub const REGISTER_ONLY_BUSY: &str = "register only (busy)";
pub const REPORT_PRUNABLE: &str = "report (prunable)";
pub const REPORT_TEMP: &str = "report (temp)";

/// One linked worktree and what `apply` would do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
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

/// Lists the linked worktrees of each repo (a repo may be named by any of its
/// worktrees) and decides an action for each. Main checkouts are skipped and a
/// repo named twice is listed once. Writes nothing.
pub fn plan(home: &BaleHome, repos: &[PathBuf]) -> Result<Vec<PlanRow>> {
    let registry = Registry::open(home.clone());
    let temp_roots = temp_roots();
    let mut seen_repos = HashSet::new();
    let mut rows = Vec::new();
    for repo in repos {
        let common = git::common_dir(repo)?;
        if !seen_repos.insert(fs::canonicalize(&common).unwrap_or(common)) {
            continue;
        }
        for w in git::worktrees(repo)? {
            if w.is_main {
                continue;
            }
            rows.push(plan_row(home, &registry, &temp_roots, &w.path, w.prunable)?);
        }
    }
    Ok(rows)
}

/// Decides in order: prunable, temp, registered non-hot, missing dir, busy,
/// bale. Only the last two probe the tree.
fn plan_row(
    home: &BaleHome,
    registry: &Registry,
    temp_roots: &[PathBuf],
    path: &Path,
    prunable: bool,
) -> Result<PlanRow> {
    let mut row = PlanRow {
        path: path.to_path_buf(),
        total_bytes: 0,
        regenerable_bytes: 0,
        dirty: false,
        busy: false,
        temp_location: false,
        prunable,
        action: REPORT_PRUNABLE.into(),
    };
    if prunable {
        return Ok(row);
    }
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if temp_roots.iter().any(|t| canonical.starts_with(t)) {
        row.temp_location = true;
        row.action = REPORT_TEMP.into();
        return Ok(row);
    }
    if let Some(record) = registry.get(&workspace_id(path))? {
        if record.level != Level::Hot {
            row.action = format!("report ({})", level_name(record.level));
            return Ok(row);
        }
    }
    if !path.is_dir() {
        // A locked worktree whose dir is gone is not prunable to git, but
        // there is nothing here to bale.
        row.prunable = true;
        return Ok(row);
    }

    (row.total_bytes, row.regenerable_bytes) = sizes(home, &canonical);
    row.dirty = git::is_dirty(path).with_context(|| format!("checking {}", path.display()))?;
    row.busy = !busy_holders(path)
        .with_context(|| format!("probing {}", path.display()))?
        .is_empty();
    row.action = if row.busy { REGISTER_ONLY_BUSY } else { BALE }.into();
    Ok(row)
}

/// Registers and bales each `bale` or `register only (busy)` row; `up`'s own
/// busy check decides at apply time. Report rows are counted and left alone.
/// A row's error is recorded and the rest still run. Never colds.
pub fn apply(home: &BaleHome, rows: &[PlanRow]) -> Result<MigrateReport> {
    let volume = rows.first().map(|r| existing_ancestor(&r.path));
    let free = |v: &Option<PathBuf>| -> Result<u64> {
        match v {
            Some(p) => fs4::available_space(p)
                .with_context(|| format!("reading free space on {}", p.display())),
            None => Ok(0),
        }
    };
    let mut report = MigrateReport {
        free_before: free(&volume)?,
        ..MigrateReport::default()
    };
    for row in rows {
        if row.action != BALE && row.action != REGISTER_ONLY_BUSY {
            report.reported += 1;
            continue;
        }
        match register(home, &row.path).and_then(|rec| up(home, &rec.id)) {
            Ok(Outcome::Done(_)) => report.baled += 1,
            Ok(Outcome::Busy(_)) => report.busy += 1,
            Ok(Outcome::Skipped(_)) => report.reported += 1,
            Err(e) => report.errors.push((row.path.clone(), format!("{e:#}"))),
        }
    }
    report.free_after = free(&volume)?;
    Ok(report)
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

fn existing_ancestor(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|p| p.exists())
        .unwrap_or(path)
        .to_path_buf()
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Hot => "hot",
        Level::Baling => "baling",
        Level::Baled => "baled",
        Level::Cooling => "cooling",
        Level::Cold => "cold",
        Level::Opening => "opening",
    }
}
