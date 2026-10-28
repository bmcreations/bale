mod support;

use bale_core::git::worktrees;
#[cfg(unix)]
use bale_core::ops::cold::cold_ignoring;
use bale_core::ops::cold::{cold, cold_if_clean, exclusions, save_ignored, save_wip};
use bale_core::ops::up::{register, resolve, Outcome};
use bale_core::registry::{workspace_id, Level, Record, Registry};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use support::{git, Fixture};

/// Commits a `.gitignore` on main, then adds a Gradle worktree `wt` with a
/// modified tracked `f`, untracked `b.txt`, ignored `.env`, ignored `build/`
/// and ignored `node_modules/`.
fn dirty_worktree(fx: &Fixture) -> PathBuf {
    fs::write(
        fx.repo.join(".gitignore"),
        ".env\n.env.local\nbuild/\nnode_modules/\n",
    )
    .unwrap();
    fs::write(fx.repo.join("settings.gradle.kts"), "").unwrap();
    git(&fx.repo, &["add", ".gitignore", "settings.gradle.kts"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("f"), "modified\n").unwrap();
    fs::write(wt.join("b.txt"), "untracked\n").unwrap();
    fs::write(wt.join(".env"), "SECRET=1\n").unwrap();
    fs::create_dir_all(wt.join("build")).unwrap();
    fs::write(wt.join("build/out.bin"), "regen").unwrap();
    fs::create_dir_all(wt.join("node_modules/x")).unwrap();
    fs::write(wt.join("node_modules/x/i.js"), "js").unwrap();
    wt
}

fn level(fx: &Fixture, id: &str) -> Level {
    Registry::open(fx.home()).get(id).unwrap().unwrap().level
}

fn set_level(fx: &Fixture, id: &str, level: Level) {
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(id).unwrap().unwrap();
    rec.level = level;
    reg.put(&rec).unwrap();
}

fn linked(fx: &Fixture) -> bale_core::git::WorktreeInfo {
    worktrees(&fx.repo)
        .unwrap()
        .into_iter()
        .find(|w| !w.is_main)
        .unwrap()
}

fn show(fx: &Fixture, rev: &str) -> String {
    git(&fx.repo, &["show", rev])
}

fn trash_beside(wt: &Path) -> Vec<PathBuf> {
    let prefix = format!(".bale-trash-{}-", wt.file_name().unwrap().to_string_lossy());
    fs::read_dir(wt.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect()
}

#[cfg(unix)]
fn symlink(target: &str, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn cold_saves_everything_and_removes_dir() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    #[cfg(unix)]
    symlink("../main/.env.local", &wt.join(".env.local"));
    let id = register(&fx.home(), &wt).unwrap().id;

    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(!wt.exists());
    assert!(trash_beside(&wt).is_empty());
    assert_eq!(level(&fx, &id), Level::Cold);

    let wip = format!("refs/bale/{id}/wip");
    assert_eq!(show(&fx, &format!("{wip}:f")), "modified\n");
    assert_eq!(show(&fx, &format!("{wip}:b.txt")), "untracked\n");
    let head = git(&fx.repo, &["rev-parse", "feature"]);
    assert_eq!(
        git(&fx.repo, &["rev-parse", &format!("refs/bale/{id}/head")]),
        head
    );
    assert_eq!(git(&fx.repo, &["rev-parse", &format!("{wip}^")]), head);

    let files = fx.home().saved_dir(&id).join("files");
    assert_eq!(
        fs::read_to_string(files.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert!(!files.join("build").exists());
    assert!(!files.join("node_modules").exists());
    let manifest = fs::read_to_string(fx.home().saved_dir(&id).join("manifest.json")).unwrap();
    assert!(manifest.contains(".env"), "{manifest}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(files.join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = fs::metadata(&files).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        let marker = fx.home().saved_dir(&id).join("trash");
        let marker_mode = fs::metadata(&marker).unwrap().permissions().mode();
        assert_eq!(marker_mode & 0o777, 0o600);
        let wip_sha = git(&fx.repo, &["rev-parse", &wip]);
        assert!(fs::read_to_string(&marker)
            .unwrap()
            .contains(wip_sha.trim()));
        let link = files.join(".env.local");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(&link).unwrap(),
            PathBuf::from("../main/.env.local")
        );
    }

    let entry = linked(&fx);
    assert_eq!(entry.locked.as_deref(), Some("baled by bale"));
}

#[test]
fn user_locked_tree_is_skipped_untouched() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &["worktree", "lock", "--reason", "mine", wt.to_str().unwrap()],
    );
    assert_eq!(
        cold(&fx.home(), &id).unwrap(),
        Outcome::Skipped("locked: mine".into())
    );
    assert!(wt.join("build/out.bin").exists());
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "modified\n");
    assert_eq!(level(&fx, &id), Level::Hot);
    assert_eq!(linked(&fx).locked.as_deref(), Some("mine"));
    assert!(git(&fx.repo, &["for-each-ref", "refs/bale/"]).is_empty());
}

