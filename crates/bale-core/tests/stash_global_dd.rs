//! Stash of Xcode's shared DerivedData. Kept in its own test binary: it
//! points HOME at a temp dir, which would race other tests. The one HOME is
//! shared by every test here; each tree's folder is told apart by its
//! `WorkspacePath`.
#![cfg(target_os = "macos")]

mod support;

use bale_core::ops::stash::{stash, StashOutcome};
use bale_core::ops::unstash::unstash;
use bale_core::ops::up::register;
use bale_core::registry::Registry;
use bale_core::store::index::stat_of;
use bale_core::store::manifest::{Entry, StashManifest};
use bale_core::store::Store;
use bale_core::BaleHome;
use filetime::{set_file_mtime, FileTime};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use support::{write_plist, Fixture};

const MIB: usize = 1 << 20;

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

/// The fake user home, created once; never the real one.
fn user_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        std::mem::forget(dir);
        std::env::set_var("HOME", &path);
        path
    })
}

const G_REL: &str = "Build/Products/Debug/g.bin";
const LOOSE: &str = "Build/Intermediates.noindex/h.o";

struct Tree {
    fx: Fixture,
    wt: PathBuf,
    id: String,
    dd: PathBuf,
}

/// A worktree with a small tracked-nothing in-tree layout and a global
/// DerivedData folder (`name`) built from it, holding two large files.
fn setup(name: &str) -> Tree {
    let home = user_home();
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::create_dir_all(wt.join("App.xcodeproj")).unwrap();
    let dd = home.join("Library/Developer/Xcode/DerivedData").join(name);
    write_plist(&dd, Some(&wt.join("App.xcodeproj")));
    for (rel, bytes) in [
        (G_REL, noise(2 * MIB, 1)),
        (LOOSE, noise(3 * MIB, 2)),
        ("Build/small.txt", noise(4096, 3)),
        ("SourcePackages/checkouts/pkg.bin", noise(2 * MIB, 4)),
    ] {
        let p = dd.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }
    set_file_mtime(
        dd.join(G_REL),
        FileTime::from_unix_time(1_700_000_000, 123_456_789),
    )
    .unwrap();
    fs::set_permissions(dd.join(LOOSE), fs::Permissions::from_mode(0o750)).unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    Tree { fx, wt, id, dd }
}

fn snapshot(p: &Path) -> (Vec<u8>, u32, (u64, i128, u64)) {
    let meta = fs::metadata(p).unwrap();
    (
        fs::read(p).unwrap(),
        meta.permissions().mode() & 0o7777,
        stat_of(&meta),
    )
}

fn manifest(home: &BaleHome, id: &str) -> StashManifest {
    let c = Registry::open(home.clone())
        .get(id)
        .unwrap()
        .unwrap()
        .stash
        .unwrap()
        .commitment;
    Store::open(home).unwrap().read_manifest(&c).unwrap()
}

fn len(p: &Path) -> u64 {
    fs::metadata(p).unwrap().len()
}

#[test]
fn stash_includes_global_derived_data() {
    let t = setup("App-incl");
    let home = t.fx.home();
    let before = [snapshot(&t.dd.join(G_REL)), snapshot(&t.dd.join(LOOSE))];

    let StashOutcome::Stashed(r) = stash(&home, &t.id, &[]).unwrap() else {
        panic!("not stashed");
    };
    assert_eq!(r.files, 2);
    assert_eq!(r.bytes, (5 * MIB) as u64);
    assert_eq!(r.git_visible, 0);
    assert_eq!([len(&t.dd.join(G_REL)), len(&t.dd.join(LOOSE))], [0, 0]);
    // Small files and inputs stay.
    assert_eq!(len(&t.dd.join("Build/small.txt")), 4096);
    assert_eq!(
        len(&t.dd.join("SourcePackages/checkouts/pkg.bin")),
        (2 * MIB) as u64
    );

    let m = manifest(&home, &t.id);
    assert_eq!(m.roots.len(), 2);
    assert_eq!(m.roots[0], fs::canonicalize(&t.wt).unwrap());
    assert_eq!(m.roots[1], fs::canonicalize(&t.dd).unwrap());
    assert!(m.entries.iter().all(|e| e.root == 1));

    let rep = unstash(&home, &t.id).unwrap().expect("was stashed");
    assert_eq!(rep.files, 2);
    let after = [snapshot(&t.dd.join(G_REL)), snapshot(&t.dd.join(LOOSE))];
    for (b, a) in before.iter().zip(&after) {
        assert!(b.0 == a.0, "bytes differ");
        assert_eq!(b.1, a.1, "mode");
        assert_eq!(b.2, a.2, "(size, mtime_ns, ino)");
    }
    assert_eq!(
        fs::metadata(t.dd.join(G_REL)).unwrap().ino(),
        before[0].2 .2
    );
}

