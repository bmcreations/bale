pub mod busy;
pub mod config;
pub mod derived_data;
pub mod detect;
pub mod git;
pub mod idle;
pub mod lock;
pub mod ops;
pub mod registry;

use std::path::PathBuf;

/// Root of bale's per-machine state (`$BALE_HOME`, default `~/.bale`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaleHome(pub PathBuf);

impl BaleHome {
    /// Reads `BALE_HOME`, falling back to `<home dir>/.bale`.
    pub fn from_env() -> BaleHome {
        match std::env::var_os("BALE_HOME") {
            Some(p) if !p.is_empty() => BaleHome(PathBuf::from(p)),
            _ => BaleHome(
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".bale"),
            ),
        }
    }

    pub fn records_dir(&self) -> PathBuf {
        self.0.join("registry")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.0.join("locks")
    }

    pub fn saved_dir(&self, id: &str) -> PathBuf {
        self.0.join(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_are_under_home() {
        let h = BaleHome(PathBuf::from("/x"));
        assert_eq!(h.records_dir(), PathBuf::from("/x/registry"));
        assert_eq!(h.locks_dir(), PathBuf::from("/x/locks"));
        assert_eq!(h.saved_dir("abc"), PathBuf::from("/x/abc"));
    }
}
