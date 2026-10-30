//! `bale sweep`: apply the idle policy to every registered workspace. Each
//! record is handled on its own; a failure in one becomes that record's row.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Map;

use crate::config::{load_config, Config};
use crate::derived_data::{global_root, index, is_derived_data_root, workspace_key, DdDir};
use crate::detect::{detect, regenerable_dirs};
use crate::git;
use crate::idle::last_source_change;
use crate::ops::cold::cold_if_clean;
use crate::ops::stash::{stash, StashOutcome};
use crate::ops::up::{bale_able_from, register, up_report, Outcome};
use crate::par::par_map;
use crate::registry::{workspace_id, Level, Record, Registry};
use crate::trace::{put_clipped, BuildLog, Size, MAX_ID, MAX_PATH};
use crate::BaleHome;

/// Report text for a record `bale open` left half done; `sweep` puts the
/// record's id in place of `<id>` in the row's result.
const INTERRUPTED_OPEN: &str = "interrupted open: run `bale open <id>`";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Keep,
    Up,
    Cold,
    /// Truncate the large build files to stubs, keeping them in the store.
    Stash,
    /// Delete Xcode DerivedData whose project is gone. Its row is keyed by the folder.
    Reap,
    /// Register a worktree made outside bale's hooks. Its row is keyed by the new id.
    Adopt,
    Report(String),
}

/// The idle policy. `dirty` must be `true` whenever cleanliness is unknown.
/// `has_regen` says whether a `Baled` tree has regrown output `up` would
/// remove; it is ignored for other levels. `stashed` says the tree's files
/// are stubs now (a stash not yet restored); such a tree is not stashed
/// again, but still goes `Up` and `Cold` on schedule. `stash_ok` says a
/// restoring hook has run for the tree; without it `Stash` is never returned.
pub fn decide(
    level: Level,
    idle_for: Duration,
    dirty: bool,
    has_regen: bool,
    stashed: bool,
    stash_ok: bool,
    cfg: &Config,
) -> Action {
    if let Some(action) = level_action(level) {
        return action;
    }
    let up_due = match level {
        Level::Hot => true,
        Level::Baled => has_regen,
        _ => false,
    };
    if !dirty && idle_for >= cfg.idle_cold {
        Action::Cold
    } else if up_due && idle_for >= cfg.idle_baled {
        Action::Up
    } else if matches!(level, Level::Hot | Level::Baled)
        && stash_ok
        && !stashed
        && idle_for >= cfg.idle_stash
    {
        Action::Stash
    } else {
        Action::Keep
    }
}

/// Decides and (unless `dry_run`) applies the policy for every record,
/// returning one `(id, action, result)` row each; a record file that does
/// not parse becomes a `Report` row keyed by its file stem. Errs only when
/// the registry directory cannot be read.
///
/// Also appends the run's usage trace (see [`crate::trace`]): a `tree` line
/// per record, a `build` line per Xcode build log not seen before, and one
/// `sweep` line; then prunes old trace files. Sizes and build logs are read
/// before any tree is changed.
pub fn sweep(home: &BaleHome, dry_run: bool) -> Result<Vec<(String, Action, String)>> {
    sweep_with(home, dry_run, |_| Vec::new())
}

/// [`sweep`], first learning the Xcode DerivedData roots at most two levels
/// under each folder `build_dirs` lists (see
/// [`crate::derived_data::roots_in`]), so that reaping covers build folders
/// no hook saw made. `build_dirs` gets the deadline that also bounds the
/// walk, and should stop listing at it. Learning is bookkeeping and happens
/// on `dry_run` too, so its report lists the reaps a real sweep would make.
pub fn sweep_with(
    home: &BaleHome,
    dry_run: bool,
    build_dirs: impl Fn(Instant) -> Vec<PathBuf>,
) -> Result<Vec<(String, Action, String)>> {
    crate::trace::with_trigger("sweep", || sweep_traced(home, dry_run, &build_dirs))
}

