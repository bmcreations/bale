//! Blobs keyed by the SHA-256 of their uncompressed content, each stored as a
//! zstd frame, optionally as a patch against another blob.
//!
//! A blob is two files under `objects/<first 2 hex>/`: `<hash>.zst` (the
//! frame) and `<hash>.json` (its [`BlobMeta`]). The frame is written and
//! renamed into place first and the meta second, and `has` looks at the meta,
//! so a crash leaves at worst an unreferenced `.zst`, never a meta whose frame
//! is missing or partial.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use super::codec::{self, LEVEL, PATCH_MAX_BYTES};
use super::hex;
use crate::BaleHome;

/// Lowercase hex SHA-256 of the uncompressed content.
pub type Hash = String;

/// Files up to this size use the whole-buffer codec; larger ones stream.
const BUFFER_MAX_BYTES: u64 = 64 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobMeta {
    pub hash: Hash,
    /// Uncompressed size.
    pub size: u64,
    /// The blob this one is a patch against, if any.
    pub base: Option<Hash>,
    /// Patch hops to a full blob: 0 for a full blob.
    pub depth: u32,
    /// Compressed size on disk.
    pub stored: u64,
}

pub struct Store {
    root: PathBuf,
}

/// Forwards writes while hashing them.
struct HashingWriter<'a, W: Write> {
    inner: &'a mut W,
    hasher: Sha256,
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Store {
    /// Opens (creating if needed) the store under `$BALE_HOME/store`.
    pub fn open(home: &BaleHome) -> Result<Store> {
        let root = home.0.join("store");
        for sub in ["objects", "manifests"] {
            let dir = root.join(sub);
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(Store { root })
    }

    fn dir_for(&self, hash: &str) -> PathBuf {
        self.root.join("objects").join(&hash[..2])
    }

    fn zst_path(&self, hash: &str) -> PathBuf {
        self.dir_for(hash).join(format!("{hash}.zst"))
    }

    fn json_path(&self, hash: &str) -> PathBuf {
        self.dir_for(hash).join(format!("{hash}.json"))
    }

    fn check(hash: &str) -> Result<()> {
        if !is_hash(hash) {
            bail!("not a sha256 hex digest: {hash:?}");
        }
        Ok(())
    }

    pub fn has(&self, hash: &Hash) -> bool {
        is_hash(hash) && self.json_path(hash).is_file()
    }

    pub fn meta(&self, hash: &Hash) -> Result<BlobMeta> {
        Self::check(hash)?;
        let path = self.json_path(hash);
        let text = fs::read_to_string(&path).with_context(|| format!("read blob meta {hash}"))?;
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Stores `src` (whose SHA-256 is `hash`) and returns its meta. A blob
    /// already present is left alone. `base` is used as a patch base only if
    /// it is present, within `max_depth` and small enough for the codec;
    /// otherwise the blob is stored full.
    pub fn put(
        &self,
        src: &Path,
        hash: &Hash,
        base: Option<&Hash>,
        max_depth: u32,
    ) -> Result<BlobMeta> {
        Self::check(hash)?;
        if self.has(hash) {
            return self.meta(hash);
        }
        let size = fs::metadata(src)
            .with_context(|| format!("stat {}", src.display()))?
            .len();

        let mut base_meta = None;
        let mut base_bytes = None;
        if let Some(b) = base.filter(|b| self.has(b)) {
            let m = self.meta(b)?;
            if m.depth < max_depth && m.size <= PATCH_MAX_BYTES && size <= PATCH_MAX_BYTES {
                base_bytes = Some(self.read(b)?);
                base_meta = Some(m);
            }
        }

        let dir = self.dir_for(hash);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let mut tmp = NamedTempFile::new_in(&dir)?;
        let stored = {
            let mut input =
                BufReader::new(File::open(src).with_context(|| format!("open {}", src.display()))?);
            let mut out = BufWriter::new(tmp.as_file_mut());
            let stored = if size <= BUFFER_MAX_BYTES {
                let mut data = Vec::with_capacity(size as usize);
                io::Read::read_to_end(&mut input, &mut data)
                    .with_context(|| format!("read {}", src.display()))?;
                let frame = match &base_bytes {
                    Some(b) => codec::compress_patch(&data, b, LEVEL)?,
                    None => codec::compress(&data, LEVEL)?,
                };
                out.write_all(&frame)?;
                frame.len() as u64
            } else {
                codec::compress_stream(&mut input, &mut out, base_bytes.as_deref(), size, LEVEL)?
            };
            out.flush()?;
            stored
        };
        tmp.as_file().sync_all()?;
        tmp.persist(self.zst_path(hash))
            .map_err(|e| anyhow!("persist blob {hash}: {}", e.error))?;

        let meta = BlobMeta {
            hash: hash.clone(),
            size,
            base: base_meta.as_ref().map(|m| m.hash.clone()),
            depth: base_meta.map_or(0, |m| m.depth + 1),
            stored,
        };
        let mut tmp = NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(&mut tmp, &meta)?;
        tmp.as_file().sync_all()?;
        tmp.persist(self.json_path(hash))
            .map_err(|e| anyhow!("persist blob meta {hash}: {}", e.error))?;
        // Make both renames durable before anyone relies on the blob.
        File::open(&dir).and_then(|d| d.sync_all()).ok();
        Ok(meta)
    }

    /// The full content of `hash`, base chain resolved and checksum verified.
    pub fn read(&self, hash: &Hash) -> Result<Vec<u8>> {
        let meta = self.meta(hash)?;
        let frame = fs::read(self.zst_path(hash)).with_context(|| format!("read blob {hash}"))?;
        let base = meta.base.as_ref().map(|b| self.read(b)).transpose()?;
        let data = codec::decompress(&frame, base.as_deref(), meta.size)
            .with_context(|| format!("decompress blob {hash}"))?;
        if hex(&Sha256::digest(&data)) != *hash {
            bail!("blob {hash} is corrupt: content hash mismatch");
        }
        Ok(data)
    }

    /// Streams the content of `hash` to `dst` and returns the bytes written.
    /// Only the base is held in memory. A corrupt blob is reported after the
    /// bytes were written, so the caller must undo what it wrote (unstash
    /// truncates the file back to 0; it never renames over a stashed path).
    pub fn read_to(&self, hash: &Hash, dst: &mut impl Write) -> Result<u64> {
        let meta = self.meta(hash)?;
        let base = meta.base.as_ref().map(|b| self.read(b)).transpose()?;
        let mut frame = BufReader::new(
            File::open(self.zst_path(hash)).with_context(|| format!("open blob {hash}"))?,
        );
        let mut out = HashingWriter {
            inner: dst,
            hasher: Sha256::new(),
        };
        let n = codec::decompress_stream(&mut frame, &mut out, base.as_deref())
            .with_context(|| format!("decompress blob {hash}"))?;
        if n != meta.size || hex(&out.hasher.finalize()) != *hash {
            bail!("blob {hash} is corrupt: content mismatch");
        }
        Ok(n)
    }

    /// Deletes the blob and returns the bytes freed. The meta goes first so a
    /// crash never leaves a meta without its frame. Does not check for blobs
    /// that use this one as a base; that is the caller's (GC's) job.
    pub fn remove(&self, hash: &Hash) -> Result<u64> {
        Self::check(hash)?;
        let mut freed = 0;
        for path in [self.json_path(hash), self.zst_path(hash)] {
            match fs::metadata(&path) {
                Ok(m) => {
                    freed += m.len();
                    fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
            }
        }
        Ok(freed)
    }

    /// The meta of every blob.
    pub fn list(&self) -> Result<Vec<BlobMeta>> {
        let mut out = Vec::new();
        let objects = self.root.join("objects");
        for shard in
            fs::read_dir(&objects).with_context(|| format!("read {}", objects.display()))?
        {
            let shard = shard?.path();
            if !shard.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&shard)? {
                let path = entry?.path();
                let stem = path.file_stem().and_then(|s| s.to_str());
                if path.extension().is_some_and(|e| e == "json") {
                    if let Some(h) = stem.filter(|s| is_hash(s)) {
                        out.push(self.meta(&h.to_string())?);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.hash.cmp(&b.hash));
        Ok(out)
    }
}
