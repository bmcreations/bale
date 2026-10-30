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
        "crlf.bin eol=crlf\nf.bin filter=custom\nauto.bin text=auto\nbinary.bin -text\n",
    )
    .unwrap();
    for (n, s) in [
        ("crlf.bin", 3),
        ("f.bin", 4),
        ("auto.bin", 5),
        ("binary.bin", 6),
    ] {
        std::fs::write(fx.repo.join(n), big(2 << 20, s)).unwrap();
    }
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-m", "files"]);

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
    use bale_core::clones::store::CloneStore;
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
    assert!(store.adopt(&key, &src).unwrap());
    assert!(store.has(&key));
    assert!(!store.adopt(&key, &src).unwrap());
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
    store.adopt(&key, &src).unwrap();
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
