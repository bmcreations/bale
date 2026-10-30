mod support;

use bale_core::clones::tracked::{candidates, parse_lfs_pointer};
use bale_core::clones::Key;
use sha2::{Digest, Sha256};
use std::path::Path;
use support::{git, Fixture};

fn big(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_add(seed)).collect()
}

#[test]
fn candidates_lists_large_files_only() {
    let fx = Fixture::new();
    std::fs::write(fx.repo.join("big.bin"), big(2 << 20, 1)).unwrap();
    std::fs::write(fx.repo.join("small.txt"), "0123456789").unwrap();
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-m", "files"]);

    let c = candidates(&fx.repo, 1 << 20).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, Path::new("big.bin"));
    assert_eq!(c[0].size, 2 << 20);
    assert_eq!(c[0].mode, 0o644);
    assert!(
        matches!(&c[0].key, Key::Git(oid) if *oid == git(&fx.repo, &["rev-parse", "HEAD:big.bin"]).trim())
    );
}

#[cfg(unix)]
#[test]
fn candidates_keep_exec_mode_and_skip_symlinks() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let fx = Fixture::new();
    let exe = fx.repo.join("tool.bin");
    std::fs::write(&exe, big(2 << 20, 2)).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink("tool.bin", fx.repo.join("link.bin")).unwrap();
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-m", "files"]);

    let c = candidates(&fx.repo, 1 << 20).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, Path::new("tool.bin"));
    assert_eq!(c[0].mode, 0o755);
}

#[test]
fn candidates_skip_converted_paths() {
    let fx = Fixture::new();
    std::fs::write(
        fx.repo.join(".gitattributes"),
        "crlf.bin eol=crlf\nf.bin filter=custom\nauto.bin text=auto\nbinary.bin -text\nid.bin ident\n",
    )
    .unwrap();
    for (n, s) in [
        ("crlf.bin", 3),
        ("f.bin", 4),
        ("auto.bin", 5),
        ("binary.bin", 6),
        ("id.bin", 12),
        ("enc.bin", 13),
    ] {
        std::fs::write(fx.repo.join(n), big(2 << 20, s)).unwrap();
    }
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-m", "files"]);
    // Set after the commit: git can't add these bytes as UTF-16.
    let mut attrs = std::fs::read_to_string(fx.repo.join(".gitattributes")).unwrap();
    attrs.push_str("enc.bin working-tree-encoding=UTF-16\n");
    std::fs::write(fx.repo.join(".gitattributes"), attrs).unwrap();

    let c = candidates(&fx.repo, 1 << 20).unwrap();
    let paths: Vec<_> = c.iter().map(|c| c.path.clone()).collect();
    assert_eq!(paths, vec![Path::new("binary.bin")]);
}

#[test]
fn candidates_empty_when_autocrlf_converts() {
    let fx = Fixture::new();
    std::fs::write(fx.repo.join("big.bin"), big(2 << 20, 7)).unwrap();
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-m", "files"]);
    assert_eq!(candidates(&fx.repo, 1 << 20).unwrap().len(), 1);
    for v in ["true", "input", "yes", "on", "1"] {
        git(&fx.repo, &["config", "core.autocrlf", v]);
        assert!(candidates(&fx.repo, 1 << 20).unwrap().is_empty());
    }
    git(&fx.repo, &["config", "core.autocrlf", "false"]);
    assert_eq!(candidates(&fx.repo, 1 << 20).unwrap().len(), 1);
}

#[test]
fn parse_lfs_pointer_reads_oid_and_size() {
    let p = b"version https://git-lfs.github.com/spec/v1\noid sha256:3eda6971f9d9c8064d45aa485b84c88516bb06bed1fb6eff9b4f16870034478f\nsize 1073741824\n";
    assert_eq!(
        parse_lfs_pointer(p),
        Some((
            "3eda6971f9d9c8064d45aa485b84c88516bb06bed1fb6eff9b4f16870034478f".into(),
            1073741824
        ))
    );
    assert_eq!(parse_lfs_pointer(b"not a pointer"), None);
}