#[test]
fn cold_refuses_main_checkout() {
    let fx = Fixture::new();
    let root = bale_core::paths::canonicalize(&fx.repo).unwrap();
    let id = workspace_id(&root);
    let rec = Record {
        id: id.clone(),
        repo_common_dir: root.join(".git"),
        path: root.clone(),
        admin_dir: root.join(".git"),
        branch: Some("main".into()),
        head: String::new(),
        level: Level::Hot,
        last_activity: SystemTime::now(),
        note: None,
        trash: Vec::new(),
        origin: None,
        pinned: false,
        scope: None,
    };
    Registry::open(fx.home()).put(&rec).unwrap();
    let err = cold(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains("main checkout"), "{err:#}");
    assert!(fx.repo.join("f").exists());
}

#[test]
fn cold_refuses_sparse() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    git(&wt, &["sparse-checkout", "set", "src"]);
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("sparse")),
        "{out:?}"
    );
    assert!(wt.exists());
    assert_eq!(level(&fx, &id), Level::Hot);
    assert_eq!(linked(&fx).locked, None);
}

#[test]
fn cold_refuses_dirty_submodule() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    git(
        &wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub.repo.to_str().unwrap(),
            "lib",
        ],
    );
    git(&wt, &["commit", "-m", "sub"]);
    fs::write(wt.join("lib/f"), "dirty\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("lib")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(wt.join("lib/f")).unwrap(), "dirty\n");
    assert_eq!(level(&fx, &id), Level::Hot);
}

#[test]
fn cold_refuses_untracked_nested_repo() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let nested = wt.join("nested");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-b", "main"]);
    fs::write(nested.join("x"), "x").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("nested")),
        "{out:?}"
    );
    assert!(nested.join("x").exists());
}

#[test]
fn real_index_untouched() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("staged.txt"), "s\n").unwrap();
    git(&wt, &["add", "staged.txt"]);
    fs::write(wt.join("loose.txt"), "l\n").unwrap();
    let index = PathBuf::from(
        git(
            &wt,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )
        .trim(),
    );
    let before = fs::read(&index).unwrap();

    save_wip(&wt, "abc").unwrap();

    assert_eq!(fs::read(&index).unwrap(), before);
    assert_eq!(
        git(&wt, &["diff", "--cached", "--name-only"]),
        "staged.txt\n"
    );
    assert!(git(&wt, &["status", "--porcelain"]).contains("?? loose.txt"));
    assert_eq!(show(&fx, "refs/bale/abc/wip:staged.txt"), "s\n");
    assert_eq!(show(&fx, "refs/bale/abc/wip:loose.txt"), "l\n");
}

#[test]
fn force_added_ignored_file_is_kept() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), "*.key\n").unwrap();
    fs::write(wt.join("a.key"), "k\n").unwrap();
    git(&wt, &["add", "-f", "a.key"]);
    save_wip(&wt, "abc").unwrap();
    assert_eq!(show(&fx, "refs/bale/abc/wip:a.key"), "k\n");
}

#[test]
fn assume_unchanged_edit_is_kept() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    git(&wt, &["update-index", "--assume-unchanged", "f"]);
    fs::write(wt.join("f"), "hidden edit\n").unwrap();
    save_wip(&wt, "abc").unwrap();
    assert_eq!(show(&fx, "refs/bale/abc/wip:f"), "hidden edit\n");
}

#[test]
fn save_ignored_honours_extra_deny() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), "*.log\ncache/\n").unwrap();
    fs::write(wt.join("a.log"), "a").unwrap();
    fs::create_dir_all(wt.join("cache/deep")).unwrap();
    fs::write(wt.join("cache/deep/c"), "c").unwrap();
    let dest = fx.home().saved_dir("x");
    let mut ex = exclusions(&fx.home(), &wt).unwrap();
    ex.deny.push("cache".to_string());
    let saved = save_ignored(&wt, &dest, &ex).unwrap();
    let rels: Vec<_> = saved.iter().map(|s| s.rel.clone()).collect();
    assert_eq!(rels, vec![PathBuf::from("a.log")]);
    assert!(dest.join("manifest.json").exists());
}

#[cfg(unix)]
#[test]
fn symlinked_ignored_dir_is_not_followed() {
    let fx = Fixture::new();
    let outside = fx.repo.parent().unwrap().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("big"), "outside").unwrap();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), "link\ndir/\n").unwrap();
    symlink(outside.to_str().unwrap(), &wt.join("link"));
    fs::create_dir_all(wt.join("dir")).unwrap();
    symlink(outside.to_str().unwrap(), &wt.join("dir/inner"));
    let dest = fx.home().saved_dir("x");
    let saved = save_ignored(&wt, &dest, &exclusions(&fx.home(), &wt).unwrap()).unwrap();
    assert_eq!(saved.len(), 2, "{saved:?}");
    assert!(saved.iter().all(|s| s.symlink_target.is_some()));
    assert!(fs::symlink_metadata(dest.join("files/link"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_to_string(outside.join("big")).unwrap(), "outside");
    assert!(fs::symlink_metadata(dest.join("files/dir/inner"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn recovers_cooling_with_dir_present() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "baled by bale",
            wt.to_str().unwrap(),
        ],
    );
    // A stale file from an earlier interrupted attempt must not survive.
    let dest = fx.home().saved_dir(&id);
    fs::create_dir_all(dest.join("files")).unwrap();
    fs::write(dest.join("files/stale"), "old").unwrap();
    set_level(&fx, &id, Level::Cooling);

    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(!wt.exists());
    assert!(!dest.join("files/stale").exists());
    assert!(dest.join("files/.env").exists());
    assert_eq!(show(&fx, &format!("refs/bale/{id}/wip:f")), "modified\n");
    assert_eq!(level(&fx, &id), Level::Cold);
}

#[test]
fn recovers_cooling_after_rename() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "baled by bale",
            wt.to_str().unwrap(),
        ],
    );
    save_wip(&wt, &id).unwrap();
    save_ignored(
        &wt,
        &fx.home().saved_dir(&id),
        &exclusions(&fx.home(), &wt).unwrap(),
    )
    .unwrap();
    let trash = wt.with_file_name(".bale-trash-wt-123");
    fs::rename(&wt, &trash).unwrap();
    set_level(&fx, &id, Level::Cooling);

    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(!trash.exists());
    assert_eq!(level(&fx, &id), Level::Cold);
}

