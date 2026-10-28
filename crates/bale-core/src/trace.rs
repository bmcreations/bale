//! The local usage trace: JSON lines under `$BALE_HOME/trace/`, one file per
//! UTC day, read later by a benchmark that replays how trees were used.
//!
//! Tracing is best-effort. A failure to write never changes what an
//! operation does or returns; the first failure in a process prints one
//! warning on stderr and later ones are silent. Nothing here leaves the
//! machine, and the only paths recorded are tree paths and repo common dirs.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use walkdir::WalkDir;

use crate::busy::Holder;
use crate::ops::disk::{free_space, total_space};
use crate::ops::gc::GcReport;
use crate::ops::open::OpenReport;
use crate::ops::up::{Outcome, UpReport};
use crate::registry::{Record, Registry};
use crate::BaleHome;

/// Format version written as `"v"` on every line.
const VERSION: u64 = 1;
/// Every line, newline included, stays below this, so one `write_all` to an
/// `O_APPEND` file lands whole even beside a concurrent writer.
const MAX_LINE: usize = 4096;
/// Trace files and seen build ids older than this many days are pruned.
pub const KEEP_DAYS: i64 = 60;
/// Free text (reasons, build titles) is cut to this many bytes.
const MAX_TEXT: usize = 256;
/// Seconds from the Unix epoch to Apple's reference date, 2001-01-01.
const APPLE_EPOCH: f64 = 978_307_200.0;
/// The ids of build logs already recorded, one `<id>\t<YYYY-MM-DD>` per line.
const SEEN_BUILDS: &str = "seen-builds";

thread_local! {
    static TRIGGER: RefCell<Option<String>> = const { RefCell::new(None) };
}

static WARNED: AtomicBool = AtomicBool::new(false);

/// Runs `f` with `trigger` recorded on every `op` line it writes on this
/// thread: `cli`, `sweep` or `hook:<name>`, as the caller names it. The
/// previous trigger is restored afterwards, so an inner call overrides an
/// outer one only for its own extent.
pub fn with_trigger<R>(trigger: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            TRIGGER.with(|t| *t.borrow_mut() = prev);
        }
    }
    let prev = TRIGGER.with(|t| t.borrow_mut().replace(trigger.to_string()));
    let _restore = Restore(prev);
    f()
}

fn trigger() -> Option<String> {
    TRIGGER.with(|t| t.borrow().clone())
}

/// Reports a trace failure: the run's one warning, the first time only.
pub(crate) fn warn(e: &anyhow::Error) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("bale: usage trace not written: {e:#}");
    }
}

/// Appends one `kind` line, best-effort.
pub(crate) fn emit(home: &BaleHome, kind: &str, fields: Map<String, Value>) {
    if let Err(e) = append(home, SystemTime::now(), kind, fields) {
        warn(&e);
    }
}