fn sweep_traced(
    home: &BaleHome,
    dry_run: bool,
    build_dirs: &dyn Fn(Instant) -> Vec<PathBuf>,
) -> Result<Vec<(String, Action, String)>> {
    let start = Instant::now();
    let mut records = Registry::open(home.clone()).all_lenient()?;
    let (adopted, mains) = adopt_missed(home, &records, dry_run);
    if adopted.iter().any(|_| !dry_run) {
        records = Registry::open(home.clone()).all_lenient()?;
    }
    // Before any tree's `up` can remove the build output a shared folder is cloned from.
    #[cfg(target_os = "macos")]
    if !dry_run {
        seed_shared_packages(home, &records, mains);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (mains, build_dirs);
    // A tree removed before its first build leaves its pre-made folder behind.
    #[cfg(target_os = "macos")]
    if !dry_run {
        drop_stubs_of_missing_trees(home, &records);
    }
    // Xcode's shared DerivedData is listed once and each folder in it sized
    // once, however many trees it belongs to.
    let global = global_root().map(|g| index(&g)).unwrap_or_default();
    let roots: Vec<Option<PathBuf>> = records
        .iter()
        .map(|(_, r)| r.as_ref().ok().map(|r| workspace_key(&r.path)))
        .collect();
    let matches = match_trees(&global, &roots);
    // Trees `bale gc` would forget hold no DerivedData: their project is gone.
    let held: Vec<PathBuf> = records
        .iter()
        .filter_map(|(_, r)| r.as_ref().ok())
        .filter(|r| !super::gc::is_gone(r))
        .flat_map(|r| [workspace_key(&r.path), r.path.clone()])
        .collect();
    let mut measured = par_map(&records, |(_, record)| match record {
        Ok(r) => measure(home, r),
        Err(_) => Measured::default(),
    });
    let wanted: Vec<bool> = measured.iter().map(|m| m.wants_global).collect();
    let global_dd = size_global_dd(&global, &matches, &wanted, |dir, deadline| {
        crate::trace::size_tree(dir, &[], deadline)
    });
    // Build logs in the shared folders, from the same listing, before any
    // tree's `up` removes one.
    for ((m, mine), dd) in measured.iter_mut().zip(&matches).zip(&global_dd.per_tree) {
        m.global_dd = *dd;
        if m.wants_global {
            for &i in mine {
                m.builds.extend(logs_of(&global[i].dir));
            }
        }
    }
    // The registered set does not change during a sweep (`up` and `cold` only move levels).
    let trees = crate::derived_data::Trees::registered(home);
    let mut rows = adopted;
    rows.reserve(records.len());
    let mut builds = Vec::new();
    let mut paths = Vec::new();
    for ((stem, record), m) in records.into_iter().zip(measured) {
        match record {
            Ok(record) => {
                let (action, result, edited) = sweep_one(home, &trees, &record, dry_run);
                trace_tree(home, &record, &action, &result, edited, &m);
                builds.extend(m.builds.into_iter().map(|b| (record.id.clone(), b)));
                paths.push(record.path.clone());
                rows.push((record.id, action, result));
            }
            Err(e) => {
                // The row is keyed by the file stem, so the cause alone suffices.
                let text = format!("corrupt record: {}", e.root_cause());
                rows.push((stem, Action::Report(text.clone()), text));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        learn_build_dirs(home, build_dirs);
        rows.extend(reap_rows(home, &global, &held, dry_run));
        rows.extend(spm_gc_rows(home, dry_run));
    }
    let now = SystemTime::now();
    crate::trace::record_builds(home, &builds, now);
    let mut fields = Map::new();
    if dry_run {
        fields.insert("dry_run".into(), true.into());
    }
    fields.insert("volumes".into(), crate::trace::volumes(home, &paths).into());
    fields.insert("global_dd_total_bytes".into(), global_dd.total.into());
    fields.insert(
        "global_dd_unattributed_bytes".into(),
        global_dd.unattributed.into(),
    );
    if global_dd.truncated {
        fields.insert("global_dd_truncated".into(), true.into());
    }
    if global_dd.unattributed_truncated {
        fields.insert("global_dd_unattributed_truncated".into(), true.into());
    }
    fields.insert("duration_ms".into(), millis(start.elapsed()).into());
    crate::trace::emit(home, "sweep", fields);
    crate::trace::prune(home, now);
    Ok(rows)
}

/// Listing one repository's worktrees longer than this is given up on, and
/// so is adopting once the whole pass has run this long.
const ADOPT_LIMIT: Duration = Duration::from_secs(20);

/// Registers the worktrees of the repositories behind `records` that exist
/// on disk but have no record, one row each (`would adopt` on `dry_run`,
/// when nothing is registered). `git worktree list` runs once per repo, on
/// up to [`crate::par::THREADS`] threads, and a repo whose listing fails or
/// misses [`ADOPT_LIMIT`] is skipped. The second value is the main checkout of
/// each repo that listed. Registering is `up::register`, so its
/// refusals (the main checkout, a gitfile that is not a linked worktree's)
/// apply, and the record starts without `restore_hook`: sweep never stashes
/// an adopted tree until a hook has restored it once.
fn adopt_missed(
    home: &BaleHome,
    records: &[(String, Result<Record>)],
    dry_run: bool,
) -> (Vec<(String, Action, String)>, Vec<PathBuf>) {
    let mut repos: Vec<PathBuf> = Vec::new();
    for record in records.iter().filter_map(|(_, r)| r.as_ref().ok()) {
        if !repos.contains(&record.repo_common_dir) {
            repos.push(record.repo_common_dir.clone());
        }
    }
    let deadline = Instant::now() + ADOPT_LIMIT;
    let listed = list_worktrees(&repos, deadline);
    let registry = Registry::open(home.clone());
    let mut rows = Vec::new();
    let mut mains = Vec::new();
    for info in listed.iter().flatten().flatten() {
        if info.is_main && !info.bare && !info.prunable {
            mains.extend(crate::paths::canonicalize(&info.path).ok());
        }
        if info.is_main || info.bare || info.prunable {
            continue;
        }
        let Ok(path) = crate::paths::canonicalize(&info.path) else {
            continue;
        };
        let id = workspace_id(&path);
        if !matches!(registry.get(&id), Ok(None)) {
            continue;
        }
        if dry_run {
            rows.push((id, Action::Adopt, "would adopt".to_string()));
        } else if Instant::now() < deadline {
            // A refusal is not news: the same worktree is refused every sweep.
            if let Ok(record) = register(home, &path) {
                rows.push((record.id, Action::Adopt, "adopted".to_string()));
            }
        }
    }
    (rows, mains)
}

/// Listing and walking `build_dirs` for DerivedData roots stops after this long.
#[cfg(target_os = "macos")]
const BUILD_DIRS_LIMIT: Duration = Duration::from_secs(10);

/// Learns the DerivedData roots in the folders `build_dirs` lists, giving
/// up on both after [`BUILD_DIRS_LIMIT`]. A root that fails to be written
/// is found again by the next sweep.
#[cfg(target_os = "macos")]
fn learn_build_dirs(home: &BaleHome, build_dirs: &dyn Fn(Instant) -> Vec<PathBuf>) {
    let deadline = Instant::now() + BUILD_DIRS_LIMIT;
    for root in crate::derived_data::roots_in(&build_dirs(deadline), deadline) {
        let _ = crate::derived_data::learn_root(home, &root);
    }
}

/// For each record whose checkout is gone, removes the pre-made DerivedData
/// folders it still names if they were never built in.
#[cfg(target_os = "macos")]
fn drop_stubs_of_missing_trees(home: &BaleHome, records: &[(String, Result<Record>)]) {
    let registry = Registry::open(home.clone());
    for record in records.iter().filter_map(|(_, r)| r.as_ref().ok()) {
        let path = record.path.clone();
        let gone = fs::symlink_metadata(&record.path)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        if !gone || record.spm.is_empty() {
            continue;
        }
        let Ok(Some(_lock)) = crate::lock::WorkspaceLock::try_acquire(home, &record.id) else {
            continue;
        };
        // The snapshot may be stale by now; act on what is recorded under the lock.
        let Ok(Some(mut record)) = registry.get(&record.id) else {
            continue;
        };
        if record.path != path || record.spm.is_empty() {
            continue;
        }
        if crate::spm_link::drop_stubs(&mut record) {
            let _ = registry.put(&record);
        }
    }
}

/// Seeds the shared Swift packages folder for every registered tree and every
/// main checkout in `mains` whose own `SourcePackages` is finished and whose
/// key has no shared folder yet. The trees keep their folders.
#[cfg(target_os = "macos")]
fn seed_shared_packages(
    home: &BaleHome,
    records: &[(String, Result<Record>)],
    mut trees: Vec<PathBuf>,
) {
    let Some(dd_root) = global_root() else { return };
    trees.extend(
        records
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .filter(|r| !matches!(r.level, Level::Cold | Level::Cooling | Level::Opening))
            .map(|r| r.path.clone()),
    );
    crate::spm_link::seed_from_trees(home, &trees, &dd_root);
}

/// `git worktree list` for each repo in `repos`, in order; `None` for a repo
/// whose listing failed or was not done by `deadline`.
fn list_worktrees(repos: &[PathBuf], deadline: Instant) -> Vec<Option<Vec<git::WorktreeInfo>>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    let repos = Arc::new(repos.to_vec());
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    for _ in 0..crate::par::THREADS.min(repos.len()) {
        let (repos, next, tx) = (repos.clone(), next.clone(), tx.clone());
        // Detached: a git that hangs past the deadline is left behind.
        std::thread::spawn(move || loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(repo) = repos.get(i) else { break };
            if tx.send((i, git::worktrees(repo).ok())).is_err() {
                break;
            }
        });
    }
    drop(tx);
    let mut out = vec![None; repos.len()];
    while let Ok((i, list)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        out[i] = list;
    }
    out
}

/// The rows for DerivedData reaped (or that `dry_run` would reap); none when
/// the config cannot be read.
#[cfg(target_os = "macos")]
fn reap_rows(
    home: &BaleHome,
    global: &[DdDir],
    trees: &[PathBuf],
    dry_run: bool,
) -> Vec<(String, Action, String)> {
    let cfg = match load_config(home, None) {
        Ok(cfg) => cfg,
        Err(e) => {
            crate::trace::warn(&e);
            return Vec::new();
        }
    };
    super::reap::reap_orphans(home, global, trees, cfg.reap_grace, dry_run)
        .into_iter()
        .map(|(dir, result)| (dir, Action::Reap, result))
        .collect()
}

/// The rows for shared Swift packages folders removed (or that `dry_run`
/// would remove). Runs whether or not `share_swift_packages` is on.
#[cfg(target_os = "macos")]
fn spm_gc_rows(home: &BaleHome, dry_run: bool) -> Vec<(String, Action, String)> {
    let cfg = match load_config(home, None) {
        Ok(cfg) => cfg,
        Err(e) => {
            crate::trace::warn(&e);
            return Vec::new();
        }
    };
    super::spm_gc::collect(home, cfg.reap_grace, dry_run)
        .into_iter()
        .map(|(dir, result)| (dir, Action::Reap, result))
        .collect()
}

/// Sizing longer than this for one tree's checkout is cut short; the bytes
/// counted so far are recorded with `sizing_truncated`.
const SIZING_LIMIT: Duration = Duration::from_secs(30);

/// Walking one folder in Xcode's shared DerivedData longer than this is cut
/// short; the bytes counted so far are recorded with `global_dd_truncated`.
/// Folders no tree claims share one such budget, from the start of the
/// shared DerivedData pass.
const GLOBAL_DD_LIMIT: Duration = Duration::from_secs(120);

/// What sweep reads from a tree before changing anything, for its trace.
#[derive(Default)]
struct Measured {
    /// `(regen_bytes, tree_bytes)`, partial when `truncated`; `None` for
    /// trees not on disk or after an error.
    sizes: Option<(u64, u64)>,
    truncated: bool,
    /// The tree is on disk and `Hot` or `Baled`, so its share of Xcode's
    /// shared DerivedData is sized too.
    wants_global: bool,
    /// Bytes of the folders in Xcode's shared DerivedData built from the
    /// tree, which `up` also removes on macOS; `None` when not sized.
    global_dd: Option<DdBytes>,
    sizing_ms: Option<u64>,
    builds: Vec<BuildLog>,
}

/// Sizes a `Hot` or `Baled` checkout and reads the Xcode build logs inside
/// it. Shared DerivedData is left to [`size_global_dd`]. Errors warn once
/// (via the trace) and leave the fields empty.
fn measure(home: &BaleHome, record: &Record) -> Measured {
    let mut m = Measured::default();
    if !matches!(record.level, Level::Hot | Level::Baled) {
        return m;
    }
    let start = Instant::now();
    let root = match crate::paths::canonicalize(&record.path) {
        Ok(root) => root,
        Err(_) => return m,
    };
    m.wants_global = true;
    let regen =
        load_config(home, Some(&root)).map(|cfg| regenerable_dirs(&root, &detect(&root), &cfg));
    match regen {
        Ok(regen) => {
            match bale_able_from(&root, regen.clone()) {
                Ok((removable, _)) => {
                    let size = crate::trace::size_tree(&root, &removable, start + SIZING_LIMIT);
                    m.sizes = Some((size.in_regen, size.tree));
                    m.truncated = size.truncated;
                    m.sizing_ms = Some(millis(start.elapsed()));
                }
                Err(e) => crate::trace::warn(&e),
            }
            for dd in regen.iter().filter(|d| is_derived_data_root(d)) {
                m.builds.extend(logs_of(dd));
            }
        }
        Err(e) => crate::trace::warn(&e),
    }
    m
}

/// The build logs of one Xcode DerivedData root; errors warn and give none.
fn logs_of(dd: &Path) -> Vec<BuildLog> {
    crate::trace::build_logs(dd).unwrap_or_else(|e| {
        crate::trace::warn(&e);
        Vec::new()
    })
}

/// For each tree root (as [`workspace_key`]; `None` for a record that did
/// not parse), the positions in `index` of the shared DerivedData folders
/// it owns (see [`DdDir::owned_by`]), as `up` and `stash` see them.
fn match_trees(index: &[DdDir], roots: &[Option<PathBuf>]) -> Vec<Vec<usize>> {
    let trees = crate::derived_data::Trees::from_keys(roots);
    roots
        .iter()
        .map(|root| match root {
            Some(root) => (0..index.len())
                .filter(|&i| index[i].owned_by(root, &trees))
                .collect(),
            None => Vec::new(),
        })
        .collect()
}

/// A tree's share of Xcode's shared DerivedData.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DdBytes {
    bytes: u64,
    /// One of its folders was cut short; `bytes` is what was counted.
    truncated: bool,
}

