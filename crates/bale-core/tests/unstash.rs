mod support;

use bale_core::lock::WorkspaceLock;
use bale_core::ops::stash::{stash, StashOutcome};
use bale_core::ops::unstash::{unstash, unstash_path, unstash_with, UnstashOpts, UnstashReport};
use bale_core::ops::up::register;
use bale_core::registry::{Record, Registry};
use bale_core::store::index::stat_of;
use bale_core::store::manifest::StashManifest;
use bale_core::store::Store;
use bale_core::BaleHome;
use filetime::{set_file_mtime, FileTime};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use support::{write_plist, Fixture};

const MIB: usize = 1 << 20;

/// Deterministic, poorly compressible bytes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

const A: &str = ".dd/Build/Products/Debug/a.bin";
const B: &str = ".dd/Build/Intermediates.noindex/b.o";

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

/// An Xcode worktree whose ignored `.dd` is a DerivedData root holding a
/// 2 MiB `A` and a 3 MiB `B`, each with an mtime that has nanoseconds.
fn setup() -> (Fixture, PathBuf, String) {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), ".dd/\n").unwrap();
    fs::create_dir_all(wt.join("App.xcodeproj")).unwrap();
    write_plist(&wt.join(".dd"), Some(&wt.join("App.xcodeproj")));
    put(&wt, A, &noise(2 * MIB, 1));
    put(&wt, B, &noise(3 * MIB, 2));
    set_file_mtime(
        wt.join(A),
        FileTime::from_unix_time(1_700_000_000, 123_456_789),
    )
    .unwrap();
    set_file_mtime(
        wt.join(B),
        FileTime::from_unix_time(1_700_000_100, 987_654_321),
    )
    .unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    (fx, wt, id)
}

fn len(p: &Path) -> u64 {
    fs::metadata(p).unwrap().len()
}

fn ino(p: &Path) -> u64 {
    fs::metadata(p).unwrap().ino()
}

fn record(home: &BaleHome, id: &str) -> Record {
    Registry::open(home.clone()).get(id).unwrap().unwrap()
}

fn manifest(home: &BaleHome, id: &str) -> StashManifest {
    let c = record(home, id).stash.unwrap().commitment;
    Store::open(home).unwrap().read_manifest(&c).unwrap()
}

fn do_stash(home: &BaleHome, id: &str) {
    match stash(home, id, &[]).unwrap() {
        StashOutcome::Stashed(_) => {}
        StashOutcome::Skipped(s) => panic!("skipped: {s}"),
    }
}

fn restored(home: &BaleHome, id: &str) -> UnstashReport {
    unstash(home, id).unwrap().expect("tree was stashed")
}

/// Bytes, permission bits and `(size, mtime_ns, ino)`.
fn snapshot(p: &Path) -> (Vec<u8>, u32, (u64, i128, u64)) {
    let meta = fs::metadata(p).unwrap();
    (
        fs::read(p).unwrap(),
        meta.permissions().mode() & 0o7777,
        stat_of(&meta),
    )
}

fn assert_restored_state(home: &BaleHome, id: &str) {
    let st = record(home, id).stash.unwrap();
    assert!(st.restored && !st.restoring, "{st:?}");
}

#[test]
fn round_trip_restores_bytes_mode_mtime_and_inode() {
    let (fx, wt, id) = setup();
    fs::set_permissions(wt.join(B), fs::Permissions::from_mode(0o750)).unwrap();
    let before = [snapshot(&wt.join(A)), snapshot(&wt.join(B))];
    do_stash(&fx.home(), &id);
    assert_eq!([len(&wt.join(A)), len(&wt.join(B))], [0, 0]);

    let r = restored(&fx.home(), &id);
    assert_eq!(r.files, 2);
    assert_eq!(r.bytes, (5 * MIB) as u64);
    assert_eq!((r.kept_newer, r.recreated), (0, 0));
    let after = [snapshot(&wt.join(A)), snapshot(&wt.join(B))];
    for (b, a) in before.iter().zip(&after) {
        assert!(b.0 == a.0, "bytes differ");
        assert_eq!(b.1, a.1, "mode");
        assert_eq!(b.2, a.2, "(size, mtime_ns, ino)");
    }
    assert_restored_state(&fx.home(), &id);
    // Nothing left to do.
    assert!(unstash(&fx.home(), &id).unwrap().is_none());
}

