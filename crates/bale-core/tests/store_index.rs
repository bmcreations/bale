use std::fs;
use std::path::Path;

use bale_core::store::index::Index;
use bale_core::store::{sha256_file, Store};
use bale_core::BaleHome;
use filetime::{set_file_mtime, FileTime};
use tempfile::tempdir;

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    fs::write(root.join(rel), bytes).unwrap();
}

#[test]
fn unchanged_file_is_not_rehashed() {
    let dir = tempdir().unwrap();
    let rel = Path::new("a.bin");
    write(dir.path(), "a.bin", b"aaaa");
    let ft = FileTime::from_unix_time(1_700_000_000, 123_456_789);
    set_file_mtime(dir.path().join(rel), ft).unwrap();

    let mut index = Index::default();
    let first = index.hash(dir.path(), rel).unwrap();

    // Same size, same inode (rewrite in place), mtime restored: stat is trusted.
    write(dir.path(), "a.bin", b"bbbb");
    set_file_mtime(dir.path().join(rel), ft).unwrap();
    assert_eq!(index.hash(dir.path(), rel).unwrap(), first);
}

#[test]
fn changed_mtime_rehashes() {
    let dir = tempdir().unwrap();
    let rel = Path::new("a.bin");
    write(dir.path(), "a.bin", b"aaaa");
    let mut index = Index::default();
    let first = index.hash(dir.path(), rel).unwrap();

    write(dir.path(), "a.bin", b"bbbb");
    set_file_mtime(
        dir.path().join(rel),
        FileTime::from_unix_time(1_600_000_000, 1),
    )
    .unwrap();
    let second = index.hash(dir.path(), rel).unwrap();
    assert_ne!(first, second);
    assert_eq!(second, sha256_file(&dir.path().join(rel)).unwrap());
}

#[test]
fn index_survives_save_and_load() {
    let dir = tempdir().unwrap();
    let home = tempdir().unwrap();
    let store = Store::open(&BaleHome(home.path().to_path_buf())).unwrap();
    let rel = Path::new("a.bin");
    write(dir.path(), "a.bin", b"aaaa");

    let mut index = Index::load(&store, "tree1").unwrap();
    let hash = index.hash(dir.path(), rel).unwrap();
    index.save(&store, "tree1").unwrap();

    let mut loaded = Index::load(&store, "tree1").unwrap();
    // Trusted stat: a same-stat content swap proves the entry came from disk.
    let ft = FileTime::from_last_modification_time(&fs::metadata(dir.path().join(rel)).unwrap());
    write(dir.path(), "a.bin", b"bbbb");
    set_file_mtime(dir.path().join(rel), ft).unwrap();
    assert_eq!(loaded.hash(dir.path(), rel).unwrap(), hash);
    assert!(Index::load(&store, "other").unwrap().is_empty());
}

#[test]
fn forget_missing_drops_unlisted_paths() {
    let dir = tempdir().unwrap();
    write(dir.path(), "a.bin", b"a");
    write(dir.path(), "b.bin", b"b");
    let mut index = Index::default();
    index.hash(dir.path(), Path::new("a.bin")).unwrap();
    index.hash(dir.path(), Path::new("b.bin")).unwrap();
    let keep = [Path::new("a.bin").to_path_buf()].into_iter().collect();
    index.forget_missing(&keep);
    assert_eq!(index.len(), 1);
}