#[test]
fn cooling_without_wip_ref_keeps_trash() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    save_ignored(
        &wt,
        &fx.home().saved_dir(&id),
        &exclusions(&fx.home(), &wt).unwrap(),
    )
    .unwrap();
    let trash = wt.with_file_name(".bale-trash-wt-123");
    fs::rename(&wt, &trash).unwrap();
    set_level(&fx, &id, Level::Cooling);

    let err = cold(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains("wip"), "{err:#}");
    assert_eq!(fs::read_to_string(trash.join("f")).unwrap(), "modified\n");
    assert_eq!(level(&fx, &id), Level::Cooling);
}

#[test]
fn cold_on_cold_skips() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;
    set_level(&fx, &id, Level::Cold);
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r == "cold"),
        "{out:?}"
    );
    assert!(wt.exists());
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
fn busy_tree_is_not_removed() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    let _child = KillOnDrop(
        std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&wt)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let out = cold(&fx.home(), &id).unwrap();
    assert!(matches!(&out, Outcome::Busy(h) if !h.is_empty()), "{out:?}");
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "modified\n");
    assert_ne!(level(&fx, &id), Level::Cold);
    // Left Baled, and Baled trees carry no git lock.
    assert_eq!(linked(&fx).locked, None);
}

#[cfg(unix)]
#[test]
fn cold_ignoring_skips_listed_holder() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    let child = KillOnDrop(
        std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&wt)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.0.id();
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Busy(h) if h.iter().any(|x| x.pid == pid)),
        "{out:?}"
    );
    let out = cold_ignoring(&fx.home(), &id, &[pid]).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
}

/// `git init` + one commit at `dir`.
fn nested_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("n"), "history\n").unwrap();
    git(dir, &["add", "n"]);
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "n",
        ],
    );
}

#[test]
fn cold_refuses_repo_nested_below_untracked_dir() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::create_dir_all(wt.join("tools")).unwrap();
    fs::write(wt.join("tools/readme"), "r\n").unwrap();
    nested_repo(&wt.join("tools/nested"));
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("tools/nested")),
        "{out:?}"
    );
    assert!(wt.join("tools/nested/.git").is_dir());
    assert_eq!(level(&fx, &id), Level::Hot);
}

#[test]
fn cold_refuses_repo_inside_ignored_dir() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), ".worktrees/\n").unwrap();
    nested_repo(&wt.join(".worktrees/x"));
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains(&native(".worktrees/x"))),
        "{out:?}"
    );
    assert!(wt.join(".worktrees/x/.git").is_dir());
}

#[test]
fn save_ignored_refuses_nested_repo() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), "vendor/\n").unwrap();
    nested_repo(&wt.join("vendor/lib"));
    let err = save_ignored(
        &wt,
        &fx.home().saved_dir("x"),
        &exclusions(&fx.home(), &wt).unwrap(),
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains(&native("vendor/lib")),
        "{err:#}"
    );
}

/// Recovering an interrupted cold removes the check dir the crash left
/// (it holds copies of the tree's files), and only that workspace's.
#[test]
fn recover_removes_stale_check_dir() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;
    set_level(&fx, &id, Level::Cooling);
    let parent = wt.parent().unwrap();
    let ours = parent.join(format!(".bale-check-{id}-Zz9yX8"));
    let theirs = parent.join(".bale-check-0123456789ab-Zz9yX8");
    for d in [&ours, &theirs] {
        fs::create_dir_all(d.join("tree")).unwrap();
        fs::write(d.join("tree/.env"), "SECRET=1\n").unwrap();
    }
    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(!ours.exists());
    assert!(theirs.join("tree/.env").exists());
}

/// Sets extended attribute `name` on `path`; false if the filesystem
/// refuses user attributes.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn set_xattr(path: &Path, name: &str, value: &[u8]) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let p = CString::new(path.as_os_str().as_bytes()).unwrap();
    let n = CString::new(name).unwrap();
    let v = value.as_ptr().cast();
    // SAFETY: both strings are NUL-terminated and `v` is valid for its length.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), v, value.len(), 0, 0) };
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::setxattr(p.as_ptr(), n.as_ptr(), v, value.len(), 0) };
    rc == 0
}