#[test]
fn read_only_file_round_trips() {
    let (fx, wt, id) = setup();
    fs::set_permissions(wt.join(A), fs::Permissions::from_mode(0o444)).unwrap();
    let before = snapshot(&wt.join(A));
    do_stash(&fx.home(), &id);
    restored(&fx.home(), &id);
    let after = snapshot(&wt.join(A));
    assert_eq!(after.1, 0o444);
    assert!(before == after);
}

#[test]
fn duplicate_content_keeps_both_inodes() {
    let (fx, wt, id) = setup();
    let d1 = ".dd/Build/Products/Debug/dup1.bin";
    let d2 = ".dd/Build/Intermediates.noindex/sub/dup2.bin";
    let bytes = noise(2 * MIB, 42);
    put(&wt, d1, &bytes);
    put(&wt, d2, &bytes);
    let inos = [ino(&wt.join(d1)), ino(&wt.join(d2))];
    do_stash(&fx.home(), &id);
    assert_eq!([len(&wt.join(d1)), len(&wt.join(d2))], [0, 0]);

    let r = restored(&fx.home(), &id);
    assert_eq!(r.files, 4);
    assert_eq!([ino(&wt.join(d1)), ino(&wt.join(d2))], inos);
    assert!(fs::read(wt.join(d1)).unwrap() == bytes);
    assert!(fs::read(wt.join(d2)).unwrap() == bytes);
}

#[test]
fn newer_file_at_path_is_kept() {
    let (fx, wt, id) = setup();
    do_stash(&fx.home(), &id);
    // A: a build wrote a new file and renamed it over the stub (new inode).
    let new_a = noise(2 * MIB, 77);
    let tmp = wt.join(".dd/Build/Products/Debug/a.bin.tmp");
    fs::write(&tmp, &new_a).unwrap();
    fs::rename(&tmp, wt.join(A)).unwrap();
    // B: a build wrote into the stub in place (same inode, not empty).
    let new_b = noise(MIB, 78);
    let b_ino = ino(&wt.join(B));
    OpenOptions::new()
        .write(true)
        .open(wt.join(B))
        .unwrap()
        .write_all(&new_b)
        .unwrap();
    assert_eq!(ino(&wt.join(B)), b_ino);

    let r = restored(&fx.home(), &id);
    assert_eq!(r.kept_newer, 2);
    assert_eq!(r.files, 0);
    assert!(fs::read(wt.join(A)).unwrap() == new_a);
    assert!(fs::read(wt.join(B)).unwrap() == new_b);
    assert_restored_state(&fx.home(), &id);
}

#[test]
fn missing_file_is_recreated() {
    let (fx, wt, id) = setup();
    let before = snapshot(&wt.join(A));
    do_stash(&fx.home(), &id);
    fs::remove_dir_all(wt.join(".dd/Build/Products")).unwrap();

    let r = restored(&fx.home(), &id);
    assert_eq!(r.recreated, 1);
    assert_eq!(r.files, 2);
    let after = snapshot(&wt.join(A));
    assert!(after.0 == before.0);
    assert_eq!(after.1, before.1);
    // Same size and mtime; the inode is new.
    assert_eq!((after.2 .0, after.2 .1), (before.2 .0, before.2 .1));
}

#[test]
fn rerun_after_partial_restore_completes() {
    let (fx, wt, id) = setup();
    let originals: Vec<(PathBuf, Vec<u8>, u64)> = [A, B]
        .iter()
        .map(|r| {
            (
                PathBuf::from(r),
                fs::read(wt.join(r)).unwrap(),
                ino(&wt.join(r)),
            )
        })
        .collect();
    do_stash(&fx.home(), &id);
    let m = manifest(&fx.home(), &id);
    assert_eq!(m.entries.len(), 2);

    let opts = UnstashOpts {
        fail_after: Some(1),
        ..Default::default()
    };
    assert!(unstash_with(&fx.home(), &id, &opts).is_err());
    let half = &m.entries[1];
    let n = len(&wt.join(&half.rel));
    assert!(n > 0 && n < half.size, "half-written: {n} of {}", half.size);
    let st = record(&fx.home(), &id).stash.unwrap();
    assert!(st.restoring && !st.restored);

    let r = restored(&fx.home(), &id);
    assert_eq!(r.kept_newer, 0);
    for (rel, bytes, i) in &originals {
        assert!(
            fs::read(wt.join(rel)).unwrap() == *bytes,
            "{}",
            rel.display()
        );
        assert_eq!(ino(&wt.join(rel)), *i);
        let e = m.entries.iter().find(|e| &e.rel == rel).unwrap();
        assert_eq!(stat_of(&fs::metadata(wt.join(rel)).unwrap()).1, e.mtime_ns);
    }
    assert_restored_state(&fx.home(), &id);
}

