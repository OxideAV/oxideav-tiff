//! Integration tests for `PhotometricInterpretation = 8` (CIE L*a*b*),
//! per TIFF 6.0 §23 "CIE L*a*b* Images" (page 110).
//!
//! Two hand-built classic-TIFF fixtures (no external library / binary
//! dependency):
//!
//! 1. **3-sample L*a*b*** — every pixel chunkified as (L*, a*, b*).
//!    Confirms the §23 conversion formulas land within the expected
//!    tolerance and the decoder routes the photometric through the
//!    new Rgb24 builder.
//! 2. **1-sample L*** — `SamplesPerPixel = 1`, `BitsPerSample = 8`,
//!    `PhotometricInterpretation = 8`. §23: "1 implies L* only, for
//!    monochrome data". Confirms the dedicated Gray8 render path.

use oxideav_tiff::{decode_tiff, DecodedTiff, TiffPixelFormat};

// ---------------------------------------------------------------------------
// Hand-built classic TIFF fixture for CIELab
// ---------------------------------------------------------------------------

/// Build a little-endian classic TIFF for the requested chunky pixel
/// buffer with the given photometric / spp / bps.
///
/// Single uncompressed strip covering the whole image. The layout
/// mirrors the planar / mask integration-test helpers in this crate
/// — header `II`, magic 42, IFD at offset 8, then a deterministic
/// blob area.
fn build_classic_tiff(
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bits_per_sample: u16,
    photometric: u16,
    pixels: &[u8],
) -> Vec<u8> {
    let row_bytes = (width as u64) * (samples_per_pixel as u64) * (bits_per_sample as u64 / 8);
    let strip_bytes = row_bytes * (height as u64);
    assert_eq!(pixels.len() as u64, strip_bytes);

    // BitsPerSample inline-vs-external decision:
    //   1 sample  -> SHORT inline (2 bytes, fits in 4-byte value slot)
    //   3 samples -> SHORT[3] = 6 bytes, spills out-of-line
    let bps_inline = samples_per_pixel == 1;
    let bps_blob_bytes: u32 = if bps_inline {
        0
    } else {
        (samples_per_pixel as u32) * 2
    };

    let num_entries: u16 = 8; // ImageWidth, ImageLength, BitsPerSample,
                              // Compression, Photometric, StripOffsets,
                              // SamplesPerPixel, StripByteCounts
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset: u32 = ifd_offset + ifd_size;

    let bps_off = blobs_offset;
    let pixels_off = bps_off + bps_blob_bytes;

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&ifd_offset.to_le_bytes());
    buf.extend_from_slice(&num_entries.to_le_bytes());

    let push = |buf: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, val: [u8; 4]| {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&field_type.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&val);
    };

    // 256 ImageWidth (LONG, 1)
    push(&mut buf, 256, 4, 1, width.to_le_bytes());
    // 257 ImageLength (LONG, 1)
    push(&mut buf, 257, 4, 1, height.to_le_bytes());
    // 258 BitsPerSample (SHORT, samples_per_pixel)
    if bps_inline {
        let mut v = [0u8; 4];
        v[..2].copy_from_slice(&bits_per_sample.to_le_bytes());
        push(&mut buf, 258, 3, 1, v);
    } else {
        push(
            &mut buf,
            258,
            3,
            samples_per_pixel as u32,
            bps_off.to_le_bytes(),
        );
    }
    // 259 Compression = 1 (SHORT, 1)
    let mut comp = [0u8; 4];
    comp[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 259, 3, 1, comp);
    // 262 PhotometricInterpretation (SHORT, 1)
    let mut ph = [0u8; 4];
    ph[..2].copy_from_slice(&photometric.to_le_bytes());
    push(&mut buf, 262, 3, 1, ph);
    // 273 StripOffsets (LONG, 1)
    push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes());
    // 277 SamplesPerPixel (SHORT, 1)
    let mut spp = [0u8; 4];
    spp[..2].copy_from_slice(&samples_per_pixel.to_le_bytes());
    push(&mut buf, 277, 3, 1, spp);
    // 279 StripByteCounts (LONG, 1)
    push(&mut buf, 279, 4, 1, (strip_bytes as u32).to_le_bytes());

    // next-IFD pointer = 0
    buf.extend_from_slice(&0u32.to_le_bytes());

    // Blob area
    if !bps_inline {
        for _ in 0..samples_per_pixel {
            buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        }
    }
    buf.extend_from_slice(pixels);

    buf
}

