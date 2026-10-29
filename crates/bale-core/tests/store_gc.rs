use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bale_core::store::gc::{collect, collect_index, collect_with, CollectOpts, GC_GRACE};
use bale_core::store::manifest::{Entry, StashManifest};
use bale_core::store::{sha256_file, Hash, Store};
use bale_core::BaleHome;
use filetime::{set_file_mtime, FileTime};

fn sample(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let block: Vec<u8> = (0..4096).map(|_| next() as u8).collect();
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let mut b = block.clone();
        for _ in 0..64 {
            let r = next();
            b[(r as usize) % 4096] = (r >> 32) as u8;
        }
        out.extend_from_slice(&b);
    }
    out.truncate(len);
    out
}

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&BaleHome(dir.path().to_path_buf())).unwrap();
    (dir, store)
}

fn write(dir: &Path, name: &str, data: &[u8]) -> (PathBuf, Hash) {
    let p = dir.join(name);
    fs::write(&p, data).unwrap();
    let h = sha256_file(&p).unwrap();
    (p, h)
}

fn blob_files(root: &Path, hash: &str) -> [PathBuf; 2] {
    let d = root.join("store/objects").join(&hash[..2]);
    [
        d.join(format!("{hash}.zst")),
        d.join(format!("{hash}.json")),
    ]
}

fn manifest_file(root: &Path, c: &str) -> PathBuf {
    root.join("store/manifests").join(format!("{c}.json"))
}

fn age(path: &Path, by: Duration) {
    set_file_mtime(path, FileTime::from_system_time(SystemTime::now() - by)).unwrap();
}

