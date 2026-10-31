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

use crate::clones::share::ShareReport;
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
/// Paths are cut to this many bytes as JSON, so a line with two of them,
/// two ids and its other fields stays below [`MAX_LINE`].
pub(crate) const MAX_PATH: usize = 1024;
/// Session ids and tool names are cut to this many bytes as JSON.
pub(crate) const MAX_ID: usize = 256;
/// Apple timestamps past this many seconds after the Unix epoch (about the
/// year 5138) are treated as corrupt.
const MAX_UNIX_SECS: f64 = 1e11;
/// Seconds from the Unix epoch to Apple's reference date, 2001-01-01.
const APPLE_EPOCH: f64 = 978_307_200.0;
/// The ids of build logs already recorded, one `<id>\t<YYYY-MM-DD>` per line.
const SEEN_BUILDS: &str = "seen-builds";

type LocalSlot = std::thread::LocalKey<RefCell<Option<String>>>;

thread_local! {
    static TRIGGER: RefCell<Option<String>> = const { RefCell::new(None) };
    static INTENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

static WARNED: AtomicBool = AtomicBool::new(false);

/// Runs `f` with `value` set in `key` on this thread, restoring the previous
/// value afterwards (also on panic).
fn scoped<R>(key: &'static LocalSlot, value: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(&'static LocalSlot, Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.1.take();
            self.0.with(|t| *t.borrow_mut() = prev);
        }
    }
    let prev = key.with(|t| t.borrow_mut().replace(value.to_string()));
    let _restore = Restore(key, prev);
    f()
}

/// Runs `f` with `trigger` recorded on every `op` line it writes on this
/// thread: `cli`, `sweep` or `hook:<name>`, as the caller names it. The
/// previous trigger is restored afterwards, so an inner call overrides an
/// outer one only for its own extent.
pub fn with_trigger<R>(trigger: &str, f: impl FnOnce() -> R) -> R {
    scoped(&TRIGGER, trigger, f)
}

/// Runs `f` with `intent` recorded on every `op` line it writes on this
/// thread, scoped as [`with_trigger`]. A caller that colds a tree because
/// its tool is deleting it passes `remove`, so a replay can tell that from
/// an idle cold.
pub fn with_intent<R>(intent: &str, f: impl FnOnce() -> R) -> R {
    scoped(&INTENT, intent, f)
}

fn trigger() -> Option<String> {
    TRIGGER.with(|t| t.borrow().clone())
}

fn intent() -> Option<String> {
    INTENT.with(|t| t.borrow().clone())
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

/// `text` cut so its JSON string form (quotes aside) takes at most `max`
/// bytes, with `…` in place of what was cut; and whether anything was.
/// Characters JSON escapes count at their escaped size.
pub(crate) fn clip_field(text: &str, max: usize) -> (String, bool) {
    let width = |c: char| match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    };
    if text.chars().map(width).sum::<usize>() <= max {
        return (text.to_string(), false);
    }
    let room = max.saturating_sub('…'.len_utf8());
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        used += width(c);
        if used > room {
            break;
        }
        out.push(c);
    }
    out.push('…');
    (out, true)
}

/// Inserts `text` as `key`, clipped to `max` (see [`clip_field`]); a clip
/// sets `"clipped": true` on the line.
pub(crate) fn put_clipped(
    fields: &mut Map<String, Value>,
    key: &str,
    text: Option<&str>,
    max: usize,
) {
    let Some(text) = text else {
        fields.insert(key.into(), Value::Null);
        return;
    };
    let (text, clipped) = clip_field(text, max);
    fields.insert(key.into(), text.into());
    if clipped {
        fields.insert("clipped".into(), true.into());
    }
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
pub(crate) fn civil(z: i64) -> (i64, u32, u32) {
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
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
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

/// An Apple reference-date timestamp as a `SystemTime`; `None` when it is
/// not finite, before 1970, or implausibly far ahead.
fn from_apple(secs: f64) -> Option<SystemTime> {
    let unix = secs + APPLE_EPOCH;
    if !(unix.is_finite() && (0.0..MAX_UNIX_SECS).contains(&unix)) {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::try_from_secs_f64(unix).ok()?)
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
    /// `(outcome, reason code)`. The code is one of a fixed set, never
    /// text from the operation, which can name paths inside the tree.
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>);
    /// Extra fields for the `op` line, such as a file count.
    fn traced_fields(&self) -> Map<String, Value> {
        Map::new()
    }
}

impl Traced for Outcome {
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Outcome::Done(_) => ("ok", None),
            Outcome::Busy(holders) if holders.is_empty() => ("busy", Some("changed")),
            Outcome::Busy(_) => ("busy", Some("busy")),
            Outcome::Skipped(reason) => ("skipped", Some(reason.code.name())),
        }
    }
}

