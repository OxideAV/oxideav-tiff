//! TIFF Predictor = 3 (the IEEE floating-point predictor) decode.
//!
//! TIFF 6.0 §14 defines the prediction transform as a reversible,
//! codec-independent step applied to the sample bytes before
//! compression. For integer samples it is horizontal *sample*
//! differencing (Predictor = 2); for IEEE 754 floating-point samples it
//! is the floating-point predictor (Predictor = 3), described in
//! `docs/image/tiff/tiff-zstd-compression-50000.md` §4 as a "byte-plane
//! reorder + horizontal differencing for IEEE floats", constrained to
//! 16-/32-/64-bit floating-point data. The encoder, per scan-line:
//!
//!   1. regroups each sample's bytes into `bytes_per_sample` contiguous
//!      planes (the most-significant byte of every sample, then the next
//!      byte of every sample, …), and
//!   2. replaces every byte of the reordered row by its difference from
//!      the preceding byte (modular `u8`, first byte verbatim).
//!
//! The decoder reverses both: an inclusive prefix sum across the row
//! undoes the differencing, then the planes are scattered back into
//! per-sample bytes. The reconstructed bytes are in the same
//! significance order as the encoder input, so the file's own ByteOrder
//! interprets them unchanged.
//!
//! Two complementary oracles are used:
//!
//!   * **Binary-independent.** Hand-built float TIFFs where the test
//!     applies the *encoder-side* transform itself, so the decode must
//!     recover byte-for-byte the same display plane as the un-predicted
//!     (Predictor = 1) twin built from identical samples.
//!   * **Black-box validator.** The `magick` (ImageMagick) binary writes
//!     a Predictor = 1 and a Predictor = 3 float TIFF from one source
//!     image; both must decode to the identical display plane. ImageMagick
//!     is used as an opaque process only — never its source.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use oxideav_tiff::{decode_tiff, DecodedTiff};

// ---------------------------------------------------------------------------
// Hand-built classic-II TIFF assembly (little-endian).
// ---------------------------------------------------------------------------

fn entry(tag: u16, ftype: u16, count: u32, value: u32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&ftype.to_le_bytes());
    e[4..8].copy_from_slice(&count.to_le_bytes());
    e[8..12].copy_from_slice(&value.to_le_bytes());
    e
}

/// Apply the encoder-side floating-point predictor to one scan-line of
/// `n` little-endian samples, each `bps / 8` bytes wide: byte-plane
/// reorder (most-significant byte plane first), then horizontal byte
/// differencing. The row is built for a little-endian (II) file, so the
/// most-significant byte of a sample is its last in-file byte
/// (`bytes - 1`); plane 0 collects those high bytes.
fn encode_float_predictor_row(row: &[u8], n: usize, bps: usize) -> Vec<u8> {
    let bytes = bps / 8;
    assert_eq!(row.len(), n * bytes);
    // Stage 1: regroup into significance-ordered byte planes (plane p =
    // significance rank p of every sample; rank 0 = most significant =
    // in-file byte `bytes-1-p` for little-endian samples).
    let mut planar = vec![0u8; row.len()];
    for p in 0..bytes {
        let src = bytes - 1 - p;
        for k in 0..n {
            planar[p * n + k] = row[k * bytes + src];
        }
    }
    // Stage 2: horizontal byte differencing across the reordered row.
    let mut out = planar.clone();
    for i in (1..out.len()).rev() {
        out[i] = planar[i].wrapping_sub(planar[i - 1]);
    }
    out
}