// ---------------------------------------------------------------------------
// 3-sample L*a*b* tests
// ---------------------------------------------------------------------------

/// Pack a logical (L*, a*, b*) triple where L* is on the perceptual
/// 0..100 scale and a*, b* are in the spec-permitted -127..127 range
/// into the three on-disk bytes per §23.
fn pack_lab(l_pct: f64, a_signed: i32, b_signed: i32) -> [u8; 3] {
    let l_byte = (l_pct * 255.0 / 100.0).round().clamp(0.0, 255.0) as u8;
    [l_byte, (a_signed as i8) as u8, (b_signed as i8) as u8]
}

#[test]
fn decode_cielab_3sample_pure_neutral_gradient() {
    // 4×1 image, a* = b* = 0 (chromatically neutral).
    // L* sweeps 0, 33, 66, 100 → expect monotonically increasing
    // grayscale output in all three RGB channels.
    let lab_pixels: Vec<u8> = [
        pack_lab(0.0, 0, 0),
        pack_lab(33.0, 0, 0),
        pack_lab(66.0, 0, 0),
        pack_lab(100.0, 0, 0),
    ]
    .concat();

    let tiff = build_classic_tiff(4, 1, 3, 8, /* PHOTO_CIELAB */ 8, &lab_pixels);
    let DecodedTiff {
        frame,
        width,
        height,
        pixel_format,
        ..
    } = decode_tiff(&tiff).expect("decode CIELab 3-sample");
    assert_eq!((width, height), (4, 1));
    assert_eq!(pixel_format, TiffPixelFormat::Rgb24);
    assert_eq!(frame.planes.len(), 1);

    let row = &frame.planes[0].data;
    assert_eq!(row.len(), 12);

    // Each triple should be near-neutral (R, G, B within a small
    // delta of each other — within the spec's "Converting between
    // RGB and CIELAB, a Caveat" tolerance for the NTSC primaries
    // approximation).
    for px in 0..4 {
        let r = row[px * 3] as i32;
        let g = row[px * 3 + 1] as i32;
        let b = row[px * 3 + 2] as i32;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        assert!(
            max - min <= 24,
            "px {px}: expected near-neutral, got RGB=({r},{g},{b}) max-min={}",
            max - min
        );
    }

    // Monotonic lightness across the gradient: the green channel
    // (closest to luminance) must strictly increase.
    let g0 = row[1] as i32;
    let g1 = row[4] as i32;
    let g2 = row[7] as i32;
    let g3 = row[10] as i32;
    assert!(
        g0 < g1 && g1 < g2 && g2 < g3,
        "gradient not monotonic: {g0},{g1},{g2},{g3}"
    );

    // L* = 0 maps to ~ pure black, L* = 100 saturates the white end
    // (some channel must hit 255 once the NTSC primaries / D65
    // approximation pushes one component out of gamut).
    assert!(g0 <= 8, "L*=0 too bright: {g0}");
    let r3 = row[9] as i32;
    let b3 = row[11] as i32;
    assert!(
        r3 >= 240 || g3 >= 240 || b3 >= 240,
        "L*=100 too dim: RGB=({r3},{g3},{b3})"
    );
}

#[test]
fn decode_cielab_3sample_positive_a_pulls_red() {
    // L* = 50, a* = +120 (strong red lean), b* = 0.
    // Expect R notably > G, B.
    let lab_pixels: Vec<u8> = pack_lab(50.0, 120, 0).to_vec();
    let tiff = build_classic_tiff(1, 1, 3, 8, 8, &lab_pixels);
    let DecodedTiff { frame, .. } = decode_tiff(&tiff).expect("decode CIELab red");
    let r = frame.planes[0].data[0] as i32;
    let g = frame.planes[0].data[1] as i32;
    let b = frame.planes[0].data[2] as i32;
    assert!(r > g && r > b, "expected red dominant, got ({r},{g},{b})");
    assert!(r - g >= 30, "red-green spread too narrow: r={r} g={g}");
}

