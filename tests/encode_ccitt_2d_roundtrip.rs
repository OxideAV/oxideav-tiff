//! Binary-independent write→read coverage for CCITT 2-D encode
//! ([`TiffCompression::CcittT4TwoD`], TIFF 6.0 §11 with T4Options
//! bit 0 set) and CCITT T.6 / Group-4 encode
//! ([`TiffCompression::CcittT6`], TIFF 6.0 §"Compression" value 4 +
//! ITU-T T.6).
//!
//! Spec recap:
//!
//! * TIFF 6.0 §11 (page 49) — Compression=3 uses ITU-T T.4. The
//!   `T4Options` tag (292) is a LONG bit-field: bit 0 set = "2-D
//!   coding" (Modified READ), bit 2 set = "EOL byte-aligned".
//! * TIFF 6.0 §"Compression" value 4 (page 30) — Compression=4 is
//!   ITU-T T.6 "Facsimile coding schemes... for Group 4 facsimile
//!   apparatus" (MMR). The `T6Options` tag (293) has only bit 1
//!   defined ("uncompressed mode allowed"); we never set it.
//! * `docs/image/tiff/ccitt-t4-t6-fax-codes.md` §1 — the Pass /
//!   Horizontal / Vertical mode-code table (Table 4/T.4 = Table 1/T.6)
//!   that both encoders emit and that the decoder consumes.
//!
//! The encoder writes Pass / Horizontal / V(-3..=3) codes from the
//! mode-code table; the decoder consumes them and reconstructs the
//! row against the previously decoded reference line (imaginary all-
//! white for the first line of T.6 and for T.4-2D row 1 if the
//! preceding row was 1-D). Each test in this file calls our encoder
//! and our decoder; the assertion is bit-exact identity between the
//! input mask and the round-tripped `Gray8` plane the decoder hands
//! back.

use oxideav_tiff::{decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, TiffCompression};

/// Build a `(width, height, packed_msb_first_bytes)` triplet for a
/// width-`w` × height-`h` bilevel image whose pixel at (x, y) is
/// `1` (black) when `f(x, y)` returns true. Layout matches
/// [`EncodePixelFormat::Bilevel`] (MSB-first within each byte).
fn make_bilevel<F: Fn(u32, u32) -> bool>(w: u32, h: u32, f: F) -> (u32, u32, Vec<u8>) {
    let row_bytes = (w as usize).div_ceil(8);
    let mut bytes = vec![0u8; row_bytes * h as usize];
    for y in 0..h {
        for x in 0..w {
            if f(x, y) {
                let i = (y as usize) * row_bytes + (x as usize) / 8;
                bytes[i] |= 1 << (7 - (x as usize % 8));
            }
        }
    }
    (w, h, bytes)
}

/// Expand a packed MSB-first bilevel byte buffer into a Gray8 plane
/// using the WhiteIsZero polarity the `Bilevel` encoder writes:
/// bit 0 → 0xFF (white), bit 1 → 0x00 (black).
fn expand_to_gray8(packed: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row_bytes = (w as usize).div_ceil(8);
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let b = packed[y * row_bytes + x / 8];
            let bit = (b >> (7 - (x % 8))) & 1;
            out.push(if bit == 0 { 0xFF } else { 0x00 });
        }
    }
    out
}

fn build_page<'a>(w: u32, h: u32, bytes: &'a [u8], compression: TiffCompression) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Bilevel { pixels: bytes },
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    }
}

fn roundtrip_assert(w: u32, h: u32, bytes: &[u8], compression: TiffCompression) {
    let page = build_page(w, h, bytes, compression);
    let file = encode_tiff(&page).expect("encode_tiff");
    let img = decode_tiff(&file).expect("decode_tiff");
    assert_eq!(img.width, w);
    assert_eq!(img.height, h);
    let expected = expand_to_gray8(bytes, w, h);
    assert_eq!(
        img.frame.planes[0].data, expected,
        "encode→decode mismatch for {compression:?}"
    );
}

// -------------------------------------------------------------------------
// T.4 2-D (Modified READ) — Compression=3 + T4Options bit 0 set.
// -------------------------------------------------------------------------