/// Xcode's shared DerivedData as sweep saw it.
struct GlobalDd {
    /// Per tree, `Some` where `wanted`.
    per_tree: Vec<Option<DdBytes>>,
    /// Every folder, each counted once.
    total: u64,
    /// Folders built from no registered tree, or with no `WorkspacePath`.
    unattributed: u64,
    /// Some folder was cut short.
    truncated: bool,
    /// Some unattributed folder was cut short or never started.
    unattributed_truncated: bool,
}

/// Sizes each folder in `index` exactly once with `size(dir, deadline)` on
/// up to [`crate::par::THREADS`] threads, then attributes the bytes. A
/// folder that `matches` puts under several trees counts in full for each
/// of them, as either tree's `up` would remove it, and once in the totals.
/// Folders some tree claims go first, each with [`GLOBAL_DD_LIMIT`] from
/// its own start; the rest share one [`GLOBAL_DD_LIMIT`] from the start.
fn size_global_dd(
    index: &[DdDir],
    matches: &[Vec<usize>],
    wanted: &[bool],
    size: impl Fn(&Path, Instant) -> Size + Sync,
) -> GlobalDd {
    let start = Instant::now();
    let mut claimed = vec![false; index.len()];
    for &i in matches.iter().flatten() {
        claimed[i] = true;
    }
    let mut order: Vec<usize> = (0..index.len()).filter(|&i| claimed[i]).collect();
    order.extend((0..index.len()).filter(|&i| !claimed[i]));
    let sized = par_map(&order, |&i| {
        if claimed[i] {
            size(&index[i].dir, Instant::now() + GLOBAL_DD_LIMIT)
        } else if start.elapsed() >= GLOBAL_DD_LIMIT {
            Size {
                truncated: true,
                ..Size::default()
            }
        } else {
            size(&index[i].dir, start + GLOBAL_DD_LIMIT)
        }
    });
    let mut sizes = vec![Size::default(); index.len()];
    for (i, s) in order.into_iter().zip(sized) {
        sizes[i] = s;
    }
    let mut out = GlobalDd {
        per_tree: Vec::with_capacity(matches.len()),
        total: 0,
        unattributed: 0,
        truncated: false,
        unattributed_truncated: false,
    };
    for (i, s) in sizes.iter().enumerate() {
        out.total += s.tree;
        out.truncated |= s.truncated;
        if !claimed[i] {
            out.unattributed += s.tree;
            out.unattributed_truncated |= s.truncated;
        }
    }
    for (mine, &want) in matches.iter().zip(wanted) {
        out.per_tree.push(want.then(|| DdBytes {
            bytes: mine.iter().map(|&i| sizes[i].tree).sum(),
            truncated: mine.iter().any(|&i| sizes[i].truncated),
        }));
    }
    out
}