/// An extended attribute the user set (Finder tags, say) is not saved, so
/// cold refuses and names the file.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn cold_refuses_file_with_extended_attribute() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("notes.txt"), "n\n").unwrap();
    if !set_xattr(&wt.join("notes.txt"), "user.bale-test", b"x") {
        eprintln!("skipped: the filesystem refuses user xattrs");
        return;
    }
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("notes.txt") && r.contains("user.bale-test")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(wt.join("notes.txt")).unwrap(), "n\n");
}

/// The worktree's root dir is not one of the walked entries, so its
/// extended attributes need their own check.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn cold_refuses_extended_attribute_on_the_root() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    if !set_xattr(&wt, "user.bale-test", b"x") {
        eprintln!("skipped: the filesystem refuses user xattrs");
        return;
    }
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("user.bale-test")),
        "{out:?}"
    );
    assert!(wt.join("f").exists());
}

/// Runs `cmd args.. path` and asserts it succeeded.
#[cfg(target_os = "macos")]
fn run_on(cmd: &str, args: &[&str], path: &Path) {
    let ok = std::process::Command::new(cmd)
        .args(args)
        .arg(path)
        .status()
        .unwrap()
        .success();
    assert!(ok, "{cmd} {args:?} {}", path.display());
}

/// A file flag (`chflags hidden`) is not saved, so cold refuses and names
/// the file.
#[cfg(target_os = "macos")]
#[test]
fn cold_refuses_file_with_bsd_flags() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("notes.txt"), "n\n").unwrap();
    run_on("chflags", &["hidden"], &wt.join("notes.txt"));
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("notes.txt") && r.contains("flags")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(wt.join("notes.txt")).unwrap(), "n\n");
}

/// An access control list (`chmod +a`) is not saved, so cold refuses and
/// names the dir.
#[cfg(target_os = "macos")]
#[test]
fn cold_refuses_entry_with_acl() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::create_dir(wt.join("shared")).unwrap();
    fs::write(wt.join("shared/notes.txt"), "n\n").unwrap();
    run_on("chmod", &["+a", "everyone allow read"], &wt.join("shared"));
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("shared") && r.contains("access control list")),
        "{out:?}"
    );
    assert!(wt.join("shared/notes.txt").exists());
}

/// What apps set on their own does not stop cold: Xcode's text encoding
/// hint on project files, and Finder's flags on `.DS_Store`.
#[cfg(target_os = "macos")]
#[test]
fn cold_allows_app_set_extended_attributes() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("contents.xcworkspacedata"), "<x/>\n").unwrap();
    fs::write(wt.join(".DS_Store"), "ds").unwrap();
    assert!(set_xattr(
        &wt.join("contents.xcworkspacedata"),
        "com.apple.TextEncoding",
        b"utf-8;134217984"
    ));
    let mut finder = [0u8; 32];
    finder[8] = 0x40;
    assert!(set_xattr(
        &wt.join(".DS_Store"),
        "com.apple.FinderInfo",
        &finder
    ));
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
}

/// Finder info elsewhere can hold a colour label, which is the user's.
#[cfg(target_os = "macos")]
#[test]
fn cold_refuses_finder_info_outside_ds_store() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join("notes.txt"), "n\n").unwrap();
    let mut finder = [0u8; 32];
    finder[9] = 0x0c; // label colour bits
    assert!(set_xattr(
        &wt.join("notes.txt"),
        "com.apple.FinderInfo",
        &finder
    ));
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("notes.txt")),
        "{out:?}"
    );
}

/// A worktree with submodule `lib` from a repo whose `.gitignore` has `.env`,
/// `build/` and `cache/`.
fn worktree_with_submodule(fx: &Fixture, sub: &Fixture) -> PathBuf {
    fs::write(sub.repo.join(".gitignore"), ".env\nbuild/\ncache/\n").unwrap();
    git(&sub.repo, &["add", ".gitignore"]);
    git(&sub.repo, &["commit", "-m", "ignore"]);
    let wt = fx.add_worktree("wt", "feature");
    git(
        &wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub.repo.to_str().unwrap(),
            "lib",
        ],
    );
    git(&wt, &["commit", "-m", "sub"]);
    wt
}

#[test]
fn cold_refuses_submodule_with_ignored_files() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let wt = worktree_with_submodule(&fx, &sub);
    // Regenerable output beside the secret must not hide it.
    gradle_with_built_submodule(&wt);
    fs::write(wt.join("lib/.env"), "SECRET=1\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("lib")),
        "{out:?}"
    );
    assert_eq!(
        fs::read_to_string(wt.join("lib/.env")).unwrap(),
        "SECRET=1\n"
    );
}

