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

/// The user's home dir: `HOME` or `USERPROFILE` (the latter first on
/// Windows), else the OS's own answer. On Windows `dirs::home_dir` asks the
/// shell for the profile folder and never reads `USERPROFILE`, so every
/// part of bale reads the variables first and agrees on one home.
pub fn home_dir() -> Option<PathBuf> {
    let order = if cfg!(windows) {
        ["USERPROFILE", "HOME"]
    } else {
        ["HOME", "USERPROFILE"]
    };
    order
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

/// The real user id of this process.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn uid() -> u32 {
    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// The subfolders of `dir` that are not symlinks, listed until `deadline`;
/// none when `dir` can't be read.
pub fn plain_subdirs(dir: &Path, deadline: std::time::Instant) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .take_while(|_| std::time::Instant::now() < deadline)
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
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
