//! Large tracked files of a tree, read from its index.

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{Candidate, Key};
use crate::git::Git;

/// LFS pointers are tiny; a larger blob under `filter=lfs` is not one.
const POINTER_MAX_BYTES: u64 = 1024;

/// Parses a Git LFS pointer into (SHA-256 hex, size).
pub fn parse_lfs_pointer(bytes: &[u8]) -> Option<(String, u64)> {
    if bytes.len() as u64 >= POINTER_MAX_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if !lines
        .next()?
        .starts_with("version https://git-lfs.github.com/spec/")
    {
        return None;
    }
    let (mut oid, mut size) = (None, None);
    for line in lines {
        if let Some(v) = line.strip_prefix("oid sha256:") {
            if v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()) {
                oid = Some(v.to_ascii_lowercase());
            }
        } else if let Some(v) = line.strip_prefix("size ") {
            size = v.parse::<u64>().ok();
        }
    }
    Some((oid?, size?))
}

struct Entry {
    path: String,
    oid: String,
    mode: u32,
}

/// Regular files of the index (stage 0, modes 100644 and 100755) that are
/// `min_bytes` or larger, keyed by blob oid or, for LFS pointers, by the
/// object's SHA-256. Paths that git would convert on checkout (`filter` other
/// than `lfs`, `text`, `eol`, `ident`, `working-tree-encoding`) are left out, and so is the whole tree when
/// `core.autocrlf` is `true` or `input`.
pub fn candidates(tree: &Path, min_bytes: u64) -> Result<Vec<Candidate>> {
    let git = Git::at(tree);
    if let Ok(v) = git.run(&["config", "--get", "core.autocrlf"]) {
        // Git's bool spellings: true/yes/on, any nonzero integer, and a
        // valueless key (empty line). `input` also converts on add.
        let v = v.trim().to_ascii_lowercase();
        let truthy = matches!(v.as_str(), "" | "true" | "yes" | "on" | "input")
            || v.parse::<i64>().is_ok_and(|n| n != 0);
        if truthy {
            return Ok(Vec::new());
        }
    }

    let listing = git.run(&["ls-files", "-s", "-z"])?;
    let mut entries = Vec::new();
    // Non-UTF8 paths are converted lossily, so they name no file and fail safe.
    for rec in listing.split('\0').filter(|r| !r.is_empty()) {
        let Some((meta, path)) = rec.split_once('\t') else {
            continue;
        };
        let mut f = meta.split(' ');
        let (Some(mode), Some(oid), Some("0")) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let mode = match mode {
            "100644" => 0o644,
            "100755" => 0o755,
            _ => continue,
        };
        entries.push(Entry {
            path: path.to_string(),
            oid: oid.to_string(),
            mode,
        });
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    // Sizes, one batch-check over the distinct oids.
    let mut oids: Vec<&str> = entries.iter().map(|e| e.oid.as_str()).collect();
    oids.sort_unstable();
    oids.dedup();
    let input = oids.iter().map(|o| format!("{o}\n")).collect::<String>();
    let checked = git.run_stdin(&["cat-file", "--batch-check"], input.as_bytes())?;
    let mut sizes: HashMap<&str, u64> = HashMap::new();
    for line in checked.lines() {
        let mut f = line.split(' ');
        if let (Some(oid), Some("blob"), Some(size)) = (f.next(), f.next(), f.next()) {
            if let Ok(size) = size.parse() {
                sizes.insert(oid, size);
            }
        }
    }

    // Only paths that can matter need their attributes read.
    let relevant: Vec<&Entry> = entries
        .iter()
        .filter(|e| {
            sizes
                .get(e.oid.as_str())
                .is_some_and(|&s| s >= min_bytes || s < POINTER_MAX_BYTES)
        })
        .collect();
    if relevant.is_empty() {
        return Ok(Vec::new());
    }
    let input: Vec<u8> = relevant
        .iter()
        .flat_map(|e| e.path.bytes().chain([0]))
        .collect();
    let attrs = git.run_stdin(
        &[
            "check-attr",
            "-z",
            "--stdin",
            "filter",
            "text",
            "eol",
            "ident",
            "working-tree-encoding",
        ],
        &input,
    )?;
    let mut by_path: HashMap<&str, [&str; 5]> = HashMap::new();
    let parts: Vec<&str> = attrs.split('\0').collect();
    for t in parts.chunks_exact(3) {
        let slot = match t[1] {
            "filter" => 0,
            "text" => 1,
            "eol" => 2,
            "ident" => 3,
            "working-tree-encoding" => 4,
            _ => continue,
        };
        by_path.entry(t[0]).or_insert(["unspecified"; 5])[slot] = t[2];
    }
    let unset = |v: &str| v == "unspecified" || v == "unset";

    let mut out = Vec::new();
    let mut lfs: Vec<(&Entry, &str)> = Vec::new();
    for e in relevant {
        let [filter, text, eol, ident, encoding] = by_path
            .get(e.path.as_str())
            .copied()
            .unwrap_or(["unspecified"; 5]);
        if !unset(text) || !unset(eol) || !unset(ident) || !unset(encoding) {
            continue;
        }
        let size = sizes[e.oid.as_str()];
        match filter {
            "lfs" if size < POINTER_MAX_BYTES => lfs.push((e, e.oid.as_str())),
            f if unset(f) && size >= min_bytes => out.push(Candidate {
                path: PathBuf::from(&e.path),
                key: Key::Git(e.oid.clone()),
                size,
                mode: e.mode,
            }),
            _ => {}
        }
    }

    if !lfs.is_empty() {
        let mut want: Vec<&str> = lfs.iter().map(|(_, o)| *o).collect();
        want.sort_unstable();
        want.dedup();
        let input = want.iter().map(|o| format!("{o}\n")).collect::<String>();
        let blobs = git.run_stdin_bytes(&["cat-file", "--batch"], input.as_bytes())?;
        let pointers = parse_batch(&blobs)?;
        for (e, oid) in lfs {
            let Some((sha, size)) = pointers.get(oid).and_then(|b| parse_lfs_pointer(b)) else {
                continue;
            };
            if size >= min_bytes {
                out.push(Candidate {
                    path: PathBuf::from(&e.path),
                    key: Key::Lfs(sha),
                    size,
                    mode: e.mode,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Splits `cat-file --batch` output into oid -> content.
fn parse_batch(mut buf: &[u8]) -> Result<HashMap<&str, &[u8]>> {
    let mut map = HashMap::new();
    while !buf.is_empty() {
        let Some(nl) = buf.iter().position(|&b| b == b'\n') else {
            bail!("truncated cat-file --batch output");
        };
        let header = std::str::from_utf8(&buf[..nl])?;
        buf = &buf[nl + 1..];
        let mut f = header.split(' ');
        let (Some(oid), Some(kind)) = (f.next(), f.next()) else {
            bail!("unexpected cat-file --batch header: {header}");
        };
        if kind == "missing" {
            continue;
        }
        let size: usize = f.next().unwrap_or("").parse()?;
        if buf.len() < size {
            bail!("truncated cat-file --batch output");
        }
        map.insert(oid, &buf[..size]);
        buf = buf.get(size + 1..).unwrap_or(&[]);
    }
    Ok(map)
}
