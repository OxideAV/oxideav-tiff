//! TIFF 6.0 §SampleFormat (tag 339, page 80) value 3 — IEEE 754
//! floating-point **RGB** decode (PhotometricInterpretation = 2,
//! SamplesPerPixel = 3).
//!
//! §SampleFormat lists value `3` as "IEEE floating point data [IEEE]"
//! and notes that SampleFormat "does not specify the size of data
//! samples; this is still done by the BitsPerSample field" — so a float
//! image is identified by `SampleFormat = 3` and sized by BitsPerSample
//! (16-bit half, 32-bit single, 64-bit double). The companion
//! SMinSampleValue (340) / SMaxSampleValue (341) bound the samples
//! "without scanning the image data"; absent them, this decoder scans the
//! finite sample extent.
//!
//! Float RGB decodes natively (IMAGE_CRATE_API): the image is
//! `RgbF32Le`, three little-endian `f32`s per pixel exactly as stored
//! (half / double widened or narrowed). The declared SMin / SMax
//! extent is reported on `TiffFormatInfo`; the contract's `to_rgb8`
//! tone-scales each channel independently by clamping to `[0, 1]` and
//! scaling to 255 (non-finite → 0), which keeps the pixel's channel
//! balance for samples inside the nominal range.
//!
//! These tests build minimal hand-crafted classic-II TIFF byte strings
//! and drive them through the public `decode_page` entry point, so the
//! expected samples are the ones written — a binary-independent oracle.

use oxideav_tiff::{decode_page, TiffPixelFormat};

/// The decoded `RgbF32Le` plane as `f32`s.
fn rgb_f32(d: &oxideav_tiff::Page) -> Vec<f32> {
    assert_eq!(d.image.format, TiffPixelFormat::RgbF32Le);
    assert_eq!(d.image.planes[0].stride, d.image.width as usize * 12);
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

/// IFD entry, FLOAT (field-type = 11) with a single inline value.
fn entry_float(tag: u16, value: f32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&11u16.to_le_bytes()); // FLOAT
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..12].copy_from_slice(&value.to_le_bytes());
    e
}

/// IFD entry, SHORT array of three values stored out-of-line. `data_off`
/// is the file offset where the three SHORTs (6 bytes) are written. Used
/// for the 3-component BitsPerSample array.
fn entry_short3(tag: u16, data_off: u32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    e[4..8].copy_from_slice(&3u32.to_le_bytes()); // count = 3
    e[8..12].copy_from_slice(&data_off.to_le_bytes());
    e
}

/// Assemble a 1-row, `w`-pixel three-channel float RGB classic-II TIFF.
/// `bits_per_sample` ∈ {16, 32, 64}; `strip` is the interleaved
/// `R G B R G B …` float payload; `extra` carries any additional IFD
/// entries (e.g. SMin/SMaxSampleValue) appended after the mandatory ten.
fn build_float_rgb_row(w: u32, bits_per_sample: u16, strip: &[u8], extra: &[[u8; 12]]) -> Vec<u8> {
    let mut out = Vec::new();

    // 0..8 — classic LE header; first-IFD offset patched below.
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    let ifd_off_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());

    // 8.. — the strip payload (StripOffsets = 8 lines up).
    let strip_off = out.len() as u32;
    out.extend_from_slice(strip);

    // The 3-component BitsPerSample array lives out-of-line (6 bytes).
    let bps_off = out.len() as u32;
    for _ in 0..3 {
        out.extend_from_slice(&bits_per_sample.to_le_bytes());
    }
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
    out.extend_from_slice(&entry_short3(258, bps_off)); // BitsPerSample [b,b,b]
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, 2)); // Photometric = RGB
    out.extend_from_slice(&entry_long(273, strip_off)); // StripOffsets
    out.extend_from_slice(&entry_short(277, 3)); // SamplesPerPixel = 3
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
/// given substring.
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
fn float32_rgb_native_samples() {
    // Two pixels: (0.0, 0.5, 1.0) and (0.25, 0.75, 1.0) decode as
    // themselves; to_rgb8 clamps and scales: 0, 128, 255, 64, 191, 255.
    let strip = f32_strip(&[0.0, 0.5, 1.0, 0.25, 0.75, 1.0]);
    let bytes = build_float_rgb_row(2, 32, &strip, &[]);
    let d = decode_page(&bytes).expect("float32 RGB must decode");
    assert_eq!((d.image.width, d.image.height), (2, 1));
    assert_eq!(rgb_f32(&d), vec![0.0, 0.5, 1.0, 0.25, 0.75, 1.0]);
    assert_eq!(d.image.to_rgb8(), vec![0u8, 128, 255, 64, 191, 255]);
    assert_eq!(
        d.image.to_rgba8(),
        vec![0u8, 128, 255, 255, 64, 191, 255, 255]
    );
}