impl Traced for UpReport {
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>) {
        self.outcome.traced_outcome()
    }
}

impl Traced for OpenReport {
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>) {
        self.outcome.traced_outcome()
    }
}

impl Traced for Record {
    fn traced_id(&self) -> Option<&str> {
        Some(&self.id)
    }
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>) {
        ("ok", None)
    }
}

impl Traced for ShareReport {
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>) {
        let code = match self.reason.as_deref() {
            None if self.had_errors() => return ("ok", Some("partial")),
            None => return ("ok", None),
            Some("locked") => "locked",
            Some("busy") => "busy",
            Some("other volume") => "other_volume",
            Some("clones unsupported") => "unsupported",
            Some("disabled") => "disabled",
            Some(_) => "other",
        };
        ("skipped", Some(code))
    }
}

impl Traced for GcReport {
    fn traced_outcome(&self) -> (&'static str, Option<&'static str>) {
        match self.errors.len() {
            0 => ("ok", None),
            _ => ("error", Some("partial")),
        }
    }
}

/// The reason code for a failed operation, by the type of error in its
/// chain: `git_error`, `missing` (a path not found), `io_error`, `other`.
fn error_code(e: &anyhow::Error) -> &'static str {
    for cause in e.chain() {
        if cause.is::<crate::git::GitError>() {
            return "git_error";
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::NotFound => "missing",
                _ => "io_error",
            };
        }
    }
    "other"
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
    if let Ok(r) = &result {
        fields.extend(r.traced_fields());
    }
    let (outcome, reason, id) = match &result {
        Ok(r) => {
            let (o, why) = r.traced_outcome();
            (o, why, r.traced_id().or(id))
        }
        Err(e) => ("error", Some(error_code(e)), id),
    };
    fields.insert("id".into(), id.into());
    fields.insert("outcome".into(), outcome.into());
    if let Some(reason) = reason {
        fields.insert("reason".into(), reason.into());
    }
    fields.insert("duration_ms".into(), ms(duration).into());
    insert_context(&mut fields);
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

/// Adds the caller's `trigger` and, when set, its `intent`, scoped to the
/// trace's own length limits.
fn insert_context(fields: &mut Map<String, Value>) {
    put_clipped(fields, "trigger", trigger().as_deref(), MAX_ID);
    if let Some(intent) = intent() {
        put_clipped(fields, "intent", Some(&intent), MAX_ID);
    }
}

/// Writes the `op` line for one reaped DerivedData folder: `bytes_freed` is
/// what the folder held when `Ok`. Best-effort.
pub fn reaped(home: &BaleHome, bytes: u64, removed: bool, duration: Duration) {
    let mut fields = Map::new();
    fields.insert("op".into(), "reap".into());
    fields.insert("id".into(), Value::Null);
    fields.insert(
        "outcome".into(),
        if removed { "ok" } else { "error" }.into(),
    );
    if !removed {
        fields.insert("reason".into(), "io".into());
    }
    fields.insert("duration_ms".into(), ms(duration).into());
    insert_context(&mut fields);
    fields.insert("bytes_freed".into(), removed.then_some(bytes).into());
    emit(home, "op", fields);
}

/// Writes the `op` line for one shared packages folder (or leftover temp
/// clone) the sweep removed, `outcome` `ok` or `error`; `bytes_freed` is what
/// it held when `ok`. Like [`reaped`], it names no path. Best-effort.
pub fn spm_gc(home: &BaleHome, bytes: u64, removed: bool, duration: Duration) {
    let mut fields = Map::new();
    fields.insert("op".into(), "spm_gc".into());
    fields.insert("id".into(), Value::Null);
    fields.insert(
        "outcome".into(),
        if removed { "ok" } else { "error" }.into(),
    );
    if !removed {
        fields.insert("reason".into(), "io".into());
    }
    fields.insert("duration_ms".into(), ms(duration).into());
    insert_context(&mut fields);
    fields.insert("bytes_freed".into(), removed.then_some(bytes).into());
    emit(home, "op", fields);
}