/// A file in a submodule whose bytes a clean filter in the submodule's own
/// config strips: status calls it clean, but checkout would write the
/// stripped bytes back.
#[cfg(unix)]
#[test]
fn cold_refuses_submodule_file_checkout_would_change() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    fs::write(sub.repo.join("notes.txt"), "a\n").unwrap();
    git(&sub.repo, &["add", "notes.txt"]);
    git(&sub.repo, &["commit", "-m", "notes"]);
    let wt = worktree_with_submodule(&fx, &sub);
    let lib = wt.join("lib");
    git(&lib, &["config", "filter.strip.clean", "sed 's/ *$//'"]);
    let attrs = git(&lib, &["rev-parse", "--git-path", "info/attributes"]);
    let attrs = lib.join(attrs.trim());
    fs::create_dir_all(attrs.parent().unwrap()).unwrap();
    fs::write(&attrs, "notes.txt filter=strip\n").unwrap();
    fs::write(lib.join("notes.txt"), "a  \n").unwrap();
    // Staged: the index takes the new size, and the cleaned blob is HEAD's.
    git(&lib, &["add", "notes.txt"]);
    assert_eq!(git(&lib, &["status", "--porcelain"]), "");

    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("lib/notes.txt")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(lib.join("notes.txt")).unwrap(), "a  \n");
}

/// A commit made inside a submodule moves its HEAD off the gitlink HEAD
/// records (` M lib`), with the submodule itself clean. Open restores HEAD's
/// gitlink, so that commit would be left only in the submodule's reflog.
#[test]
fn cold_refuses_committed_submodule_head_move() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let wt = worktree_with_submodule(&fx, &sub);
    let lib = wt.join("lib");
    fs::write(lib.join("new.txt"), "x\n").unwrap();
    git(&lib, &["add", "new.txt"]);
    git(
        &lib,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "moved",
        ],
    );
    let moved = git(&lib, &["rev-parse", "HEAD"]);
    assert_eq!(git(&wt, &["status", "--porcelain"]), " M lib\n");

    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("submodule lib") && r.contains("commit")),
        "{out:?}"
    );
    // Refused before anything is saved: the post-rename check would refuse
    // too, but only after writing the wip ref and the saved dir.
    assert_eq!(level(&fx, &id), Level::Hot);
    assert!(!wip_resolves(&fx, &id));
    assert!(!fx.home().saved_dir(&id).exists());
    assert_eq!(git(&lib, &["rev-parse", "HEAD"]), moved);
    assert_eq!(fs::read_to_string(lib.join("new.txt")).unwrap(), "x\n");
}

#[test]
fn cold_refuses_submodule_with_git_dir() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    git(&wt, &["clone", "-q", sub.repo.to_str().unwrap(), "lib"]);
    git(
        &wt,
        &["submodule", "add", sub.repo.to_str().unwrap(), "lib"],
    );
    git(&wt, &["commit", "-m", "sub"]);
    assert!(wt.join("lib/.git").is_dir());
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("lib")),
        "{out:?}"
    );
    assert!(wt.join("lib/.git").is_dir());
}

#[test]
fn untracked_dir_of_only_ignored_files_is_saved_once() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), ".env\n").unwrap();
    fs::create_dir_all(wt.join("newdir")).unwrap();
    fs::write(wt.join("newdir/.env"), "S=1\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    let saved = fx.home().saved_dir(&id).join("files/newdir/.env");
    assert_eq!(fs::read_to_string(saved).unwrap(), "S=1\n");
}

#[test]
fn recovery_refuses_dirty_submodule() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let wt = worktree_with_submodule(&fx, &sub);
    fs::write(wt.join("lib/f"), "dirty\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    set_level(&fx, &id, Level::Cooling);
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("lib")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(wt.join("lib/f")).unwrap(), "dirty\n");
    assert_eq!(level(&fx, &id), Level::Baled);
}

#[test]
fn recovery_keeps_trash_that_fails_verify() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "baled by bale",
            wt.to_str().unwrap(),
        ],
    );
    save_wip(&wt, &id).unwrap();
    save_ignored(
        &wt,
        &fx.home().saved_dir(&id),
        &exclusions(&fx.home(), &wt).unwrap(),
    )
    .unwrap();
    // Edited after the save, e.g. a stale wip ref from an earlier cycle.
    fs::write(wt.join("f"), "newer\n").unwrap();
    let trash = wt.with_file_name(".bale-trash-wt-123");
    fs::rename(&wt, &trash).unwrap();
    set_level(&fx, &id, Level::Cooling);

    // A changed tree is not an error: cold moves it back and reports busy.
    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Busy(Vec::new()));
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "newer\n");
    assert!(!trash.exists());
    assert_eq!(level(&fx, &id), Level::Baled);
    assert_eq!(linked(&fx).locked, None);
}

/// A trash recovery can't check goes back too, with the error.
#[test]
fn recovery_moves_back_trash_it_cannot_check() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "baled by bale",
            wt.to_str().unwrap(),
        ],
    );
    save_wip(&wt, &id).unwrap();
    let dest = fx.home().saved_dir(&id);
    save_ignored(&wt, &dest, &exclusions(&fx.home(), &wt).unwrap()).unwrap();
    fs::write(dest.join("manifest.json"), "not json").unwrap();
    let trash = wt.with_file_name(".bale-trash-wt-123");
    fs::rename(&wt, &trash).unwrap();
    set_level(&fx, &id, Level::Cooling);

    let err = cold(&fx.home(), &id).unwrap_err();
    assert!(format!("{err:#}").contains("could not check"), "{err:#}");
    assert_eq!(fs::read_to_string(wt.join("f")).unwrap(), "modified\n");
    assert!(!trash.exists());
    assert_eq!(level(&fx, &id), Level::Baled);
    assert_eq!(linked(&fx).locked, None);
}

