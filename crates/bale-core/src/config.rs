use crate::BaleHome;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_IDLE_BALED_HOURS: u64 = 4;
const DEFAULT_IDLE_COLD_DAYS: u64 = 7;
const DEFAULT_IDLE_STASH_MINUTES: u64 = 10;
const DEFAULT_REAP_GRACE_MINUTES: u64 = 60;
const DEFAULT_STASH_MIN_BYTES: u64 = 1 << 20;
const DEFAULT_PATCH_MAX_DEPTH: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub idle_baled: Duration,
    pub idle_cold: Duration,
    pub idle_stash: Duration,
    /// How long orphaned Xcode DerivedData must be untouched before sweep reaps it.
    pub reap_grace: Duration,
    /// Link new trees to one shared Swift packages folder per `Package.resolved`.
    /// Read from the machine `config.toml` only.
    pub share_swift_packages: bool,
    pub stash_min_bytes: u64,
    pub patch_max_depth: u32,
    pub regenerable_extra: Vec<String>,
    pub ignored_denylist_extra: Vec<String>,
    pub worktree_root: Option<PathBuf>,
}

/// One config file as written on disk; every key is optional.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    idle_baled_hours: Option<u64>,
    idle_cold_days: Option<u64>,
    idle_stash_minutes: Option<u64>,
    reap_grace_minutes: Option<u64>,
    share_swift_packages: Option<bool>,
    stash_min_bytes: Option<u64>,
    patch_max_depth: Option<u32>,
    regenerable: Option<Vec<String>>,
    ignored_denylist: Option<Vec<String>>,
    worktree_root: Option<PathBuf>,
}

fn read_raw(path: &Path) -> Result<RawConfig> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RawConfig::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Loads `$BALE_HOME/config.toml`, then overlays `<repo_root>/.bale.toml`.
/// Repo keys win; a list key set in the repo file replaces the machine list.
pub fn load_config(home: &BaleHome, repo_root: Option<&Path>) -> Result<Config> {
    let machine = read_raw(&home.0.join("config.toml"))?;
    let repo = match repo_root {
        Some(r) => read_raw(&r.join(".bale.toml"))?,
        None => RawConfig::default(),
    };
    let hours = repo
        .idle_baled_hours
        .or(machine.idle_baled_hours)
        .unwrap_or(DEFAULT_IDLE_BALED_HOURS);
    let days = repo
        .idle_cold_days
        .or(machine.idle_cold_days)
        .unwrap_or(DEFAULT_IDLE_COLD_DAYS);
    let minutes = repo
        .idle_stash_minutes
        .or(machine.idle_stash_minutes)
        .unwrap_or(DEFAULT_IDLE_STASH_MINUTES);
    let reap_minutes = repo
        .reap_grace_minutes
        .or(machine.reap_grace_minutes)
        .unwrap_or(DEFAULT_REAP_GRACE_MINUTES);
    Ok(Config {
        idle_baled: Duration::from_secs(hours * 3600),
        idle_cold: Duration::from_secs(days * 86400),
        idle_stash: Duration::from_secs(minutes * 60),
        reap_grace: Duration::from_secs(reap_minutes * 60),
        share_swift_packages: machine.share_swift_packages.unwrap_or(true),
        stash_min_bytes: repo
            .stash_min_bytes
            .or(machine.stash_min_bytes)
            .unwrap_or(DEFAULT_STASH_MIN_BYTES),
        patch_max_depth: repo
            .patch_max_depth
            .or(machine.patch_max_depth)
            .unwrap_or(DEFAULT_PATCH_MAX_DEPTH),
        regenerable_extra: repo.regenerable.or(machine.regenerable).unwrap_or_default(),
        ignored_denylist_extra: repo
            .ignored_denylist
            .or(machine.ignored_denylist)
            .unwrap_or_default(),
        worktree_root: repo.worktree_root.or(machine.worktree_root),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn defaults_when_no_files() {
        let h = tempfile::tempdir().unwrap();
        let c = load_config(&BaleHome(h.path().into()), None).unwrap();
        assert_eq!(c.idle_baled, Duration::from_secs(4 * 3600));
        assert_eq!(c.idle_cold, Duration::from_secs(7 * 86400));
        assert_eq!(c.reap_grace, Duration::from_secs(3600));
        assert!(c.regenerable_extra.is_empty());
        assert!(c.ignored_denylist_extra.is_empty());
        assert_eq!(c.worktree_root, None);
    }

    #[test]
    fn repo_overrides_machine() {
        let h = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        fs::write(
            h.path().join("config.toml"),
            "idle_baled_hours = 2\nidle_cold_days = 3\nregenerable = [\"out\"]\n",
        )
        .unwrap();
        fs::write(r.path().join(".bale.toml"), "idle_baled_hours = 1\n").unwrap();
        let c = load_config(&BaleHome(h.path().into()), Some(r.path())).unwrap();
        assert_eq!(c.idle_baled, Duration::from_secs(3600));
        assert_eq!(c.idle_cold, Duration::from_secs(3 * 86400));
        assert_eq!(c.regenerable_extra, vec!["out".to_string()]);
    }

    #[test]
    fn reap_grace_is_configurable() {
        let h = tempfile::tempdir().unwrap();
        fs::write(h.path().join("config.toml"), "reap_grace_minutes = 5\n").unwrap();
        let c = load_config(&BaleHome(h.path().into()), None).unwrap();
        assert_eq!(c.reap_grace, Duration::from_secs(300));
    }

    #[test]
    fn share_swift_packages_defaults_on_and_reads_the_machine_file_only() {
        let h = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        let home = BaleHome(h.path().into());
        assert!(load_config(&home, None).unwrap().share_swift_packages);
        fs::write(
            r.path().join(".bale.toml"),
            "share_swift_packages = false\n",
        )
        .unwrap();
        assert!(
            load_config(&home, Some(r.path()))
                .unwrap()
                .share_swift_packages
        );
        fs::write(
            h.path().join("config.toml"),
            "share_swift_packages = false\n",
        )
        .unwrap();
        assert!(!load_config(&home, None).unwrap().share_swift_packages);
    }

    #[test]
    fn repo_list_replaces_machine_list() {
        let h = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        fs::write(
            h.path().join("config.toml"),
            "regenerable = [\"out\"]\nignored_denylist = [\"a\"]\nworktree_root = \"/m\"\n",
        )
        .unwrap();
        fs::write(r.path().join(".bale.toml"), "regenerable = [\"dist\"]\n").unwrap();
        let c = load_config(&BaleHome(h.path().into()), Some(r.path())).unwrap();
        assert_eq!(c.regenerable_extra, vec!["dist".to_string()]);
        assert_eq!(c.ignored_denylist_extra, vec!["a".to_string()]);
        assert_eq!(c.worktree_root, Some("/m".into()));
    }

    #[test]
    fn bad_toml_names_the_file() {
        let h = tempfile::tempdir().unwrap();
        let r = tempfile::tempdir().unwrap();
        fs::write(r.path().join(".bale.toml"), "idle_baled_hours = 'x'").unwrap();
        let e = load_config(&BaleHome(h.path().into()), Some(r.path())).unwrap_err();
        assert!(format!("{e:#}").contains(".bale.toml"), "{e:#}");
    }
}
