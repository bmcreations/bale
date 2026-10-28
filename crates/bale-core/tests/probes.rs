#[cfg(unix)]
use bale_core::busy::{busy_holders, busy_holders_many};
use bale_core::idle::last_source_change;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn set_mtime(path: &Path, t: SystemTime) {
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(t).unwrap();
}

fn write_at(path: &Path, t: SystemTime) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "x").unwrap();
    set_mtime(path, t);
}

#[cfg(unix)]
struct Child(std::process::Child);

#[cfg(unix)]
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn sleep_in(dir: &Path) -> Child {
    let child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(dir)
        .spawn()
        .unwrap();
    let child = Child(child);
    std::thread::sleep(Duration::from_millis(300));
    child
}

#[cfg(unix)]
#[test]
fn open_file_is_a_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("wt");
    fs::create_dir_all(root.join("app")).unwrap();
    let child = sleep_in(&root.join("app"));
    let holders = busy_holders(&root).unwrap();
    assert!(
        holders.iter().any(|h| h.pid == child.0.id()),
        "holders: {holders:?}"
    );
    let h = holders.iter().find(|h| h.pid == child.0.id()).unwrap();
    assert!(h.command.contains("sleep"), "command: {}", h.command);
    assert_eq!(holders.iter().filter(|h| h.pid == child.0.id()).count(), 1);
}

#[cfg(unix)]
#[test]
fn open_file_under_root_with_cwd_outside_is_a_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("wt");
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(root.join("build")).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let file = root.join("build/out.bin");
    fs::write(&file, "x").unwrap();
    let child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(&elsewhere)
        .stdin(fs::File::open(&file).unwrap())
        .spawn()
        .unwrap();
    let child = Child(child);
    std::thread::sleep(Duration::from_millis(300));
    let holders = busy_holders(&root).unwrap();
    assert!(
        holders.iter().any(|h| h.pid == child.0.id()),
        "holders: {holders:?}"
    );
}

#[cfg(unix)]
#[test]
fn unrelated_process_is_not_a_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("wt");
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let _child = sleep_in(&elsewhere);
    let holders = busy_holders(&root).unwrap();
    assert!(holders.is_empty(), "holders: {holders:?}");
}

#[test]
fn build_dir_mtime_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let later = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    write_at(&root.join("src/a.kt"), t);
    write_at(&root.join("app/build/out"), later);
    write_at(&root.join(".git"), later); // linked-worktree style .git file
    write_at(&root.join("sub/.git/HEAD"), later);
    assert_eq!(
        last_source_change(root, &[root.join("app/build")]).unwrap(),
        t
    );
}

#[test]
fn no_files_returns_epoch() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("build")).unwrap();
    fs::write(tmp.path().join("build/x"), "x").unwrap();
    let got = last_source_change(tmp.path(), &[tmp.path().join("build")]).unwrap();
    assert_eq!(got, UNIX_EPOCH);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let later = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    write_at(&root.join("src/a.kt"), t);
    write_at(&outside.path().join("big"), later);
    std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
    let got = last_source_change(root, &[]).unwrap();
    // The symlink itself is an entry, but its target's files are not visited.
    let link_mtime = fs::symlink_metadata(root.join("link"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(got, t.max(link_mtime));
    assert_ne!(got, later);
}

#[test]
fn missing_root_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(last_source_change(&tmp.path().join("nope"), &[]).is_err());
}

#[cfg(unix)]
#[test]
fn many_roots_from_one_scan_match_each_root() {
    let tmp = tempfile::tempdir().unwrap();
    let held = tmp.path().join("held");
    let free = tmp.path().join("free");
    fs::create_dir_all(held.join("app")).unwrap();
    fs::create_dir_all(&free).unwrap();
    let child = sleep_in(&held.join("app"));
    let pid = child.0.id();

    let lists = busy_holders_many(&[free.clone(), held.clone(), held.clone()]).unwrap();
    assert_eq!(lists.len(), 3);
    assert!(lists[0].iter().all(|h| h.pid != pid), "{lists:?}");
    assert_eq!(
        lists[1].iter().filter(|h| h.pid == pid).count(),
        1,
        "{lists:?}"
    );
    assert_eq!(lists[1], lists[2]);
    assert!(busy_holders_many(&[]).unwrap().is_empty());
    assert!(busy_holders_many(&[tmp.path().join("missing")]).is_err());
}
