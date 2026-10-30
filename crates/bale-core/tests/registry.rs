use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use bale_core::lock::WorkspaceLock;
use bale_core::registry::{workspace_id, Level, Record, Registry};
use bale_core::BaleHome;

fn record(id: &str) -> Record {
    Record {
        id: id.to_string(),
        repo_common_dir: PathBuf::from("/repo/.git"),
        path: PathBuf::from("/repo-wt"),
        admin_dir: PathBuf::from("/repo/.git/worktrees/wt"),
        branch: Some("feat/x".to_string()),
        head: "abc123".to_string(),
        level: Level::Baled,
        last_activity: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        note: None,
        trash: Vec::new(),
        origin: None,
        pinned: false,
        scope: None,
        stash: None,
        restore_hook: false,
        spm: Vec::new(),
        shared_head: None,
    }
}

#[test]
fn put_get_roundtrip() {
    let t = tempfile::tempdir().unwrap();
    let reg = Registry::open(BaleHome(t.path().to_path_buf()));
    let r = record("abc");
    reg.put(&r).unwrap();
    assert_eq!(reg.get("abc").unwrap(), Some(r.clone()));
    assert_eq!(reg.get("missing").unwrap(), None);
    assert_eq!(reg.all().unwrap(), vec![r]);
    reg.remove("abc").unwrap();
    assert_eq!(reg.get("abc").unwrap(), None);
    reg.remove("abc").unwrap();
}

#[test]
fn record_without_restore_hook_field_parses_as_false() {
    let mut v = serde_json::to_value(record("abc")).unwrap();
    assert!(v.as_object_mut().unwrap().remove("restore_hook").is_none());
    let r: Record = serde_json::from_value(v).unwrap();
    assert!(!r.restore_hook);
    let mut r = record("abc");
    r.restore_hook = true;
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["restore_hook"], true);
}

#[test]
fn level_serializes_lowercase() {
    let t = tempfile::tempdir().unwrap();
    let h = BaleHome(t.path().to_path_buf());
    Registry::open(h.clone()).put(&record("abc")).unwrap();
    let text = std::fs::read_to_string(h.records_dir().join("abc.json")).unwrap();
    assert!(text.contains("\"baled\""), "{text}");
}

#[test]
fn put_is_atomic() {
    let t = tempfile::tempdir().unwrap();
    let h = BaleHome(t.path().to_path_buf());
    let reg = Registry::open(h.clone());
    reg.put(&record("abc")).unwrap();
    reg.put(&record("abc")).unwrap();
    let names: Vec<String> = std::fs::read_dir(h.records_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["abc.json".to_string()]);
}

#[test]
fn corrupt_record_is_reported_not_skipped() {
    let t = tempfile::tempdir().unwrap();
    let h = BaleHome(t.path().to_path_buf());
    let reg = Registry::open(h.clone());
    reg.put(&record("good")).unwrap();
    let bad = h.records_dir().join("bad.json");
    std::fs::write(&bad, "{").unwrap();
    let err = reg.all().unwrap_err();
    assert!(
        format!("{err:#}").contains(&bad.display().to_string()),
        "{err:#}"
    );
    assert!(reg.get("bad").is_err());
}

#[test]
fn all_on_missing_dir_is_empty() {
    let t = tempfile::tempdir().unwrap();
    let reg = Registry::open(BaleHome(t.path().join("nope")));
    assert!(reg.all().unwrap().is_empty());
}

#[test]
fn lock_is_exclusive() {
    let t = tempfile::tempdir().unwrap();
    let h = BaleHome(t.path().to_path_buf());
    let a = WorkspaceLock::try_acquire(&h, "abc").unwrap();
    assert!(a.is_some());
    assert!(WorkspaceLock::try_acquire(&h, "abc").unwrap().is_none());
    assert!(WorkspaceLock::try_acquire(&h, "other").unwrap().is_some());
    drop(a);
    assert!(WorkspaceLock::try_acquire(&h, "abc").unwrap().is_some());
}

#[test]
fn id_stable_for_same_path() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("a");
    let p2 = t.path().join("b");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::create_dir_all(&p2).unwrap();
    let id = workspace_id(&p);
    assert_eq!(id.len(), 12);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(id, workspace_id(&p));
    assert_ne!(id, workspace_id(&p2));
    // The id must survive the directory being removed (cold workspaces).
    std::fs::remove_dir(&p).unwrap();
    assert_eq!(workspace_id(&p), id);
    let nested = p.join("gone").join("deeper");
    std::fs::create_dir_all(&nested).unwrap();
    let nid = workspace_id(&nested);
    std::fs::remove_dir_all(&p).unwrap();
    assert_eq!(workspace_id(&nested), nid);
}

#[test]
fn level_name_matches_stored_name() {
    for level in [
        Level::Hot,
        Level::Baling,
        Level::Baled,
        Level::Cooling,
        Level::Cold,
        Level::Opening,
    ] {
        assert_eq!(
            serde_json::to_string(&level).unwrap(),
            format!("\"{}\"", level.name())
        );
    }
}

#[test]
fn record_without_spm_field_loads_with_no_links() {
    let mut v = serde_json::to_value(record("abc")).unwrap();
    assert!(v.as_object_mut().unwrap().remove("spm").is_none());
    let r: Record = serde_json::from_value(v).unwrap();
    assert!(r.spm.is_empty());
}

#[test]
fn spm_links_roundtrip() {
    let t = tempfile::tempdir().unwrap();
    let reg = Registry::open(BaleHome(t.path().to_path_buf()));
    let mut r = record("abc");
    r.spm.push(bale_core::registry::SpmLink {
        project: "/repo-wt/App.xcodeproj".into(),
        resolved:
            "/repo-wt/App.xcodeproj/project.xcworkspace/xcshareddata/swiftpm/Package.resolved"
                .into(),
        mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001),
        size: 42,
        key: "0123456789abcdef".into(),
        folder: "/dd/App-xyz".into(),
    });
    reg.put(&r).unwrap();
    assert_eq!(reg.get("abc").unwrap(), Some(r));
}
