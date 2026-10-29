//! zstd frames for stashed files, optionally as a delta against a base.
//!
//! A delta is what `zstd --patch-from` produces: the base is referenced as a
//! prefix, so the frame only carries what changed. The same base must be
//! passed to decompress; a frame is checksummed, so the wrong base is an error
//! rather than silently wrong bytes.

use std::io::{Read, Write};

use anyhow::{anyhow, bail, Context, Result};
use zstd_safe::{CCtx, CParameter, DCtx, DParameter, InBuffer, OutBuffer};

/// Compression level the store uses.
pub const LEVEL: i32 = 3;

/// Files larger than this, or with a base larger than this, are stored without
/// a base (the window would not fit in zstd's 2 GiB limit).
pub const PATCH_MAX_BYTES: u64 = 1 << 30;

/// Buffer size for the streaming variants.
const CHUNK: usize = 1 << 20;

fn zerr(code: usize) -> anyhow::Error {
    anyhow!("zstd: {}", zstd_safe::get_error_name(code))
}

/// Smallest `w` in 10..=31 with `1 << w >= len`.
fn window_log(len: usize) -> u32 {
    let w = usize::BITS - len.saturating_sub(1).leading_zeros();
    w.clamp(10, 31)
}

fn usable_base(base: Option<&[u8]>) -> Option<&[u8]> {
    base.filter(|b| !b.is_empty() && (b.len() as u64) <= PATCH_MAX_BYTES)
}

fn new_cctx<'a>(level: i32, base: Option<&'a [u8]>, window: usize) -> Result<CCtx<'a>> {
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(level))
        .map_err(zerr)?;
    cctx.set_parameter(CParameter::ChecksumFlag(true))
        .map_err(zerr)?;
    if let Some(base) = base {
        cctx.set_parameter(CParameter::WindowLog(window_log(window.max(base.len()))))
            .map_err(zerr)?;
        cctx.set_parameter(CParameter::EnableLongDistanceMatching(true))
            .map_err(zerr)?;
        // Applies to the next frame only, and must outlive it (the `'a` bound).
        cctx.ref_prefix(base).map_err(zerr)?;
    }
    Ok(cctx)
}

fn new_dctx<'a>(base: Option<&'a [u8]>) -> Result<DCtx<'a>> {
    let mut dctx = DCtx::create();
    if let Some(base) = base {
        dctx.set_parameter(DParameter::WindowLogMax(31))
            .map_err(zerr)?;
        dctx.ref_prefix(base).map_err(zerr)?;
    }
    Ok(dctx)
}

/// `data` as a standalone zstd frame.
pub fn compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
    let mut cctx = new_cctx(level, None, 0)?;
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).map_err(zerr)?;
    Ok(out)
}

/// `data` as a frame that references `base`. Falls back to [`compress`] when
/// either side exceeds [`PATCH_MAX_BYTES`].
pub fn compress_patch(data: &[u8], base: &[u8], level: i32) -> Result<Vec<u8>> {
    let base = usable_base(Some(base)).filter(|_| data.len() as u64 <= PATCH_MAX_BYTES);
    let mut cctx = new_cctx(level, base, data.len())?;
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).map_err(zerr)?;
    Ok(out)
}

/// The bytes `frame` encodes. `base` must be the one the frame was made
/// against (or `None`). `size_hint` is the expected output size; a frame that
/// decodes to more is an error.
pub fn decompress(frame: &[u8], base: Option<&[u8]>, size_hint: u64) -> Result<Vec<u8>> {
    let mut dctx = new_dctx(usable_base(base))?;
    let cap = usize::try_from(size_hint).context("size hint too large")?;
    let mut out = Vec::with_capacity(cap);
    dctx.decompress(&mut out, frame).map_err(zerr)?;
    Ok(out)
}

/// Compresses `src` to `dst` in 1 MiB steps, holding only `base` in memory.
/// Returns the compressed bytes written.
pub fn compress_stream(
    src: &mut impl Read,
    dst: &mut impl Write,
    base: Option<&[u8]>,
    level: i32,
) -> Result<u64> {
    // The source length is unknown, so the window is sized from the base alone.
    let mut cctx = new_cctx(level, usable_base(base), 0)?;
    let mut in_buf = vec![0u8; CHUNK];
    let mut out_buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    let mut eof = false;
    while !eof {
        let n = read_full(src, &mut in_buf)?;
        eof = n < in_buf.len();
        let mut input = InBuffer::around(&in_buf[..n]);
        let op = if eof {
            zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_end
        } else {
            zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_continue
        };
        loop {
            let mut output = OutBuffer::around(&mut out_buf[..]);
            let remaining = cctx
                .compress_stream2(&mut output, &mut input, op)
                .map_err(zerr)?;
            let pos = output.pos();
            dst.write_all(&out_buf[..pos])?;
            written += pos as u64;
            // For `end` the frame is done at 0 remaining; for `continue` once
            // the input is consumed.
            let done = if eof {
                remaining == 0
            } else {
                input.pos() == n
            };
            if done {
                break;
            }
        }
    }
    Ok(written)
}

/// Decompresses one frame from `src` to `dst` in 1 MiB steps. `base` must be
/// the one the frame was made against. Returns the decompressed bytes written.
pub fn decompress_stream(
    src: &mut impl Read,
    dst: &mut impl Write,
    base: Option<&[u8]>,
) -> Result<u64> {
    let mut dctx = new_dctx(usable_base(base))?;
    let mut in_buf = vec![0u8; CHUNK];
    let mut out_buf = vec![0u8; CHUNK];
    let mut written = 0u64;
    loop {
        let n = read_full(src, &mut in_buf)?;
        if n == 0 {
            bail!("zstd: truncated frame");
        }
        let mut input = InBuffer::around(&in_buf[..n]);
        // Drain until the input is consumed and the decoder has nothing
        // buffered (a full output buffer means more may be pending).
        loop {
            let mut output = OutBuffer::around(&mut out_buf[..]);
            let hint = dctx
                .decompress_stream(&mut output, &mut input)
                .map_err(zerr)?;
            let pos = output.pos();
            dst.write_all(&out_buf[..pos])?;
            written += pos as u64;
            if hint == 0 {
                return Ok(written);
            }
            if input.pos() == n && pos < out_buf.len() {
                break;
            }
        }
    }
}

/// Reads until `buf` is full or `src` hits EOF; returns the bytes read.
fn read_full(src: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}
