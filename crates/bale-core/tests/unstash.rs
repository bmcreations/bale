mod support;

use bale_core::lock::WorkspaceLock;
use bale_core::ops::stash::{stash, stash_with, StashOpts, StashOutcome};
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
    let d1 = ".dd/Build/Intermediates.noindex/dup1.bin";
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

const PRODUCT_DUP: &str = ".dd/Build/Products/Debug/App.app/dup.bin";
const INTERMEDIATE_DUP: &str = ".dd/Build/Intermediates.noindex/dup.bin";

/// A 2 MiB product under `Build/Products` and its identical twin among the
/// intermediates, stashed.
fn stashed_duplicates() -> (Fixture, PathBuf, String, Vec<u8>) {
    let (fx, wt, id) = setup();
    let bytes = noise(2 * MIB, 42);
    put(&wt, PRODUCT_DUP, &bytes);
    put(&wt, INTERMEDIATE_DUP, &bytes);
    set_file_mtime(
        wt.join(PRODUCT_DUP),
        FileTime::from_unix_time(1_700_000_200, 111_222_333),
    )
    .unwrap();
    fs::set_permissions(wt.join(PRODUCT_DUP), fs::Permissions::from_mode(0o751)).unwrap();
    do_stash(&fx.home(), &id);
    (fx, wt, id, bytes)
}

#[cfg(target_os = "macos")]
#[test]
fn duplicate_product_is_cloned() {
    let (fx, wt, id) = setup();
    let bytes = noise(2 * MIB, 42);
    put(&wt, PRODUCT_DUP, &bytes);
    put(&wt, INTERMEDIATE_DUP, &bytes);
    set_file_mtime(
        wt.join(PRODUCT_DUP),
        FileTime::from_unix_time(1_700_000_200, 111_222_333),
    )
    .unwrap();
    fs::set_permissions(wt.join(PRODUCT_DUP), fs::Permissions::from_mode(0o751)).unwrap();
    let want_product = snapshot(&wt.join(PRODUCT_DUP));
    let inter_ino = ino(&wt.join(INTERMEDIATE_DUP));
    let product_ino = ino(&wt.join(PRODUCT_DUP));
    do_stash(&fx.home(), &id);

    let r = restored(&fx.home(), &id);
    assert_eq!(r.files, 4);
    assert_eq!(r.bytes, (9 * MIB) as u64);
    assert_eq!((r.cloned_files, r.cloned_bytes), (1, (2 * MIB) as u64));
    let got = snapshot(&wt.join(PRODUCT_DUP));
    assert!(got.0 == bytes, "product bytes");
    assert_eq!(got.1, want_product.1, "mode");
    assert_eq!(got.2 .0, want_product.2 .0, "size");
    assert_eq!(got.2 .1, want_product.2 .1, "mtime_ns");
    assert_ne!(got.2 .2, product_ino, "the product is a new inode");
    assert_eq!(ino(&wt.join(INTERMEDIATE_DUP)), inter_ino);
    assert!(fs::read(wt.join(INTERMEDIATE_DUP)).unwrap() == bytes);
    assert_restored_state(&fx.home(), &id);
    // No temp files left behind.
    let dir = wt.join(PRODUCT_DUP).parent().unwrap().to_path_buf();
    assert_eq!(fs::read_dir(dir).unwrap().count(), 1);
}

#[test]
fn duplicate_outside_products_is_rewritten_in_place() {
    let (fx, wt, id) = setup();
    let bytes = noise(2 * MIB, 42);
    let d1 = ".dd/Build/Intermediates.noindex/x/dup.bin";
    put(&wt, d1, &bytes);
    put(&wt, INTERMEDIATE_DUP, &bytes);
    let inos = [ino(&wt.join(d1)), ino(&wt.join(INTERMEDIATE_DUP))];
    do_stash(&fx.home(), &id);
    let r = restored(&fx.home(), &id);
    assert_eq!((r.cloned_files, r.cloned_bytes), (0, 0));
    assert_eq!(r.files, 4);
    assert_eq!([ino(&wt.join(d1)), ino(&wt.join(INTERMEDIATE_DUP))], inos);
    assert!(fs::read(wt.join(d1)).unwrap() == bytes);
}