#[test]
fn t4_2d_solid_white_16x8() {
    // Every coding row identical to imaginary white (row 1) and to
    // the previous coding line thereafter → V(0)-only stream.
    let (w, h, bytes) = make_bilevel(16, 8, |_, _| false);
    roundtrip_assert(
        w,
        h,
        &bytes,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

#[test]
fn t4_2d_solid_black_16x8() {
    // Row 0 (1-D) emits 0-len white + 16 black; rows 1..7 emit V(0)
    // against the previous all-black coding line.
    let (w, h, bytes) = make_bilevel(16, 8, |_, _| true);
    roundtrip_assert(
        w,
        h,
        &bytes,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

#[test]
fn t4_2d_two_rectangles_64x8() {
    // Two solid black rectangles inside a white field. The vertical
    // edges of each rectangle drive V(0) mode for the inner rows and
    // V(±1) for the row above / below each rectangle.
    let (w, h, bytes) = make_bilevel(64, 8, |x, y| {
        let in_a = (4..=30).contains(&x) && (1..=5).contains(&y);
        let in_b = (40..=55).contains(&x) && (2..=4).contains(&y);
        in_a || in_b
    });
    roundtrip_assert(
        w,
        h,
        &bytes,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

#[test]
fn t4_2d_diagonal_32x32() {
    // One black pixel per row at column = row. Drives a mix of V(±1)
    // (most rows) and Horizontal at the line where the diagonal
    // first arrives.
    let (w, h, bytes) = make_bilevel(32, 32, |x, y| x == y);
    roundtrip_assert(
        w,
        h,
        &bytes,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

#[test]
fn t4_2d_byte_aligned_eol_16x4() {
    // T4Options bit 2 must end up in the IFD and the decoder must
    // accept the byte-aligned EOL framing.
    let (w, h, bytes) = make_bilevel(16, 4, |x, y| x >= y * 2);
    roundtrip_assert(
        w,
        h,
        &bytes,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: true,
        },
    );
}

#[test]
fn t4_2d_blackiszero_polarity_16x4_via_inverted_input() {
    // The `Bilevel` encoder writes WhiteIsZero; to render a
    // BlackIsZero image, the caller inverts the input before
    // encoding. Sanity-check that the inversion round-trips through
    // the 2-D codec just like through the 1-D one.
    let (w, h, bytes) = make_bilevel(16, 4, |x, _| x < 8);
    let inverted: Vec<u8> = bytes.iter().map(|b| !b).collect();
    let page = build_page(
        w,
        h,
        &inverted,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
    let file = encode_tiff(&page).expect("encode_tiff");
    let img = decode_tiff(&file).expect("decode_tiff");
    let expected = expand_to_gray8(&inverted, w, h);
    assert_eq!(img.frame.planes[0].data, expected);
}

// -------------------------------------------------------------------------
// T.6 / Group 4 (MMR) — Compression=4. No EOL framing; every row is
// 2-D against the previously coded row (the first against imaginary
// white per T.6 §2.2.1).
// -------------------------------------------------------------------------

#[test]
fn t6_solid_white_16x8() {
    // All-white image. T.6 emits a single V(0) per row from row 0
    // (reference is imaginary white).
    let (w, h, bytes) = make_bilevel(16, 8, |_, _| false);
    roundtrip_assert(w, h, &bytes, TiffCompression::CcittT6);
}

#[test]
fn t6_solid_black_16x8() {
    // All-black image. Row 0 emits Horizontal (reference is
    // imaginary white) then rows 1..7 emit V(0) against the
    // all-black coding line.
    let (w, h, bytes) = make_bilevel(16, 8, |_, _| true);
    roundtrip_assert(w, h, &bytes, TiffCompression::CcittT6);
}

#[test]
fn t6_two_rectangles_64x8() {
    let (w, h, bytes) = make_bilevel(64, 8, |x, y| {
        let in_a = (4..=30).contains(&x) && (1..=5).contains(&y);
        let in_b = (40..=55).contains(&x) && (2..=4).contains(&y);
        in_a || in_b
    });
    roundtrip_assert(w, h, &bytes, TiffCompression::CcittT6);
}

#[test]
fn t6_diagonal_32x32() {
    let (w, h, bytes) = make_bilevel(32, 32, |x, y| x == y);
    roundtrip_assert(w, h, &bytes, TiffCompression::CcittT6);
}

#[test]
fn t6_wide_pattern_128x4() {
    // Width > 64 forces a make-up code inside any Horizontal-mode
    // emission on row 0 (against imaginary white).
    let (w, h, bytes) = make_bilevel(128, 4, |x, _| x < 100);
    roundtrip_assert(w, h, &bytes, TiffCompression::CcittT6);
}

// -------------------------------------------------------------------------
// Negative tests: the bilevel-only constraint for 2-D variants
// matches the existing CCITT-RLE / T.4-1D rejection paths.
// -------------------------------------------------------------------------

#[test]
fn t4_2d_rejects_non_bilevel_input() {
    // EncodePixelFormat::Gray8 is not a permissible bilevel input
    // and must error rather than silently corrupt the stream.
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::Gray8 { pixels: &[0u8; 16] },
        compression: TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err());
}

#[test]
fn t6_rejects_non_bilevel_input() {
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::Gray8 { pixels: &[0u8; 16] },
        compression: TiffCompression::CcittT6,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err());
}
