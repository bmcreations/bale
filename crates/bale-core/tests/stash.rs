mod support;

use bale_core::ops::stash::{stash, stash_with, StashOpts, StashOutcome, StashReport};
use bale_core::ops::up::register;
use bale_core::registry::{Record, Registry};
use bale_core::store::manifest::StashManifest;
use bale_core::store::Store;
use bale_core::BaleHome;
use filetime::{set_file_mtime, FileTime};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
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
const SMALL: &str = ".dd/Build/small.txt";
const PKG: &str = ".dd/SourcePackages/checkouts/pkg.bin";
const SRC: &str = "src/big.dat";

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

/// An Xcode worktree whose ignored `.dd` is a DerivedData root holding two
/// large build files (a 2 MiB `A`, a 3 MiB `B`), a small one, a large
/// SourcePackages file, plus a large source file outside any build dir.
fn xcode_worktree(fx: &Fixture) -> PathBuf {
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), ".dd/\n").unwrap();
    fs::create_dir_all(wt.join("App.xcodeproj")).unwrap();
    write_plist(&wt.join(".dd"), Some(&wt.join("App.xcodeproj")));
    put(&wt, A, &noise(2 * MIB, 1));
    put(&wt, B, &noise(3 * MIB, 2));
    put(&wt, SMALL, &noise(4096, 3));
    put(&wt, PKG, &noise(2 * MIB, 4));
    put(&wt, SRC, &noise(5 * MIB, 5));
    wt
}

fn setup() -> (Fixture, PathBuf, String) {
    let fx = Fixture::new();
    let wt = xcode_worktree(&fx);
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

fn stashed(out: StashOutcome) -> StashReport {
    match out {
        StashOutcome::Stashed(r) => r,
        StashOutcome::Skipped(s) => panic!("skipped: {s}"),
    }
}

#[test]
fn stashes_only_large_build_files() {
    let (fx, wt, id) = setup();
    let inos = [ino(&wt.join(A)), ino(&wt.join(B))];

    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 2);
    assert_eq!(r.bytes, (5 * MIB) as u64);
    assert!(r.stored_bytes > 0);
    assert_eq!(r.reused, 0);

    assert_eq!([len(&wt.join(A)), len(&wt.join(B))], [0, 0]);
    assert_eq!([ino(&wt.join(A)), ino(&wt.join(B))], inos);
    assert_eq!(len(&wt.join(SMALL)), 4096);
    assert_eq!(len(&wt.join(PKG)), (2 * MIB) as u64);
    assert_eq!(len(&wt.join(SRC)), (5 * MIB) as u64);

    let st = record(&fx.home(), &id).stash.unwrap();
    assert!(!st.restored && !st.restoring);
    let m = manifest(&fx.home(), &id);
    let mut rels: Vec<_> = m.entries.iter().map(|e| e.rel.clone()).collect();
    rels.sort();
    assert_eq!(rels, vec![PathBuf::from(B), PathBuf::from(A)]);
    let a = m.entries.iter().find(|e| e.rel == Path::new(A)).unwrap();
    assert_eq!((a.size, a.ino), ((2 * MIB) as u64, inos[0]));

    // Everything is stubbed: nothing left to do.
    let again = stash(&fx.home(), &id, &[]).unwrap();
    assert!(
        matches!(&again, StashOutcome::Skipped(s) if s == "already stashed"),
        "{again:?}"
    );
}

/// Writes each manifest entry's original bytes back into its stub in place
/// and restores mode and mtime, as unstash will; then marks the stash
/// restored.
fn simulate_unstash(home: &BaleHome, id: &str, wt: &Path, originals: &[(&str, Vec<u8>)]) {
    let m = manifest(home, id);
    for e in &m.entries {
        let Some((_, bytes)) = originals.iter().find(|(r, _)| Path::new(r) == e.rel) else {
            continue;
        };
        let p = wt.join(&e.rel);
        let mut f = OpenOptions::new().write(true).open(&p).unwrap();
        f.write_all(bytes).unwrap();
        drop(f);
        fs::set_permissions(&p, fs::Permissions::from_mode(e.mode & 0o7777)).unwrap();
        let secs = e.mtime_ns.div_euclid(1_000_000_000) as i64;
        let ns = e.mtime_ns.rem_euclid(1_000_000_000) as u32;
        set_file_mtime(&p, FileTime::from_unix_time(secs, ns)).unwrap();
        assert_eq!(ino(&p), e.ino);
    }
    let mut rec = record(home, id);
    rec.stash.as_mut().unwrap().restored = true;
    Registry::open(home.clone()).put(&rec).unwrap();
}