fn append(home: &BaleHome, now: SystemTime, kind: &str, fields: Map<String, Value>) -> Result<()> {
    let mut obj = Map::new();
    obj.insert("v".into(), VERSION.into());
    obj.insert("ts".into(), rfc3339(now).into());
    obj.insert("kind".into(), kind.into());
    obj.extend(fields);
    let mut line = serde_json::to_string(&Value::Object(obj))?;
    line.push('\n');
    debug_assert!(line.len() < MAX_LINE, "trace line of {} bytes", line.len());
    if line.len() >= MAX_LINE {
        bail!("a {kind} line of {} bytes exceeds 4 KiB", line.len());
    }
    let dir = home.trace_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}.jsonl", utc_date(now)));
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Cuts `text` to at most [`MAX_TEXT`] bytes on a char boundary.
pub(crate) fn clip(text: &str) -> String {
    if text.len() <= MAX_TEXT {
        return text.to_string();
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn ms(d: Duration) -> u64 {
    d.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Days since the Unix epoch for a time, floored.
fn days(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() / 86_400) as i64,
        Err(e) => -(e.duration().as_secs().div_ceil(86_400) as i64),
    }
}

/// `(year, month, day)` for days since the Unix epoch (Howard Hinnant's
/// `civil_from_days`).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Days since the Unix epoch for a civil date (Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The UTC day of `t` as `YYYY-MM-DD`, the name of its trace file.
pub fn utc_date(t: SystemTime) -> String {
    let (y, m, d) = civil(days(t));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since the Unix epoch for a `YYYY-MM-DD` string.
fn parse_date(s: &str) -> Option<i64> {
    let mut parts = s.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if s.len() != 10 || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// `t` in RFC 3339 UTC with milliseconds, e.g. `2026-10-28T16:16:36.153Z`.
pub fn rfc3339(t: SystemTime) -> String {
    let (secs, millis) = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_millis()),
        Err(_) => (0, 0),
    };
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// An Apple reference-date timestamp as a `SystemTime`.
fn from_apple(secs: f64) -> Option<SystemTime> {
    let unix = secs + APPLE_EPOCH;
    (unix.is_finite() && unix >= 0.0).then(|| UNIX_EPOCH + Duration::from_secs_f64(unix))
}

/// Deletes trace files whose UTC day is more than [`KEEP_DAYS`] before
/// `now`, and the seen build ids recorded that long ago. Best-effort.
pub fn prune(home: &BaleHome, now: SystemTime) {
    if let Err(e) = prune_inner(home, now) {
        warn(&e);
    }
}

fn prune_inner(home: &BaleHome, now: SystemTime) -> Result<()> {
    let dir = home.trace_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let today = days(now);
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(day) = name.strip_suffix(".jsonl").and_then(parse_date) else {
            continue;
        };
        if today - day > KEEP_DAYS {
            fs::remove_file(entry.path())
                .with_context(|| format!("removing {}", entry.path().display()))?;
        }
    }
    let seen = dir.join(SEEN_BUILDS);
    let text = match fs::read_to_string(&seen) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", seen.display())),
    };
    let kept: String = text
        .lines()
        .filter(|l| {
            let day = l.rsplit_once('\t').and_then(|(_, d)| parse_date(d));
            day.is_some_and(|d| today - d <= KEEP_DAYS)
        })
        .map(|l| format!("{l}\n"))
        .collect();
    if kept != text {
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        tmp.write_all(kept.as_bytes())?;
        tmp.persist(&seen)
            .with_context(|| format!("replacing {}", seen.display()))?;
    }
    Ok(())
}

/// What a core operation's result says for its `op` line.
pub(crate) trait Traced {
    /// The tree's id when the caller did not know it beforehand.
    fn traced_id(&self) -> Option<&str> {
        None
    }
    /// `(outcome, reason)`.
    fn traced_outcome(&self) -> (&'static str, Option<String>);
}

impl Traced for Outcome {
    fn traced_outcome(&self) -> (&'static str, Option<String>) {
        match self {
            Outcome::Done(_) => ("ok", None),
            Outcome::Busy(holders) => ("busy", busy_reason(holders)),
            Outcome::Skipped(reason) => ("skipped", Some(clip(reason))),
        }
    }
}

/// The holders' commands, without their paths; `None` when cold's empty
/// `Busy` says the tree changed instead.
fn busy_reason(holders: &[Holder]) -> Option<String> {
    let mut names: Vec<&str> = holders.iter().map(|h| h.command.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    (!names.is_empty()).then(|| clip(&names.join(", ")))
}

impl Traced for UpReport {
    fn traced_outcome(&self) -> (&'static str, Option<String>) {
        self.outcome.traced_outcome()
    }
}

impl Traced for OpenReport {
    fn traced_outcome(&self) -> (&'static str, Option<String>) {
        self.outcome.traced_outcome()
    }
}

impl Traced for Record {
    fn traced_id(&self) -> Option<&str> {
        Some(&self.id)
    }
    fn traced_outcome(&self) -> (&'static str, Option<String>) {
        ("ok", None)
    }
}

impl Traced for GcReport {
    fn traced_outcome(&self) -> (&'static str, Option<String>) {
        match self.errors.len() {
            0 => ("ok", None),
            n => ("error", Some(format!("{n} records not checked"))),
        }
    }
}

/// Runs core operation `op` and appends its `op` line. For `up` and `cold`
/// the free space on the tree's volume is read before and after;
/// `bytes_freed` is the gain, which other writers on the volume can blur.
pub(crate) fn op<T: Traced>(
    home: &BaleHome,
    op: &str,
    id: Option<&str>,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let probe = match (op, id) {
        ("up" | "cold", Some(id)) => volume_probe(home, id),
        _ => None,
    };
    let before = probe.as_deref().and_then(|p| free_space(p).ok());
    let start = Instant::now();
    let result = f();
    let duration = start.elapsed();
    let after = probe.as_deref().and_then(|p| free_space(p).ok());

    let mut fields = Map::new();
    fields.insert("op".into(), op.into());
    let (outcome, reason, id) = match &result {
        Ok(r) => {
            let (o, why) = r.traced_outcome();
            (o, why, r.traced_id().or(id))
        }
        Err(e) => ("error", Some(clip(&e.root_cause().to_string())), id),
    };
    fields.insert("id".into(), id.into());
    fields.insert("outcome".into(), outcome.into());
    if let Some(reason) = reason {
        fields.insert("reason".into(), reason.into());
    }
    fields.insert("duration_ms".into(), ms(duration).into());
    fields.insert("trigger".into(), trigger().into());
    if probe.is_some() {
        let freed = before
            .zip(after)
            .map(|(b, a)| i128::from(a) - i128::from(b))
            .map(|d| d.clamp(i64::MIN.into(), i64::MAX.into()) as i64);
        fields.insert("bytes_freed".into(), freed.into());
    }
    emit(home, "op", fields);
    result
}

/// An existing dir on the volume of `id`'s tree that outlives `cold`: the
/// nearest existing ancestor of the tree's parent.
fn volume_probe(home: &BaleHome, id: &str) -> Option<PathBuf> {
    let record = Registry::open(home.clone()).get(id).ok()??;
    existing_ancestor(record.path.parent()?)
}

fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| !p.as_os_str().is_empty() && fs::metadata(p).is_ok())
        .map(Path::to_path_buf)
}

