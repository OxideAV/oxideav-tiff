//! TIFF 6.0 §SampleFormat (tag 339, page 80) value 3 — IEEE 754
//! floating-point grayscale decode.
//!
//! §SampleFormat lists value `3` as "IEEE floating point data [IEEE]"
//! and notes that SampleFormat "does not specify the size of data
//! samples; this is still done by the BitsPerSample field" — so a float
//! image is identified by `SampleFormat = 3` and sized by BitsPerSample
//! (16-bit half, 32-bit single, 64-bit double). The companion
//! SMinSampleValue (tag 340) / SMaxSampleValue (tag 341) "makes it
//! possible for readers to assume that data samples are bound to the
//! range [SMinSampleValue, SMaxSampleValue] without scanning the image
//! data"; absent them, this decoder scans the finite sample extent.
//!
//! Float samples decode natively (IMAGE_CRATE_API): the image is
//! `GrayF32Le`, one little-endian `f32` per pixel exactly as stored
//! (half / double widened or narrowed), with the WhiteIsZero polarity
//! re-expressed as `1 − x`. The declared SMin / SMax extent is
//! reported on `TiffFormatInfo` for callers who want a file-declared
//! mapping; the contract's `to_rgb8` tone-scales by clamping each
//! sample to `[0, 1]` and scaling to 255 (non-finite → 0).
//!
//! These tests build minimal hand-crafted classic-II TIFF byte strings
//! and drive them through the public `decode_page` entry point, so the
//! expected samples are the ones written — a binary-independent oracle.

use oxideav_tiff::{decode_page, TiffPixelFormat};

/// The decoded `GrayF32Le` plane as `f32`s.
fn gray_f32(d: &oxideav_tiff::Page) -> Vec<f32> {
    assert_eq!(d.image.format, TiffPixelFormat::GrayF32Le);
    assert_eq!(d.image.planes[0].stride, d.image.width as usize * 4);
    d.image.planes[0]
        .data
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// IFD entry, SHORT (field-type = 3) with a single inline value.
fn entry_short(tag: u16, value: u16) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..10].copy_from_slice(&value.to_le_bytes());
    e
}

/// IFD entry, LONG (field-type = 4) with a single inline value.
fn entry_long(tag: u16, value: u32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&4u16.to_le_bytes()); // LONG
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..12].copy_from_slice(&value.to_le_bytes());
    e
}

/// IFD entry, FLOAT (field-type = 11) with a single inline value (a
/// FLOAT is 4 bytes, fits the value/offset slot).
fn entry_float(tag: u16, value: f32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&11u16.to_le_bytes()); // FLOAT
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..12].copy_from_slice(&value.to_le_bytes());
    e
}