#[test]
fn second_stash_patches_against_first() {
    let (fx, wt, id) = setup();
    let originals = [
        (A, fs::read(wt.join(A)).unwrap()),
        (B, fs::read(wt.join(B)).unwrap()),
    ];
    stashed(stash(&fx.home(), &id, &[]).unwrap());
    let first = record(&fx.home(), &id).stash.unwrap().commitment;
    let old_a = manifest(&fx.home(), &id)
        .entries
        .into_iter()
        .find(|e| e.rel == Path::new(A))
        .unwrap()
        .hash;

    simulate_unstash(&fx.home(), &id, &wt, &originals);
    let mut f = OpenOptions::new().write(true).open(wt.join(A)).unwrap();
    f.seek(SeekFrom::Start(4096)).unwrap();
    f.write_all(&[0xAB; 100]).unwrap();
    drop(f);

    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 2);
    assert_eq!(r.reused, 1, "B is unchanged");
    let store = Store::open(&fx.home()).unwrap();
    let new_a = manifest(&fx.home(), &id)
        .entries
        .into_iter()
        .find(|e| e.rel == Path::new(A))
        .unwrap()
        .hash;
    assert_ne!(new_a, old_a);
    let meta = store.meta(&new_a).unwrap();
    assert_eq!(meta.base, Some(old_a));
    assert_eq!(meta.depth, 1);
    assert!(meta.stored < 64 * 1024, "patch is small: {}", meta.stored);
    // The base manifest is gone; its blobs stay for GC.
    assert!(!store.list_manifests().unwrap().contains(&first));
}

#[cfg(unix)]
struct KillOnDrop(std::process::Child);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[test]
fn refuses_busy_tree() {
    let (fx, wt, id) = setup();
    let held = OpenOptions::new().append(true).open(wt.join(A)).unwrap();
    let _child = KillOnDrop(
        std::process::Command::new("sleep")
            .arg("30")
            .stdout(held)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let out = stash(&fx.home(), &id, &[]).unwrap();
    assert!(
        matches!(&out, StashOutcome::Skipped(s) if s.starts_with("busy: ")),
        "{out:?}"
    );
    assert_eq!(len(&wt.join(A)), (2 * MIB) as u64);
    assert_eq!(len(&wt.join(B)), (3 * MIB) as u64);
    assert!(record(&fx.home(), &id).stash.is_none());
}

#[cfg(unix)]
#[test]
fn build_starting_mid_stash_stops_truncation() {
    let (fx, wt, id) = setup();
    let child = std::sync::Mutex::new(None);
    let start_build = || {
        let held = OpenOptions::new().append(true).open(wt.join(B)).unwrap();
        let c = std::process::Command::new("sleep")
            .arg("30")
            .stdout(held)
            .spawn()
            .unwrap();
        *child.lock().unwrap() = Some(KillOnDrop(c));
        std::thread::sleep(std::time::Duration::from_millis(300));
    };
    let opts = StashOpts {
        before_truncate: Some(&start_build),
        ..Default::default()
    };
    let out = stash_with(&fx.home(), &id, &[], &opts).unwrap();
    assert!(
        matches!(&out, StashOutcome::Skipped(s) if s.starts_with("busy: ")),
        "{out:?}"
    );
    assert_eq!(len(&wt.join(A)), (2 * MIB) as u64);
    assert_eq!(len(&wt.join(B)), (3 * MIB) as u64);
    // The record points at the manifest; once the build is gone a rerun
    // finishes the stash.
    manifest(&fx.home(), &id);
    drop(child.lock().unwrap().take());
    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 2);
    assert_eq!([len(&wt.join(A)), len(&wt.join(B))], [0, 0]);
}

#[test]
fn crash_before_delete_leaves_files_and_rerun_completes() {
    let (fx, wt, id) = setup();
    let opts = StashOpts {
        fail_before_truncate: true,
        ..Default::default()
    };
    assert!(stash_with(&fx.home(), &id, &[], &opts).is_err());
    assert_eq!(len(&wt.join(A)), (2 * MIB) as u64);
    assert_eq!(len(&wt.join(B)), (3 * MIB) as u64);
    let st = record(&fx.home(), &id)
        .stash
        .expect("record points at the manifest");
    assert!(!st.restored);
    // The record never points at a missing manifest.
    manifest(&fx.home(), &id);

    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 2);
    assert_eq!([len(&wt.join(A)), len(&wt.join(B))], [0, 0]);
}

#[test]
fn crash_mid_truncate_rerun_finishes() {
    let (fx, wt, id) = setup();
    let opts = StashOpts {
        fail_before_truncate: true,
        ..Default::default()
    };
    assert!(stash_with(&fx.home(), &id, &[], &opts).is_err());
    // As if the crash came after A was truncated.
    let a_ino = ino(&wt.join(A));
    OpenOptions::new()
        .write(true)
        .open(wt.join(A))
        .unwrap()
        .set_len(0)
        .unwrap();

    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 1);
    assert_eq!(r.bytes, (3 * MIB) as u64);
    assert_eq!([len(&wt.join(A)), len(&wt.join(B))], [0, 0]);
    assert_eq!(ino(&wt.join(A)), a_ino);
    assert_eq!(manifest(&fx.home(), &id).entries.len(), 2);
}