/// Assemble a single-strip, uncompressed (Compression = 1) IEEE
/// floating-point grayscale TIFF. `bps` ∈ {16, 32, 64}. `sample_bytes`
/// is the raw (un-predicted) per-row sample byte stream, row-major.
/// When `predictor` is 3 the per-row float predictor is applied here so
/// the decoder must reverse it.
fn build_float_gray_tiff(w: u32, h: u32, bps: u16, predictor: u16, sample_bytes: &[u8]) -> Vec<u8> {
    let bytes = (bps / 8) as usize;
    let row_bytes = w as usize * bytes;
    assert_eq!(sample_bytes.len(), row_bytes * h as usize);

    // Apply the predictor row by row when requested.
    let strip: Vec<u8> = if predictor == 3 {
        let mut out = Vec::with_capacity(sample_bytes.len());
        for r in 0..h as usize {
            let row = &sample_bytes[r * row_bytes..r * row_bytes + row_bytes];
            out.extend_from_slice(&encode_float_predictor_row(row, w as usize, bps as usize));
        }
        out
    } else {
        sample_bytes.to_vec()
    };

    // Header (8) + strip data + IFD. Place strip data right after header.
    let mut file = Vec::new();
    file.extend_from_slice(b"II");
    file.extend_from_slice(&42u16.to_le_bytes());
    let strip_off = 8u32;
    let ifd_off = strip_off + strip.len() as u32;
    file.extend_from_slice(&ifd_off.to_le_bytes());
    file.extend_from_slice(&strip);

    let entries: Vec<[u8; 12]> = vec![
        entry(256, 4, 1, w),                  // ImageWidth (LONG)
        entry(257, 4, 1, h),                  // ImageLength (LONG)
        entry(258, 3, 1, bps as u32),         // BitsPerSample (SHORT)
        entry(259, 3, 1, 1),                  // Compression = none
        entry(262, 3, 1, 1),                  // Photometric = BlackIsZero
        entry(273, 4, 1, strip_off),          // StripOffsets
        entry(277, 3, 1, 1),                  // SamplesPerPixel
        entry(278, 4, 1, h),                  // RowsPerStrip = h
        entry(279, 4, 1, strip.len() as u32), // StripByteCounts
        entry(317, 3, 1, predictor as u32),   // Predictor
        entry(339, 3, 1, 3),                  // SampleFormat = IEEE float
    ];
    file.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in &entries {
        file.extend_from_slice(e);
    }
    file.extend_from_slice(&0u32.to_le_bytes()); // next-IFD = 0
    file
}

fn frame_to_gray8(d: &DecodedTiff) -> Vec<u8> {
    assert_eq!(d.frame.planes.len(), 1);
    let stride = d.frame.planes[0].stride;
    let row = d.width as usize;
    let mut out = Vec::with_capacity(row * d.height as usize);
    for y in 0..d.height as usize {
        out.extend_from_slice(&d.frame.planes[0].data[y * stride..y * stride + row]);
    }
    out
}