/// Makes `wt` a Gradle build and gives its submodule `lib` a `build/` dir.
fn gradle_with_built_submodule(wt: &Path) {
    fs::write(wt.join("settings.gradle.kts"), "").unwrap();
    git(wt, &["add", "settings.gradle.kts"]);
    git(wt, &["commit", "-m", "gradle"]);
    fs::create_dir_all(wt.join("lib/build")).unwrap();
    fs::write(wt.join("lib/build/out.bin"), "regen").unwrap();
}

#[test]
fn submodule_with_only_regenerable_output_goes_cold() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let wt = worktree_with_submodule(&fx, &sub);
    gradle_with_built_submodule(&wt);
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(!wt.exists());
    assert_eq!(level(&fx, &id), Level::Cold);
}

/// A deleted submodule dir is what `add -A` saves (the gitlink removed);
/// the check of the moved tree must agree instead of failing on it.
#[test]
fn tree_with_deleted_submodule_dir_goes_cold() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let wt = worktree_with_submodule(&fx, &sub);
    fs::remove_dir_all(wt.join("lib")).unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(!wt.exists());
}

/// Root ignores permission bits, so the chmod-based tests prove nothing.
#[cfg(unix)]
fn running_as_root() -> bool {
    let out = std::process::Command::new("id").arg("-u").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim() == "0"
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
#[test]
fn cold_skips_unreadable_dir_under_untracked_dir() {
    if running_as_root() {
        return;
    }
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    fs::create_dir_all(wt.join("u/locked")).unwrap();
    fs::write(wt.join("u/a"), "a\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    // A nested repo could hide in a dir the walk cannot read.
    chmod(&wt.join("u/locked"), 0o000);
    let out = cold(&fx.home(), &id);
    if wt.join("u/locked").exists() {
        chmod(&wt.join("u/locked"), 0o755);
    }
    let out = out.unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("unreadable") && r.contains("locked")),
        "{out:?}"
    );
    assert_eq!(fs::read_to_string(wt.join("u/a")).unwrap(), "a\n");
}

#[cfg(unix)]
#[test]
fn recovery_finishes_partly_removed_trash() {
    if running_as_root() {
        return;
    }
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    // remove_dir_all empties `locked/inner`, then cannot unlink it from
    // `locked`, so the trash no longer matches the wip.
    fs::create_dir_all(wt.join("locked/inner")).unwrap();
    fs::write(wt.join("locked/inner/x"), "x\n").unwrap();
    chmod(&wt.join("locked"), 0o500);
    let id = register(&fx.home(), &wt).unwrap().id;

    let first = cold(&fx.home(), &id);
    for t in trash_beside(&wt) {
        if t.join("locked").exists() {
            chmod(&t.join("locked"), 0o755);
        }
    }
    if wt.join("locked").exists() {
        chmod(&wt.join("locked"), 0o755);
    }
    assert!(first.is_err(), "{first:?}");
    assert!(!wt.exists());
    assert_eq!(trash_beside(&wt).len(), 1);
    assert_eq!(level(&fx, &id), Level::Cooling);

    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(trash_beside(&wt).is_empty());
    assert_eq!(level(&fx, &id), Level::Cold);
    assert_eq!(
        show(&fx, &format!("refs/bale/{id}/wip:locked/inner/x")),
        "x\n"
    );
}

/// Every file under `dir`, relative to it.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push(e.path().strip_prefix(dir).unwrap().to_path_buf());
            }
        }
    }
    out.sort();
    out
}

