mod support;

use bale_core::ops::cold::cold;
use bale_core::ops::sweep::sweep;
use bale_core::ops::up::{register, Outcome};
use bale_core::BaleHome;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use support::{git, Fixture};

/// Every line of every trace file, parsed; each must carry `v: 1`, `ts` and `kind`.
fn lines(home: &BaleHome) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(home.trace_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".jsonl") {
            continue;
        }
        for line in fs::read_to_string(entry.path()).unwrap().lines() {
            let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
            assert_eq!(v["v"], 1, "{line}");
            assert!(v["ts"].as_str().is_some_and(|t| t.ends_with('Z')), "{line}");
            assert!(v["kind"].is_string(), "{line}");
            out.push(v);
        }
    }
    out
}

fn of_kind<'a>(all: &'a [Value], kind: &str) -> Vec<&'a Value> {
    all.iter().filter(|v| v["kind"] == kind).collect()
}

/// Commits a `.gitignore` for Gradle output so worktrees stay clean.
fn ignore_build(fx: &Fixture) {
    fs::write(fx.repo.join(".gitignore"), "build/\nsettings.gradle.kts\n").unwrap();
    git(&fx.repo, &["add", ".gitignore"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
}

#[test]
fn sweep_writes_one_sweep_line_and_one_tree_line_per_tree() {
    let fx = Fixture::new();
    ignore_build(&fx);
    let home = fx.home();
    let a = fx.add_worktree("a", "a");
    fs::write(a.join("settings.gradle.kts"), "").unwrap();
    fs::create_dir_all(a.join("build")).unwrap();
    let payload = vec![7u8; 200_000];
    fs::write(a.join("build/out.bin"), &payload).unwrap();
    let b = fx.add_worktree("b", "b");
    let id_a = register(&home, &a).unwrap().id;
    register(&home, &b).unwrap();

    let rows = sweep(&home, false).unwrap();
    assert_eq!(rows.len(), 2);

    let all = lines(&home);
    let sweeps = of_kind(&all, "sweep");
    assert_eq!(sweeps.len(), 1, "{all:#?}");
    let s = sweeps[0];
    assert!(s["duration_ms"].is_u64(), "{s}");
    let vols = s["volumes"].as_array().unwrap();
    assert_eq!(vols.len(), 1, "{s}");
    assert!(vols[0]["free_bytes"].as_u64().unwrap() > 0, "{s}");
    assert!(vols[0]["total_bytes"].as_u64().unwrap() >= vols[0]["free_bytes"].as_u64().unwrap());
    assert!(vols[0]["mount_or_root"].is_string(), "{s}");

    let trees = of_kind(&all, "tree");
    assert_eq!(trees.len(), 2, "{all:#?}");
    let ta = trees.iter().find(|t| t["id"] == id_a.as_str()).unwrap();
    assert!(
        ta["regen_bytes"].as_u64().unwrap() >= payload.len() as u64,
        "{ta}"
    );
    assert!(
        ta["tree_bytes"].as_u64().unwrap() >= ta["regen_bytes"].as_u64().unwrap(),
        "{ta}"
    );
    assert_eq!(ta["level"], "hot");
    assert_eq!(ta["action"], "keep");
    assert_eq!(ta["pinned"], false);
    assert!(ta["last_source_edit"].is_string(), "{ta}");
    assert!(ta["repo"].is_string() && ta["path"].is_string(), "{ta}");
    assert!(ta["sizing_ms"].is_u64(), "{ta}");
}

#[test]
fn dry_run_sweep_is_marked() {
    let fx = Fixture::new();
    let home = fx.home();
    register(&home, &fx.add_worktree("a", "a")).unwrap();
    sweep(&home, true).unwrap();
    let all = lines(&home);
    let sweeps = of_kind(&all, "sweep");
    assert_eq!(sweeps.len(), 1);
    assert_eq!(sweeps[0]["dry_run"], true);
}

#[test]
fn cold_op_writes_one_op_line() {
    let fx = Fixture::new();
    let home = fx.home();
    let wt = fx.add_worktree("a", "a");
    let id = register(&home, &wt).unwrap().id;
    assert!(matches!(cold(&home, &id).unwrap(), Outcome::Done(_)));
    let all = lines(&home);
    let ops: Vec<_> = of_kind(&all, "op")
        .into_iter()
        .filter(|v| v["op"] == "cold")
        .collect();
    assert_eq!(ops.len(), 1, "{all:#?}");
    assert_eq!(ops[0]["outcome"], "ok");
    assert_eq!(ops[0]["id"], id.as_str());
    assert!(ops[0]["duration_ms"].is_u64());
    assert!(ops[0].get("bytes_freed").is_some(), "{}", ops[0]);
}

#[cfg(unix)]
#[test]
fn busy_cold_writes_a_busy_op_line() {
    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let fx = Fixture::new();
    let home = fx.home();
    let wt = fx.add_worktree("a", "a");
    let id = register(&home, &wt).unwrap().id;
    let _child = KillOnDrop(
        std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&wt)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(matches!(cold(&home, &id).unwrap(), Outcome::Busy(_)));
    let all = lines(&home);
    let ops: Vec<_> = of_kind(&all, "op")
        .into_iter()
        .filter(|v| v["op"] == "cold")
        .collect();
    assert_eq!(ops.len(), 1, "{all:#?}");
    assert_eq!(ops[0]["outcome"], "busy");
}

/// An Xcode worktree whose in-tree DerivedData holds a build log manifest
/// with two entries.
fn xcode_worktree(fx: &Fixture) -> PathBuf {
    let wt = fx.add_worktree("x", "x");
    fs::create_dir_all(wt.join("App.xcodeproj")).unwrap();
    let dd = wt.join("DD");
    fs::create_dir_all(dd.join("Build")).unwrap();
    fs::create_dir_all(dd.join("Logs/Build")).unwrap();
    let mut info = plist::Dictionary::new();
    info.insert(
        "WorkspacePath".into(),
        plist::Value::String(wt.join("App.xcodeproj").to_string_lossy().into_owned()),
    );
    plist::Value::Dictionary(info)
        .to_file_xml(dd.join("info.plist"))
        .unwrap();
    let entry = |started: f64, stopped: f64, title: &str, errors: i64| {
        let mut obs = plist::Dictionary::new();
        obs.insert(
            "totalNumberOfErrors".into(),
            plist::Value::Integer(errors.into()),
        );
        let mut e = plist::Dictionary::new();
        e.insert("timeStartedRecording".into(), plist::Value::Real(started));
        e.insert("timeStoppedRecording".into(), plist::Value::Real(stopped));
        e.insert("title".into(), plist::Value::String(title.into()));
        e.insert(
            "fileName".into(),
            plist::Value::String("x.xcactivitylog".into()),
        );
        e.insert("primaryObservable".into(), plist::Value::Dictionary(obs));
        plist::Value::Dictionary(e)
    };
    // Recent builds, so none count as older than the trace keeps.
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - 978_307_200.0;
    let mut logs = plist::Dictionary::new();
    logs.insert(
        "AAA-1".into(),
        entry(
            now - 200.0,
            now - 100.0,
            "Testing project Code with scheme Flipcash",
            3,
        ),
    );
    logs.insert(
        "BBB-2".into(),
        entry(now - 50.0, now - 20.0, "Build App", 0),
    );
    let mut manifest = plist::Dictionary::new();
    manifest.insert("logs".into(), plist::Value::Dictionary(logs));
    plist::Value::Dictionary(manifest)
        .to_file_xml(dd.join("Logs/Build/LogStoreManifest.plist"))
        .unwrap();
    wt
}

#[test]
fn build_logs_are_recorded_once() {
    let fx = Fixture::new();
    let home = fx.home();
    let wt = xcode_worktree(&fx);
    let id = register(&home, &wt).unwrap().id;

    sweep(&home, false).unwrap();
    let first = lines(&home);
    let builds = of_kind(&first, "build");
    assert_eq!(builds.len(), 2, "{first:#?}");
    let a = builds.iter().find(|b| b["log_id"] == "AAA-1").unwrap();
    assert_eq!(a["tree_id"], id.as_str());
    assert_eq!(a["errors"], 3);
    assert_eq!(a["title"], "Testing project Code with scheme Flipcash");
    assert!(
        (a["duration_s"].as_f64().unwrap() - 100.0).abs() < 0.01,
        "{a}"
    );
    assert!(a["started"].as_str().unwrap().ends_with('Z'));
    assert!(a["stopped"].as_str().unwrap().ends_with('Z'));

    sweep(&home, false).unwrap();
    let second = lines(&home);
    assert_eq!(of_kind(&second, "build").len(), 2, "{second:#?}");
}

/// Writes an empty trace file for the UTC day `days_ago` before `now`.
fn day_file(home: &BaleHome, now: SystemTime, days_ago: u64) -> PathBuf {
    let t = now - Duration::from_secs(days_ago * 86_400);
    let name = format!("{}.jsonl", bale_core::trace::utc_date(t));
    let path = home.trace_dir().join(name);
    fs::create_dir_all(home.trace_dir()).unwrap();
    fs::write(&path, "").unwrap();
    path
}

#[test]
fn files_older_than_60_days_are_pruned() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = BaleHome(tmp.path().to_path_buf());
    let now = SystemTime::now();
    let old = day_file(&home, now, 61);
    let kept = day_file(&home, now, 59);
    let edge = day_file(&home, now, 60);
    let other = home.trace_dir().join("notes.txt");
    fs::write(&other, "").unwrap();
    let seen = home.trace_dir().join("seen-builds");
    let day = |n: u64| bale_core::trace::utc_date(now - Duration::from_secs(n * 86_400));
    fs::write(&seen, format!("old\t{}\nnew\t{}\n", day(61), day(59))).unwrap();

    bale_core::trace::prune(&home, now);

    assert!(!exists(&old));
    assert!(exists(&kept));
    assert!(exists(&edge));
    assert!(exists(&other));
    assert_eq!(
        fs::read_to_string(&seen).unwrap(),
        format!("new\t{}\n", day(59))
    );
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

#[test]
fn sweep_succeeds_when_the_trace_cannot_be_written() {
    let fx = Fixture::new();
    let home = fx.home();
    register(&home, &fx.add_worktree("a", "a")).unwrap();
    // Registering already traced; a regular file in place of the dir makes
    // every later write fail.
    fs::remove_dir_all(home.trace_dir()).unwrap();
    fs::write(home.trace_dir(), "not a dir").unwrap();
    let rows = sweep(&home, false).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "nothing to do");
}