#[test]
fn candidates_key_lfs_files_on_pointer() {
    if std::process::Command::new("git")
        .args(["lfs", "version"])
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        return;
    }
    let fx = Fixture::new();
    git(&fx.repo, &["lfs", "install", "--local"]);
    git(&fx.repo, &["lfs", "track", "*.bin"]);
    let data = big(2 << 20, 8);
    std::fs::write(fx.repo.join("big.bin"), &data).unwrap();
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-m", "lfs"]);

    let c = candidates(&fx.repo, 1 << 20).unwrap();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].path, Path::new("big.bin"));
    assert_eq!(c[0].size, 2 << 20);
    let sha: String = Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(c[0].key, Key::Lfs(sha));
}

#[cfg(unix)]
fn clones_work(fx: &Fixture) -> bool {
    let a = fx.repo.join("probe-src");
    std::fs::write(&a, b"x").unwrap();
    let r = bale_core::clones::reflink::clone_file(&a, &fx.repo.join("probe-dst"));
    !matches!(r, Err(e) if e.kind() == std::io::ErrorKind::Unsupported)
}

#[cfg(unix)]
#[test]
fn adopt_then_place_round_trips() {
    use bale_core::clones::store::{Adopted, CloneStore};
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, UNIX_EPOCH};
    let fx = Fixture::new();
    if !clones_work(&fx) {
        return;
    }
    let data = big(1 << 20, 9);
    let src = fx.repo.join("src.bin");
    std::fs::write(&src, &data).unwrap();
    let store = CloneStore::open(&fx.home()).unwrap();
    let key = Key::Git("ab".to_string() + &"c".repeat(38));
    assert!(!store.has(&key));
    assert_eq!(
        store.adopt(&key, &src, || Ok(true)).unwrap(),
        Adopted::Stored
    );
    assert!(store.has(&key));
    assert_eq!(
        store.adopt(&key, &src, || Ok(true)).unwrap(),
        Adopted::Present
    );
    assert!(store
        .path(&key)
        .ends_with(format!("git/ab/ab{}", "c".repeat(38))));
    let mode = std::fs::metadata(store.path(&key))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o444);

    let dst = fx.repo.join("sub/dst.bin");
    let t = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    store.place(&key, &dst, 0o755, Some(t)).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), data);
    let md = std::fs::metadata(&dst).unwrap();
    assert_eq!(md.permissions().mode() & 0o777, 0o755);
    assert_eq!(md.modified().unwrap(), t);
}

#[cfg(unix)]
#[test]
fn place_over_existing_file_leaves_no_temp() {
    use bale_core::clones::store::CloneStore;
    let fx = Fixture::new();
    if !clones_work(&fx) {
        return;
    }
    let data = big(1 << 20, 10);
    let src = fx.repo.join("src.bin");
    std::fs::write(&src, &data).unwrap();
    let store = CloneStore::open(&fx.home()).unwrap();
    let key = Key::Lfs("de".to_string() + &"f".repeat(62));
    store.adopt(&key, &src, || Ok(true)).unwrap();
    assert!(store
        .path(&key)
        .ends_with(format!("lfs/de/{}", "de".to_string() + &"f".repeat(62))));

    let dst = fx.repo.join("dst.bin");
    std::fs::write(&dst, b"old").unwrap();
    store.place(&key, &dst, 0o644, None).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), data);
    let leftovers: Vec<_> = std::fs::read_dir(&fx.repo)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".bale-share"))
        .collect();
    assert!(leftovers.is_empty());
}

