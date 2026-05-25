//! Criterion benchmarks for the TIFF LZW encoder hot path
//! (`oxideav_tiff::compress::pack_lzw`).
//!
//! Round 129 (depth-mode benchmarks): the LZW encoder is the slowest
//! piece of the Compression=5 strip writer. Round 129 replaced the
//! per-byte `HashMap<(u16, u8), u16>` lookup with a flat-array trie
//! (`first_child` / `next_sibling` / `suffix`, all `[u16; 4096]` /
//! `[u8; 4096]`). This bench file is the A/B harness for that change
//! and for any future encoder tweaks (Clear-on-fill heuristics, code
//! width search, dictionary-pre-seeding for known-prefix corpora).
//!
//! Every fixture is built deterministically in-bench — no committed
//! payload files, no `docs/` reads, no external library calls — so
//! the numbers are reproducible against any future tweak.
//!
//! Scenarios:
//!
//!   - **lzw_random_64k**: 64 KiB of xorshift32 output. The "worst
//!     case" for LZW: very few prefix repeats, so the trie's child
//!     lists stay shallow and the dictionary fills slowly. Sets the
//!     baseline for the per-byte lookup overhead.
//!   - **lzw_repeating_motif_64k**: 16-byte motif repeated 4096
//!     times. Tight prefix reuse — the trie hits long matches early
//!     and the bench is dominated by the inner-loop lookup walk.
//!     This is the case where head-insertion ordering in
//!     `LzwTrie::insert` (most-recently-added-child first) matters
//!     most.
//!   - **lzw_zeros_256k**: 256 KiB of all-zero bytes. Triggers the
//!     longest-possible run of incremental dictionary growth before
//!     `LZW_MAX_CODE = 4095` is hit and the Clear path fires; useful
//!     for measuring the per-Clear reset cost (`first_child.fill(0)`
//!     dominates the post-reset window).
//!   - **lzw_natural_image_64k**: 256x256 8-bit greyscale gradient
//!     (vertical ramp + horizontal sinusoid). A more realistic
//!     image-like payload: medium prefix reuse, no pathological
//!     patterns, code width climbs steadily from 9 to 12 bits over
//!     the strip. Most representative of a real TIFF strip.
//!
//! Run with:
//!     cargo bench -p oxideav-tiff --bench lzw

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use oxideav_tiff::compress::pack_lzw;

/// xorshift32 — same shape as the flac/tta benches so cross-codec
/// numbers stay comparable.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn fixture_random(size: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..size).map(|_| xorshift32(&mut s) as u8).collect()
}

fn fixture_repeating_motif(motif: &[u8], reps: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(motif.len() * reps);
    for _ in 0..reps {
        v.extend_from_slice(motif);
    }
    v
}

fn fixture_zeros(size: usize) -> Vec<u8> {
    vec![0u8; size]
}

/// Build a 256x256 8-bit greyscale "natural-ish" image: a vertical
/// luminance ramp with a per-row horizontal sinusoid riding on top.
/// Each row is row-major, so the byte stream sees gentle local
/// correlation — the realistic case for a TIFF strip.
fn fixture_natural_image_64k() -> Vec<u8> {
    let w = 256usize;
    let h = 256usize;
    let mut v = Vec::with_capacity(w * h);
    for y in 0..h {
        let base = y as i32; // 0..256
        for x in 0..w {
            // Cheap integer sinusoid approximation: triangle wave
            // with period 32.
            let phase = (x as i32) & 31;
            let tri = if phase < 16 { phase } else { 31 - phase };
            // 0..15 → -8..7 amplitude
            let amp = tri - 8;
            let val = (base + amp).clamp(0, 255) as u8;
            v.push(val);
        }
    }
    v
}

fn bench_pack_lzw(c: &mut Criterion) {
    let mut group = c.benchmark_group("pack_lzw");

    // Random — worst case (no prefix reuse).
    let rand = fixture_random(64 * 1024, 0xDEAD_BEEF);
    group.throughput(Throughput::Bytes(rand.len() as u64));
    group.bench_function("lzw_random_64k", |b| {
        b.iter(|| {
            let out = pack_lzw(black_box(&rand));
            black_box(out);
        });
    });

    // Repeating motif — high prefix reuse.
    let motif = b"abcdefghijklmnop";
    let rep = fixture_repeating_motif(motif, 4096); // 64 KiB
    group.throughput(Throughput::Bytes(rep.len() as u64));
    group.bench_function("lzw_repeating_motif_64k", |b| {
        b.iter(|| {
            let out = pack_lzw(black_box(&rep));
            black_box(out);
        });
    });

    // All-zero 256 KiB — triggers the table-fill / Clear-reset path.
    let zeros = fixture_zeros(256 * 1024);
    group.throughput(Throughput::Bytes(zeros.len() as u64));
    group.bench_function("lzw_zeros_256k", |b| {
        b.iter(|| {
            let out = pack_lzw(black_box(&zeros));
            black_box(out);
        });
    });

    // Natural-image-like 256x256 greyscale.
    let natural = fixture_natural_image_64k();
    group.throughput(Throughput::Bytes(natural.len() as u64));
    group.bench_function("lzw_natural_image_64k", |b| {
        b.iter(|| {
            let out = pack_lzw(black_box(&natural));
            black_box(out);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_pack_lzw);
criterion_main!(benches);
