use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use bale_core::config::load_config;
use bale_core::registry::Record;
use bale_core::store::manifest::{Entry, StashManifest};
use bale_core::store::Store;
use bale_core::BaleHome;

fn entry(rel: &str, n: u8) -> Entry {
    Entry {
        rel: PathBuf::from(rel),
        hash: format!("{n:02x}").repeat(32),
        size: 2_000_000 + u64::from(n),
        mode: 0o100644,
        mtime_ns: 1_700_000_000_000_000_000 + i128::from(n),
        ino: 1000 + u64::from(n),
    }
}

fn manifest() -> StashManifest {
    StashManifest {
        tree_id: "tree1".into(),
        created_unix: 1_700_000_000,
        entries: vec![entry("a/x.o", 1), entry("b/y.o", 2), entry("c/z.o", 3)],
    }
}

#[test]
fn commitment_ignores_entry_order() {
    let a = manifest();
    let mut b = manifest();
    b.entries.reverse();
    assert_eq!(a.commitment(), b.commitment());
}

#[test]
fn commitment_changes_with_mtime() {
    let a = manifest();
    let mut b = manifest();
    b.entries[1].mtime_ns += 1;
    assert_ne!(a.commitment(), b.commitment());
}

#[test]
fn write_read_list_remove_roundtrip() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(&BaleHome(t.path().to_path_buf())).unwrap();
    let m = manifest();
    let c = store.write_manifest(&m).unwrap();
    assert_eq!(c, m.commitment());
    assert_eq!(store.read_manifest(&c).unwrap(), m);
    assert_eq!(store.list_manifests().unwrap(), vec![c.clone()]);
    store.remove_manifest(&c).unwrap();
    assert!(store.list_manifests().unwrap().is_empty());
    store.remove_manifest(&c).unwrap();
}

#[test]
fn read_manifest_rejects_tampering() {
    let t = tempfile::tempdir().unwrap();
    let store = Store::open(&BaleHome(t.path().to_path_buf())).unwrap();
    let m = manifest();
    let c = store.write_manifest(&m).unwrap();
    let path = t.path().join("store/manifests").join(format!("{c}.json"));
    let text = fs::read_to_string(&path).unwrap();
    let tampered = text.replacen(&entry("a/x.o", 1).hash, &"ff".repeat(32), 1);
    assert_ne!(text, tampered);
    fs::write(&path, tampered).unwrap();
    assert!(store.read_manifest(&c).is_err());
}

#[test]
fn old_records_without_stash_still_load() {
    let json = r#"{"id":"abc","repo_common_dir":"/repo/.git","path":"/repo-wt",
        "admin_dir":"/repo/.git/worktrees/wt","branch":null,"head":"abc123",
        "level":"baled","last_activity":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},
        "note":null}"#;
    let r: Record = serde_json::from_str(json).unwrap();
    assert!(r.stash.is_none());
    assert!(!serde_json::to_string(&r).unwrap().contains("stash"));
}

#[test]
fn config_defaults_and_repo_override() {
    let h = tempfile::tempdir().unwrap();
    let r = tempfile::tempdir().unwrap();
    let c = load_config(&BaleHome(h.path().into()), Some(r.path())).unwrap();
    assert_eq!(c.idle_stash, Duration::from_secs(600));
    assert_eq!(c.stash_min_bytes, 1_048_576);
    assert_eq!(c.patch_max_depth, 4);
    fs::write(r.path().join(".bale.toml"), "idle_stash_minutes = 3\n").unwrap();
    let c = load_config(&BaleHome(h.path().into()), Some(r.path())).unwrap();
    assert_eq!(c.idle_stash, Duration::from_secs(180));
}