/// A source that changes during the clone is never published, and its temp
/// copy is removed.
#[cfg(unix)]
#[test]
fn adopt_drops_a_copy_whose_source_changed() {
    use bale_core::clones::store::{Adopted, CloneStore};
    let fx = Fixture::new();
    if !clones_work(&fx) {
        return;
    }
    let src = fx.repo.join("src.bin");
    std::fs::write(&src, big(1 << 20, 14)).unwrap();
    let store = CloneStore::open(&fx.home()).unwrap();
    let key = Key::Git("12".repeat(20));
    let r = store
        .adopt(&key, &src, || {
            std::fs::write(&src, big(1 << 20, 15))?;
            Ok(false)
        })
        .unwrap();
    assert_eq!(r, Adopted::Changed);
    assert!(!store.has(&key));
    assert_eq!(store_files(&fx.home()), 0);
}

/// The temp copy gets the time of the clone, not the source's mtime, so gc's
/// grace covers a fresh store file.
#[cfg(unix)]
#[test]
fn adopt_gives_the_store_file_a_fresh_mtime() {
    use bale_core::clones::store::CloneStore;
    let fx = Fixture::new();
    if !clones_work(&fx) {
        return;
    }
    let src = fx.repo.join("src.bin");
    std::fs::write(&src, big(1 << 20, 16)).unwrap();
    age_file(&src, HOUR2);
    let store = CloneStore::open(&fx.home()).unwrap();
    let key = Key::Git("34".repeat(20));
    store.adopt(&key, &src, || Ok(true)).unwrap();
    let age = std::fs::metadata(store.path(&key))
        .unwrap()
        .modified()
        .unwrap()
        .elapsed()
        .unwrap_or_default();
    assert!(age < std::time::Duration::from_secs(600), "{age:?}");
}

/// `share_checkouts` and `clone_min_bytes` come from the tree's `.bale.toml`.
#[test]
fn bale_toml_sets_share_checkouts_and_clone_min_bytes() {
    let fx = Fixture::new();
    std::fs::write(fx.repo.join("mid.bin"), big(4096, 17)).unwrap();
    git(&fx.repo, &["add", "mid.bin"]);
    git(&fx.repo, &["commit", "-m", "mid"]);
    let home = fx.home();
    let cfg = bale_core::config::load_config(&home, Some(&fx.repo)).unwrap();
    assert!(cfg.share_checkouts);
    assert!(candidates(&fx.repo, cfg.clone_min_bytes)
        .unwrap()
        .is_empty());

    std::fs::write(
        fx.repo.join(".bale.toml"),
        "share_checkouts = false\nclone_min_bytes = 1024\n",
    )
    .unwrap();
    let cfg = bale_core::config::load_config(&home, Some(&fx.repo)).unwrap();
    assert!(!cfg.share_checkouts);
    assert_eq!(cfg.clone_min_bytes, 1024);
    let c = candidates(&fx.repo, cfg.clone_min_bytes).unwrap();
    let paths: Vec<_> = c.iter().map(|c| c.path.clone()).collect();
    assert_eq!(paths, vec![Path::new("mid.bin")]);
}