#[test]
fn products_without_a_twin_outside_products_are_rewritten_in_place() {
    let (fx, wt, id) = setup();
    let bytes = noise(2 * MIB, 42);
    let other = ".dd/Build/Products/Debug/Other.app/dup.bin";
    put(&wt, PRODUCT_DUP, &bytes);
    put(&wt, other, &bytes);
    let inos = [ino(&wt.join(PRODUCT_DUP)), ino(&wt.join(other))];
    do_stash(&fx.home(), &id);
    let r = restored(&fx.home(), &id);
    assert_eq!(r.cloned_bytes, 0);
    assert_eq!([ino(&wt.join(PRODUCT_DUP)), ino(&wt.join(other))], inos);
}

/// The twin was rebuilt while the tree was stashed, so it is not a source;
/// the product is decoded from the store and keeps its inode.
#[test]
fn clone_falls_back_when_source_is_not_restored() {
    let (fx, wt, id, bytes) = stashed_duplicates();
    let product_ino = ino(&wt.join(PRODUCT_DUP));
    fs::write(wt.join(INTERMEDIATE_DUP), b"rebuilt").unwrap();

    let r = restored(&fx.home(), &id);
    assert_eq!(r.kept_newer, 1);
    assert_eq!((r.cloned_files, r.cloned_bytes), (0, 0));
    assert!(fs::read(wt.join(PRODUCT_DUP)).unwrap() == bytes);
    assert_eq!(ino(&wt.join(PRODUCT_DUP)), product_ino);
    assert_eq!(fs::read(wt.join(INTERMEDIATE_DUP)).unwrap(), b"rebuilt");
}

/// Both copies share one blob, so when it is lost the product's fallback
/// fails too and the usual lost-blob handling applies to both.
#[test]
fn clone_falls_back_when_source_lost() {
    let (fx, wt, id, _) = stashed_duplicates();
    corrupt_blob(&fx.home(), &id, INTERMEDIATE_DUP);
    let err = unstash(&fx.home(), &id).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains(PRODUCT_DUP) && msg.contains(INTERMEDIATE_DUP),
        "{msg}"
    );
    assert_eq!(len(&wt.join(PRODUCT_DUP)), 0);
    assert_eq!(len(&wt.join(INTERMEDIATE_DUP)), 0);
    let m = manifest(&fx.home(), &id);
    assert!(!m.entries.iter().any(|e| e.rel == Path::new(PRODUCT_DUP)));
    assert_restored_state(&fx.home(), &id);
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
    // The other file is restored, the lost one leaves the manifest, and the
    // tree is no longer mid-restore: the next prompt has nothing to do.
    assert_eq!(len(&wt.join(B)), (3 * MIB) as u64);
    let m = manifest(&fx.home(), &id);
    assert_eq!(m.entries.len(), 1);
    assert_eq!(m.entries[0].rel, Path::new(B));
    assert_restored_state(&fx.home(), &id);
    assert!(unstash(&fx.home(), &id).unwrap().is_none());
}

/// Corrupts the stored blob of the manifest entry at `rel`.
fn corrupt_blob(home: &BaleHome, id: &str, rel: &str) {
    let e = manifest(home, id)
        .entries
        .into_iter()
        .find(|e| e.rel == Path::new(rel))
        .unwrap();
    let zst = walkdir::WalkDir::new(home.0.join("store/objects"))
        .into_iter()
        .flatten()
        .find(|f| f.file_name().to_str() == Some(&format!("{}.zst", e.hash)))
        .unwrap()
        .into_path();
    let mut frame = fs::read(&zst).unwrap();
    let mid = frame.len() / 2;
    for b in &mut frame[mid..mid + 64] {
        *b ^= 0x5A;
    }
    fs::write(&zst, frame).unwrap();
}

