//! Content-addressed storage for stashed build files.

pub mod blob;
pub mod codec;
pub mod index;
pub mod manifest;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

pub use blob::{BlobMeta, Hash, Store};

/// Lowercase hex SHA-256 of the file's bytes, read in 1 MiB steps.
pub fn sha256_file(path: &Path) -> Result<Hash> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