/// Appends the record's `tree` line.
fn trace_tree(
    home: &BaleHome,
    record: &Record,
    action: &Action,
    result: &str,
    edited: Option<SystemTime>,
    m: &Measured,
) {
    let (name, reason) = match action {
        Action::Keep if record.pinned => ("skip", Some("pinned".to_string())),
        Action::Keep => ("keep", None),
        Action::Up => ("up", None),
        Action::Cold => ("cold", None),
        Action::Stash => ("stash", result.strip_prefix("skipped: ").map(skip_reason)),
        // Probe errors name paths inside the tree; the trace keeps none.
        Action::Report(text) if text == "missing" || text.starts_with("interrupted") => {
            ("report", Some(text.replace("<id>", &record.id)))
        }
        Action::Report(_) => ("report", Some("probe failed".to_string())),
        Action::Reap | Action::Adopt => unreachable!("reap and adopt rows are not trees"),
    };
    let mut f = Map::new();
    f.insert("id".into(), record.id.clone().into());
    let path = record.path.to_string_lossy();
    put_clipped(&mut f, "path", Some(&path), MAX_PATH);
    let repo = record.repo_common_dir.to_string_lossy();
    put_clipped(&mut f, "repo", Some(&repo), MAX_PATH);
    f.insert("level".into(), record.level.name().into());
    f.insert("pinned".into(), record.pinned.into());
    let origin = record.origin.as_ref();
    put_clipped(
        &mut f,
        "origin_tool",
        origin.map(|o| o.tool.as_str()),
        MAX_ID,
    );
    put_clipped(
        &mut f,
        "session_id",
        origin.map(|o| o.session.as_str()),
        MAX_ID,
    );
    f.insert(
        "last_source_edit".into(),
        edited.map(crate::trace::rfc3339).into(),
    );
    f.insert("regen_bytes".into(), m.sizes.map(|s| s.0).into());
    f.insert("tree_bytes".into(), m.sizes.map(|s| s.1).into());
    f.insert(
        "global_dd_bytes".into(),
        m.global_dd.map(|d| d.bytes).into(),
    );
    if m.global_dd.is_some_and(|d| d.truncated) {
        f.insert("global_dd_truncated".into(), true.into());
    }
    if m.truncated {
        f.insert("sizing_truncated".into(), true.into());
    }
    f.insert("sizing_ms".into(), m.sizing_ms.into());
    f.insert("action".into(), name.into());
    if let Some(reason) = reason {
        f.insert("reason".into(), crate::trace::clip(&reason).into());
    }
    crate::trace::emit(home, "tree", f);
}