/// A file a build wrote into in place while stashed must survive a restore
/// that stops part way and the rerun that finishes it.
#[test]
fn newer_file_survives_rerun_after_partial_restore() {
    let (fx, wt, id) = setup();
    let a_bytes = fs::read(wt.join(A)).unwrap();
    do_stash(&fx.home(), &id);
    let new_b = noise(MIB, 78);
    OpenOptions::new()
        .write(true)
        .open(wt.join(B))
        .unwrap()
        .write_all(&new_b)
        .unwrap();

    // B leaves the manifest, so A is its only entry.
    let opts = UnstashOpts {
        fail_after: Some(0),
        ..Default::default()
    };
    assert!(unstash_with(&fx.home(), &id, &opts).is_err());
    let m = manifest(&fx.home(), &id);
    assert_eq!(m.entries.len(), 1);
    assert_eq!(m.entries[0].rel, Path::new(A));
    assert!(len(&wt.join(A)) > 0 && len(&wt.join(A)) < (2 * MIB) as u64);

    restored(&fx.home(), &id);
    assert!(fs::read(wt.join(A)).unwrap() == a_bytes);
    assert!(fs::read(wt.join(B)).unwrap() == new_b);
    assert_restored_state(&fx.home(), &id);
    // Only the current manifest is left.
    let store = Store::open(&fx.home()).unwrap();
    assert_eq!(store.list_manifests().unwrap().len(), 1);
}

#[test]
fn corrupt_blob_errs_and_leaves_stub() {
    let (fx, wt, id) = setup();
    do_stash(&fx.home(), &id);
    let a = manifest(&fx.home(), &id)
        .entries
        .into_iter()
        .find(|e| e.rel == Path::new(A))
        .unwrap();
    let zst = walkdir::WalkDir::new(fx.home().0.join("store/objects"))
        .into_iter()
        .flatten()
        .find(|e| e.file_name().to_str() == Some(&format!("{}.zst", a.hash)))
        .expect("A's blob")
        .into_path();
    let mut frame = fs::read(&zst).unwrap();
    let mid = frame.len() / 2;
    for b in &mut frame[mid..mid + 64] {
        *b ^= 0x5A;
    }
    fs::write(&zst, frame).unwrap();

    let err = unstash(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains(A), "{err:#}");
    assert_eq!(len(&wt.join(A)), 0);
    assert_eq!(ino(&wt.join(A)), a.ino);
    let st = record(&fx.home(), &id).stash.unwrap();
    assert!(st.restoring && !st.restored);
}

#[test]
fn waits_for_held_lock() {
    let (fx, wt, id) = setup();
    do_stash(&fx.home(), &id);
    let (tx, rx) = std::sync::mpsc::channel();
    let home = fx.home();
    let lock_id = id.clone();
    let holder = std::thread::spawn(move || {
        let lock = WorkspaceLock::try_acquire(&home, &lock_id)
            .unwrap()
            .unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        drop(lock);
    });
    rx.recv().unwrap();
    let start = Instant::now();
    let r = restored(&fx.home(), &id);
    assert!(
        start.elapsed() >= Duration::from_millis(250),
        "did not wait"
    );
    assert_eq!(r.files, 2);
    assert_eq!(len(&wt.join(A)), (2 * MIB) as u64);
    holder.join().unwrap();
}

#[test]
fn not_stashed_is_none() {
    let (fx, _wt, id) = setup();
    assert!(unstash(&fx.home(), &id).unwrap().is_none());
}

#[test]
fn unstash_path_finds_tree_from_subdir() {
    let (fx, wt, id) = setup();
    do_stash(&fx.home(), &id);
    let sub = wt.join(".dd/Build");
    let r = unstash_path(&fx.home(), &sub)
        .unwrap()
        .expect("found the tree");
    assert_eq!(r.files, 2);
    assert_eq!(len(&wt.join(A)), (2 * MIB) as u64);
    assert_restored_state(&fx.home(), &id);
    // A path in no tree.
    let outside = tempfile::tempdir().unwrap();
    assert!(unstash_path(&fx.home(), outside.path()).unwrap().is_none());
}
