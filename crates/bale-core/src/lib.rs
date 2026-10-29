pub mod busy;
pub mod config;
pub mod derived_data;
pub mod detect;
pub mod git;
pub mod idle;
pub mod lock;
pub mod ops;
pub mod par;
pub mod paths;
pub mod registry;
pub mod store;
pub mod trace;

use std::path::PathBuf;

/// Bytes in binary units with one decimal, e.g. `1.5 GiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Root of bale's per-machine state (`$BALE_HOME`, default `~/.bale`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaleHome(pub PathBuf);

impl BaleHome {
    /// Reads `BALE_HOME`, falling back to `<home dir>/.bale`.
    pub fn from_env() -> BaleHome {
        match std::env::var_os("BALE_HOME") {
            Some(p) if !p.is_empty() => BaleHome(PathBuf::from(p)),
            _ => BaleHome(
                crate::paths::home_dir()
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

    /// DerivedData roots learned from `-derivedDataPath`, one path per line.
    pub fn dd_roots_file(&self) -> PathBuf {
        self.0.join("dd-roots")
    }

    /// Where the local usage trace is kept (see [`trace`]).
    pub fn trace_dir(&self) -> PathBuf {
        self.0.join("trace")
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