/// Commits a 2 MiB `big.bin` (mode `0o755` when `exec`), sets
/// `clone_min_bytes = 1024` (the fixture's 4-byte `f` stays out), and returns the registered worktree's path and id.
fn shared_fixture(fx: &Fixture, names: &[&str], exec: bool) -> Vec<(std::path::PathBuf, String)> {
    let big_path = fx.repo.join("big.bin");
    std::fs::write(&big_path, big(2 << 20, 11)).unwrap();
    #[cfg(unix)]
    if exec {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&big_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = exec;
    git(&fx.repo, &["add", "big.bin"]);
    git(&fx.repo, &["commit", "-m", "big"]);
    let home = fx.home();
    std::fs::create_dir_all(&home.0).unwrap();
    std::fs::write(home.0.join("config.toml"), "clone_min_bytes = 1024\n").unwrap();
    names
        .iter()
        .map(|n| {
            let wt = fx.add_worktree(n, n);
            let id = bale_core::ops::up::register(&home, &wt).unwrap().id;
            (wt, id)
        })
        .collect()
}

#[cfg(unix)]
fn store_files(home: &bale_core::BaleHome) -> usize {
    walkdir::WalkDir::new(home.clones_dir())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count()
}

#[cfg(unix)]
#[test]
fn share_two_worktrees_uses_one_store_file() {
    use bale_core::clones::share::share;
    use bale_core::registry::Registry;
    let fx = Fixture::new();
    if !clones_work(&fx) {
        return;
    }
    let trees = shared_fixture(&fx, &["w1", "w2"], false);
    let home = fx.home();
    let data = big(2 << 20, 11);

    let r1 = share(&home, &trees[0].1).unwrap();
    assert_eq!(r1.reason, None, "{r1:?}");
    assert_eq!((r1.files, r1.stored, r1.bytes), (1, 1, 2 << 20), "{r1:?}");
    let r2 = share(&home, &trees[1].1).unwrap();
    assert_eq!(r2.reason, None, "{r2:?}");
    assert_eq!((r2.files, r2.stored), (1, 0), "{r2:?}");
    assert!(r2.skipped.is_empty(), "{r2:?}");
    assert_eq!(store_files(&home), 1);

    let registry = Registry::open(home.clone());
    for (wt, id) in &trees {
        assert_eq!(std::fs::read(wt.join("big.bin")).unwrap(), data);
        assert_eq!(git(wt, &["status", "--porcelain"]), "");
        let rec = registry.get(id).unwrap().unwrap();
        assert_eq!(rec.shared_head.as_deref(), Some(rec.head.as_str()));
    }
}

#[cfg(unix)]
#[test]
fn share_leaves_modified_file() {
    use bale_core::clones::share::share;
    let fx = Fixture::new();
    if !clones_work(&fx) {
        return;
    }
    let trees = shared_fixture(&fx, &["w1"], false);
    let (wt, id) = &trees[0];
    let other = big(2 << 20, 99);
    std::fs::write(wt.join("big.bin"), &other).unwrap();

    let r = share(&fx.home(), id).unwrap();
    assert_eq!(r.files, 0, "{r:?}");
    assert_eq!(
        r.skipped,
        vec![(std::path::PathBuf::from("big.bin"), "modified".to_string())]
    );
    assert_eq!(std::fs::read(wt.join("big.bin")).unwrap(), other);
    assert_eq!(store_files(&fx.home()), 0);
}

#[cfg(unix)]
#[test]
fn share_keeps_mode_and_mtime() {
    use bale_core::clones::share::share;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, UNIX_EPOCH};
    let fx = Fixture::new();
    if !clones_work(&fx) {
        return;
    }
    let trees = shared_fixture(&fx, &["w1"], true);
    let (wt, id) = &trees[0];
    let file = wt.join("big.bin");
    let t = UNIX_EPOCH + Duration::new(1_700_000_000, 987_654_321);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(t)
        .unwrap();

    let r = share(&fx.home(), id).unwrap();
    assert_eq!(r.files, 1, "{r:?}");
    let md = std::fs::metadata(&file).unwrap();
    assert_eq!(md.permissions().mode() & 0o777, 0o755);
    assert_eq!(md.modified().unwrap(), t);
}

/// Where clones don't work (e.g. ext4 on Linux CI), share changes nothing and says why.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn share_reports_unsupported() {
    use bale_core::clones::share::share;
    use std::os::unix::fs::MetadataExt;
    let fx = Fixture::new();
    if clones_work(&fx) {
        return;
    }
    let trees = shared_fixture(&fx, &["w1"], false);
    let (wt, id) = &trees[0];
    let file = wt.join("big.bin");
    let ino = std::fs::metadata(&file).unwrap().ino();

    let r = share(&fx.home(), id).unwrap();
    assert_eq!(r.reason.as_deref(), Some("clones unsupported"), "{r:?}");
    assert_eq!(r.files, 0);
    assert_eq!(std::fs::read(&file).unwrap(), big(2 << 20, 11));
    assert_eq!(std::fs::metadata(&file).unwrap().ino(), ino);
    let rec = bale_core::registry::Registry::open(fx.home())
        .get(id)
        .unwrap()
        .unwrap();
    assert_eq!(rec.shared_head, None);
}

