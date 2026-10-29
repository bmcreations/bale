//! Kept in its own test binary: it points HOME at a temp dir, which would
//! race the other sweep tests.
#![cfg(target_os = "macos")]

mod support;

use bale_core::ops::sweep::{sweep, Action};
use bale_core::ops::up::register;
use bale_core::registry::{Level, Registry};
use std::fs;
use support::{git, write_plist, Fixture};

#[test]
fn baled_tree_with_regrown_shared_derived_data_is_baled_again() {
    let fx = Fixture::new();
    fs::write(fx.repo.join(".gitignore"), "App.xcworkspace/\n").unwrap();
    git(&fx.repo, &["add", ".gitignore"]);
    git(&fx.repo, &["commit", "-m", "ignore"]);
    fs::create_dir_all(&fx.home().0).unwrap();
    fs::write(fx.home().0.join("config.toml"), "idle_baled_hours = 0\n").unwrap();
    let wt = fx.add_worktree("wt", "wt");
    fs::create_dir_all(wt.join("App.xcworkspace")).unwrap();

    let user_home = fx.home().0.parent().unwrap().join("user");
    std::env::set_var("HOME", &user_home);
    let dd = user_home.join("Library/Developer/Xcode/DerivedData/App-abc");
    write_plist(&dd, Some(&wt.join("App.xcworkspace")));
    fs::create_dir_all(dd.join("Build/Products")).unwrap();

    let id = register(&fx.home(), &wt).unwrap().id;
    let reg = Registry::open(fx.home());
    let mut rec = reg.get(&id).unwrap().unwrap();
    rec.level = Level::Baled;
    reg.put(&rec).unwrap();

    let rows = sweep(&fx.home(), false).unwrap();
    let row = rows.iter().find(|r| r.0 == id).unwrap();
    assert_eq!(row.1, Action::Up, "{rows:?}");
    assert!(!dd.exists());
    assert_eq!(reg.get(&id).unwrap().unwrap().level, Level::Baled);

    // With the shared folder gone, the next sweep keeps the tree.
    let rows = sweep(&fx.home(), false).unwrap();
    assert_eq!(rows.iter().find(|r| r.0 == id).unwrap().1, Action::Keep);
}