/// The space on disk taken by an entry: allocated blocks on Unix, the
/// file length elsewhere.
fn allocated(meta: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

/// `(whole tree, under any of regen)` allocated bytes below `root`, without
/// following symlinks, or `None` once `deadline` passes.
pub(crate) fn size_tree(root: &Path, regen: &[PathBuf], deadline: Instant) -> Option<(u64, u64)> {
    let (mut tree, mut in_regen) = (0u64, 0u64);
    for entry in WalkDir::new(root).follow_links(false) {
        if Instant::now() >= deadline {
            return None;
        }
        // Entries that vanish or can't be read mid-walk are skipped.
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        let size = allocated(&meta);
        tree += size;
        if regen.iter().any(|r| entry.path().starts_with(r)) {
            in_regen += size;
        }
    }
    Some((tree, in_regen))
}

/// A volume key and the mount point (Unix) or path root (elsewhere) of the
/// volume holding `path`, which must exist.
fn volume_of(path: &Path) -> Option<(String, PathBuf)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = fs::metadata(path).ok()?.dev();
        let mut mount = path.to_path_buf();
        while let Some(parent) = mount.parent() {
            match fs::metadata(parent) {
                Ok(m) if m.dev() == dev => mount = parent.to_path_buf(),
                _ => break,
            }
        }
        Some((dev.to_string(), mount))
    }
    #[cfg(not(unix))]
    {
        let root: PathBuf = path
            .components()
            .take_while(|c| {
                matches!(
                    c,
                    std::path::Component::Prefix(_) | std::path::Component::RootDir
                )
            })
            .collect();
        Some((root.to_string_lossy().to_lowercase(), root))
    }
}

/// `volumes` for the `sweep` line: free and total bytes of each distinct
/// volume holding one of `paths` (or, for a tree that is gone, its nearest
/// existing ancestor).
pub(crate) fn volumes(paths: &[PathBuf]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        let Some(existing) = existing_ancestor(path) else {
            continue;
        };
        let Some((key, mount)) = volume_of(&existing) else {
            continue;
        };
        if !seen.insert(key) {
            continue;
        }
        let free = free_space(&mount);
        let total = total_space(&mount);
        match (free, total) {
            (Ok(free), Ok(total)) => out.push(json!({
                "mount_or_root": mount.to_string_lossy(),
                "free_bytes": free,
                "total_bytes": total,
            })),
            (Err(e), _) | (_, Err(e)) => warn(&e),
        }
    }
    out
}

/// One entry of an Xcode `LogStoreManifest.plist`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BuildLog {
    pub log_id: String,
    pub started: Option<f64>,
    pub stopped: Option<f64>,
    pub errors: Option<i64>,
    pub title: Option<String>,
}

/// The build logs in `<dd_root>/Logs/Build/LogStoreManifest.plist`; none
/// when the manifest does not exist.
pub(crate) fn build_logs(dd_root: &Path) -> Result<Vec<BuildLog>> {
    let path = dd_root.join("Logs/Build/LogStoreManifest.plist");
    if fs::symlink_metadata(&path).is_err() {
        return Ok(Vec::new());
    }
    let value =
        plist::Value::from_file(&path).with_context(|| format!("parsing {}", path.display()))?;
    let Some(logs) = value
        .as_dictionary()
        .and_then(|d| d.get("logs"))
        .and_then(plist::Value::as_dictionary)
    else {
        bail!("{} has no logs dictionary", path.display());
    };
    let real = |v: Option<&plist::Value>| {
        v.and_then(|v| {
            v.as_real()
                .or_else(|| v.as_signed_integer().map(|i| i as f64))
        })
    };
    Ok(logs
        .iter()
        .map(|(key, entry)| {
            let entry = entry.as_dictionary();
            let get = |k: &str| entry.and_then(|d| d.get(k));
            BuildLog {
                log_id: key.clone(),
                started: real(get("timeStartedRecording")),
                stopped: real(get("timeStoppedRecording")),
                errors: get("primaryObservable")
                    .and_then(plist::Value::as_dictionary)
                    .and_then(|o| o.get("totalNumberOfErrors"))
                    .and_then(plist::Value::as_signed_integer),
                title: get("title").and_then(plist::Value::as_string).map(clip),
            }
        })
        .collect())
}

