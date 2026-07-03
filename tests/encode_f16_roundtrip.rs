//! f16 (IEEE 754 binary16, `SampleFormat = 3`, `BitsPerSample = 16`)
//! *encode* round-trip.
//!
//! The decoder has widened 16-bit half floats onto the Gray8 / Rgb24
//! display planes since the float decode round; this file exercises the
//! new write side (`EncodePixelFormat::{GrayF16, RgbF16}` — raw
//! binary16 bit patterns) plus the `f32_to_f16_bits` /
//! `f16_bits_to_f32` narrowing/widening helpers:
//!
//!   * **Conversion oracle.** All 65 536 binary16 bit patterns widen to
//!     `f32` and narrow back bit-exactly (NaN patterns must stay NaN);
//!     directed cases pin the round-to-nearest-even, overflow-to-Inf,
//!     and underflow-to-zero edges.
//!   * **Display-plane oracle.** Encoded f16 pages decode through the
//!     public `decode_tiff` to the same display bytes a test-local map
//!     computes from the widened samples — across the compressor ×
//!     predictor × strip/tile × chunky/planar × classic/BigTIFF matrix.
//!   * **Raw-byte oracle.** For the uncompressed no-predictor page the
//!     strip payload must be the input bit patterns verbatim
//!     (little-endian), and the IFD must carry BitsPerSample = 16,
//!     SampleFormat = 3, and Predictor = 3 exactly when requested.
//!
//! Everything here is TIFF 6.0 §SampleFormat (page 80) + §14
//! (floating-point predictor) + §15 (tiles); no external
//! implementation is consulted.

use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
use oxideav_tiff::{
    decode_tiff, encode_tiff, f16_bits_to_f32, f32_to_f16_bits, EncodePage, EncodePixelFormat,
    TiffCompression, TiffPixelFormat,
};

// ---------------------------------------------------------------------------
// Conversion helpers: exhaustive + directed
// ---------------------------------------------------------------------------

#[test]
fn f16_widen_narrow_is_identity_for_all_bit_patterns() {
    for bits in 0..=u16::MAX {
        let wide = f16_bits_to_f32(bits);
        let exp = (bits >> 10) & 0x1F;
        let man = bits & 0x3FF;
        if exp == 0x1F && man != 0 {
            // NaN: the round-trip must stay NaN (payload may be
            // canonicalised by the widening, quiet bit forced by the
            // narrowing) — never collapse to Inf or a finite value.
            assert!(wide.is_nan(), "widen({bits:#06x}) should be NaN");
            let back = f32_to_f16_bits(wide);
            assert_eq!((back >> 10) & 0x1F, 0x1F, "narrow(NaN) exponent");
            assert_ne!(back & 0x3FF, 0, "narrow(NaN) must keep a mantissa bit");
        } else {
            let back = f32_to_f16_bits(wide);
            assert_eq!(
                back, bits,
                "binary16 {bits:#06x} -> f32 {wide} -> {back:#06x} not identity"
            );
        }
    }
}

#[test]
fn f16_narrowing_directed_edges() {
    // Exact representables.
    assert_eq!(f32_to_f16_bits(0.0), 0x0000);
    assert_eq!(f32_to_f16_bits(-0.0), 0x8000);
    assert_eq!(f32_to_f16_bits(1.0), 0x3C00);
    assert_eq!(f32_to_f16_bits(-2.0), 0xC000);
    assert_eq!(f32_to_f16_bits(0.5), 0x3800);
    // Largest finite half: 65504 = (2 - 2^-10) * 2^15.
    assert_eq!(f32_to_f16_bits(65504.0), 0x7BFF);
    // 65520 is the exact midpoint between 65504 and the (unrepresentable)
    // 65536 — round-to-nearest-even ties away to the "even" successor,
    // which is the infinity pattern.
    assert_eq!(f32_to_f16_bits(65520.0), 0x7C00);
    assert_eq!(f32_to_f16_bits(65519.996), 0x7BFF);
    assert_eq!(f32_to_f16_bits(f32::INFINITY), 0x7C00);
    assert_eq!(f32_to_f16_bits(f32::NEG_INFINITY), 0xFC00);
    // Smallest binary16 subnormal is 2^-24.
    assert_eq!(f32_to_f16_bits(2.0f32.powi(-24)), 0x0001);
    // 2^-25 is the exact midpoint between 0 and 2^-24: ties-to-even
    // rounds to 0. Anything strictly above the midpoint rounds up.
    assert_eq!(f32_to_f16_bits(2.0f32.powi(-25)), 0x0000);
    assert_eq!(f32_to_f16_bits(2.0f32.powi(-25) * 1.0001), 0x0001);
    // binary32 subnormals collapse to signed zero.
    assert_eq!(f32_to_f16_bits(f32::from_bits(1)), 0x0000);
    assert_eq!(f32_to_f16_bits(-f32::from_bits(1)), 0x8000);
    // Round-to-nearest-even inside the normal range: 1 + 2^-11 is the
    // midpoint between 1.0 (mantissa 0, even) and 1 + 2^-10; ties go
    // to even (1.0). 1 + 3*2^-11 ties between odd mantissa 1 and even
    // mantissa 2 — goes up to 2.
    assert_eq!(f32_to_f16_bits(1.0 + 2.0f32.powi(-11)), 0x3C00);
    assert_eq!(f32_to_f16_bits(1.0 + 3.0 * 2.0f32.powi(-11)), 0x3C02);
    // NaN survives narrowing.
    assert!(f16_bits_to_f32(f32_to_f16_bits(f32::NAN)).is_nan());
}