/// Assemble a 1-row, `w`-pixel single-channel float grayscale classic-II
/// TIFF. `bits_per_sample` ∈ {16, 32, 64}; `photometric` is 0
/// (WhiteIsZero) or 1 (BlackIsZero); `extra` carries any additional IFD
/// entries (e.g. SMin/SMaxSampleValue) appended after the mandatory ten.
fn build_float_row(
    w: u32,
    bits_per_sample: u16,
    photometric: u16,
    strip: &[u8],
    extra: &[[u8; 12]],
) -> Vec<u8> {
    let mut out = Vec::new();

    // 0..8 — classic LE header; first-IFD offset patched below.
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    let ifd_off_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());

    // 8.. — the strip payload (StripOffsets = 8 lines up).
    let strip_off = out.len() as u32;
    out.extend_from_slice(strip);
    if out.len() % 2 != 0 {
        out.push(0); // word-align the IFD start (TIFF 6.0 §2).
    }

    // IFD starts here.
    let ifd_off = out.len() as u32;
    out[ifd_off_pos..ifd_off_pos + 4].copy_from_slice(&ifd_off.to_le_bytes());

    let n_entries = 10u16 + extra.len() as u16;
    out.extend_from_slice(&n_entries.to_le_bytes());
    out.extend_from_slice(&entry_short(256, w as u16)); // ImageWidth
    out.extend_from_slice(&entry_short(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short(258, bits_per_sample)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, photometric)); // Photometric
    out.extend_from_slice(&entry_long(273, strip_off)); // StripOffsets
    out.extend_from_slice(&entry_short(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_long(279, strip.len() as u32)); // StripByteCounts
    out.extend_from_slice(&entry_short(339, 3)); // SampleFormat = 3
    for e in extra {
        out.extend_from_slice(e);
    }

    // next_ifd = 0.
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn f32_strip(vals: &[f32]) -> Vec<u8> {
    let mut s = Vec::new();
    for &v in vals {
        s.extend_from_slice(&v.to_le_bytes());
    }
    s
}

/// IEEE 754 binary16 encode (test-side oracle, independent of the
/// decoder's binary16 → f32 widening).
fn f32_to_half_bits(x: f32) -> u16 {
    // Sufficient for the small finite values used in these tests.
    if x == 0.0 {
        return 0;
    }
    let sign = if x < 0.0 { 0x8000u16 } else { 0 };
    let ax = x.abs();
    let mut exp = ax.log2().floor() as i32;
    let mut mant = ax / 2.0f32.powi(exp);
    if mant >= 2.0 {
        mant /= 2.0;
        exp += 1;
    }
    let biased = (exp + 15) as u16;
    let frac = ((mant - 1.0) * 1024.0).round() as u16;
    sign | (biased << 10) | (frac & 0x3ff)
}

fn half_strip(vals: &[f32]) -> Vec<u8> {
    let mut s = Vec::new();
    for &v in vals {
        s.extend_from_slice(&f32_to_half_bits(v).to_le_bytes());
    }
    s
}

fn f64_strip(vals: &[f64]) -> Vec<u8> {
    let mut s = Vec::new();
    for &v in vals {
        s.extend_from_slice(&v.to_le_bytes());
    }
    s
}

/// Assert `decode_page` returned an error whose Display includes the
/// given substring (`Page` is not `Debug`).
fn expect_err_containing(bytes: &[u8], needle: &str) {
    match decode_page(bytes) {
        Ok(_) => panic!("expected an error containing {needle:?}, got Ok(..)"),
        Err(e) => {
            let msg = format!("{e}");
            assert!(
                msg.contains(needle),
                "expected error to contain {needle:?}, got: {msg}"
            );
        }
    }
}

#[test]
fn float32_blackiszero_native_samples() {
    // Samples 0.0, 0.25, 0.5, 0.75, 1.0 decode as themselves; to_rgb8
    // clamps to [0, 1] and scales with round-half-up: 0, 64, 128, 191, 255.
    let strip = f32_strip(&[0.0, 0.25, 0.5, 0.75, 1.0]);
    let bytes = build_float_row(5, 32, 1, &strip, &[]);
    let d = decode_page(&bytes).expect("float32 grayscale must decode");
    assert_eq!((d.image.width, d.image.height), (5, 1));
    assert_eq!(gray_f32(&d), vec![0.0, 0.25, 0.5, 0.75, 1.0]);
    assert_eq!(
        d.image.to_rgb8(),
        [0u8, 64, 128, 191, 255]
            .iter()
            .flat_map(|&g| [g, g, g])
            .collect::<Vec<u8>>()
    );
    assert_eq!(d.layout.smin_sample_value, None);
    assert_eq!(d.layout.smax_sample_value, None);
}

#[test]
fn float32_negative_samples_kept_and_clamped_in_to_rgb8() {
    // Native plane keeps -1.0; the 8-bit view clamps it to 0.
    let strip = f32_strip(&[-1.0, 0.0, 1.0]);
    let bytes = build_float_row(3, 32, 1, &strip, &[]);
    let d = decode_page(&bytes).expect("float32 grayscale must decode");
    assert_eq!(gray_f32(&d), vec![-1.0, 0.0, 1.0]);
    assert_eq!(d.image.to_rgb8(), vec![0u8, 0, 0, 0, 0, 0, 255, 255, 255]);
}

#[test]
fn float32_smin_smax_reported_not_applied() {
    // SMin = -1, SMax = 3 are surfaced on the format info; the samples
    // are untouched and to_rgb8 still uses the [0, 1] clamp.
    let strip = f32_strip(&[0.0, 0.5, 1.0]);
    let extra = [entry_float(340, -1.0), entry_float(341, 3.0)];
    let bytes = build_float_row(3, 32, 1, &strip, &extra);
    let d = decode_page(&bytes).expect("float32 with SMin/SMax must decode");
    assert_eq!(gray_f32(&d), vec![0.0, 0.5, 1.0]);
    assert_eq!(d.layout.smin_sample_value, Some(-1.0));
    assert_eq!(d.layout.smax_sample_value, Some(3.0));
    assert_eq!(d.image.to_rgb8()[..7], [0u8, 0, 0, 128, 128, 128, 255]);
}

#[test]
fn float32_whiteiszero_polarity() {
    // WhiteIsZero float is re-expressed black-is-zero: x → 1 − x.
    let strip = f32_strip(&[0.0, 1.0]);
    let bytes = build_float_row(2, 32, 0, &strip, &[]);
    let d = decode_page(&bytes).expect("float32 WhiteIsZero must decode");
    assert_eq!(gray_f32(&d), vec![1.0, 0.0]);
    assert_eq!(d.image.to_rgb8(), vec![255u8, 255, 255, 0, 0, 0]);
}

#[test]
fn float32_nonfinite_kept_and_renders_floor() {
    // NaN and +Inf survive in the native plane; to_rgb8 renders them 0.
    let strip = f32_strip(&[0.0, f32::NAN, 1.0, f32::INFINITY]);
    let bytes = build_float_row(4, 32, 1, &strip, &[]);
    let d = decode_page(&bytes).expect("float32 with non-finite must decode");
    let g = gray_f32(&d);
    assert_eq!(g[0], 0.0);
    assert!(g[1].is_nan());
    assert_eq!(g[2], 1.0);
    assert_eq!(g[3], f32::INFINITY);
    assert_eq!(
        d.image.to_rgb8(),
        vec![0u8, 0, 0, 0, 0, 0, 255, 255, 255, 0, 0, 0]
    );
}

#[test]
fn float32_flat_image_is_flat() {
    // All samples equal: natively 2.5 everywhere, clamped to 255 in 8-bit.
    let strip = f32_strip(&[2.5, 2.5, 2.5]);
    let bytes = build_float_row(3, 32, 1, &strip, &[]);
    let d = decode_page(&bytes).expect("flat float32 must decode");
    assert_eq!(gray_f32(&d), vec![2.5, 2.5, 2.5]);
    assert_eq!(d.image.to_rgb8(), vec![255u8; 9]);
}

#[test]
fn float16_half_precision_widened() {
    // binary16 samples 0.0, 0.5, 1.0 widen exactly to f32.
    let strip = half_strip(&[0.0, 0.5, 1.0]);
    let bytes = build_float_row(3, 16, 1, &strip, &[]);
    let d = decode_page(&bytes).expect("float16 grayscale must decode");
    assert_eq!(gray_f32(&d), vec![0.0, 0.5, 1.0]);
    assert_eq!(d.image.to_rgb8()[..7], [0u8, 0, 0, 128, 128, 128, 255]);
}

#[test]
fn float64_double_precision_narrowed() {
    // binary64 samples 0.0, 0.25, 1.0 narrow exactly to f32.
    let strip = f64_strip(&[0.0, 0.25, 1.0]);
    let bytes = build_float_row(3, 64, 1, &strip, &[]);
    let d = decode_page(&bytes).expect("float64 grayscale must decode");
    assert_eq!(gray_f32(&d), vec![0.0, 0.25, 1.0]);
    assert_eq!(d.image.to_rgb8()[..7], [0u8, 0, 0, 64, 64, 64, 255]);
}

#[test]
fn float_rejects_rgb_photometric_wrong_spp() {
    // SampleFormat = 3 RGB is supported, but only at SamplesPerPixel = 3
    // (one float per colour channel). Photometric = 2 (RGB) declared with
    // SamplesPerPixel = 1 — as this single-channel row builder writes — is
    // a shape the float decoder cannot map, so it must error gracefully
    // rather than mis-render.
    let strip = f32_strip(&[0.0, 0.5]);
    let bytes = build_float_row(2, 32, 2, &strip, &[]);
    expect_err_containing(&bytes, "SampleFormat=3");
}

#[test]
fn float_predictor_rejected() {
    // Predictor = 2 (§14 horizontal differencing) is integer-only; a
    // float image declaring it must be rejected, not mis-decoded.
    let strip = f32_strip(&[0.0, 1.0]);
    let extra = [entry_short(317, 2)]; // Predictor = 2
    let bytes = build_float_row(2, 32, 1, &strip, &extra);
    expect_err_containing(&bytes, "predictor");
}