#[test]
fn share_skips_locked_tree() {
    use bale_core::clones::share::share;
    use bale_core::lock::WorkspaceLock;
    let fx = Fixture::new();
    let trees = shared_fixture(&fx, &["w1"], false);
    let (wt, id) = &trees[0];
    let _lock = WorkspaceLock::try_acquire(&fx.home(), id).unwrap().unwrap();
    let r = share(&fx.home(), id).unwrap();
    assert_eq!(r.reason.as_deref(), Some("locked"));
    assert_eq!(r.files, 0);
    assert_eq!(std::fs::read(wt.join("big.bin")).unwrap(), big(2 << 20, 11));
}

#[test]
fn share_skips_a_tree_with_sharing_disabled() {
    use bale_core::clones::share::share;
    let fx = Fixture::new();
    let trees = shared_fixture(&fx, &["w1"], false);
    let (wt, id) = &trees[0];
    std::fs::write(wt.join(".bale.toml"), "share_checkouts = false\n").unwrap();
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(wt.join("big.bin")).unwrap());
    let r = share(&fx.home(), id).unwrap();
    assert_eq!(r.reason.as_deref(), Some("disabled"), "{r:?}");
    assert_eq!(r.files, 0);
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(wt.join("big.bin")).unwrap()),
        ino
    );
    let rec = bale_core::registry::Registry::open(fx.home())
        .get(id)
        .unwrap()
        .unwrap();
    assert_eq!(rec.shared_head, None);
}

fn age_file(path: &Path, by: std::time::Duration) {
    let f = std::fs::File::open(path).unwrap();
    f.set_modified(std::time::SystemTime::now() - by).unwrap();
}

/// Writes `bytes` straight into the store under `key`, aged by `by`.
fn put_store_file(
    store: &bale_core::clones::store::CloneStore,
    key: &Key,
    bytes: &[u8],
    by: std::time::Duration,
) -> std::path::PathBuf {
    let p = store.path(key);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    age_file(&p, by);
    p
}

const HOUR2: std::time::Duration = std::time::Duration::from_secs(2 * 3600);

#[test]
fn gc_keeps_listed_and_young_files() {
    use bale_core::clones::{gc::collect, store::CloneStore};
    use bale_core::store::gc::GC_GRACE;
    let fx = Fixture::new();
    let store = CloneStore::open(&fx.home()).unwrap();
    let listed = Key::Git("aa".repeat(20));
    let young = Key::Git("bb".repeat(20));
    let a = put_store_file(&store, &listed, b"listed", HOUR2);
    let b = put_store_file(&store, &young, b"young", std::time::Duration::ZERO);
    let live = std::collections::HashSet::from([listed]);
    assert_eq!(collect(&store, &live, GC_GRACE, false).unwrap(), 0);
    assert!(a.exists() && b.exists());
}

#[test]
fn gc_removes_unlisted_old_files() {
    use bale_core::clones::{gc::collect, store::CloneStore};
    use bale_core::store::gc::GC_GRACE;
    let fx = Fixture::new();
    let store = CloneStore::open(&fx.home()).unwrap();
    let key = Key::Lfs("cd".repeat(32));
    let p = put_store_file(&store, &key, b"twelve bytes", HOUR2);
    let tmp = p.with_file_name(format!("{}.tmp-1-0", "cd".repeat(32)));
    std::fs::write(&tmp, b"abc").unwrap();
    age_file(&tmp, HOUR2);
    let odd = p.with_file_name("not-a-key");
    std::fs::write(&odd, b"zz").unwrap();
    age_file(&odd, HOUR2);
    let live = std::collections::HashSet::new();
    assert_eq!(collect(&store, &live, GC_GRACE, true).unwrap(), 15);
    assert!(p.exists() && tmp.exists());
    assert_eq!(collect(&store, &live, GC_GRACE, false).unwrap(), 15);
    assert!(!p.exists() && !tmp.exists());
    assert!(odd.exists());
}