#[test]
fn recovery_keeps_trash_without_git_entry() {
    let fx = Fixture::new();
    // Something `add -A` in the main repo would write a blob for.
    fs::write(fx.repo.join("main-only.txt"), "main only\n").unwrap();
    let wt = fx.repo.join("nest").join("wt");
    git(
        &fx.repo,
        &["worktree", "add", "-b", "feature", wt.to_str().unwrap()],
    );
    fs::write(wt.join("b.txt"), "untracked\n").unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    git(
        &fx.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "baled by bale",
            wt.to_str().unwrap(),
        ],
    );
    save_wip(&wt, &id).unwrap();
    save_ignored(
        &wt,
        &fx.home().saved_dir(&id),
        &exclusions(&fx.home(), &wt).unwrap(),
    )
    .unwrap();
    let trash = wt.with_file_name(".bale-trash-wt-123");
    fs::rename(&wt, &trash).unwrap();
    // A removal that got as far as the `.git` file: git run in the trash
    // would walk up to the main repo.
    fs::remove_file(trash.join(".git")).unwrap();
    set_level(&fx, &id, Level::Cooling);
    let git_dir = fx.repo.join(".git");
    let index = fs::read(git_dir.join("index")).unwrap();
    let objects = files_under(&git_dir.join("objects"));

    let err = cold(&fx.home(), &id).unwrap_err();
    assert_eq!(fs::read(git_dir.join("index")).unwrap(), index);
    assert_eq!(files_under(&git_dir.join("objects")), objects);
    assert!(format!("{err:#}").contains("no .git"), "{err:#}");
    assert_eq!(
        fs::read_to_string(trash.join("b.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(level(&fx, &id), Level::Cooling);
}

/// A submodule `lib` holding ignored `lib/cache/x/secret`, with the machine
/// config denylist set to `deny`.
fn submodule_cache_with_deny(fx: &Fixture, sub: &Fixture, deny: &str) -> (PathBuf, String) {
    let wt = worktree_with_submodule(fx, sub);
    fs::create_dir_all(wt.join("lib/cache/x")).unwrap();
    fs::write(wt.join("lib/cache/x/secret"), "S=1\n").unwrap();
    fs::create_dir_all(&fx.home().0).unwrap();
    fs::write(
        fx.home().0.join("config.toml"),
        format!("ignored_denylist = [\"{deny}\"]\n"),
    )
    .unwrap();
    let id = register(&fx.home(), &wt).unwrap().id;
    (wt, id)
}

#[test]
fn root_slashed_deny_does_not_apply_inside_submodule() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let (wt, id) = submodule_cache_with_deny(&fx, &sub, "cache/x");
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("lib")),
        "{out:?}"
    );
    assert_eq!(
        fs::read_to_string(wt.join("lib/cache/x/secret")).unwrap(),
        "S=1\n"
    );
}

#[test]
fn submodule_prefixed_deny_applies_inside_submodule() {
    let fx = Fixture::new();
    let sub = Fixture::new();
    let (wt, id) = submodule_cache_with_deny(&fx, &sub, "lib/cache");
    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(!wt.exists());
}

#[test]
fn cold_if_clean_skips_dirty_tree_untouched() {
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;

    let out = cold_if_clean(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Skipped("dirty".into()));
    assert_eq!(level(&fx, &id), Level::Hot);
    assert!(wt.join("build/out.bin").exists());
    assert_eq!(git(&fx.repo, &["for-each-ref", "refs/bale/"]), "");
    assert_eq!(linked(&fx).locked, None);
    assert!(!fx.home().saved_dir(&id).exists());
}

/// A `.git` that points elsewhere fails the checkout check before the
/// dirty probe runs, so the tree is skipped untouched.
#[test]
fn cold_if_clean_skips_a_tree_whose_git_dir_is_wrong() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;
    fs::write(wt.join(".git"), "gitdir: /nonexistent/bale-test\n").unwrap();

    let out = cold_if_clean(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("not this worktree's checkout")),
        "{out:?}"
    );
    assert_eq!(level(&fx, &id), Level::Hot);
    assert!(wt.join("f").exists());
}

#[test]
fn cold_if_clean_colds_clean_tree() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    let id = register(&fx.home(), &wt).unwrap().id;

    let out = cold_if_clean(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold));
    assert!(!wt.exists());
}

/// A cold that fails after taking the git lock puts the tree back to Baled
/// without bale's lock.
#[cfg(unix)]
#[test]
fn failed_save_returns_to_baled_unlocked() {
    if running_as_root() {
        return;
    }
    let fx = Fixture::new();
    let wt = dirty_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    chmod(&wt.join(".env"), 0o000);
    let out = cold(&fx.home(), &id);
    chmod(&wt.join(".env"), 0o600);
    assert!(out.is_err(), "{out:?}");
    assert_eq!(level(&fx, &id), Level::Baled);
    assert_eq!(linked(&fx).locked, None);
    assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1\n");
}

fn wip_resolves(fx: &Fixture, id: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/bale/{id}/wip"))
        .current_dir(&fx.repo)
        .status()
        .unwrap()
        .success()
}

/// `path` as bale reports it: canonical, so with long names and native
/// separators on Windows.
fn canonical(path: &Path) -> String {
    bale_core::paths::canonicalize(path)
        .unwrap()
        .display()
        .to_string()
}

/// A relative `a/b` path with the platform's separator, as bale prints it.
fn native(rel: &str) -> String {
    rel.split('/')
        .collect::<std::path::PathBuf>()
        .display()
        .to_string()
}

/// Replaces the checkout at `wt` with a folder an editor might recreate.
fn replace_with_stray(wt: &Path) {
    fs::remove_dir_all(wt).unwrap();
    fs::create_dir_all(wt.join(".idea")).unwrap();
    fs::write(wt.join(".idea/workspace.xml"), "<x/>\n").unwrap();
}

/// Cold refuses a folder at the record's path that is not the worktree's
/// checkout, and writes nothing.
fn assert_cold_refuses_stray(fx: &Fixture, wt: &Path) {
    let id = register(&fx.home(), wt).unwrap().id;
    replace_with_stray(wt);
    let out = cold(&fx.home(), &id).unwrap();
    let shown = canonical(wt);
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains(&shown)),
        "{out:?}"
    );
    assert_eq!(level(fx, &id), Level::Hot);
    assert!(!wip_resolves(fx, &id));
    assert!(!fx.home().saved_dir(&id).exists());
    assert!(trash_beside(wt).is_empty());
    assert_eq!(
        fs::read_to_string(wt.join(".idea/workspace.xml")).unwrap(),
        "<x/>\n"
    );
    assert_eq!(linked(fx).locked, None);
}

