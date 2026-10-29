use std::io::Cursor;

use bale_core::store::codec;

/// Pseudo-random but compressible bytes: a seeded 4 KiB block repeated with
/// small mutations, so a full compress shrinks it but not to nothing.
fn sample(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let block: Vec<u8> = (0..4096).map(|_| next() as u8).collect();
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let mut b = block.clone();
        for _ in 0..64 {
            let r = next();
            b[(r as usize) % 4096] = (r >> 32) as u8;
        }
        out.extend_from_slice(&b);
    }
    out.truncate(len);
    out
}

#[test]
fn full_round_trip() {
    let data = sample(3 << 20, 7);
    let z = codec::compress(&data, codec::LEVEL).unwrap();
    assert!(z.len() < data.len());
    assert_eq!(
        codec::decompress(&z, None, data.len() as u64).unwrap(),
        data
    );
}

#[test]
fn patch_is_much_smaller_than_full_and_round_trips() {
    let base = sample(8 << 20, 1);
    let mut new = base.clone();
    new[4_000_000..4_000_100].fill(0xAB);
    let full = codec::compress(&new, codec::LEVEL).unwrap();
    let patch = codec::compress_patch(&new, &base, codec::LEVEL).unwrap();
    assert!(
        patch.len() * 20 < full.len(),
        "patch {} full {}",
        patch.len(),
        full.len()
    );
    assert_eq!(
        codec::decompress(&patch, Some(&base), new.len() as u64).unwrap(),
        new
    );
}

#[test]
fn patch_with_wrong_base_fails_or_differs() {
    // `new` is close to `base`, so the frame really depends on it; with
    // unrelated data the encoder would ignore the base and any base decodes.
    let base = sample(2 << 20, 1);
    let mut new = base.clone();
    new[1000..1100].fill(0x5A);
    let patch = codec::compress_patch(&new, &base, codec::LEVEL).unwrap();
    let other = sample(2 << 20, 3);
    let out = codec::decompress(&patch, Some(&other), new.len() as u64);
    assert!(out.map(|o| o != new).unwrap_or(true));
}

#[test]
fn undersized_hint_is_an_error() {
    let data = sample(1 << 20, 4);
    let z = codec::compress(&data, codec::LEVEL).unwrap();
    assert!(codec::decompress(&z, None, 1024).is_err());
}

fn stream_round_trip(data: &[u8], base: Option<&[u8]>) -> usize {
    let mut z = Vec::new();
    let written = codec::compress_stream(
        &mut Cursor::new(data),
        &mut z,
        base,
        data.len() as u64,
        codec::LEVEL,
    )
    .unwrap();
    assert_eq!(written, z.len() as u64);
    let mut out = Vec::new();
    let n = codec::decompress_stream(&mut Cursor::new(&z), &mut out, base).unwrap();
    assert_eq!(n, data.len() as u64);
    assert!(out == data, "stream round trip differs");
    z.len()
}

#[test]
fn stream_round_trip_small_and_empty() {
    stream_round_trip(&[], None);
    stream_round_trip(&sample(3 << 20, 5), None);
    let base = sample(3 << 20, 5);
    let mut new = base.clone();
    new[100..200].fill(1);
    stream_round_trip(&new, Some(&base));
}

#[test]
fn stream_round_trip_100_mib_with_and_without_base() {
    let base = sample(100 << 20, 11);
    let mut new = base.clone();
    new[50 << 20..(50 << 20) + 4096].fill(0xCD);
    let plain = stream_round_trip(&new, None);
    let patched = stream_round_trip(&new, Some(&base));
    assert!(patched * 20 < plain, "patch {patched} plain {plain}");
}

#[test]
fn stream_patch_window_covers_a_grown_file() {
    let base = sample(8 << 20, 21);
    let mut new = base.clone();
    new[1_000_000..1_000_100].fill(0xEE);
    // The appended tail repeats the start of the base, so its matches sit
    // further back than the base length.
    new.extend_from_slice(&base[..4 << 20]);
    let plain = stream_round_trip(&new, None);
    let patched = stream_round_trip(&new, Some(&base));
    assert!(patched * 10 < plain, "patch {patched} plain {plain}");
}