#[test]
fn float32_rgb_smin_smax_reported_not_applied() {
    // SMin = -1, SMax = 3 ride on the format info; samples untouched.
    let strip = f32_strip(&[0.0, 0.5, 1.0]);
    let extra = [entry_float(340, -1.0), entry_float(341, 3.0)];
    let bytes = build_float_rgb_row(1, 32, &strip, &extra);
    let d = decode_page(&bytes).expect("float32 RGB with SMin/SMax must decode");
    assert_eq!(rgb_f32(&d), vec![0.0, 0.5, 1.0]);
    assert_eq!(d.layout.smin_sample_value, Some(-1.0));
    assert_eq!(d.layout.smax_sample_value, Some(3.0));
    assert_eq!(d.image.to_rgb8(), vec![0u8, 128, 255]);
}

#[test]
fn float32_rgb_negative_kept_and_clamped() {
    let strip = f32_strip(&[-1.0, 0.0, 1.0]);
    let bytes = build_float_rgb_row(1, 32, &strip, &[]);
    let d = decode_page(&bytes).expect("float32 RGB must decode");
    assert_eq!(rgb_f32(&d), vec![-1.0, 0.0, 1.0]);
    assert_eq!(d.image.to_rgb8(), vec![0u8, 0, 255]);
}

#[test]
fn float32_rgb_nonfinite_kept_and_renders_floor() {
    // Pixel 0 = (0.0, NaN, 1.0), pixel 1 = (+Inf, 0.5, 1.0): the native
    // plane keeps them, the 8-bit view renders non-finite as 0.
    let strip = f32_strip(&[0.0, f32::NAN, 1.0, f32::INFINITY, 0.5, 1.0]);
    let bytes = build_float_rgb_row(2, 32, &strip, &[]);
    let d = decode_page(&bytes).expect("float32 RGB with non-finite must decode");
    let v = rgb_f32(&d);
    assert!(v[1].is_nan());
    assert_eq!(v[3], f32::INFINITY);
    assert_eq!(d.image.to_rgb8(), vec![0u8, 0, 255, 0, 128, 255]);
}

#[test]
fn float32_rgb_flat_image_is_flat() {
    let strip = f32_strip(&[2.5, 2.5, 2.5]);
    let bytes = build_float_rgb_row(1, 32, &strip, &[]);
    let d = decode_page(&bytes).expect("flat float32 RGB must decode");
    assert_eq!(rgb_f32(&d), vec![2.5, 2.5, 2.5]);
    assert_eq!(d.image.to_rgb8(), vec![255u8, 255, 255]);
}

#[test]
fn float16_rgb_half_precision_widened() {
    let strip = half_strip(&[0.0, 0.5, 1.0]);
    let bytes = build_float_rgb_row(1, 16, &strip, &[]);
    let d = decode_page(&bytes).expect("float16 RGB must decode");
    assert_eq!(rgb_f32(&d), vec![0.0, 0.5, 1.0]);
    assert_eq!(d.image.to_rgb8(), vec![0u8, 128, 255]);
}

#[test]
fn float64_rgb_double_precision_narrowed() {
    let strip = f64_strip(&[0.0, 0.25, 1.0]);
    let bytes = build_float_rgb_row(1, 64, &strip, &[]);
    let d = decode_page(&bytes).expect("float64 RGB must decode");
    assert_eq!(rgb_f32(&d), vec![0.0, 0.25, 1.0]);
    assert_eq!(d.image.to_rgb8(), vec![0u8, 64, 255]);
}

#[test]
fn float_rgb_predictor_rejected() {
    // Predictor = 2 (§14 horizontal differencing) is integer-only; a
    // float RGB image declaring it must be rejected, not mis-decoded.
    let strip = f32_strip(&[0.0, 0.5, 1.0]);
    let extra = [entry_short(317, 2)]; // Predictor = 2
    let bytes = build_float_rgb_row(1, 32, &strip, &extra);
    expect_err_containing(&bytes, "predictor");
}