#[test]
fn cold_refuses_a_folder_inside_the_repo_that_is_not_the_checkout() {
    let fx = Fixture::new();
    let wt = fx.repo.join(".claude/worktrees/w");
    git(
        &fx.repo,
        &["worktree", "add", "-b", "w", wt.to_str().unwrap()],
    );
    assert_cold_refuses_stray(&fx, &wt);
}

#[test]
fn cold_refuses_a_folder_outside_any_repo() {
    let fx = Fixture::new();
    let wt = fx.add_worktree("wt", "feature");
    assert_cold_refuses_stray(&fx, &wt);
}

/// Re-registering a stray folder at a registered path must not repoint the
/// record at the enclosing repo, or every checkout check would pass.
#[test]
fn resolve_keeps_admin_dir_when_the_folder_is_not_the_checkout() {
    let fx = Fixture::new();
    let wt = fx.repo.join(".claude/worktrees/w");
    git(
        &fx.repo,
        &["worktree", "add", "-b", "w", wt.to_str().unwrap()],
    );
    let before = register(&fx.home(), &wt).unwrap();
    replace_with_stray(&wt);

    let err = resolve(&fx.home(), wt.to_str().unwrap()).unwrap_err();
    assert!(format!("{err:#}").contains(&canonical(&wt)), "{err:#}");
    assert!(register(&fx.home(), &wt).is_err());
    let after = Registry::open(fx.home()).get(&before.id).unwrap().unwrap();
    assert_eq!(after.admin_dir, before.admin_dir);

    let out = cold(&fx.home(), &before.id).unwrap();
    assert!(matches!(&out, Outcome::Skipped(_)), "{out:?}");
    assert!(!wip_resolves(&fx, &before.id));
    assert_eq!(level(&fx, &before.id), Level::Hot);
}

/// A stray whose `.git` file points at the main repo's git dir is not a
/// linked checkout either: its admin dir would be the common dir.
#[test]
fn resolve_refuses_a_gitfile_pointing_at_the_main_repo() {
    let fx = Fixture::new();
    let wt = fx.repo.join(".claude/worktrees/w");
    git(
        &fx.repo,
        &["worktree", "add", "-b", "w", wt.to_str().unwrap()],
    );
    let before = register(&fx.home(), &wt).unwrap();
    replace_with_stray(&wt);
    let main_git = bale_core::paths::canonicalize(fx.repo.join(".git")).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}\n", main_git.display())).unwrap();

    assert!(resolve(&fx.home(), wt.to_str().unwrap()).is_err());
    let after = Registry::open(fx.home()).get(&before.id).unwrap().unwrap();
    assert_eq!(after.admin_dir, before.admin_dir);
    let out = cold(&fx.home(), &before.id).unwrap();
    assert!(matches!(&out, Outcome::Skipped(_)), "{out:?}");
    assert!(!wip_resolves(&fx, &before.id));
}

/// An Xcode worktree whose ignored `build/dd` is a DerivedData root holding
/// an SPM checkout (a git clone).
fn xcode_derived_data_worktree(fx: &Fixture) -> PathBuf {
    let wt = fx.add_worktree("wt", "feature");
    fs::write(wt.join(".gitignore"), "build/\nvendor-ignored/\n").unwrap();
    fs::create_dir_all(wt.join("App.xcodeproj")).unwrap();
    let dd = wt.join("build/dd");
    fs::create_dir_all(dd.join("Build")).unwrap();
    let mut dict = plist::Dictionary::new();
    dict.insert(
        "WorkspacePath".into(),
        plist::Value::String(wt.join("App.xcodeproj").to_string_lossy().into_owned()),
    );
    plist::Value::Dictionary(dict)
        .to_file_xml(dd.join("info.plist"))
        .unwrap();
    nested_repo(&dd.join("SourcePackages/checkouts/pkg"));
    wt
}

#[test]
fn cold_removes_derived_data_with_spm_checkouts() {
    let fx = Fixture::new();
    let wt = xcode_derived_data_worktree(&fx);
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert_eq!(out, Outcome::Done(Level::Cold), "{out:?}");
    assert!(!wt.exists());

    let report = bale_core::ops::open::open(&fx.home(), &id).unwrap();
    assert_eq!(report.outcome, Outcome::Done(Level::Hot));
    assert!(wt.join("App.xcodeproj").is_dir());
    assert!(!wt.join("build/dd").exists());
}

#[test]
fn cold_still_refuses_user_repo_beside_derived_data() {
    let fx = Fixture::new();
    let wt = xcode_derived_data_worktree(&fx);
    nested_repo(&wt.join("vendor-ignored/lib"));
    let id = register(&fx.home(), &wt).unwrap().id;
    let out = cold(&fx.home(), &id).unwrap();
    assert!(
        matches!(&out, Outcome::Skipped(r) if r.contains("nested repository")),
        "{out:?}"
    );
    assert!(wt.join("vendor-ignored/lib/.git").is_dir());
}
