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
//! A float sample carries no intrinsic display range, so — like the §23
//! CIELab path's "some conversion to a display range will be required"
//! latitude — the decoder maps the resolved extent linearly onto the
//! 8-bit Rgb24 display plane. The extent is *shared across all three
//! colour channels* so the relative R / G / B magnitudes (the pixel's
//! chromaticity) survive the conversion; a per-channel extent would
//! re-balance the colour.
//!
//! These tests build minimal hand-crafted classic-II TIFF byte strings
//! and drive them through the public `decode_tiff` entry point, so the
//! expected display bytes are computed directly from the linear-mapping
//! definition — a binary-independent oracle.

use oxideav_tiff::decode_tiff;

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

/// Assert `decode_tiff` returned an error whose Display includes the
/// given substring.
fn expect_err_containing(bytes: &[u8], needle: &str) {
    match decode_tiff(bytes) {
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
fn float32_rgb_scanned_extent_shared() {
    // Two pixels: (0.0, 0.5, 1.0) and (0.25, 0.75, 1.0). The shared
    // scanned extent across all six samples is [0.0, 1.0]. Each sample
    // maps t = (s - 0) / 1 to round(t*255):
    //   0.0  -> 0     0.25 -> 64
    //   0.5  -> 128   0.75 -> 191
    //   1.0  -> 255
    let strip = f32_strip(&[0.0, 0.5, 1.0, 0.25, 0.75, 1.0]);
    let bytes = build_float_rgb_row(2, 32, &strip, &[]);
    let d = decode_tiff(&bytes).expect("float32 RGB must decode");
    assert_eq!((d.width, d.height), (2, 1));
    assert_eq!(d.frame.planes[0].data, vec![0u8, 128, 255, 64, 191, 255]);
}

#[test]
fn float32_rgb_shared_extent_preserves_balance() {
    // A single pixel (0.0, 0.5, 1.0). The shared extent is [0.0, 1.0]
    // (scanned across the three channels), so the channels keep their
    // relative magnitudes: 0, 128, 255 — not a per-channel renormalise
    // that would flatten the pixel to (0, 0, 0) / full white.
    let strip = f32_strip(&[0.0, 0.5, 1.0]);
    let bytes = build_float_rgb_row(1, 32, &strip, &[]);
    let d = decode_tiff(&bytes).expect("float32 RGB must decode");
    assert_eq!(d.frame.planes[0].data, vec![0u8, 128, 255]);
}

#[test]
fn float32_rgb_smin_smax_bound_overrides_scan() {
    // Samples in a single pixel (0.0, 0.5, 1.0) but SMin = -1, SMax = 3
    // declared, so the shared mapping uses span 4:
    //   0.0 -> (0-(-1))/4 = 0.25 -> 64
    //   0.5 -> 1.5/4 = 0.375 -> 96
    //   1.0 -> 2/4 = 0.5 -> 128
    let strip = f32_strip(&[0.0, 0.5, 1.0]);
    let extra = [entry_float(340, -1.0), entry_float(341, 3.0)];
    let bytes = build_float_rgb_row(1, 32, &strip, &extra);
    let d = decode_tiff(&bytes).expect("float32 RGB with SMin/SMax must decode");
    assert_eq!(d.frame.planes[0].data, vec![64u8, 96, 128]);
}

#[test]
fn float32_rgb_negative_to_positive_extent() {
    // Shared extent [-1.0, +1.0]; midpoint 0.0 -> 128, -1 -> 0, +1 -> 255.
    let strip = f32_strip(&[-1.0, 0.0, 1.0]);
    let bytes = build_float_rgb_row(1, 32, &strip, &[]);
    let d = decode_tiff(&bytes).expect("float32 RGB must decode");
    assert_eq!(d.frame.planes[0].data, vec![0u8, 128, 255]);
}

#[test]
fn float32_rgb_nonfinite_renders_floor() {
    // Pixel 0 = (0.0, NaN, 1.0), pixel 1 = (+Inf, 0.5, 1.0). Non-finite
    // samples are excluded from the scanned extent ([0.0, 1.0]) and
    // render at the display floor (0).
    let strip = f32_strip(&[0.0, f32::NAN, 1.0, f32::INFINITY, 0.5, 1.0]);
    let bytes = build_float_rgb_row(2, 32, &strip, &[]);
    let d = decode_tiff(&bytes).expect("float32 RGB with non-finite must decode");
    assert_eq!(d.frame.planes[0].data, vec![0u8, 0, 255, 0, 128, 255]);
}

#[test]
fn float32_rgb_flat_image_renders_floor() {
    // All samples equal -> degenerate span -> flat 0 plane.
    let strip = f32_strip(&[2.5, 2.5, 2.5]);
    let bytes = build_float_rgb_row(1, 32, &strip, &[]);
    let d = decode_tiff(&bytes).expect("flat float32 RGB must decode");
    assert_eq!(d.frame.planes[0].data, vec![0u8, 0, 0]);
}

#[test]
fn float16_rgb_half_precision_scanned_extent() {
    // binary16 single pixel (0.0, 0.5, 1.0); scanned extent [0,1]:
    // 0, 128, 255.
    let strip = half_strip(&[0.0, 0.5, 1.0]);
    let bytes = build_float_rgb_row(1, 16, &strip, &[]);
    let d = decode_tiff(&bytes).expect("float16 RGB must decode");
    assert_eq!(d.frame.planes[0].data, vec![0u8, 128, 255]);
}

#[test]
fn float64_rgb_double_precision_scanned_extent() {
    // binary64 single pixel (0.0, 0.25, 1.0); scanned extent [0,1]:
    // 0, 64, 255.
    let strip = f64_strip(&[0.0, 0.25, 1.0]);
    let bytes = build_float_rgb_row(1, 64, &strip, &[]);
    let d = decode_tiff(&bytes).expect("float64 RGB must decode");
    assert_eq!(d.frame.planes[0].data, vec![0u8, 64, 255]);
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