// ---------------------------------------------------------------------------
// Display-plane oracle (mirrors the decoder's float display map).
// ---------------------------------------------------------------------------

/// Linear-map samples onto 0..=255 over the shared finite extent,
/// matching `decoder::build_*_from_float` when SMin/SMax are absent.
fn display_map(samples: &[f64]) -> Vec<u8> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &s in samples {
        if s.is_finite() {
            lo = lo.min(s);
            hi = hi.max(s);
        }
    }
    let span = if lo.is_finite() && hi.is_finite() {
        hi - lo
    } else {
        0.0
    };
    samples
        .iter()
        .map(|&s| {
            if !s.is_finite() || span <= 0.0 {
                0u8
            } else {
                let t = ((s - lo) / span).clamp(0.0, 1.0);
                (t * 255.0 + 0.5) as u8
            }
        })
        .collect()
}

fn gray_bits(w: u32, h: u32) -> Vec<u16> {
    // A ramp of genuinely half-precision values (so the display map on
    // the widened samples is exact): quarter steps spanning -8..+8.
    (0..w * h)
        .map(|i| f32_to_f16_bits((i as f32) * 0.25 - 8.0))
        .collect()
}

fn rgb_bits(w: u32, h: u32) -> Vec<u16> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(f32_to_f16_bits(x as f32 * 0.5));
            v.push(f32_to_f16_bits(y as f32 * 0.5 - 4.0));
            v.push(f32_to_f16_bits(((x + y) % 7) as f32 * 1.5));
        }
    }
    v
}

fn expect_gray(bits: &[u16]) -> Vec<u8> {
    display_map(
        &bits
            .iter()
            .map(|&b| f16_bits_to_f32(b) as f64)
            .collect::<Vec<_>>(),
    )
}

const COMPRESSORS: [TiffCompression; 5] = [
    TiffCompression::None,
    TiffCompression::PackBits,
    TiffCompression::Lzw,
    TiffCompression::Deflate,
    TiffCompression::Zstd,
];

#[test]
fn encode_gray_f16_matrix_decodes_to_display_map() {
    let (w, h) = (23u32, 9u32);
    let bits = gray_bits(w, h);
    let want = expect_gray(&bits);
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            for tiling in [None, Some((16u32, 16u32))] {
                for bigtiff in [false, true] {
                    let page = EncodePage {
                        width: w,
                        height: h,
                        kind: EncodePixelFormat::GrayF16 { pixels: &bits },
                        compression,
                        predictor,
                        planar: false,
                        tiling,
                        bigtiff,
                    };
                    let file = encode_tiff(&page).expect("encode GrayF16");
                    let img = decode_tiff(&file).expect("decode GrayF16");
                    assert_eq!(img.pixel_format, TiffPixelFormat::Gray8);
                    assert_eq!(
                        img.frame.planes[0].data, want,
                        "GrayF16 display mismatch (compression={compression:?} \
                         predictor={predictor} tiling={tiling:?} bigtiff={bigtiff})"
                    );
                }
            }
        }
    }
}

