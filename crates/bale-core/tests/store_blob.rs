use std::fs;
use std::path::{Path, PathBuf};

use bale_core::store::{sha256_file, Hash, Store};
use bale_core::BaleHome;

/// Pseudo-random but compressible bytes: a seeded 4 KiB block repeated with
/// small mutations, so a full compress shrinks it but not to nothing.
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

fn count(dir: &Path, ext: &str) -> usize {
    walkdir::WalkDir::new(dir.join("store/objects"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == ext))
        .count()
}

#[test]
fn put_then_read_round_trips_and_dedups() {
    let (dir, store) = open();
    let data = sample(300_000, 1);
    let (p, h) = write(dir.path(), "a.bin", &data);
    let m = store.put(&p, &h, None, 8).unwrap();
    assert_eq!(
        (m.size, m.depth, m.base.clone()),
        (data.len() as u64, 0, None)
    );
    assert!(m.stored > 0 && m.stored < m.size);
    assert!(store.has(&h));
    assert_eq!(store.read(&h).unwrap(), data);
    assert_eq!(store.meta(&h).unwrap(), m);

    let (zst, json) = (count(dir.path(), "zst"), count(dir.path(), "json"));
    assert_eq!(store.put(&p, &h, None, 8).unwrap(), m);
    assert_eq!(
        (count(dir.path(), "zst"), count(dir.path(), "json")),
        (zst, json)
    );
    assert_eq!(store.list().unwrap(), vec![m.clone()]);

    let mut out = Vec::new();
    assert_eq!(store.read_to(&h, &mut out).unwrap(), data.len() as u64);
    assert_eq!(out, data);

    // Frame plus meta.
    assert!(store.remove(&h).unwrap() >= m.stored);
    assert!(!store.has(&h));
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn patch_chain_respects_max_depth() {
    let (dir, store) = open();
    let mut data = sample(400_000, 2);
    let mut prev: Option<Hash> = None;
    let mut seen = Vec::new();
    for i in 0..4 {
        data[i * 1000] ^= 0xFF;
        let (p, h) = write(dir.path(), &format!("v{i}.bin"), &data);
        let m = store.put(&p, &h, prev.as_ref(), 2).unwrap();
        seen.push((m.depth, m.base.clone()));
        assert_eq!(store.read(&h).unwrap(), data);
        prev = Some(h);
    }
    let hashes: Vec<_> = store.list().unwrap();
    assert_eq!(hashes.len(), 4);
    assert_eq!(seen[0], (0, None));
    assert_eq!(seen[1].0, 1);
    assert_eq!(seen[2].0, 2);
    assert!(seen[1].1.is_some() && seen[2].1.is_some());
    assert_eq!(seen[3], (0, None));
}

#[test]
fn read_detects_corruption() {
    let (dir, store) = open();
    let (p, h) = write(dir.path(), "a.bin", &sample(100_000, 3));
    store.put(&p, &h, None, 8).unwrap();
    let zst = dir
        .path()
        .join("store/objects")
        .join(&h[..2])
        .join(format!("{h}.zst"));
    let mut bytes = fs::read(&zst).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&zst, bytes).unwrap();
    let err = format!("{:#}", store.read(&h).unwrap_err());
    assert!(err.contains(&h), "{err}");
    let mut sink = Vec::new();
    assert!(store.read_to(&h, &mut sink).is_err());
}

#[test]
fn missing_base_stores_full() {
    let (dir, store) = open();
    let (p, h) = write(dir.path(), "a.bin", &sample(100_000, 4));
    let unknown = "ab".repeat(32);
    let m = store.put(&p, &h, Some(&unknown), 8).unwrap();
    assert_eq!((m.depth, m.base), (0, None));
}

/// Files over 64 MiB take the streaming path in both put and read_to.
#[test]
fn large_file_streams_with_and_without_base() {
    let (dir, store) = open();
    let len = 64 * 1024 * 1024 + 12_345;
    let mut data = sample(len, 5);
    let (p1, h1) = write(dir.path(), "big1.bin", &data);
    let m1 = store.put(&p1, &h1, None, 8).unwrap();
    assert_eq!((m1.size, m1.depth, m1.base.clone()), (len as u64, 0, None));
    let mut out = Vec::new();
    assert_eq!(store.read_to(&h1, &mut out).unwrap(), len as u64);
    assert!(out == data);

    data[len / 2] ^= 0xFF;
    let (p2, h2) = write(dir.path(), "big2.bin", &data);
    let m2 = store.put(&p2, &h2, Some(&h1), 8).unwrap();
    assert_eq!((m2.depth, m2.base), (1, Some(h1)));
    assert!(m2.stored < m1.stored / 2, "{} vs {}", m2.stored, m1.stored);
    out.clear();
    assert_eq!(store.read_to(&h2, &mut out).unwrap(), len as u64);
    assert!(out == data);
}

#[test]
fn put_rejects_content_that_does_not_match_its_hash() {
    let (dir, store) = open();
    let (p, h) = write(dir.path(), "a", &sample(2 << 20, 1));
    // The file changes after the caller hashed it.
    fs::write(&p, sample(2 << 20, 2)).unwrap();
    let err = store.put(&p, &h, None, 8).unwrap_err();
    assert!(
        err.to_string().contains("changed while it was stored"),
        "{err}"
    );
    assert!(!store.has(&h));
    assert_eq!(count(dir.path(), "zst"), 0);
}