/// Makes the blob and manifest files older than the grace period.
fn age_all(root: &Path) {
    for e in walkdir::WalkDir::new(root.join("store"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        age(e.path(), GC_GRACE * 2);
    }
}

fn manifest(tree: &str, hashes: &[&Hash]) -> StashManifest {
    StashManifest {
        tree_id: tree.into(),
        created_unix: 0,
        entries: hashes
            .iter()
            .enumerate()
            .map(|(i, h)| Entry {
                rel: PathBuf::from(format!("f{i}")),
                hash: (*h).clone(),
                size: 1,
                mode: 0o644,
                mtime_ns: i as i128,
                ino: 1,
            })
            .collect(),
    }
}

fn no_grace() -> CollectOpts {
    CollectOpts {
        grace: Duration::ZERO,
    }
}

fn dir_bytes(root: &Path) -> u64 {
    walkdir::WalkDir::new(root.join("store"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().unwrap().len())
        .sum()
}

#[test]
fn keeps_base_of_live_patch() {
    let (dir, store) = open();
    let v1 = sample(200_000, 1);
    let mut v2 = v1.clone();
    v2[100] ^= 0xff;
    let (p1, h1) = write(dir.path(), "v1", &v1);
    let (p2, h2) = write(dir.path(), "v2", &v2);
    store.put(&p1, &h1, None, 4).unwrap();
    assert_eq!(
        store.put(&p2, &h2, Some(&h1), 4).unwrap().base,
        Some(h1.clone())
    );
    let c1 = store.write_manifest(&manifest("t", &[&h1])).unwrap();
    let c2 = store.write_manifest(&manifest("t", &[&h2])).unwrap();
    age_all(dir.path());

    collect_with(&store, std::slice::from_ref(&c2), false, &no_grace()).unwrap();
    assert!(store.has(&h1), "base of a live patch must stay");
    assert!(store.has(&h2));
    assert!(manifest_file(dir.path(), &c2).exists());
    assert!(!manifest_file(dir.path(), &c1).exists());
    assert_eq!(store.read(&h2).unwrap(), v2);
}

#[test]
fn removes_unreferenced_blobs_and_manifests() {
    let (dir, store) = open();
    let (pa, ha) = write(dir.path(), "a", &sample(100_000, 2));
    let (pb, hb) = write(dir.path(), "b", &sample(100_000, 3));
    store.put(&pa, &ha, None, 4).unwrap();
    store.put(&pb, &hb, None, 4).unwrap();
    let ca = store.write_manifest(&manifest("t", &[&ha])).unwrap();
    store.write_manifest(&manifest("u", &[&hb])).unwrap();
    age_all(dir.path());
    let before = dir_bytes(dir.path());
    let live_size = blob_files(dir.path(), &ha)
        .iter()
        .chain([&manifest_file(dir.path(), &ca)])
        .map(|p| fs::metadata(p).unwrap().len())
        .sum::<u64>();

    let freed = collect_with(&store, std::slice::from_ref(&ca), false, &no_grace()).unwrap();
    assert!(store.has(&ha));
    assert!(!store.has(&hb));
    assert!(!blob_files(dir.path(), &hb)[0].exists());
    assert_eq!(dir_bytes(dir.path()), live_size);
    assert_eq!(freed, before - live_size);

    // nothing live: everything goes
    collect_with(&store, &[], false, &no_grace()).unwrap();
    assert!(store.list().unwrap().is_empty());
    assert!(store.list_manifests().unwrap().is_empty());
}

#[test]
fn dry_run_removes_nothing() {
    let (dir, store) = open();
    let (pa, ha) = write(dir.path(), "a", &sample(100_000, 4));
    store.put(&pa, &ha, None, 4).unwrap();
    store.write_manifest(&manifest("t", &[&ha])).unwrap();
    age_all(dir.path());
    let before = dir_bytes(dir.path());
    let would = collect_with(&store, &[], true, &no_grace()).unwrap();
    assert_eq!(would, before);
    assert_eq!(dir_bytes(dir.path()), before);
    assert!(store.has(&ha));
}

#[test]
fn grace_keeps_young_unreferenced_blob() {
    let (dir, store) = open();
    let (pa, ha) = write(dir.path(), "a", &sample(100_000, 5));
    let (pb, hb) = write(dir.path(), "b", &sample(100_000, 6));
    store.put(&pa, &ha, None, 4).unwrap();
    store.put(&pb, &hb, None, 4).unwrap();
    let cb = store.write_manifest(&manifest("t", &[&hb])).unwrap();
    // a is old; b and its manifest are young.
    for p in blob_files(dir.path(), &ha) {
        age(&p, GC_GRACE * 2);
    }
    let opts = CollectOpts { grace: GC_GRACE };
    collect_with(&store, &[], false, &opts).unwrap();
    assert!(!store.has(&ha), "old unreferenced blob goes");
    assert!(store.has(&hb), "young blob stays");
    assert!(
        manifest_file(dir.path(), &cb).exists(),
        "young manifest stays"
    );
    // The default entry point uses GC_GRACE.
    collect(&store, &[], false).unwrap();
    assert!(store.has(&hb));
}

#[test]
fn young_manifest_keeps_its_old_blobs() {
    let (dir, store) = open();
    let (pa, ha) = write(dir.path(), "a", &sample(100_000, 7));
    store.put(&pa, &ha, None, 4).unwrap();
    let c = store.write_manifest(&manifest("t", &[&ha])).unwrap();
    for p in blob_files(dir.path(), &ha) {
        age(&p, GC_GRACE * 2);
    }
    collect_with(&store, &[], false, &CollectOpts { grace: GC_GRACE }).unwrap();
    assert!(store.has(&ha));
    assert!(manifest_file(dir.path(), &c).exists());
}

#[test]
fn young_blob_keeps_its_old_base() {
    let (dir, store) = open();
    let v1 = sample(200_000, 8);
    let mut v2 = v1.clone();
    v2[5] ^= 1;
    let (p1, h1) = write(dir.path(), "v1", &v1);
    let (p2, h2) = write(dir.path(), "v2", &v2);
    store.put(&p1, &h1, None, 4).unwrap();
    store.put(&p2, &h2, Some(&h1), 4).unwrap();
    for p in blob_files(dir.path(), &h1) {
        age(&p, GC_GRACE * 2);
    }
    collect_with(&store, &[], false, &CollectOpts { grace: GC_GRACE }).unwrap();
    assert!(store.has(&h1));
    assert!(store.has(&h2));
}

fn mtime(p: &Path) -> SystemTime {
    fs::metadata(p).unwrap().modified().unwrap()
}

#[test]
fn put_refreshes_reused_blob_mtime() {
    let (dir, store) = open();
    let (pa, ha) = write(dir.path(), "a", &sample(100_000, 9));
    store.put(&pa, &ha, None, 4).unwrap();
    for p in blob_files(dir.path(), &ha) {
        age(&p, GC_GRACE * 2);
    }
    store.put(&pa, &ha, None, 4).unwrap();
    for p in blob_files(dir.path(), &ha) {
        let since = SystemTime::now().duration_since(mtime(&p)).unwrap();
        assert!(since < Duration::from_secs(60), "{}", p.display());
    }
}

#[test]
fn put_refreshes_base_mtime() {
    let (dir, store) = open();
    let v1 = sample(200_000, 10);
    let mut v2 = v1.clone();
    v2[7] ^= 1;
    let (p1, h1) = write(dir.path(), "v1", &v1);
    let (p2, h2) = write(dir.path(), "v2", &v2);
    store.put(&p1, &h1, None, 4).unwrap();
    for p in blob_files(dir.path(), &h1) {
        age(&p, GC_GRACE * 2);
    }
    assert_eq!(
        store.put(&p2, &h2, Some(&h1), 4).unwrap().base,
        Some(h1.clone())
    );
    for p in blob_files(dir.path(), &h1) {
        let since = SystemTime::now().duration_since(mtime(&p)).unwrap();
        assert!(since < Duration::from_secs(60), "{}", p.display());
    }
}

#[test]
fn unreadable_manifest_removes_nothing() {
    let (dir, store) = open();
    let (pa, ha) = write(dir.path(), "a", &sample(100_000, 11));
    let (pb, hb) = write(dir.path(), "b", &sample(100_000, 12));
    store.put(&pa, &ha, None, 4).unwrap();
    store.put(&pb, &hb, None, 4).unwrap();
    let ca = store.write_manifest(&manifest("t", &[&ha])).unwrap();
    let cb = store.write_manifest(&manifest("u", &[&hb])).unwrap();
    fs::write(manifest_file(dir.path(), &cb), "{ not json").unwrap();
    age_all(dir.path());
    let before = dir_bytes(dir.path());

    let err = collect_with(&store, &[ca.clone(), cb.clone()], false, &no_grace()).unwrap_err();
    assert!(format!("{err:#}").contains(&cb), "{err:#}");
    assert_eq!(dir_bytes(dir.path()), before);
    assert!(store.has(&hb));

    // a live commitment with no manifest file fails closed too
    fs::remove_file(manifest_file(dir.path(), &cb)).unwrap();
    assert!(collect_with(&store, &[ca, cb], false, &no_grace()).is_err());
    assert!(store.has(&ha) && store.has(&hb));
}

#[test]
fn removes_index_of_ids_without_a_record() {
    let (dir, store) = open();
    let idx = dir.path().join("store/index");
    fs::create_dir_all(&idx).unwrap();
    fs::write(idx.join("aaaaaaaaaaaa.json"), "{}").unwrap();
    fs::write(idx.join("bbbbbbbbbbbb.json"), "{\"entries\":{}}").unwrap();
    let size = fs::metadata(idx.join("bbbbbbbbbbbb.json")).unwrap().len();
    let live = vec!["aaaaaaaaaaaa".to_string()];
    assert_eq!(collect_index(&store, &live, true).unwrap(), size);
    assert!(idx.join("bbbbbbbbbbbb.json").exists());
    assert_eq!(collect_index(&store, &live, false).unwrap(), size);
    assert!(idx.join("aaaaaaaaaaaa.json").exists());
    assert!(!idx.join("bbbbbbbbbbbb.json").exists());
}
