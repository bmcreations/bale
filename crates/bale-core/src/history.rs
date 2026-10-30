//! Disk history read back from the usage trace: the free space each sweep
//! measured, and the bytes each operation freed. Feeds `bale history`.
//!
//! Only `ok` operations count: `up` and `cold` record the change in the
//! volume's free space, so a skipped or busy one records other writers'
//! churn. Lines that do not parse, and lines of other kinds, are skipped, so
//! a trace written by an older or newer bale still reads.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use crate::trace::{civil, days_from_civil};
use crate::BaleHome;

/// Free space on the tightest volume one sweep measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sample {
    /// Seconds since the Unix epoch.
    pub at: i64,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// Bytes one operation freed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Freed {
    /// Seconds since the Unix epoch.
    pub at: i64,
    /// `up`, `cold`, `reap`, `spm_gc` or `gc`.
    pub op: String,
    pub bytes: u64,
}

/// Everything the trace holds from `since` on, oldest first.
#[derive(Debug, Default, Serialize)]
pub struct History {
    pub samples: Vec<Sample>,
    pub freed: Vec<Freed>,
}

/// One UTC day of [`History`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Day {
    /// `YYYY-MM-DD`, UTC.
    pub date: String,
    pub sweeps: usize,
    /// The day's lowest free space, `None` when no sweep ran.
    pub lowest: Option<Sample>,
    /// The day's last free space, `None` when no sweep ran.
    pub last: Option<Sample>,
    /// Bytes freed per operation.
    pub freed: BTreeMap<String, u64>,
}

impl History {
    /// Reads the trace files whose UTC day is `since_day` (days since the
    /// Unix epoch) or later; `None` reads them all.
    pub fn load(home: &BaleHome, since_day: Option<i64>) -> Result<History> {
        load_dir(&home.trace_dir(), since_day)
    }

    /// Total bytes freed per operation.
    pub fn freed_by_op(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for f in &self.freed {
            *out.entry(f.op.clone()).or_insert(0) += f.bytes;
        }
        out
    }

    /// The lowest free space any sweep measured.
    pub fn lowest(&self) -> Option<&Sample> {
        self.samples.iter().min_by_key(|s| s.free_bytes)
    }

    /// The history split into UTC days, oldest first. Days with neither a
    /// sweep nor a freeing operation are left out.
    pub fn days(&self) -> Vec<Day> {
        let mut days: BTreeMap<i64, Day> = BTreeMap::new();
        let day = |days: &mut BTreeMap<i64, Day>, at: i64| -> i64 {
            let d = at.div_euclid(86_400);
            days.entry(d).or_insert_with(|| {
                let (y, m, dd) = civil(d);
                Day {
                    date: format!("{y:04}-{m:02}-{dd:02}"),
                    sweeps: 0,
                    lowest: None,
                    last: None,
                    freed: BTreeMap::new(),
                }
            });
            d
        };
        for s in &self.samples {
            let d = day(&mut days, s.at);
            let e = days.get_mut(&d).expect("inserted above");
            e.sweeps += 1;
            if e.lowest
                .as_ref()
                .is_none_or(|l| s.free_bytes < l.free_bytes)
            {
                e.lowest = Some(s.clone());
            }
            e.last = Some(s.clone());
        }
        for f in &self.freed {
            let d = day(&mut days, f.at);
            let e = days.get_mut(&d).expect("inserted above");
            *e.freed.entry(f.op.clone()).or_insert(0) += f.bytes;
        }
        days.into_values().collect()
    }
}

fn load_dir(dir: &Path, since_day: Option<i64>) -> Result<History> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(History::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(day) = name.strip_suffix(".jsonl").and_then(parse_day) else {
            continue;
        };
        if since_day.is_none_or(|since| day >= since) {
            files.push((day, entry.path()));
        }
    }
    files.sort();
    let mut history = History::default();
    for (_, path) in files {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                read_line(&v, &mut history);
            }
        }
    }
    history.samples.sort_by_key(|s| s.at);
    history.freed.sort_by_key(|f| f.at);
    Ok(history)
}

/// Adds what one trace line says about free or freed space.
fn read_line(v: &Value, history: &mut History) {
    let Some(at) = v["ts"].as_str().and_then(parse_ts) else {
        return;
    };
    match v["kind"].as_str() {
        Some("sweep") => {
            let tightest = v["volumes"].as_array().and_then(|vols| {
                vols.iter()
                    .filter_map(|vol| {
                        Some((vol["free_bytes"].as_u64()?, vol["total_bytes"].as_u64()?))
                    })
                    .min_by_key(|&(free, _)| free)
            });
            if let Some((free_bytes, total_bytes)) = tightest {
                history.samples.push(Sample {
                    at,
                    free_bytes,
                    total_bytes,
                });
            }
        }
        Some("op") => {
            if v["outcome"].as_str() != Some("ok") {
                return;
            }
            let (Some(op), Some(bytes)) = (v["op"].as_str(), v["bytes_freed"].as_u64()) else {
                return;
            };
            if bytes > 0 {
                history.freed.push(Freed {
                    at,
                    op: op.to_string(),
                    bytes,
                });
            }
        }
        _ => {}
    }
}

