//! Path helpers shared by every command.

use std::io;
use std::path::{Path, PathBuf};

/// `fs::canonicalize`, but on Windows without the `\\?\` verbatim prefix
/// when the path can be written without it. Git does not accept verbatim
/// paths (`git worktree add \\?\C:\...` fails), and every path bale hands
/// to git or stores starts here. Elsewhere it is `fs::canonicalize`.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_std_for_an_existing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let std = std::fs::canonicalize(tmp.path()).unwrap();
        let ours = canonicalize(tmp.path()).unwrap();
        if cfg!(windows) {
            assert!(!ours.to_string_lossy().starts_with(r"\\?\"), "{ours:?}");
            assert_eq!(std::fs::canonicalize(&ours).unwrap(), std);
        } else {
            assert_eq!(ours, std);
        }
    }
}