/// A lost blob must not keep the tree mid-restore, or every later prompt
/// would overwrite what the build wrote meanwhile.
#[test]
fn lost_blob_does_not_clobber_rebuilt_file() {
    let (fx, wt, id) = setup();
    let b_old = fs::read(wt.join(B)).unwrap();
    do_stash(&fx.home(), &id);
    corrupt_blob(&fx.home(), &id, A);
    assert!(unstash(&fx.home(), &id).is_err());
    assert!(
        fs::read(wt.join(B)).unwrap() == b_old,
        "B restored by run 1"
    );
    // A build now rewrites B in place.
    let new_b = noise(MIB, 77);
    let mut f = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(wt.join(B))
        .unwrap();
    f.write_all(&new_b).unwrap();
    drop(f);
    assert!(unstash(&fx.home(), &id).unwrap().is_none());
    assert!(
        fs::read(wt.join(B)).unwrap() == new_b,
        "build's B overwritten"
    );
}

/// A prompt that arrives while a stash holds the lock (hashing, record not
/// yet moved) waits for it instead of reporting nothing to do, and asks the
/// stash to stop.
#[test]
fn prompt_waits_for_a_stash_holding_the_lock() {
    let (fx, _wt, id) = setup();
    let (tx, rx) = std::sync::mpsc::channel();
    let home = fx.home();
    let lock_id = id.clone();
    let holder = std::thread::spawn(move || {
        let lock = WorkspaceLock::try_acquire(&home, &lock_id)
            .unwrap()
            .unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        lock.requested()
    });
    rx.recv().unwrap();
    let t = Instant::now();
    assert!(unstash(&fx.home(), &id).unwrap().is_none());
    assert!(t.elapsed() >= Duration::from_millis(250), "did not wait");
    assert!(holder.join().unwrap(), "the holder was not asked to stop");
}

/// A stash that a prompt asks to stop gives up before committing, leaving
/// every file whole.
#[test]
fn stash_stops_for_a_waiting_prompt() {
    let (fx, wt, id) = setup();
    let home = fx.home();
    let waiter = std::sync::Mutex::new(None);
    let prompt = || {
        let (home, id) = (home.clone(), id.clone());
        *waiter.lock().unwrap() = Some(std::thread::spawn(move || unstash(&home, &id)));
        std::thread::sleep(Duration::from_millis(200));
    };
    let opts = StashOpts {
        before_commit: Some(&prompt),
        ..Default::default()
    };
    let out = stash_with(&fx.home(), &id, &[], &opts).unwrap();
    assert!(
        matches!(&out, StashOutcome::Skipped(s) if s == "prompt waiting"),
        "{out:?}"
    );
    let r = waiter.lock().unwrap().take().unwrap().join().unwrap();
    assert!(r.unwrap().is_none());
    assert_eq!(len(&wt.join(A)), (2 * MIB) as u64);
    assert_eq!(len(&wt.join(B)), (3 * MIB) as u64);
    assert!(record(&fx.home(), &id).stash.is_none());
}

/// After a stash committed its manifest but stopped before truncating, the
/// files are whole; unstash drops them without calling them newer.
#[test]
fn never_stubbed_files_are_not_counted_newer() {
    let (fx, wt, id) = setup();
    let before = [snapshot(&wt.join(A)), snapshot(&wt.join(B))];
    let opts = StashOpts {
        fail_before_truncate: true,
        ..Default::default()
    };
    assert!(stash_with(&fx.home(), &id, &[], &opts).is_err());
    assert!(!record(&fx.home(), &id).stash.unwrap().restored);

    let r = restored(&fx.home(), &id);
    assert_eq!((r.files, r.kept_newer, r.recreated), (0, 0, 0));
    assert!([snapshot(&wt.join(A)), snapshot(&wt.join(B))] == before);
    assert_restored_state(&fx.home(), &id);
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