/// Writes the `op` line for a sweep whose shared packages cleanup was
/// skipped because it could not tell what is still linked: outcome `skipped`
/// and `reason` (an error kind, no path). Best-effort.
pub fn spm_gc_skipped(home: &BaleHome, reason: &str) {
    let mut fields = Map::new();
    fields.insert("op".into(), "spm_gc".into());
    fields.insert("id".into(), Value::Null);
    fields.insert("outcome".into(), "skipped".into());
    fields.insert("reason".into(), reason.into());
    insert_context(&mut fields);
    fields.insert("bytes_freed".into(), Value::Null);
    emit(home, "op", fields);
}

/// Writes the `op` line for operation `op` on tree `id` that failed where
/// the caller carries on without it (a hook's best-effort step): outcome
/// `error` and the error's kind as `reason`, no message or path.
pub fn failed(home: &BaleHome, op: &str, id: &str, e: &anyhow::Error) {
    let mut fields = Map::new();
    fields.insert("op".into(), op.into());
    fields.insert("id".into(), id.into());
    fields.insert("outcome".into(), "error".into());
    fields.insert("reason".into(), error_code(e).into());
    insert_context(&mut fields);
    emit(home, "op", fields);
}

/// Writes the `op` line for an operation whose target was gone before it
/// could run: outcome and reason `missing`, the target's `path`, and its
/// id when a record for that path exists. Best-effort.
pub fn missing(home: &BaleHome, op: &str, path: &Path) {
    let id = std::path::absolute(path)
        .ok()
        .map(|p| crate::registry::workspace_id(&p))
        .filter(|id| {
            Registry::open(home.clone())
                .get(id)
                .ok()
                .flatten()
                .is_some()
        });
    let mut fields = Map::new();
    fields.insert("op".into(), op.into());
    fields.insert("id".into(), id.into());
    put_clipped(&mut fields, "path", Some(&path.to_string_lossy()), MAX_PATH);
    fields.insert("outcome".into(), "missing".into());
    fields.insert("reason".into(), "missing".into());
    fields.insert("duration_ms".into(), 0.into());
    insert_context(&mut fields);
    emit(home, "op", fields);
}

/// The distinct `path` of every `tree` line in the kept trace files, sorted,
/// so a tree whose record is gone can still be named for up to [`KEEP_DAYS`].
/// Unreadable files and lines are skipped.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn tree_paths(home: &BaleHome) -> Vec<PathBuf> {
    let mut paths = std::collections::BTreeSet::new();
    let Ok(entries) = fs::read_dir(home.trace_dir()) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        if entry.path().extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in text.lines().filter(|l| l.contains(r#""kind":"tree""#)) {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let (Some("tree"), Some(path)) = (v["kind"].as_str(), v["path"].as_str()) {
                paths.insert(PathBuf::from(path));
            }
        }
    }
    paths.into_iter().collect()
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

/// Allocated bytes counted by [`size_tree`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Size {
    /// The whole walk.
    pub tree: u64,
    /// The part under any of the `regen` dirs.
    pub in_regen: u64,
    /// The walk was cut short; the counts cover only what it reached.
    pub truncated: bool,
}

/// Allocated bytes below `root`, without following symlinks. A walk still
/// running at `deadline` stops there and returns what it counted so far.
pub(crate) fn size_tree(root: &Path, regen: &[PathBuf], deadline: Instant) -> Size {
    size_tree_until(root, regen, || Instant::now() >= deadline)
}

/// [`size_tree`] that stops when `over_budget` returns true, which it asks
/// before every entry after the first.
pub(crate) fn size_tree_until(
    root: &Path,
    regen: &[PathBuf],
    mut over_budget: impl FnMut() -> bool,
) -> Size {
    let mut size = Size::default();
    for (i, entry) in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if i > 0 && over_budget() {
            size.truncated = true;
            break;
        }
        // Entries that vanish or can't be read mid-walk are skipped.
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        let bytes = allocated(&meta);
        size.tree += bytes;
        if regen.iter().any(|r| entry.path().starts_with(r)) {
            size.in_regen += bytes;
        }
    }
    size
}