/// The category of a stash skip, without the process names or paths its
/// text can carry: `busy: node (12)` gives `busy`.
fn skip_reason(text: &str) -> String {
    if text.contains("is not the worktree's checkout") {
        return "not the worktree's checkout".into();
    }
    text.split(':').next().unwrap_or(text).trim().to_string()
}

fn millis(d: Duration) -> u64 {
    d.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Decides and applies the policy for one record: `(action, result, last
/// source edit)`, the last as sweep's idle logic saw it (`None` when it was
/// not probed or no file remains).
fn sweep_one(
    home: &BaleHome,
    trees: &crate::derived_data::Trees,
    record: &Record,
    dry_run: bool,
) -> (Action, String, Option<SystemTime>) {
    // A pinned tree is left to its owner, even mid-transition. An
    // interrupted transition still gets a hint, since that changes nothing:
    // open's own, or for up and cold the command that finishes it.
    if record.pinned {
        let stranded = match record.level {
            Level::Opening => None,
            Level::Baling => Some(("baling", "up")),
            Level::Cooling => Some(("going cold", "cold")),
            _ => return (Action::Keep, "pinned".into(), None),
        };
        if let Some((doing, cmd)) = stranded {
            let text = format!(
                "interrupted while {doing}; pinned, so sweep leaves it: run `bale {cmd} {}`",
                record.id
            );
            return (Action::Report(text.clone()), text, None);
        }
    }
    let (action, edited) = match plan(home, trees, record) {
        Ok(planned) => planned,
        Err(e) => {
            let text = format!("{e:#}");
            return (Action::Report(text.clone()), text, None);
        }
    };
    let result = match &action {
        Action::Report(text) => text.replace("<id>", &record.id),
        Action::Keep => "nothing to do".into(),
        _ if dry_run => "dry run, not applied".into(),
        Action::Up => match up_report(home, &record.id) {
            Ok(r) if !r.kept.is_empty() => {
                let kept: Vec<String> = r.kept.iter().map(|d| d.display().to_string()).collect();
                format!(
                    "{}; kept (holds files git tracks or does not ignore, or is in a submodule or nested repo): {}",
                    outcome_text(Ok(r.outcome)),
                    kept.join(", ")
                )
            }
            other => outcome_text(other.map(|r| r.outcome)),
        },
        Action::Cold => outcome_text(cold_if_clean(home, &record.id)),
        Action::Stash => stash_text(stash(home, &record.id, &[])),
        Action::Reap | Action::Adopt => unreachable!("reap and adopt rows are not trees"),
    };
    (action, result, edited)
}

/// The action fixed by the level alone, without probes or config.
fn level_action(level: Level) -> Option<Action> {
    match level {
        Level::Cold => Some(Action::Keep),
        Level::Opening => Some(Action::Report(INTERRUPTED_OPEN.into())),
        Level::Baling => Some(Action::Up),
        Level::Cooling => Some(Action::Cold),
        Level::Hot | Level::Baled => None,
    }
}

/// Probes the workspace and decides. Probe failures are errors, so they
/// become `Report` rows and can never lead to `Cold`. Level-only actions
/// load no config, so a broken config cannot block a resume or the
/// interrupted-open hint.
fn plan(
    home: &BaleHome,
    trees: &crate::derived_data::Trees,
    record: &Record,
) -> Result<(Action, Option<SystemTime>)> {
    // Cooling may legitimately have no checkout; cold's recovery handles it.
    if matches!(record.level, Level::Cold | Level::Opening | Level::Cooling) {
        return Ok((level_action(record.level).expect("level-only action"), None));
    }
    if fs::symlink_metadata(&record.path).is_err() {
        return Ok((Action::Report("missing".into()), None));
    }
    if let Some(action) = level_action(record.level) {
        return Ok((action, None));
    }
    let root = crate::paths::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    let cfg = load_config(home, Some(&root))?;
    let regen = regenerable_dirs(&root, &detect(&root), &cfg);
    let changed = last_source_change(&root, &regen)?;
    let idle_for = idle_between(SystemTime::now(), changed, record.last_activity);
    // Only a tree past the cold threshold needs the (costly) dirty probe;
    // below it, dirty = true changes nothing.
    let dirty = idle_for < cfg.idle_cold || git::is_dirty(&root)?;
    // Only a Baled tree past the baled threshold needs to know whether its
    // output regrew; the same filter as `up` keeps sweep from re-running up
    // on a tree whose only regenerable-named dirs hold real content. Xcode
    // builds into its shared DerivedData and learned `-derivedDataPath`
    // roots, which `up` also removes on macOS.
    let has_regen = record.level == Level::Baled
        && idle_for >= cfg.idle_baled
        && (has_shared_derived_data(home, trees, &root)
            || !bale_able_from(&root, regen)?.0.is_empty());
    let edited = (changed > UNIX_EPOCH).then_some(changed);
    Ok((
        decide(
            record.level,
            idle_for,
            dirty,
            has_regen,
            record.stash.as_ref().is_some_and(|s| !s.restored),
            record.restore_hook,
            &cfg,
        ),
        edited,
    ))
}

/// Whether Xcode's shared DerivedData, or a learned `-derivedDataPath`
/// root, holds a folder built from `root`. Only macOS, the one place `up`
/// removes those folders; elsewhere a `true` would re-run `up` on every sweep.
#[cfg(target_os = "macos")]
fn has_shared_derived_data(
    home: &BaleHome,
    trees: &crate::derived_data::Trees,
    root: &Path,
) -> bool {
    !crate::derived_data::global_owned(root, trees).is_empty()
        || !crate::derived_data::learned_for(home, root, trees).is_empty()
}

#[cfg(not(target_os = "macos"))]
fn has_shared_derived_data(
    _home: &BaleHome,
    _trees: &crate::derived_data::Trees,
    _root: &Path,
) -> bool {
    false
}

/// `now` minus the later of the two times; zero when that lies in the future.
fn idle_between(now: SystemTime, last_change: SystemTime, last_activity: SystemTime) -> Duration {
    now.duration_since(last_change.max(last_activity))
        .unwrap_or(Duration::ZERO)
}

/// A stash that skips (busy, a prompt waiting, nothing to do) is not an
/// error: the next sweep tries again.
fn stash_text(outcome: Result<StashOutcome>) -> String {
    match outcome {
        Ok(StashOutcome::Stashed(r)) => format!(
            "stashed {} files, {} bytes -> {} bytes",
            r.files, r.bytes, r.stored_bytes
        ),
        Ok(StashOutcome::Skipped(reason)) => format!("skipped: {reason}"),
        Err(e) => format!("error: {e:#}"),
    }
}

fn outcome_text(outcome: Result<Outcome>) -> String {
    match outcome {
        Ok(Outcome::Done(level)) => format!("{level:?}").to_lowercase(),
        Ok(Outcome::Busy(holders)) => {
            let who: Vec<String> = holders
                .iter()
                .map(|h| format!("pid {} {}", h.pid, h.command))
                .collect();
            format!("busy: {}", who.join(", "))
        }
        Ok(Outcome::Skipped(reason)) => format!("skipped: {reason}"),
        Err(e) => format!("error: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            idle_baled: Duration::from_secs(4 * 3600),
            idle_cold: Duration::from_secs(7 * 86400),
            idle_stash: Duration::from_secs(600),
            reap_grace: Duration::from_secs(3600),
            share_swift_packages: true,
            stash_min_bytes: 1 << 20,
            patch_max_depth: 4,
            regenerable_extra: Vec::new(),
            ignored_denylist_extra: Vec::new(),
            worktree_root: None,
        }
    }

    fn h(n: u64) -> Duration {
        Duration::from_secs(n * 3600)
    }

    fn d(n: u64) -> Duration {
        Duration::from_secs(n * 86400)
    }

    /// A shared DerivedData dir under `dd_root` named `name`, built from the
    /// project at `workspace` (or with no plist when `None`).
    fn dd_dir(dd_root: &Path, name: &str, workspace: Option<&Path>) -> PathBuf {
        let dir = dd_root.join(name);
        fs::create_dir_all(dir.join("Build")).unwrap();
        if let Some(ws) = workspace {
            fs::create_dir_all(ws).unwrap();
            let ws = ws.to_string_lossy().replace('&', "&amp;");
            fs::write(
                dir.join("info.plist"),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
                     <key>WorkspacePath</key><string>{ws}</string></dict></plist>\n"
                ),
            )
            .unwrap();
        }
        dir
    }

    /// Global DerivedData under a temp dir: `shared` built from a project
    /// in `outer/sub`, `stray` from a project in no tree, and `cache` with
    /// no plist. Trees are registered at `outer` and `outer/sub`.
    fn nested_trees() -> (tempfile::TempDir, Vec<DdDir>, Vec<Option<PathBuf>>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = crate::paths::canonicalize(tmp.path()).unwrap();
        let dd_root = base.join("DerivedData");
        let outer = base.join("outer");
        let inner = outer.join("sub");
        dd_dir(&dd_root, "shared", Some(&inner.join("App.xcodeproj")));
        dd_dir(
            &dd_root,
            "stray",
            Some(&base.join("elsewhere/App.xcodeproj")),
        );
        dd_dir(&dd_root, "cache", None);
        let index = crate::derived_data::index(&dd_root);
        (tmp, index, vec![Some(outer), Some(inner)])
    }

    fn fake_size(path: &Path) -> Size {
        let name = path.file_name().unwrap().to_string_lossy();
        match name.as_ref() {
            "shared" => Size {
                tree: 100,
                in_regen: 0,
                truncated: false,
            },
            "stray" => Size {
                tree: 10,
                in_regen: 0,
                truncated: true,
            },
            _ => Size {
                tree: 1,
                in_regen: 0,
                truncated: false,
            },
        }
    }

    #[test]
    fn a_global_dd_dir_counts_for_the_deepest_tree_only_and_is_walked_once() {
        let (_tmp, index, roots) = nested_trees();
        assert_eq!(index.len(), 3);
        let matches = match_trees(&index, &roots);
        assert!(matches[0].is_empty(), "{matches:?}");
        assert_eq!(matches[1].len(), 1, "{matches:?}");

        let walks = std::sync::Mutex::new(Vec::new());
        let global = size_global_dd(&index, &matches, &[true, true], |dir, _| {
            walks.lock().unwrap().push(dir.to_path_buf());
            fake_size(dir)
        });
        let mut walks = walks.into_inner().unwrap();
        walks.sort();
        let before = walks.len();
        walks.dedup();
        assert_eq!(walks.len(), before, "a dir was walked twice: {walks:?}");
        assert_eq!(walks.len(), 3, "{walks:?}");

        let shared = Some(DdBytes {
            bytes: 100,
            truncated: false,
        });
        let none = Some(DdBytes {
            bytes: 0,
            truncated: false,
        });
        assert_eq!(global.per_tree, vec![none, shared]);
    }

    #[test]
    fn an_unparsed_record_leaves_dd_of_a_nested_checkout_unmatched() {
        let (tmp, index, roots) = nested_trees();
        let outer = roots[0].clone();
        // The inner tree's record did not parse.
        let roots = vec![outer.clone(), None];
        assert_eq!(match_trees(&index, &roots)[0].len(), 1);
        // Once `outer/sub` has a `.git` file, it is taken for that tree.
        let base = crate::paths::canonicalize(tmp.path()).unwrap();
        fs::write(base.join("outer/sub/.git"), "gitdir: x\n").unwrap();
        assert!(match_trees(&index, &roots)[0].is_empty());
    }

    #[test]
    fn unattributed_global_dd_is_totalled_with_partial_counts() {
        let (_tmp, index, roots) = nested_trees();
        let matches = match_trees(&index, &roots);
        // Only the inner tree is measured; the outer one still counts as
        // registered, so `shared` is attributed.
        let global = size_global_dd(&index, &matches, &[false, true], |dir, _| fake_size(dir));
        assert_eq!(
            global.per_tree,
            vec![
                None,
                Some(DdBytes {
                    bytes: 100,
                    truncated: false
                })
            ]
        );
        assert_eq!(global.total, 111);
        // `stray` was cut short at 10 bytes: counted, not dropped to 0.
        assert_eq!(global.unattributed, 11);
        assert!(global.unattributed_truncated);
        assert!(global.truncated);
    }

    #[test]
    fn hot_idle_5h_goes_up() {
        assert_eq!(
            decide(Level::Hot, h(5), false, true, true, true, &cfg()),
            Action::Up
        );
    }

    #[test]
    fn hot_idle_3h_keeps() {
        assert_eq!(
            decide(Level::Hot, h(3), false, true, true, true, &cfg()),
            Action::Keep
        );
    }

    #[test]
    fn hot_at_threshold_goes_up() {
        assert_eq!(
            decide(Level::Hot, h(4), false, true, true, true, &cfg()),
            Action::Up
        );
    }

    #[test]
    fn baled_idle_with_regrown_output_goes_up() {
        assert_eq!(
            decide(Level::Baled, h(5), false, true, true, true, &cfg()),
            Action::Up
        );
        // up does not touch source, so a dirty tree qualifies too.
        assert_eq!(
            decide(Level::Baled, h(5), true, true, true, true, &cfg()),
            Action::Up
        );
    }

    #[test]
    fn baled_idle_without_regenerable_output_keeps() {
        assert_eq!(
            decide(Level::Baled, h(5), false, false, true, true, &cfg()),
            Action::Keep
        );
        assert_eq!(
            decide(Level::Baled, d(8), true, false, true, true, &cfg()),
            Action::Keep
        );
    }

    #[test]
    fn baled_not_idle_keeps() {
        assert_eq!(
            decide(Level::Baled, h(3), false, true, true, true, &cfg()),
            Action::Keep
        );
        assert_eq!(
            decide(Level::Baled, h(3), true, true, true, true, &cfg()),
            Action::Keep
        );
    }

    #[test]
    fn baled_idle_past_cold_threshold_and_clean_goes_cold() {
        assert_eq!(
            decide(Level::Baled, d(8), false, true, true, true, &cfg()),
            Action::Cold
        );
        assert_eq!(
            decide(Level::Baled, d(8), false, false, true, true, &cfg()),
            Action::Cold
        );
    }

    #[test]
    fn clean_8d_goes_cold() {
        assert_eq!(
            decide(Level::Baled, d(8), false, true, true, true, &cfg()),
            Action::Cold
        );
        assert_eq!(
            decide(Level::Hot, d(8), false, true, true, true, &cfg()),
            Action::Cold
        );
    }

    #[test]
    fn dirty_8d_never_cold() {
        assert_ne!(
            decide(Level::Baled, d(8), true, true, true, true, &cfg()),
            Action::Cold
        );
        assert_eq!(
            decide(Level::Baled, d(8), true, true, true, true, &cfg()),
            Action::Up
        );
        assert_eq!(
            decide(Level::Baled, d(8), true, false, true, true, &cfg()),
            Action::Keep
        );
        assert_eq!(
            decide(Level::Hot, d(8), true, true, true, true, &cfg()),
            Action::Up
        );
    }

    #[test]
    fn interrupted_resumes() {
        assert_eq!(
            decide(Level::Baling, h(0), false, true, true, true, &cfg()),
            Action::Up
        );
    }

    #[test]
    fn cooling_resumes_cold_even_when_dirty() {
        assert_eq!(
            decide(Level::Cooling, h(0), true, true, true, true, &cfg()),
            Action::Cold
        );
    }

    #[test]
    fn opening_is_reported_not_restored() {
        assert_eq!(
            decide(Level::Opening, d(30), false, true, true, true, &cfg()),
            Action::Report("interrupted open: run `bale open <id>`".into())
        );
    }

    #[test]
    fn cold_keeps() {
        assert_eq!(
            decide(Level::Cold, d(30), false, true, true, true, &cfg()),
            Action::Keep
        );
    }

    #[test]
    fn cold_threshold_below_baled_threshold_still_colds() {
        let c = Config {
            idle_baled: d(2),
            idle_cold: d(1),
            ..cfg()
        };
        assert_eq!(
            decide(Level::Hot, h(30), false, true, true, true, &c),
            Action::Cold
        );
    }

    #[test]
    fn idle_between_saturates_for_future_activity() {
        let now = SystemTime::now();
        let past = now - h(10);
        assert_eq!(idle_between(now, past, now + h(1)), Duration::ZERO);
    }

    #[test]
    fn idle_between_uses_later_activity() {
        let now = SystemTime::now();
        assert_eq!(idle_between(now, now - h(10), now - h(2)), h(2));
    }

    #[test]
    fn idle_between_uses_later_source_change() {
        let now = SystemTime::now();
        assert_eq!(idle_between(now, now - h(3), now - h(10)), h(3));
    }
}
