//! Sharing large tracked files between worktrees through file clones.

pub mod reflink;
pub mod store;
pub mod tracked;

use std::path::PathBuf;

/// What names a file's bytes in the clone store.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// Git blob oid, hex.
    Git(String),
    /// Git LFS object SHA-256, hex.
    Lfs(String),
}

/// A large tracked file that can be shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Path relative to the tree.
    pub path: PathBuf,
    pub key: Key,
    pub size: u64,
    /// `0o644` or `0o755`.
    pub mode: u32,
}
