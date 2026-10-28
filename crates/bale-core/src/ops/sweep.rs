//! `bale sweep`: apply the idle policy to every registered workspace. Each
//! record is handled on its own; a failure in one becomes that record's row.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Map;

use crate::config::{load_config, Config};
use crate::derived_data::{derived_data_for, is_derived_data_root};
use crate::detect::{detect, regenerable_dirs};
use crate::git;
use crate::idle::last_source_change;
use crate::ops::cold::cold_if_clean;
use crate::ops::up::{bale_able_from, up_report, Outcome};
use crate::par::par_map;
use crate::registry::{Level, Record, Registry};
use crate::trace::BuildLog;
use crate::BaleHome;

/// Report text for a record `bale open` left half done; `sweep` puts the
/// record's id in place of `<id>` in the row's result.
const INTERRUPTED_OPEN: &str = "interrupted open: run `bale open <id>`";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Keep,
    Up,
    Cold,
    Report(String),
}

/// The idle policy. `dirty` must be `true` whenever cleanliness is unknown.
/// `has_regen` says whether a `Baled` tree has regrown output `up` would
/// remove; it is ignored for other levels.
pub fn decide(
    level: Level,
    idle_for: Duration,
    dirty: bool,
    has_regen: bool,
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
    crate::trace::with_trigger("sweep", || sweep_traced(home, dry_run))
}