#[test]
fn decode_cielab_3sample_negative_a_pulls_green() {
    // L* = 50, a* = -120 (strong green lean), b* = 0.
    let lab_pixels: Vec<u8> = pack_lab(50.0, -120, 0).to_vec();
    let tiff = build_classic_tiff(1, 1, 3, 8, 8, &lab_pixels);
    let DecodedTiff { frame, .. } = decode_tiff(&tiff).expect("decode CIELab green");
    let r = frame.planes[0].data[0] as i32;
    let g = frame.planes[0].data[1] as i32;
    let b = frame.planes[0].data[2] as i32;
    assert!(g > r && g > b, "expected green dominant, got ({r},{g},{b})");
}

#[test]
fn decode_cielab_3sample_positive_b_pulls_yellow() {
    // L* = 70, a* = 0, b* = +120 (strong yellow lean).
    // Yellow = R+G high, B low.
    let lab_pixels: Vec<u8> = pack_lab(70.0, 0, 120).to_vec();
    let tiff = build_classic_tiff(1, 1, 3, 8, 8, &lab_pixels);
    let DecodedTiff { frame, .. } = decode_tiff(&tiff).expect("decode CIELab yellow");
    let r = frame.planes[0].data[0] as i32;
    let g = frame.planes[0].data[1] as i32;
    let b = frame.planes[0].data[2] as i32;
    assert!(r > b && g > b, "expected yellow (low B), got ({r},{g},{b})");
}

#[test]
fn decode_cielab_3sample_negative_b_pulls_blue() {
    // L* = 30, a* = 0, b* = -120 (strong blue lean).
    let lab_pixels: Vec<u8> = pack_lab(30.0, 0, -120).to_vec();
    let tiff = build_classic_tiff(1, 1, 3, 8, 8, &lab_pixels);
    let DecodedTiff { frame, .. } = decode_tiff(&tiff).expect("decode CIELab blue");
    let r = frame.planes[0].data[0] as i32;
    let g = frame.planes[0].data[1] as i32;
    let b = frame.planes[0].data[2] as i32;
    assert!(b > r && b > g, "expected blue dominant, got ({r},{g},{b})");
}

// ---------------------------------------------------------------------------
// 1-sample L*-only tests
// ---------------------------------------------------------------------------

#[test]
fn decode_cielab_1sample_l_only_emits_gray8() {
    // 4×1 monochrome CIELab: L* = 0, 33, 66, 100 (as 8-bit bytes).
    let l_bytes: Vec<u8> = vec![
        ((0.0_f64) * 255.0 / 100.0).round() as u8,
        ((33.0_f64) * 255.0 / 100.0).round() as u8,
        ((66.0_f64) * 255.0 / 100.0).round() as u8,
        ((100.0_f64) * 255.0 / 100.0).round() as u8,
    ];
    let tiff = build_classic_tiff(4, 1, 1, 8, /* PHOTO_CIELAB */ 8, &l_bytes);
    let DecodedTiff {
        frame,
        width,
        height,
        pixel_format,
        ..
    } = decode_tiff(&tiff).expect("decode CIELab L*-only");
    assert_eq!((width, height), (4, 1));
    assert_eq!(pixel_format, TiffPixelFormat::Gray8);
    assert_eq!(frame.planes.len(), 1);
    let row = &frame.planes[0].data;
    assert_eq!(row.len(), 4);
    // Monotonic + endpoints near black / white.
    assert!(row[0] <= 8, "L*=0 not dark: {}", row[0]);
    assert!(row[3] >= 247, "L*=100 not white: {}", row[3]);
    assert!(
        row[0] < row[1] && row[1] < row[2] && row[2] < row[3],
        "monotonic violation: {:?}",
        row
    );
}

#[test]
fn decode_cielab_1sample_matches_3sample_for_a_b_zero() {
    // A 1×1 L* = 50 monochrome CIELab pixel should produce the
    // same Gray8 byte as the green channel of a 3-sample
    // L*=50/a*=0/b*=0 CIELab pixel (the green primary tracks
    // luminance closest under the spec's NTSC matrix).
    let mono = build_classic_tiff(1, 1, 1, 8, 8, &[127]); // L* ≈ 49.8
    let tri = build_classic_tiff(1, 1, 3, 8, 8, &pack_lab(49.8, 0, 0));
    let DecodedTiff { frame: m, .. } = decode_tiff(&mono).unwrap();
    let DecodedTiff { frame: t, .. } = decode_tiff(&tri).unwrap();
    let gray = m.planes[0].data[0] as i32;
    let g = t.planes[0].data[1] as i32;
    assert!(
        (gray - g).abs() <= 2,
        "1-sample L* gray {gray} vs 3-sample G {g}"
    );
}