/// Days since the Unix epoch for `YYYY-MM-DD`.
pub fn parse_day(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Seconds since the Unix epoch for an RFC 3339 UTC time as the trace writes
/// it, e.g. `2026-10-28T16:16:36.153Z`. Fractions are dropped.
fn parse_ts(s: &str) -> Option<i64> {
    let day = parse_day(s.get(0..10)?)?;
    let rest = s.get(10..)?;
    let b = rest.as_bytes();
    if b.len() < 10 || b[0] != b'T' || b[3] != b':' || b[6] != b':' || !rest.ends_with('Z') {
        return None;
    }
    let h: i64 = rest[1..3].parse().ok()?;
    let mi: i64 = rest[4..6].parse().ok()?;
    let sec: i64 = rest[7..9].parse().ok()?;
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(day * 86_400 + h * 3600 + mi * 60 + sec)
}

/// `at` as `YYYY-MM-DD HH:MM` UTC.
pub fn utc_minute(at: i64) -> String {
    let (y, m, d) = civil(at.div_euclid(86_400));
    let s = at.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", s / 3600, s / 60 % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn write(dir: &Path, name: &str, lines: &[&str]) {
        fs::write(dir.join(name), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn parses_trace_times() {
        assert_eq!(parse_ts("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_ts("2026-10-28T16:16:36.153Z"), Some(1_793_204_196));
        assert_eq!(parse_ts("2026-10-28T16:16:36Z"), Some(1_793_204_196));
        assert_eq!(parse_ts("2026-10-28 16:16:36Z"), None);
        assert_eq!(parse_ts("2026-10-28T16:16:36.153+01:00"), None);
        assert_eq!(utc_minute(1_793_204_196), "2026-10-28 16:16");
    }

    #[test]
    fn reads_sweeps_and_freed_bytes_by_day() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "2026-10-28.jsonl",
            &[
                r#"{"v":1,"ts":"2026-10-28T10:00:00.000Z","kind":"sweep","volumes":[{"key":"a","free_bytes":53687091200,"total_bytes":107374182400},{"key":"b","free_bytes":21474836480,"total_bytes":107374182400}],"duration_ms":5}"#,
                r#"{"v":1,"ts":"2026-10-28T11:00:00.000Z","kind":"sweep","volumes":[{"key":"a","free_bytes":42949672960,"total_bytes":107374182400}],"duration_ms":5}"#,
                r#"{"v":1,"ts":"2026-10-28T10:30:00.000Z","kind":"op","op":"up","id":"x","outcome":"ok","bytes_freed":2147483648}"#,
                r#"{"v":1,"ts":"2026-10-28T10:31:00.000Z","kind":"op","op":"up","id":"x","outcome":"busy","bytes_freed":4294967296}"#,
                r#"{"v":1,"ts":"2026-10-28T10:32:00.000Z","kind":"op","op":"reap","id":null,"outcome":"ok","bytes_freed":1073741824}"#,
                r#"{"v":1,"ts":"2026-10-28T10:33:00.000Z","kind":"op","op":"touch","id":"x","outcome":"ok"}"#,
                "not json",
            ],
        );
        write(
            tmp.path(),
            "2026-10-29.jsonl",
            &[
                r#"{"v":1,"ts":"2026-10-29T01:00:00.000Z","kind":"sweep","volumes":[{"key":"a","free_bytes":64424509440,"total_bytes":107374182400}],"duration_ms":5}"#,
                r#"{"v":1,"ts":"2026-10-29T02:00:00.000Z","kind":"op","op":"cold","id":"y","outcome":"ok","bytes_freed":3221225472}"#,
            ],
        );
        write(tmp.path(), "seen-builds", &["abc\t2026-10-28"]);

        let h = load_dir(tmp.path(), None).unwrap();
        assert_eq!(h.samples.len(), 3);
        assert_eq!(h.samples[0].free_bytes, 20 * GIB, "tightest volume");
        assert_eq!(h.lowest().unwrap().free_bytes, 20 * GIB);
        assert_eq!(
            h.freed_by_op(),
            BTreeMap::from([
                ("cold".to_string(), 3 * GIB),
                ("reap".to_string(), GIB),
                ("up".to_string(), 2 * GIB),
            ])
        );

        let days = h.days();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, "2026-10-28");
        assert_eq!(days[0].sweeps, 2);
        assert_eq!(days[0].lowest.as_ref().unwrap().free_bytes, 20 * GIB);
        assert_eq!(days[0].last.as_ref().unwrap().free_bytes, 40 * GIB);
        assert_eq!(days[0].freed.values().sum::<u64>(), 3 * GIB);
        assert_eq!(days[1].freed["cold"], 3 * GIB);

        let since = load_dir(tmp.path(), parse_day("2026-10-29")).unwrap();
        assert_eq!(since.samples.len(), 1);
        assert_eq!(since.freed.len(), 1);
    }

    #[test]
    fn missing_trace_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let h = load_dir(&tmp.path().join("trace"), None).unwrap();
        assert!(h.samples.is_empty() && h.freed.is_empty());
        assert!(h.days().is_empty());
    }
}