#[test]
fn gc_removes_nothing_when_a_listing_fails() {
    use bale_core::clones::store::CloneStore;
    use bale_core::registry::Registry;
    let fx = Fixture::new();
    let home = fx.home();
    let wt = fx.add_worktree("w1", "w1");
    let id = bale_core::ops::up::register(&home, &wt).unwrap().id;
    let store = CloneStore::open(&home).unwrap();
    let p = put_store_file(&store, &Key::Git("ee".repeat(20)), b"data", HOUR2);
    // The path still exists but is no longer a git tree.
    let rec = Registry::open(home.clone()).get(&id).unwrap().unwrap();
    std::fs::remove_dir_all(&rec.path).unwrap();
    std::fs::create_dir_all(&rec.path).unwrap();
    let report = bale_core::ops::gc::gc(&home, false).unwrap();
    assert_eq!(report.clones_bytes_freed, 0, "{report:?}");
    assert!(
        report.errors.iter().any(|(k, _)| k == "clones"),
        "{report:?}"
    );
    assert!(p.exists());
}

#[test]
fn gc_does_not_create_a_missing_store() {
    let fx = Fixture::new();
    let home = fx.home();
    let report = bale_core::ops::gc::gc(&home, false).unwrap();
    assert_eq!(report.clones_bytes_freed, 0);
    assert!(!home.clones_dir().exists());
}

#[cfg(unix)]
#[test]
fn gc_keeps_a_shared_file_while_its_tree_exists_then_frees_it() {
    use bale_core::clones::share::share;
    let fx = Fixture::new();
    if !clones_work(&fx) {
        return;
    }
    let trees = shared_fixture(&fx, &["w1"], false);
    let home = fx.home();
    let r = share(&home, &trees[0].1).unwrap();
    assert_eq!(r.reason, None, "{r:?}");
    let store = bale_core::clones::store::CloneStore::open(&home).unwrap();
    let files: Vec<_> = walkdir::WalkDir::new(store.root())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();
    assert_eq!(files.len(), 1);
    age_file(&files[0], HOUR2);

    let report = bale_core::ops::gc::gc(&home, false).unwrap();
    assert_eq!(report.clones_bytes_freed, 0, "{report:?}");
    assert!(files[0].exists());

    std::fs::remove_dir_all(&trees[0].0).unwrap();
    let report = bale_core::ops::gc::gc(&home, false).unwrap();
    assert_eq!(report.clones_bytes_freed, 2 << 20, "{report:?}");
    assert!(!files[0].exists());
}

#[test]
fn gc_removes_nothing_when_a_record_is_unreadable() {
    use bale_core::clones::store::CloneStore;
    let fx = Fixture::new();
    let home = fx.home();
    let store = CloneStore::open(&home).unwrap();
    let p = put_store_file(&store, &Key::Git("ab".repeat(20)), b"data", HOUR2);
    std::fs::create_dir_all(home.records_dir()).unwrap();
    std::fs::write(home.records_dir().join("0123456789ab.json"), b"{not json").unwrap();
    let report = bale_core::ops::gc::gc(&home, false).unwrap();
    assert_eq!(report.clones_bytes_freed, 0, "{report:?}");
    assert!(
        report.errors.iter().any(|(k, _)| k == "clones"),
        "{report:?}"
    );
    assert!(p.exists());
}

#[test]
fn share_summary_counts_io_errors() {
    use bale_core::clones::share::ShareReport;
    let r = ShareReport {
        files: 1,
        bytes: 2 << 20,
        skipped: vec![
            ("a.bin".into(), "modified".into()),
            ("b.bin".into(), "Permission denied (os error 13)".into()),
        ],
        ..ShareReport::default()
    };
    assert!(r.had_errors());
    assert_eq!(r.summary(), "shared 1 files (2.0 MiB), stored 0, 1 errors");
}