fn sweep_traced(home: &BaleHome, dry_run: bool) -> Result<Vec<(String, Action, String)>> {
    let start = Instant::now();
    let records = Registry::open(home.clone()).all_lenient()?;
    let measured = par_map(&records, |(_, record)| match record {
        Ok(r) => measure(home, r),
        Err(_) => Measured::default(),
    });
    let mut rows = Vec::with_capacity(records.len());
    let mut builds = Vec::new();
    let mut paths = Vec::new();
    for ((stem, record), m) in records.into_iter().zip(measured) {
        match record {
            Ok(record) => {
                let (action, result, edited) = sweep_one(home, &record, dry_run);
                trace_tree(home, &record, &action, edited, &m);
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
    let now = SystemTime::now();
    crate::trace::record_builds(home, &builds, now);
    let mut fields = Map::new();
    if dry_run {
        fields.insert("dry_run".into(), true.into());
    }
    fields.insert("volumes".into(), crate::trace::volumes(&paths).into());
    fields.insert("duration_ms".into(), millis(start.elapsed()).into());
    crate::trace::emit(home, "sweep", fields);
    crate::trace::prune(home, now);
    Ok(rows)
}

/// Sizing longer than this for one tree is cut short and recorded as null.
const SIZING_LIMIT: Duration = Duration::from_secs(30);

/// What sweep reads from a tree before changing anything, for its trace.
#[derive(Default)]
struct Measured {
    /// `(regen_bytes, tree_bytes)`; `None` for trees not on disk, after an
    /// error, or when cut short.
    sizes: Option<(u64, u64)>,
    truncated: bool,
    sizing_ms: Option<u64>,
    builds: Vec<BuildLog>,
}

/// Sizes a `Hot` or `Baled` tree and reads its Xcode build logs. Errors
/// warn once (via the trace) and leave the fields empty.
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
    let regen =
        load_config(home, Some(&root)).map(|cfg| regenerable_dirs(&root, &detect(&root), &cfg));
    match regen {
        Ok(regen) => {
            match bale_able_from(&root, regen.clone()) {
                Ok((removable, _)) => {
                    match crate::trace::size_tree(&root, &removable, start + SIZING_LIMIT) {
                        Some((tree, in_regen)) => m.sizes = Some((in_regen, tree)),
                        None => m.truncated = true,
                    }
                    m.sizing_ms = Some(millis(start.elapsed()));
                }
                Err(e) => crate::trace::warn(&e),
            }
            m.builds = build_logs_for(&root, &regen);
        }
        Err(e) => crate::trace::warn(&e),
    }
    m
}

/// The build logs of every Xcode DerivedData root that belongs to the tree:
/// the in-tree ones among `regen` and the global ones whose `WorkspacePath`
/// lies in it.
fn build_logs_for(root: &Path, regen: &[PathBuf]) -> Vec<BuildLog> {
    let mut roots: Vec<PathBuf> = regen
        .iter()
        .filter(|d| is_derived_data_root(d))
        .cloned()
        .collect();
    if let Some(home_dir) = dirs::home_dir() {
        let global = home_dir.join("Library/Developer/Xcode/DerivedData");
        roots.extend(derived_data_for(root, &global));
    }
    let mut out = Vec::new();
    for dd in roots {
        match crate::trace::build_logs(&dd) {
            Ok(logs) => out.extend(logs),
            Err(e) => crate::trace::warn(&e),
        }
    }
    out
}

/// Appends the record's `tree` line.
fn trace_tree(
    home: &BaleHome,
    record: &Record,
    action: &Action,
    edited: Option<SystemTime>,
    m: &Measured,
) {
    let (name, reason) = match action {
        Action::Keep if record.pinned => ("skip", Some("pinned".to_string())),
        Action::Keep => ("keep", None),
        Action::Up => ("up", None),
        Action::Cold => ("cold", None),
        // Probe errors name paths inside the tree; the trace keeps none.
        Action::Report(text) if text == "missing" || text.starts_with("interrupted") => {
            ("report", Some(text.replace("<id>", &record.id)))
        }
        Action::Report(_) => ("report", Some("probe failed".to_string())),
    };
    let mut f = Map::new();
    f.insert("id".into(), record.id.clone().into());
    f.insert("path".into(), record.path.to_string_lossy().into());
    f.insert(
        "repo".into(),
        record.repo_common_dir.to_string_lossy().into(),
    );
    f.insert("level".into(), record.level.name().into());
    f.insert("pinned".into(), record.pinned.into());
    let origin = record.origin.as_ref();
    f.insert("origin_tool".into(), origin.map(|o| o.tool.clone()).into());
    f.insert(
        "session_id".into(),
        origin.map(|o| o.session.clone()).into(),
    );
    f.insert(
        "last_source_edit".into(),
        edited.map(crate::trace::rfc3339).into(),
    );
    f.insert("regen_bytes".into(), m.sizes.map(|s| s.0).into());
    f.insert("tree_bytes".into(), m.sizes.map(|s| s.1).into());
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

fn millis(d: Duration) -> u64 {
    d.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Decides and applies the policy for one record: `(action, result, last
/// source edit)`, the last as sweep's idle logic saw it (`None` when it was
/// not probed or no file remains).
fn sweep_one(
    home: &BaleHome,
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
    let (action, edited) = match plan(home, record) {
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
fn plan(home: &BaleHome, record: &Record) -> Result<(Action, Option<SystemTime>)> {
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
    // on a tree whose only regenerable-named dirs hold real content.
    let has_regen = record.level == Level::Baled
        && idle_for >= cfg.idle_baled
        && !bale_able_from(&root, regen)?.0.is_empty();
    let edited = (changed > UNIX_EPOCH).then_some(changed);
    Ok((
        decide(record.level, idle_for, dirty, has_regen, &cfg),
        edited,
    ))
}

/// `now` minus the later of the two times; zero when that lies in the future.
fn idle_between(now: SystemTime, last_change: SystemTime, last_activity: SystemTime) -> Duration {
    now.duration_since(last_change.max(last_activity))
        .unwrap_or(Duration::ZERO)
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

    #[test]
    fn hot_idle_5h_goes_up() {
        assert_eq!(decide(Level::Hot, h(5), false, true, &cfg()), Action::Up);
    }

    #[test]
    fn hot_idle_3h_keeps() {
        assert_eq!(decide(Level::Hot, h(3), false, true, &cfg()), Action::Keep);
    }

    #[test]
    fn hot_at_threshold_goes_up() {
        assert_eq!(decide(Level::Hot, h(4), false, true, &cfg()), Action::Up);
    }

    #[test]
    fn baled_idle_with_regrown_output_goes_up() {
        assert_eq!(decide(Level::Baled, h(5), false, true, &cfg()), Action::Up);
        // up does not touch source, so a dirty tree qualifies too.
        assert_eq!(decide(Level::Baled, h(5), true, true, &cfg()), Action::Up);
    }

    #[test]
    fn baled_idle_without_regenerable_output_keeps() {
        assert_eq!(
            decide(Level::Baled, h(5), false, false, &cfg()),
            Action::Keep
        );
        assert_eq!(
            decide(Level::Baled, d(8), true, false, &cfg()),
            Action::Keep
        );
    }

    #[test]
    fn baled_not_idle_keeps() {
        assert_eq!(
            decide(Level::Baled, h(3), false, true, &cfg()),
            Action::Keep
        );
        assert_eq!(decide(Level::Baled, h(3), true, true, &cfg()), Action::Keep);
    }

    #[test]
    fn baled_idle_past_cold_threshold_and_clean_goes_cold() {
        assert_eq!(
            decide(Level::Baled, d(8), false, true, &cfg()),
            Action::Cold
        );
        assert_eq!(
            decide(Level::Baled, d(8), false, false, &cfg()),
            Action::Cold
        );
    }

    #[test]
    fn clean_8d_goes_cold() {
        assert_eq!(
            decide(Level::Baled, d(8), false, true, &cfg()),
            Action::Cold
        );
        assert_eq!(decide(Level::Hot, d(8), false, true, &cfg()), Action::Cold);
    }

    #[test]
    fn dirty_8d_never_cold() {
        assert_ne!(decide(Level::Baled, d(8), true, true, &cfg()), Action::Cold);
        assert_eq!(decide(Level::Baled, d(8), true, true, &cfg()), Action::Up);
        assert_eq!(
            decide(Level::Baled, d(8), true, false, &cfg()),
            Action::Keep
        );
        assert_eq!(decide(Level::Hot, d(8), true, true, &cfg()), Action::Up);
    }

    #[test]
    fn interrupted_resumes() {
        assert_eq!(decide(Level::Baling, h(0), false, true, &cfg()), Action::Up);
    }

    #[test]
    fn cooling_resumes_cold_even_when_dirty() {
        assert_eq!(
            decide(Level::Cooling, h(0), true, true, &cfg()),
            Action::Cold
        );
    }

    #[test]
    fn opening_is_reported_not_restored() {
        assert_eq!(
            decide(Level::Opening, d(30), false, true, &cfg()),
            Action::Report("interrupted open: run `bale open <id>`".into())
        );
    }

    #[test]
    fn cold_keeps() {
        assert_eq!(
            decide(Level::Cold, d(30), false, true, &cfg()),
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
        assert_eq!(decide(Level::Hot, h(30), false, true, &c), Action::Cold);
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