/// 32-bit float sample bytes for a `w×h` linear ramp in [0,1].
fn ramp_f32(w: u32, h: u32) -> Vec<u8> {
    let n = (w * h) as usize;
    let mut out = Vec::with_capacity(n * 4);
    for i in 0..n {
        let v = i as f32 / (n.max(2) - 1) as f32;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn ramp_f64(w: u32, h: u32) -> Vec<u8> {
    let n = (w * h) as usize;
    let mut out = Vec::with_capacity(n * 8);
    for i in 0..n {
        let v = i as f64 / (n.max(2) - 1) as f64;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// 16-bit IEEE half sample bytes for a `w×h` ramp over a few exact halves.
fn ramp_f16(w: u32, h: u32) -> Vec<u8> {
    // A handful of exactly-representable half values, repeated.
    let halves: [u16; 8] = [
        0x0000, // 0.0
        0x3000, // 0.125
        0x3400, // 0.25
        0x3600, // 0.375
        0x3800, // 0.5
        0x3A00, // 0.75
        0x3BFF, // ~0.99951
        0x3C00, // 1.0
    ];
    let n = (w * h) as usize;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        out.extend_from_slice(&halves[i % halves.len()].to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// Binary-independent oracle: predicted decode == un-predicted decode.
// ---------------------------------------------------------------------------

fn assert_pred3_matches_pred1(w: u32, h: u32, bps: u16, raw: &[u8]) {
    let p1 = build_float_gray_tiff(w, h, bps, 1, raw);
    let p3 = build_float_gray_tiff(w, h, bps, 3, raw);
    let d1 = decode_tiff(&p1).expect("decode predictor=1 float TIFF");
    let d3 = decode_tiff(&p3).expect("decode predictor=3 float TIFF");
    assert_eq!((d1.width, d1.height), (w, h));
    assert_eq!((d3.width, d3.height), (w, h));
    assert_eq!(
        frame_to_gray8(&d1),
        frame_to_gray8(&d3),
        "Predictor=3 decode must equal Predictor=1 decode for the same {bps}-bit float samples"
    );
}

#[test]
fn float_predictor_f32_grayscale_roundtrip() {
    assert_pred3_matches_pred1(16, 8, 32, &ramp_f32(16, 8));
}

#[test]
fn float_predictor_f64_grayscale_roundtrip() {
    assert_pred3_matches_pred1(12, 6, 64, &ramp_f64(12, 6));
}

#[test]
fn float_predictor_f16_grayscale_roundtrip() {
    assert_pred3_matches_pred1(8, 8, 16, &ramp_f16(8, 8));
}

#[test]
fn float_predictor_single_row() {
    // A single scan-line still exercises the per-row plane reorder.
    assert_pred3_matches_pred1(32, 1, 32, &ramp_f32(32, 1));
}

#[test]
fn float_predictor_wide_high_dynamic_range() {
    // Larger magnitudes (not bounded to [0,1]) stress the high-byte
    // plane, which is exactly where the predictor wins compression.
    let w = 20u32;
    let h = 5u32;
    let n = (w * h) as usize;
    let mut raw = Vec::with_capacity(n * 4);
    for i in 0..n {
        let v = (i as f32) * 137.0 - 9000.0;
        raw.extend_from_slice(&v.to_le_bytes());
    }
    assert_pred3_matches_pred1(w, h, 32, &raw);
}

#[test]
fn float_predictor_rejects_non_float_sampleformat() {
    // Predictor=3 with integer SampleFormat must be rejected (no float
    // byte-significance ordering exists to reverse).
    let w = 4u32;
    let h = 2u32;
    let mut file = Vec::new();
    file.extend_from_slice(b"II");
    file.extend_from_slice(&42u16.to_le_bytes());
    let strip: Vec<u8> = (0..(w * h * 2) as usize).map(|i| i as u8).collect();
    let strip_off = 8u32;
    let ifd_off = strip_off + strip.len() as u32;
    file.extend_from_slice(&ifd_off.to_le_bytes());
    file.extend_from_slice(&strip);
    let entries: Vec<[u8; 12]> = vec![
        entry(256, 4, 1, w),
        entry(257, 4, 1, h),
        entry(258, 3, 1, 16),
        entry(259, 3, 1, 1),
        entry(262, 3, 1, 1),
        entry(273, 4, 1, strip_off),
        entry(277, 3, 1, 1),
        entry(278, 4, 1, h),
        entry(279, 4, 1, strip.len() as u32),
        entry(317, 3, 1, 3), // Predictor=3 …
        entry(339, 3, 1, 1), // … but SampleFormat = unsigned int (invalid)
    ];
    file.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in &entries {
        file.extend_from_slice(e);
    }
    file.extend_from_slice(&0u32.to_le_bytes());
    assert!(
        decode_tiff(&file).is_err(),
        "Predictor=3 with integer SampleFormat must be rejected"
    );
}

// ---------------------------------------------------------------------------
// Black-box validator: ImageMagick writes both predictors; decodes match.
// ---------------------------------------------------------------------------

fn magick_bin() -> Option<&'static str> {
    ["magick", "convert"].into_iter().find(|b| {
        Command::new(b)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Produce a float-TIFF from a PGM source with the requested predictor,
/// using ImageMagick as an opaque process. Returns the .tif bytes.
fn magick_float_tiff(
    bin: &str,
    pgm: &[u8],
    bps: u32,
    predictor: u32,
    compress: &str,
) -> Option<Vec<u8>> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-fp-{}-{}-{}-{}-{}",
        std::process::id(),
        predictor,
        bps,
        compress,
        SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join("in.pgm");
    let out_path = dir.join("out.tif");
    fs::File::create(&in_path).ok()?.write_all(pgm).ok()?;
    let status = Command::new(bin)
        .arg(&in_path)
        .arg("-depth")
        .arg(bps.to_string())
        .arg("-define")
        .arg("quantum:format=floating-point")
        .arg("-define")
        .arg(format!("tiff:predictor={predictor}"))
        .arg("-compress")
        .arg(compress)
        .arg(&out_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        let _ = fs::remove_dir_all(&dir);
        return None;
    }
    let bytes = fs::read(&out_path).ok();
    let _ = fs::remove_dir_all(&dir);
    bytes
}

fn make_pgm(w: u32, h: u32) -> Vec<u8> {
    let mut v = format!("P5\n{w} {h}\n255\n").into_bytes();
    for y in 0..h {
        for x in 0..w {
            v.push(((x * 7 + y * 13) & 0xFF) as u8);
        }
    }
    v
}

/// Like `magick_float_tiff` but takes an arbitrary source image and a set
/// of extra `-define` directives (e.g. a tile geometry). The source
/// extension drives ImageMagick's reader (`pgm` / `ppm`).
fn magick_float_tiff_ex(
    bin: &str,
    src: &[u8],
    src_ext: &str,
    bps: u32,
    predictor: u32,
    compress: &str,
    extra_defines: &[&str],
) -> Option<Vec<u8>> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1_000_000);
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-fpx-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join(format!("in.{src_ext}"));
    let out_path = dir.join("out.tif");
    fs::File::create(&in_path).ok()?.write_all(src).ok()?;
    let mut cmd = Command::new(bin);
    cmd.arg(&in_path)
        .arg("-depth")
        .arg(bps.to_string())
        .arg("-define")
        .arg("quantum:format=floating-point")
        .arg("-define")
        .arg(format!("tiff:predictor={predictor}"));
    for d in extra_defines {
        cmd.arg("-define").arg(d);
    }
    let status = cmd
        .arg("-compress")
        .arg(compress)
        .arg(&out_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        let _ = fs::remove_dir_all(&dir);
        return None;
    }
    let bytes = fs::read(&out_path).ok();
    let _ = fs::remove_dir_all(&dir);
    bytes
}

fn frame_to_rgb24(d: &DecodedTiff) -> Vec<u8> {
    assert_eq!(d.frame.planes.len(), 1);
    let stride = d.frame.planes[0].stride;
    let row = d.width as usize * 3;
    let mut out = Vec::with_capacity(row * d.height as usize);
    for y in 0..d.height as usize {
        out.extend_from_slice(&d.frame.planes[0].data[y * stride..y * stride + row]);
    }
    out
}

fn validate_magick(bps: u32, compress: &str) {
    let Some(bin) = magick_bin() else {
        eprintln!("skipping: no `magick`/`convert` binary");
        return;
    };
    let (w, h) = (24u32, 16u32);
    let pgm = make_pgm(w, h);
    let Some(p1) = magick_float_tiff(bin, &pgm, bps, 1, compress) else {
        eprintln!("skipping: magick did not produce a predictor=1 {bps}-bit float TIFF");
        return;
    };
    let Some(p3) = magick_float_tiff(bin, &pgm, bps, 3, compress) else {
        eprintln!("skipping: magick did not produce a predictor=3 {bps}-bit float TIFF");
        return;
    };
    let d1 = decode_tiff(&p1).expect("decode magick predictor=1 float TIFF");
    let d3 = decode_tiff(&p3).expect("decode magick predictor=3 float TIFF");
    assert_eq!((d1.width, d1.height), (w, h));
    assert_eq!((d3.width, d3.height), (w, h));
    assert_eq!(
        frame_to_gray8(&d1),
        frame_to_gray8(&d3),
        "magick {bps}-bit float Predictor=3 ({compress}) must decode identically to Predictor=1"
    );
}

#[test]
fn float_predictor_magick_f32_lzw() {
    validate_magick(32, "LZW");
}

#[test]
fn float_predictor_magick_f32_deflate() {
    validate_magick(32, "Zip");
}

#[test]
fn float_predictor_magick_f64_lzw() {
    validate_magick(64, "LZW");
}

/// Assemble a single-strip, uncompressed IEEE float **RGB** (photometric
/// 2, SamplesPerPixel 3) TIFF, applying the float predictor per row when
/// `predictor == 3`. The per-row predictor spans `w × 3` interleaved
/// samples (chunky layout), exactly as the decoder's chunky strip arm
/// drives it.
fn build_float_rgb_tiff(w: u32, h: u32, bps: u16, predictor: u16, sample_bytes: &[u8]) -> Vec<u8> {
    let bytes = (bps / 8) as usize;
    let row_bytes = w as usize * 3 * bytes;
    assert_eq!(sample_bytes.len(), row_bytes * h as usize);
    let strip: Vec<u8> = if predictor == 3 {
        let mut out = Vec::with_capacity(sample_bytes.len());
        for r in 0..h as usize {
            let row = &sample_bytes[r * row_bytes..r * row_bytes + row_bytes];
            out.extend_from_slice(&encode_float_predictor_row(
                row,
                w as usize * 3,
                bps as usize,
            ));
        }
        out
    } else {
        sample_bytes.to_vec()
    };
    // Two out-of-line SHORT[3] arrays (BitsPerSample, SampleFormat) are
    // appended after the IFD — a 3-element SHORT array does not fit the
    // 4-byte inline value slot.
    let mut file = Vec::new();
    file.extend_from_slice(b"II");
    file.extend_from_slice(&42u16.to_le_bytes());
    let strip_off = 8u32;
    let ifd_off = strip_off + strip.len() as u32;
    file.extend_from_slice(&ifd_off.to_le_bytes());
    file.extend_from_slice(&strip);
    let n_entries = 11u16;
    let ifd_end = ifd_off + 2 + 12 * n_entries as u32 + 4;
    let bps_off = ifd_end;
    let sf_off = ifd_end + 6;
    let entries: Vec<[u8; 12]> = vec![
        entry(256, 4, 1, w),
        entry(257, 4, 1, h),
        entry(258, 3, 3, bps_off),
        entry(259, 3, 1, 1),
        entry(262, 3, 1, 2),
        entry(273, 4, 1, strip_off),
        entry(277, 3, 1, 3),
        entry(278, 4, 1, h),
        entry(279, 4, 1, strip.len() as u32),
        entry(317, 3, 1, predictor as u32),
        entry(339, 3, 3, sf_off),
    ];
    file.extend_from_slice(&n_entries.to_le_bytes());
    for e in &entries {
        file.extend_from_slice(e);
    }
    file.extend_from_slice(&0u32.to_le_bytes());
    // BitsPerSample[3] and SampleFormat[3] arrays.
    for _ in 0..3 {
        file.extend_from_slice(&bps.to_le_bytes());
    }
    for _ in 0..3 {
        file.extend_from_slice(&3u16.to_le_bytes());
    }
    file
}

#[test]
fn float_predictor_rgb_f32_handbuilt() {
    let (w, h) = (10u32, 6u32);
    let n = (w * h) as usize;
    let mut raw = Vec::with_capacity(n * 3 * 4);
    for i in 0..n {
        // Distinct per-channel ramps so a mis-ordered de-plane shows up.
        let r = i as f32 / (n - 1) as f32;
        let g = 1.0 - r;
        let b = (i as f32 * 0.5).fract();
        raw.extend_from_slice(&r.to_le_bytes());
        raw.extend_from_slice(&g.to_le_bytes());
        raw.extend_from_slice(&b.to_le_bytes());
    }
    let p1 = build_float_rgb_tiff(w, h, 32, 1, &raw);
    let p3 = build_float_rgb_tiff(w, h, 32, 3, &raw);
    let d1 = decode_tiff(&p1).expect("decode predictor=1 RGB float TIFF");
    let d3 = decode_tiff(&p3).expect("decode predictor=3 RGB float TIFF");
    assert_eq!((d3.width, d3.height), (w, h));
    assert_eq!(
        frame_to_rgb24(&d1),
        frame_to_rgb24(&d3),
        "RGB float Predictor=3 must decode identically to Predictor=1"
    );
}

#[test]
fn float_predictor_magick_tiled_f32_lzw() {
    // Tiled layout: the float predictor runs per tile-row with the tile
    // width as the per-row sample count.
    let Some(bin) = magick_bin() else {
        eprintln!("skipping: no `magick`/`convert` binary");
        return;
    };
    let (w, h) = (40u32, 40u32);
    let pgm = make_pgm(w, h);
    let tile = "tiff:tile-geometry=16x16";
    let Some(p1) = magick_float_tiff_ex(bin, &pgm, "pgm", 32, 1, "LZW", &[tile]) else {
        eprintln!("skipping: magick did not produce predictor=1 tiled float TIFF");
        return;
    };
    let Some(p3) = magick_float_tiff_ex(bin, &pgm, "pgm", 32, 3, "LZW", &[tile]) else {
        eprintln!("skipping: magick did not produce predictor=3 tiled float TIFF");
        return;
    };
    let d1 = decode_tiff(&p1).expect("decode magick predictor=1 tiled float TIFF");
    let d3 = decode_tiff(&p3).expect("decode magick predictor=3 tiled float TIFF");
    assert_eq!((d3.width, d3.height), (w, h));
    assert_eq!(
        frame_to_gray8(&d1),
        frame_to_gray8(&d3),
        "magick tiled float Predictor=3 must decode identically to Predictor=1"
    );
}