/// Appends a `build` line for each of `logs` (`(tree id, log)`) not yet in
/// `seen-builds` and stopped within [`KEEP_DAYS`], then records their ids.
/// Builds older than that are skipped, so ids pruned from `seen-builds`
/// never come back.
pub(crate) fn record_builds(home: &BaleHome, logs: &[(String, BuildLog)], now: SystemTime) {
    if logs.is_empty() {
        return;
    }
    if let Err(e) = record_builds_inner(home, logs, now) {
        warn(&e);
    }
}

fn record_builds_inner(
    home: &BaleHome,
    logs: &[(String, BuildLog)],
    now: SystemTime,
) -> Result<()> {
    let dir = home.trace_dir();
    let seen_path = dir.join(SEEN_BUILDS);
    let mut seen: HashSet<String> = match fs::read_to_string(&seen_path) {
        Ok(text) => text
            .lines()
            .map(|l| l.split('\t').next().unwrap_or(l).to_string())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", seen_path.display())),
    };
    let oldest = now - Duration::from_secs(KEEP_DAYS as u64 * 86_400);
    let today = utc_date(now);
    let mut new_ids = String::new();
    for (tree_id, log) in logs {
        if seen.contains(&log.log_id) || log.log_id.contains(['\n', '\t']) {
            continue;
        }
        let started = log.started.and_then(from_apple);
        let stopped = log.stopped.and_then(from_apple);
        if stopped.or(started).is_some_and(|t| t < oldest) {
            continue;
        }
        let duration = log.started.zip(log.stopped).map(|(a, b)| b - a);
        let fields = json!({
            "tree_id": tree_id,
            "log_id": clip(&log.log_id),
            "started": started.map(rfc3339),
            "stopped": stopped.map(rfc3339),
            "duration_s": duration,
            "errors": log.errors,
            "title": log.title,
        });
        let Value::Object(fields) = fields else {
            unreachable!()
        };
        append(home, now, "build", fields)?;
        seen.insert(log.log_id.clone());
        new_ids.push_str(&format!("{}\t{today}\n", log.log_id));
    }
    if !new_ids.is_empty() {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&seen_path)
            .with_context(|| format!("opening {}", seen_path.display()))?;
        file.write_all(new_ids.as_bytes())
            .with_context(|| format!("writing {}", seen_path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_sample_converts_to_utc() {
        let t = from_apple(814_896_996.153_806).unwrap();
        assert_eq!(rfc3339(t), "2026-10-28T16:16:36.153Z");
        assert_eq!(utc_date(t), "2026-10-28");
    }

    #[test]
    fn dates_round_trip() {
        for z in [-1, 0, 59, 11_016, 20_754, 100_000] {
            let (y, m, d) = civil(z);
            assert_eq!(days_from_civil(y, m, d), z);
            assert_eq!(parse_date(&format!("{y:04}-{m:02}-{d:02}")), Some(z));
        }
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("notes"), None);
    }

    #[test]
    fn epoch_formats() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn clip_respects_char_boundaries() {
        let text = "é".repeat(200);
        let c = clip(&text);
        assert!(c.len() <= MAX_TEXT && text.starts_with(&c));
    }

    #[test]
    fn trigger_is_scoped() {
        assert_eq!(trigger(), None);
        with_trigger("outer", || {
            with_trigger("inner", || assert_eq!(trigger().as_deref(), Some("inner")));
            assert_eq!(trigger().as_deref(), Some("outer"));
        });
        assert_eq!(trigger(), None);
    }

    #[test]
    fn size_tree_stops_at_the_deadline() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("f"), "x").unwrap();
        assert_eq!(size_tree(tmp.path(), &[], Instant::now()), None);
        assert!(size_tree(tmp.path(), &[], Instant::now() + Duration::from_secs(30)).is_some());
    }
}