#[test]
fn encode_rgb_f16_matrix_decodes_to_display_map() {
    let (w, h) = (17u32, 6u32);
    let bits = rgb_bits(w, h);
    // The decoder maps RGB float through one shared extent — same
    // shape as the grayscale map over the interleaved samples.
    let want = expect_gray(&bits);
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            for planar in [false, true] {
                for tiling in [None, Some((16u32, 16u32))] {
                    let page = EncodePage {
                        width: w,
                        height: h,
                        kind: EncodePixelFormat::RgbF16 { pixels: &bits },
                        compression,
                        predictor,
                        planar,
                        tiling,
                        bigtiff: false,
                    };
                    let file = encode_tiff(&page).expect("encode RgbF16");
                    let img = decode_tiff(&file).expect("decode RgbF16");
                    assert_eq!(img.pixel_format, TiffPixelFormat::Rgb24);
                    assert_eq!(
                        img.frame.planes[0].data, want,
                        "RgbF16 display mismatch (compression={compression:?} \
                         predictor={predictor} planar={planar} tiling={tiling:?})"
                    );
                }
            }
        }
    }
}

#[test]
fn encode_f16_nonfinite_samples_render_at_display_floor() {
    // NaN / ±Inf are legal binary16 patterns; the decoder excludes them
    // from the extent and renders them at the floor. The encoder must
    // transport them verbatim rather than erroring.
    let (w, h) = (4u32, 2u32);
    let bits: Vec<u16> = vec![
        0x7C00, // +Inf
        0xFC00, // -Inf
        0x7E01, // NaN
        f32_to_f16_bits(0.0),
        f32_to_f16_bits(1.0),
        f32_to_f16_bits(2.0),
        f32_to_f16_bits(3.0),
        f32_to_f16_bits(4.0),
    ];
    let want = expect_gray(&bits);
    assert_eq!(&want[..3], &[0, 0, 0], "non-finite must floor");
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::GrayF16 { pixels: &bits },
        compression: TiffCompression::Deflate,
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let img = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
    assert_eq!(img.frame.planes[0].data, want);
}

// ---------------------------------------------------------------------------
// Raw-byte oracle: IFD fields + verbatim strip payload
// ---------------------------------------------------------------------------

#[test]
fn encode_f16_ifd_fields_and_verbatim_strip() {
    let (w, h) = (7u32, 5u32);
    let bits = gray_bits(w, h);
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::GrayF16 { pixels: &bits },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let file = encode_tiff(&page).unwrap();
    let hdr = parse_header(&file).unwrap();
    let (entries, next) = parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset)
        .expect("parse encoded IFD");
    assert_eq!(next, 0, "single page");
    let get = |tag: u16| {
        find(&entries, tag)
            .unwrap_or_else(|| panic!("tag {tag} missing"))
            .as_u32(hdr.byte_order)
            .unwrap()
    };
    assert_eq!(get(258), 16, "BitsPerSample");
    assert_eq!(get(277), 1, "SamplesPerPixel");
    assert_eq!(get(339), 3, "SampleFormat = 3 (IEEE floating point)");
    assert!(find(&entries, 317).is_none(), "no Predictor tag when off");
    // Strip payload = the raw binary16 patterns, little-endian.
    let off = get(273) as usize;
    let len = get(279) as usize;
    assert_eq!(len, bits.len() * 2);
    let mut want = Vec::with_capacity(len);
    for b in &bits {
        want.extend_from_slice(&b.to_le_bytes());
    }
    assert_eq!(&file[off..off + len], &want[..], "verbatim f16 payload");

    // With predictor on, tag 317 must carry 3 (the §14 float predictor).
    let page_p = EncodePage {
        predictor: true,
        ..page
    };
    let file_p = encode_tiff(&page_p).unwrap();
    let hdr_p = parse_header(&file_p).unwrap();
    let (entries_p, _) = parse_ifd(
        &file_p,
        hdr_p.byte_order,
        hdr_p.variant,
        hdr_p.first_ifd_offset,
    )
    .unwrap();
    let pred = find(&entries_p, 317).expect("Predictor tag present");
    assert_eq!(pred.as_u32(hdr_p.byte_order).unwrap(), 3, "Predictor = 3");
}

#[test]
fn encode_f16_rejects_ccitt() {
    let bits = gray_bits(8, 8);
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::GrayF16 { pixels: &bits },
        compression: TiffCompression::CcittRle,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err(), "CCITT is bilevel-only");
}

#[test]
fn encode_gray_f16_rejects_planar() {
    let bits = gray_bits(8, 8);
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::GrayF16 { pixels: &bits },
        compression: TiffCompression::None,
        predictor: false,
        planar: true,
        tiling: None,
        bigtiff: false,
    };
    assert!(
        encode_tiff(&page).is_err(),
        "PlanarConfiguration=2 is irrelevant at SamplesPerPixel=1"
    );
}

#[test]
fn encode_f16_wrong_buffer_length_rejected() {
    let bits = vec![0u16; 10];
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::GrayF16 { pixels: &bits },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err());
}