#[test]
fn file_changed_during_stash_is_kept() {
    let (fx, wt, id) = setup();
    let rewritten = noise(3 * MIB, 99);
    let rewrite = || {
        let p = wt.join(B);
        let mut f = OpenOptions::new().write(true).open(&p).unwrap();
        f.write_all(&rewritten).unwrap();
        drop(f);
        set_file_mtime(&p, FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
    };
    let opts = StashOpts {
        before_truncate: Some(&rewrite),
        ..Default::default()
    };
    let r = stashed(stash_with(&fx.home(), &id, &[], &opts).unwrap());
    assert_eq!(r.files, 1);
    assert_eq!(len(&wt.join(A)), 0);
    assert_eq!(fs::read(wt.join(B)).unwrap(), rewritten);

    let m = manifest(&fx.home(), &id);
    assert_eq!(m.entries.len(), 1);
    assert_eq!(m.entries[0].rel, Path::new(A));
    // The superseded manifest is removed once the record moved off it.
    let store = Store::open(&fx.home()).unwrap();
    assert_eq!(store.list_manifests().unwrap().len(), 1);
}

#[test]
fn read_only_file_keeps_its_mode() {
    let (fx, wt, id) = setup();
    fs::set_permissions(wt.join(A), fs::Permissions::from_mode(0o444)).unwrap();
    let a_ino = ino(&wt.join(A));
    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 2);
    let meta = fs::metadata(wt.join(A)).unwrap();
    assert_eq!(meta.len(), 0);
    assert_eq!(meta.ino(), a_ino);
    assert_eq!(meta.permissions().mode() & 0o7777, 0o444);
    let e = manifest(&fx.home(), &id)
        .entries
        .into_iter()
        .find(|e| e.rel == Path::new(A))
        .unwrap();
    assert_eq!(e.mode & 0o7777, 0o444);
}

#[test]
fn links_are_not_stashed() {
    let (fx, wt, id) = setup();
    let dir = wt.join(".dd/Build/Products/Debug");
    fs::hard_link(dir.join("a.bin"), dir.join("a-link.bin")).unwrap();
    std::os::unix::fs::symlink(wt.join(B), dir.join("b-sym.o")).unwrap();
    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(
        r.files, 1,
        "only B: A has two links, the symlink is skipped"
    );
    assert_eq!(len(&wt.join(A)), (2 * MIB) as u64);
    assert_eq!(len(&wt.join(B)), 0);
}

#[test]
fn non_utf8_name_is_skipped() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let (fx, wt, id) = setup();
    let bad = wt
        .join(".dd/Build/Products")
        .join(OsStr::from_bytes(b"bad\xff.bin"));
    if let Err(e) = fs::write(&bad, noise(2 * MIB, 7)) {
        // APFS rejects names that are not valid UTF-8 (EILSEQ).
        eprintln!("skipping: filesystem refused a non-UTF-8 name: {e}");
        return;
    }
    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 2);
    assert_eq!(len(&bad), (2 * MIB) as u64);
}

#[test]
fn gradle_build_files_are_stashed() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), "build/\n.gradle/\n").unwrap();
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    put(&wt, "app/build/outputs/app.apk", &noise(2 * MIB, 11));
    put(&wt, ".gradle/caches/c.bin", &noise(2 * MIB, 12));
    let id = register(&fx.home(), &wt).unwrap().id;
    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 1);
    assert_eq!(len(&wt.join("app/build/outputs/app.apk")), 0);
    assert_eq!(len(&wt.join(".gradle/caches/c.bin")), (2 * MIB) as u64);
}

#[test]
fn interrupted_unstash_is_not_restashed() {
    let (fx, wt, id) = setup();
    let originals = [
        (A, fs::read(wt.join(A)).unwrap()),
        (B, fs::read(wt.join(B)).unwrap()),
    ];
    stashed(stash(&fx.home(), &id, &[]).unwrap());
    // An unstash wrote A back, then stopped before marking the stash restored.
    simulate_unstash(&fx.home(), &id, &wt, &originals[..1]);
    let mut rec = record(&fx.home(), &id);
    let st = rec.stash.as_mut().unwrap();
    st.restored = false;
    st.restoring = true;
    Registry::open(fx.home()).put(&rec).unwrap();

    let out = stash(&fx.home(), &id, &[]).unwrap();
    assert!(
        matches!(&out, StashOutcome::Skipped(s) if s == "unstash in progress"),
        "{out:?}"
    );
    assert_eq!(len(&wt.join(A)), (2 * MIB) as u64);
}

#[test]
fn git_tracked_build_files_are_not_stashed() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), "build/\n").unwrap();
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    put(&wt, "app/build/tracked.bin", &noise(2 * MIB, 21));
    support::git(&wt, &["add", "-f", "app/build/tracked.bin"]);
    support::git(&wt, &["commit", "-qm", "track a build file"]);
    put(&wt, "app/build/loose.bin", &noise(2 * MIB, 22));
    let id = register(&fx.home(), &wt).unwrap().id;

    let r = stashed(stash(&fx.home(), &id, &[]).unwrap());
    assert_eq!(r.files, 1);
    assert_eq!(len(&wt.join("app/build/tracked.bin")), (2 * MIB) as u64);
    assert_eq!(len(&wt.join("app/build/loose.bin")), 0);
    let m = manifest(&fx.home(), &id);
    assert_eq!(m.entries.len(), 1);
    assert_eq!(m.entries[0].rel, Path::new("app/build/loose.bin"));
}
