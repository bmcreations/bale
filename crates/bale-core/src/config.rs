use crate::BaleHome;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_IDLE_BALED_HOURS: u64 = 4;
const DEFAULT_IDLE_COLD_DAYS: u64 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub idle_baled: Duration,
    pub idle_cold: Duration,
    pub regenerable_extra: Vec<String>,
    pub ignored_denylist_extra: Vec<String>,
    pub worktree_root: Option<PathBuf>,
}

/// One config file as written on disk; every key is optional.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    idle_baled_hours: Option<u64>,
    idle_cold_days: Option<u64>,
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
    Ok(Config {
        idle_baled: Duration::from_secs(hours * 3600),
        idle_cold: Duration::from_secs(days * 86400),
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
