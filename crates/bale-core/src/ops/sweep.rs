//! `bale sweep`: apply the idle policy to every registered workspace. Each
//! record is handled on its own; a failure in one becomes that record's row.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::config::{load_config, Config};
use crate::detect::{detect, regenerable_dirs};
use crate::git;
use crate::idle::last_source_change;
use crate::ops::cold::cold;
use crate::ops::up::{up, Outcome};
use crate::registry::{Level, Record, Registry};
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
pub fn decide(level: Level, idle_for: Duration, dirty: bool, cfg: &Config) -> Action {
    match level {
        Level::Cold => Action::Keep,
        Level::Opening => Action::Report(INTERRUPTED_OPEN.into()),
        Level::Baling => Action::Up,
        Level::Cooling => Action::Cold,
        Level::Hot | Level::Baled => {
            if !dirty && idle_for >= cfg.idle_cold {
                Action::Cold
            } else if level == Level::Hot && idle_for >= cfg.idle_baled {
                Action::Up
            } else {
                Action::Keep
            }
        }
    }
}

/// Decides and (unless `dry_run`) applies the policy for every record,
/// returning one `(id, action, result)` row each. Errs only when the
/// registry cannot be read.
pub fn sweep(home: &BaleHome, dry_run: bool) -> Result<Vec<(String, Action, String)>> {
    let records = Registry::open(home.clone()).all()?;
    Ok(records
        .into_iter()
        .map(|record| {
            let (action, result) = sweep_one(home, &record, dry_run);
            (record.id, action, result)
        })
        .collect())
}

fn sweep_one(home: &BaleHome, record: &Record, dry_run: bool) -> (Action, String) {
    let action = match plan(home, record) {
        Ok(action) => action,
        Err(e) => {
            let text = format!("{e:#}");
            return (Action::Report(text.clone()), text);
        }
    };
    let result = match &action {
        Action::Report(text) => text.replace("<id>", &record.id),
        Action::Keep => "nothing to do".into(),
        _ if dry_run => "dry run, not applied".into(),
        Action::Up => outcome_text(up(home, &record.id)),
        Action::Cold => outcome_text(cold(home, &record.id)),
    };
    (action, result)
}

/// Probes the workspace and decides. Probe failures are errors, so they
/// become `Report` rows and can never lead to `Cold`.
fn plan(home: &BaleHome, record: &Record) -> Result<Action> {
    match record.level {
        // Cooling may legitimately have no checkout; cold's recovery handles it.
        Level::Cold | Level::Opening | Level::Cooling => {
            return Ok(decide(record.level, Duration::ZERO, true, &global(home)?));
        }
        Level::Hot | Level::Baled | Level::Baling => {}
    }
    if fs::symlink_metadata(&record.path).is_err() {
        return Ok(Action::Report("missing".into()));
    }
    if record.level == Level::Baling {
        return Ok(decide(record.level, Duration::ZERO, true, &global(home)?));
    }
    let root = fs::canonicalize(&record.path)
        .with_context(|| format!("resolving {}", record.path.display()))?;
    let cfg = load_config(home, Some(&root))?;
    let idle_for = idle_for(record, &root, &cfg)?;
    // Only a tree past the cold threshold needs the (costly) dirty probe;
    // below it, dirty = true changes nothing.
    let dirty = idle_for < cfg.idle_cold || git::is_dirty(&root)?;
    Ok(decide(record.level, idle_for, dirty, &cfg))
}

fn global(home: &BaleHome) -> Result<Config> {
    load_config(home, None)
}

/// Now minus the later of the newest source mtime and the recorded activity,
/// zero when that lies in the future.
fn idle_for(record: &Record, root: &Path, cfg: &Config) -> Result<Duration> {
    let regen = regenerable_dirs(root, &detect(root), cfg);
    let changed = last_source_change(root, &regen)?;
    let last = changed.max(record.last_activity);
    Ok(SystemTime::now()
        .duration_since(last)
        .unwrap_or(Duration::ZERO))
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
        assert_eq!(decide(Level::Hot, h(5), false, &cfg()), Action::Up);
    }

    #[test]
    fn hot_idle_3h_keeps() {
        assert_eq!(decide(Level::Hot, h(3), false, &cfg()), Action::Keep);
    }

    #[test]
    fn hot_at_threshold_goes_up() {
        assert_eq!(decide(Level::Hot, h(4), false, &cfg()), Action::Up);
    }

    #[test]
    fn baled_idle_5h_keeps() {
        assert_eq!(decide(Level::Baled, h(5), false, &cfg()), Action::Keep);
    }

    #[test]
    fn clean_8d_goes_cold() {
        assert_eq!(decide(Level::Baled, d(8), false, &cfg()), Action::Cold);
        assert_eq!(decide(Level::Hot, d(8), false, &cfg()), Action::Cold);
    }

    #[test]
    fn dirty_8d_never_cold() {
        assert_ne!(decide(Level::Baled, d(8), true, &cfg()), Action::Cold);
        assert_eq!(decide(Level::Baled, d(8), true, &cfg()), Action::Keep);
        assert_eq!(decide(Level::Hot, d(8), true, &cfg()), Action::Up);
    }

    #[test]
    fn interrupted_resumes() {
        assert_eq!(decide(Level::Baling, h(0), false, &cfg()), Action::Up);
    }

    #[test]
    fn cooling_resumes_cold_even_when_dirty() {
        assert_eq!(decide(Level::Cooling, h(0), true, &cfg()), Action::Cold);
    }

    #[test]
    fn opening_is_reported_not_restored() {
        assert_eq!(
            decide(Level::Opening, d(30), false, &cfg()),
            Action::Report("interrupted open: run `bale open <id>`".into())
        );
    }

    #[test]
    fn cold_keeps() {
        assert_eq!(decide(Level::Cold, d(30), false, &cfg()), Action::Keep);
    }

    #[test]
    fn cold_threshold_below_baled_threshold_still_colds() {
        let c = Config {
            idle_baled: d(2),
            idle_cold: d(1),
            ..cfg()
        };
        assert_eq!(decide(Level::Hot, h(30), false, &c), Action::Cold);
    }
}