#[test]
fn global_dd_outside_git_is_not_kept() {
    let t = setup("App-nogit");
    // A tree dir that does exist, so git runs for in-tree candidates.
    fs::write(t.wt.join(".gitignore"), ".dd/\n").unwrap();
    let StashOutcome::Stashed(r) = stash(&t.fx.home(), &t.id, &[]).unwrap() else {
        panic!("not stashed");
    };
    assert_eq!((r.files, r.git_visible), (2, 0));
}

#[test]
fn unstash_refuses_global_root_that_moved() {
    let t = setup("App-moved");
    let home = t.fx.home();
    stash(&home, &t.id, &[]).unwrap();
    assert_eq!(len(&t.dd.join(G_REL)), 0);

    // The folder is now another tree's.
    let other = t.fx.home().0.parent().unwrap().join("other");
    fs::create_dir_all(other.join("Other.xcodeproj")).unwrap();
    write_plist(&t.dd, Some(&other.join("Other.xcodeproj")));

    let err = unstash(&home, &t.id).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("g.bin") && text.contains("h.o"), "{text}");
    assert_eq!([len(&t.dd.join(G_REL)), len(&t.dd.join(LOOSE))], [0, 0]);
    // Lost entries leave the manifest, as for a lost blob; no retry loop.
    assert!(unstash(&home, &t.id).unwrap().is_none());
}

#[test]
fn old_manifest_without_roots_still_restores() {
    let t = setup("App-old");
    let home = t.fx.home();
    // An in-tree stash (no global folder involved): drop the global DD's
    // link to this tree first.
    write_plist(&t.dd, None);
    let dd = t.wt.join(".dd");
    write_plist(&dd, Some(&t.wt.join("App.xcodeproj")));
    fs::create_dir_all(dd.join("Build")).unwrap();
    fs::write(dd.join("Build/a.bin"), noise(2 * MIB, 9)).unwrap();
    fs::write(t.wt.join(".gitignore"), ".dd/\n").unwrap();
    let want = fs::read(dd.join("Build/a.bin")).unwrap();
    stash(&home, &t.id, &[]).unwrap();
    assert_eq!(len(&dd.join("Build/a.bin")), 0);

    // Rewrite the manifest as an older bale wrote it: no `roots`, no `root`.
    let reg = Registry::open(home.clone());
    let mut rec = reg.get(&t.id).unwrap().unwrap();
    let mut m = manifest(&home, &t.id);
    m.roots.clear();
    let c = m.commitment();
    let store = Store::open(&home).unwrap();
    store.write_manifest(&m).unwrap();
    let path = home.0.join("store/manifests").join(format!("{c}.json"));
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("roots").unwrap();
    for e in v["entries"].as_array_mut().unwrap() {
        e.as_object_mut().unwrap().remove("root").unwrap();
    }
    fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    rec.stash.as_mut().unwrap().commitment = c;
    reg.put(&rec).unwrap();

    unstash(&home, &t.id).unwrap().expect("was stashed");
    assert!(fs::read(dd.join("Build/a.bin")).unwrap() == want);
}

#[test]
fn commitment_changes_when_root_changes() {
    let entry = |root| Entry {
        rel: PathBuf::from("Build/a.bin"),
        root,
        hash: "a".repeat(64),
        size: 1,
        mode: 0o644,
        mtime_ns: 5,
        ino: 7,
    };
    let base = StashManifest {
        tree_id: "t".into(),
        created_unix: 0,
        roots: vec!["/tree".into(), "/dd/A".into()],
        entries: vec![entry(1)],
    };
    let mut swapped = base.clone();
    swapped.roots[1] = "/dd/B".into();
    assert_ne!(base.commitment(), swapped.commitment());

    let mut other_index = base.clone();
    other_index.entries = vec![entry(0)];
    assert_ne!(base.commitment(), other_index.commitment());

    // path_of follows the entry's own root.
    assert_eq!(
        base.path_of(&entry(1), Path::new("/tree")),
        PathBuf::from("/dd/A/Build/a.bin")
    );
    assert_eq!(
        base.path_of(&entry(0), Path::new("/tree")),
        PathBuf::from("/tree/Build/a.bin")
    );
}