/// A key for the volume holding `path`, which must exist, that stays the
/// same across runs: `dev:<st_dev>` on Unix, the path's prefix (such as
/// `c:`) elsewhere.
fn volume_key(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("dev:{}", fs::metadata(path).ok()?.dev()))
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
        Some(root.to_string_lossy().to_lowercase())
    }
}

/// At most this many volumes are listed on a `sweep` line.
const MAX_VOLUMES: usize = 8;
/// A volume's `sample_path` is cut to this many bytes as JSON.
const MAX_SAMPLE: usize = 256;

/// `volumes` for the `sweep` line: free and total bytes of the volume
/// holding the state home and of each distinct volume holding one of
/// `paths` (or, for a tree that is gone, its nearest existing ancestor).
/// Each has a stable `key` and, for people reading the trace, the first
/// tree path on it as `sample_path` (null for a volume with no tree).
pub(crate) fn volumes(home: &BaleHome, paths: &[PathBuf]) -> Vec<Value> {
    let mut found: Vec<(String, PathBuf, Option<&PathBuf>)> = Vec::new();
    let candidates = std::iter::once((&home.0, None)).chain(paths.iter().map(|p| (p, Some(p))));
    for (path, tree) in candidates {
        let Some(existing) = existing_ancestor(path) else {
            continue;
        };
        let Some(key) = volume_key(&existing) else {
            continue;
        };
        match found.iter_mut().find(|(k, _, _)| *k == key) {
            Some((_, _, sample)) => {
                if sample.is_none() {
                    *sample = tree;
                }
            }
            None => found.push((key, existing, tree)),
        }
    }
    let mut out = Vec::new();
    for (key, probe, sample) in found.into_iter().take(MAX_VOLUMES) {
        match (free_space(&probe), total_space(&probe)) {
            (Ok(free), Ok(total)) => {
                let mut v = Map::new();
                v.insert("key".into(), key.into());
                let sample = sample.map(|p| p.to_string_lossy());
                put_clipped(&mut v, "sample_path", sample.as_deref(), MAX_SAMPLE);
                v.insert("free_bytes".into(), free.into());
                v.insert("total_bytes".into(), total.into());
                out.push(Value::Object(v));
            }
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
    record_builds_with(home, logs, now, |fields| append(home, now, "build", fields))
}

/// [`record_builds`] with `write` appending each line. Each id goes into
/// `seen-builds` right after its line is written, so a failure partway
/// does not lead the next run to write the earlier lines again.
fn record_builds_with(
    home: &BaleHome,
    logs: &[(String, BuildLog)],
    now: SystemTime,
    mut write: impl FnMut(Map<String, Value>) -> Result<()>,
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
    for (tree_id, log) in logs {
        if seen.contains(&log.log_id) || log.log_id.contains(['\n', '\t']) {
            continue;
        }
        let started = log.started.and_then(from_apple);
        let stopped = log.stopped.and_then(from_apple);
        if stopped.or(started).is_some_and(|t| t < oldest) {
            continue;
        }
        let duration = started.zip(stopped).and_then(|(a, b)| {
            log.started
                .zip(log.stopped)
                .map(|(x, y)| y - x)
                .filter(|_| b >= a)
        });
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
        write(fields)?;
        seen.insert(log.log_id.clone());
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&seen_path)
            .with_context(|| format!("opening {}", seen_path.display()))?;
        file.write_all(format!("{}\t{today}\n", log.log_id).as_bytes())
            .with_context(|| format!("writing {}", seen_path.display()))?;
    }
    Ok(())
}

/// Records the build logs of the Xcode DerivedData `roots` as `tree_id`'s,
/// as sweep does, so builds are not lost when an operation is about to
/// remove the roots. Best-effort.
pub(crate) fn harvest(home: &BaleHome, tree_id: &str, roots: &[PathBuf]) {
    let mut logs = Vec::new();
    for dd in roots {
        match build_logs(dd) {
            Ok(found) => logs.extend(found.into_iter().map(|b| (tree_id.to_string(), b))),
            Err(e) => warn(&e),
        }
    }
    record_builds(home, &logs, SystemTime::now());
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
    fn out_of_range_apple_times_are_none() {
        assert_eq!(from_apple(i64::MAX as f64), None);
        assert_eq!(from_apple(f64::NAN), None);
        assert_eq!(from_apple(1e19), None);
        assert_eq!(from_apple(-1e12), None);
    }

    fn log(id: &str, started: Option<f64>, stopped: Option<f64>) -> (String, BuildLog) {
        (
            "tree".into(),
            BuildLog {
                log_id: id.into(),
                started,
                stopped,
                errors: None,
                title: None,
            },
        )
    }

    fn read_lines(home: &BaleHome) -> Vec<Value> {
        let mut out = Vec::new();
        for entry in fs::read_dir(home.trace_dir()).unwrap().flatten() {
            if entry.file_name().to_string_lossy().ends_with(".jsonl") {
                for l in fs::read_to_string(entry.path()).unwrap().lines() {
                    out.push(serde_json::from_str(l).unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn a_build_with_bad_timestamps_records_nulls() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = BaleHome(tmp.path().to_path_buf());
        let logs = [
            log("A", Some(i64::MAX as f64), Some(f64::NAN)),
            log("B", Some(f64::NAN), Some(i64::MAX as f64)),
        ];
        record_builds(&home, &logs, SystemTime::now());
        let lines = read_lines(&home);
        assert_eq!(lines.len(), 2, "{lines:#?}");
        for l in &lines {
            assert!(l["started"].is_null() && l["stopped"].is_null(), "{l}");
            assert!(l["duration_s"].is_null(), "{l}");
        }
    }

    #[test]
    fn builds_written_before_a_failure_are_marked_seen() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = BaleHome(tmp.path().to_path_buf());
        let logs = [
            log("A", None, None),
            log("B", None, None),
            log("C", None, None),
        ];
        let mut calls = 0;
        let result = record_builds_with(&home, &logs, SystemTime::now(), |fields| {
            calls += 1;
            if calls == 2 {
                bail!("disk full");
            }
            append(&home, SystemTime::now(), "build", fields)
        });
        assert!(result.is_err());
        let seen = fs::read_to_string(home.trace_dir().join(SEEN_BUILDS)).unwrap();
        let ids: Vec<&str> = seen.lines().filter_map(|l| l.split('\t').next()).collect();
        assert_eq!(ids, ["A"]);
        // The next run writes only what is left.
        record_builds(&home, &logs, SystemTime::now());
        let written: Vec<String> = read_lines(&home)
            .iter()
            .map(|l| l["log_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(written, ["A", "B", "C"]);
    }

    #[test]
    fn clip_field_marks_long_values() {
        let (short, clipped) = clip_field("abc", 10);
        assert_eq!((short.as_str(), clipped), ("abc", false));
        let (long, clipped) = clip_field(&"x".repeat(100), 10);
        assert!(clipped);
        assert!(long.ends_with('…'));
        assert!(serde_json::to_string(&long).unwrap().len() <= 10 + 2);
        // Escaped control characters count at their escaped size.
        let (ctl, clipped) = clip_field(&"\u{1}".repeat(100), 64);
        assert!(clipped);
        assert!(
            serde_json::to_string(&ctl).unwrap().len() <= 64 + 2,
            "{ctl:?}"
        );
    }

    #[test]
    fn size_tree_stops_at_the_deadline() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("f"), "x").unwrap();
        assert!(size_tree(tmp.path(), &[], Instant::now()).truncated);
        let full = size_tree(tmp.path(), &[], Instant::now() + Duration::from_secs(30));
        assert!(!full.truncated);
        assert!(full.tree > 0);
    }

    #[test]
    fn a_truncated_walk_keeps_the_bytes_it_counted() {
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["a", "b", "c", "d"] {
            fs::write(tmp.path().join(name), vec![1u8; 20_000]).unwrap();
        }
        let full = size_tree_until(tmp.path(), &[], || false);
        assert!(!full.truncated);
        let mut checks = 0;
        let cut = size_tree_until(tmp.path(), &[], || {
            checks += 1;
            checks >= 3
        });
        assert!(cut.truncated, "{cut:?}");
        assert!(cut.tree >= 20_000, "{cut:?}");
        assert!(cut.tree < full.tree, "{cut:?} vs {full:?}");
    }
}
