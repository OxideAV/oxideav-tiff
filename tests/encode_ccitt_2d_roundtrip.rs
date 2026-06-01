//! Binary-independent self-roundtrip suite for the round-206 CCITT
//! 2-D encoders: `TiffCompression::CcittT4TwoD` (Compression=3 with
//! `T4Options` bit 0 set — Modified READ / MR) and
//! `TiffCompression::CcittT6` (Compression=4 — Modified Modified READ
//! / MMR, i.e. ITU-T T.6 / Group 4).
//!
//! The decoder side of both variants is the round-130 implementation
//! validated against `tiffcp -c g4` and `tiffcp -c g3:2d` external
//! fixtures (`tests/decode_ccitt_fixtures.rs`); these tests close the
//! loop by encoding through our writer and decoding through our reader
//! and asserting pixel equality, exercising every step of the 2-D
//! encoder (Pass / Horizontal / Vertical mode selection, EOL+tag
//! framing for T.4-2D, no-framing for T.6, the imaginary all-white
//! first reference line per T.4 §4.2 / T.6 §2.2.1, and the IFD-level
//! `T4Options` / `T6Options` tag emission).
//!
//! No external binary or library is invoked. The tests run on every
//! CI host regardless of `tiffcp` availability.

use oxideav_tiff::{
    decode_tiff, encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat, TiffCompression,
};

// ---------------------------------------------------------------------------
// Test fixtures: bilevel pixel buffers in MSB-first 1-bit packing.
// ---------------------------------------------------------------------------

/// Pack a 1-pixel checkerboard. Exercises the worst case for 2-D
/// coding because every column is a changing element and there is no
/// vertical structure to lean on — the encoder must emit Horizontal
/// mode (the V(0) trap that would mis-align on a real checkerboard
/// is what the implementation has to defend against).
fn checkerboard(w: u32, h: u32) -> Vec<u8> {
    let rb = (w as usize).div_ceil(8);
    let mut out = vec![0u8; rb * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            if ((x ^ y) & 1) == 1 {
                out[y * rb + x / 8] |= 1 << (7 - (x % 8));
            }
        }
    }
    out
}

/// Pack `h` identical rows of vertical stripes. Every row's transition
/// columns line up with the previous row's, so the encoder spends
/// almost every iteration in V(0) — the most-frequent shortest mode.
fn stripes_aligned(w: u32, h: u32, period: u32) -> Vec<u8> {
    let rb = (w as usize).div_ceil(8);
    let mut out = vec![0u8; rb * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            if ((x as u32) / period) & 1 == 1 {
                out[y * rb + x / 8] |= 1 << (7 - (x % 8));
            }
        }
    }
    out
}

/// Pack a diagonal line of black pels (one bit per row, column = row).
/// Each row's a1 is exactly one pel right of the previous row's, so
/// the encoder picks Vertical(+1) at every step — exercises the VR(1)
/// = `011` code path repeatedly.
fn diagonal(w: u32, h: u32) -> Vec<u8> {
    let rb = (w as usize).div_ceil(8);
    let mut out = vec![0u8; rb * h as usize];
    for y in 0..h as usize {
        let x = (y as u32).min(w - 1) as usize;
        out[y * rb + x / 8] |= 1 << (7 - (x % 8));
    }
    out
}

/// Pack a tall blank row at the top (all white) followed by a row
/// with one black pel at the right edge. The encoder on the second
/// row must emit Pass first (b2 is just-past the right edge while a1
/// is at the right-edge column), then Horizontal or V. Exercises the
/// Pass-mode code (`0001`).
fn blank_then_black_corner(w: u32, h: u32) -> Vec<u8> {
    let rb = (w as usize).div_ceil(8);
    let mut out = vec![0u8; rb * h as usize];
    if h >= 1 && w >= 1 {
        let last_x = (w - 1) as usize;
        let last_y = (h - 1) as usize;
        out[last_y * rb + last_x / 8] |= 1 << (7 - (last_x % 8));
    }
    out
}

/// Pack a fully-black image — every pel is black. Tests the row-start
/// case where the imaginary white pel at column -1 differs from the
/// row's first pel; the encoder must emit a zero-length white-then-
/// black transition.
fn all_black(w: u32, h: u32) -> Vec<u8> {
    let rb = (w as usize).div_ceil(8);
    let mut out = vec![0xFFu8; rb * h as usize];
    // Mask off trailing bits in the last byte of each row if the
    // width isn't a multiple of 8 (the decoder ignores them, but a
    // tidy fixture keeps comparisons unambiguous).
    if w % 8 != 0 {
        let keep_bits = (w % 8) as usize;
        let mask: u8 = (0xFFu16 << (8 - keep_bits)) as u8;
        for y in 0..h as usize {
            out[y * rb + rb - 1] &= mask;
        }
    }
    out
}

/// Pack a fully-white image. Every row is identical to the all-white
/// reference line; the encoder should pick V(0) for every transition,
/// but there are no transitions, so the row terminates without
/// emitting any mode code (the while-loop exits with a0 == width
/// because `first_change_after` returns `width` for an all-white
/// line).
fn all_white(w: u32, h: u32) -> Vec<u8> {
    let rb = (w as usize).div_ceil(8);
    vec![0u8; rb * h as usize]
}

/// Inflate an MSB-first 1-bit bilevel buffer to Gray8 the way the
/// decoder will: WhiteIsZero photometric (the encoder's default for
/// `Bilevel`), so bit 0 → 0xFF and bit 1 → 0x00.
fn bilevel_to_gray8(packed: &[u8], w: u32, h: u32) -> Vec<u8> {
    let rb = (w as usize).div_ceil(8);
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        let row = &packed[y * rb..(y + 1) * rb];
        for x in 0..w as usize {
            let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
            out.push(if bit == 0 { 0xFF } else { 0x00 });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Helper: encode + decode + compare to the expected Gray8 rendering.
// ---------------------------------------------------------------------------

fn run_roundtrip(packed: &[u8], w: u32, h: u32, comp: TiffCompression) {
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Bilevel { pixels: packed },
        compression: comp,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).expect("encode_tiff");
    let d = decode_tiff(&bytes).expect("decode_tiff");
    assert_eq!((d.width, d.height), (w, h));
    let want = bilevel_to_gray8(packed, w, h);
    assert_eq!(d.frame.planes[0].data, want, "comp={comp:?} w={w} h={h}");
}

// ---------------------------------------------------------------------------
// T.6 (Compression = 4) tests
// ---------------------------------------------------------------------------

#[test]
fn t6_roundtrip_checkerboard_16x8() {
    run_roundtrip(&checkerboard(16, 8), 16, 8, TiffCompression::CcittT6);
}

#[test]
fn t6_roundtrip_stripes_aligned_64x6_period8() {
    run_roundtrip(&stripes_aligned(64, 6, 8), 64, 6, TiffCompression::CcittT6);
}

#[test]
fn t6_roundtrip_diagonal_32x32() {
    // Diagonal exercises VR(1) repeatedly: each row's single-pel
    // black run is one column right of the previous row's, so the
    // encoder picks V(+1) at every changing element.
    run_roundtrip(&diagonal(32, 32), 32, 32, TiffCompression::CcittT6);
}

#[test]
fn t6_roundtrip_pass_mode_corner_32x32() {
    // Top row is fully white; the bottom row has a single black pel
    // in the corner. On the bottom row the reference is still
    // mostly all-white (every row before the last is blank), so b1
    // and b2 are at column = width (no changing elements on the
    // reference). The encoder takes the Horizontal branch (because
    // b2 >= a1), exercising the row-start + far-right-change path.
    run_roundtrip(
        &blank_then_black_corner(32, 32),
        32,
        32,
        TiffCompression::CcittT6,
    );
}

#[test]
fn t6_roundtrip_all_black_24x8() {
    // Every row is fully black against an imaginary-white reference
    // on row 0, then against an all-black reference on rows 1+. The
    // a1 / b1 split forces Horizontal mode on row 0 (b1 = width, no
    // V applicable) and a clean while-loop exit on rows 1+ (a1 =
    // width on the coding line because there's no transition).
    run_roundtrip(&all_black(24, 8), 24, 8, TiffCompression::CcittT6);
}

#[test]
fn t6_roundtrip_all_white_24x8() {
    run_roundtrip(&all_white(24, 8), 24, 8, TiffCompression::CcittT6);
}

#[test]
fn t6_roundtrip_non_multiple_of_8_width() {
    // Width = 13 forces partial trailing bits and exercises the
    // mask-off path in `all_black` / decoder bilevel expander on a
    // non-byte-aligned row.
    let mut packed = vec![0u8; 2 * 5];
    // Black diagonal.
    for y in 0..5usize {
        let x = y.min(12);
        packed[y * 2 + x / 8] |= 1 << (7 - (x % 8));
    }
    run_roundtrip(&packed, 13, 5, TiffCompression::CcittT6);
}

// ---------------------------------------------------------------------------
// T.4 2-D (Compression = 3 + T4Options bit 0) tests
// ---------------------------------------------------------------------------

#[test]
fn t4_2d_roundtrip_checkerboard_16x8() {
    run_roundtrip(
        &checkerboard(16, 8),
        16,
        8,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

#[test]
fn t4_2d_roundtrip_stripes_aligned_64x6_period8() {
    run_roundtrip(
        &stripes_aligned(64, 6, 8),
        64,
        6,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

#[test]
fn t4_2d_roundtrip_diagonal_32x32() {
    run_roundtrip(
        &diagonal(32, 32),
        32,
        32,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

#[test]
fn t4_2d_roundtrip_eol_byte_aligned() {
    // Same fixture as the non-byte-aligned case but with T4Options
    // bit 2 set; the EOL+tag-bit framing must still produce a
    // decoder-readable stream.
    run_roundtrip(
        &diagonal(48, 12),
        48,
        12,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: true,
        },
    );
}

#[test]
fn t4_2d_roundtrip_all_white() {
    // Even a row with no transitions still emits the EOL+tag prefix
    // and consumes no mode-code bits — the decoder must skip
    // straight to the next row's EOL.
    run_roundtrip(
        &all_white(32, 4),
        32,
        4,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

#[test]
fn t4_2d_roundtrip_all_black() {
    run_roundtrip(
        &all_black(32, 4),
        32,
        4,
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
    );
}

// ---------------------------------------------------------------------------
// IFD-level sanity: confirm T6Options (tag 293) is written for T.6 and
// the T4Options bit 0 (2-D coding) is set for T.4-2D.
// ---------------------------------------------------------------------------

/// Find an IFD entry by tag in a raw classic-II little-endian TIFF
/// file. Returns `(field_type, count, value_or_offset)` as a parsed
/// LONG (4 bytes inline). Panics on missing tag / unexpected layout —
/// the test wants a precise failure if our writer regresses.
fn find_classic_ifd_entry(bytes: &[u8], tag: u16) -> (u16, u32, u32) {
    // Header: "II" + 42 + first-IFD-offset (4 bytes).
    assert_eq!(&bytes[0..2], b"II");
    assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 42);
    let off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    // IFD: 2-byte count + N x 12-byte entries.
    let n = u16::from_le_bytes([bytes[off], bytes[off + 1]]) as usize;
    for i in 0..n {
        let e = off + 2 + i * 12;
        let t = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
        if t == tag {
            let ft = u16::from_le_bytes([bytes[e + 2], bytes[e + 3]]);
            let c = u32::from_le_bytes([bytes[e + 4], bytes[e + 5], bytes[e + 6], bytes[e + 7]]);
            let v = u32::from_le_bytes([bytes[e + 8], bytes[e + 9], bytes[e + 10], bytes[e + 11]]);
            return (ft, c, v);
        }
    }
    panic!("tag {tag} not found in IFD");
}

#[test]
fn t6_writes_t6options_tag_293() {
    let packed = checkerboard(16, 8);
    let page = EncodePage {
        width: 16,
        height: 8,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittT6,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    // Compression (259) must be 4 (T.6).
    let (_ft, _c, v) = find_classic_ifd_entry(&bytes, 259);
    assert_eq!(v & 0xFFFF, 4, "Compression tag value must be 4 (T.6)");
    // T6Options (293) must be present and LONG, count = 1, value = 0
    // (no uncompressed mode).
    let (ft, c, v) = find_classic_ifd_entry(&bytes, 293);
    assert_eq!(ft, 4, "T6Options field type must be LONG (4)");
    assert_eq!(c, 1, "T6Options count must be 1");
    assert_eq!(v, 0, "T6Options bits should all be 0");
}

#[test]
fn t4_2d_writes_t4options_bit_0_set() {
    let packed = checkerboard(16, 8);
    let page = EncodePage {
        width: 16,
        height: 8,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    // Compression (259) must be 3 (T.4).
    let (_ft, _c, v) = find_classic_ifd_entry(&bytes, 259);
    assert_eq!(v & 0xFFFF, 3, "Compression tag value must be 3 (T.4)");
    // T4Options (292) must be present with bit 0 set.
    let (ft, c, v) = find_classic_ifd_entry(&bytes, 292);
    assert_eq!(ft, 4, "T4Options field type must be LONG (4)");
    assert_eq!(c, 1, "T4Options count must be 1");
    assert_eq!(v & 1, 1, "T4Options bit 0 (2-D coding) must be set");
    assert_eq!(
        v & 4,
        0,
        "T4Options bit 2 must NOT be set when eol_byte_aligned=false"
    );
}

#[test]
fn t4_2d_writes_t4options_bit_2_when_byte_aligned() {
    let packed = checkerboard(16, 8);
    let page = EncodePage {
        width: 16,
        height: 8,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittT4TwoD {
            eol_byte_aligned: true,
        },
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    let (_ft, _c, v) = find_classic_ifd_entry(&bytes, 292);
    assert_eq!(v & 1, 1, "T4Options bit 0 (2-D coding) must be set");
    assert_eq!(
        v & 4,
        4,
        "T4Options bit 2 must be set when eol_byte_aligned=true"
    );
}

// ---------------------------------------------------------------------------
// Negative tests
// ---------------------------------------------------------------------------

#[test]
fn t6_rejects_non_bilevel_input() {
    // CCITT schemes are bilevel-only (TIFF 6.0 §10/§11). The encoder
    // must surface a precise error rather than silently treating
    // Gray8 bytes as 1-bit data.
    let gray = vec![0u8; 16 * 8];
    let page = EncodePage {
        width: 16,
        height: 8,
        kind: EncodePixelFormat::Gray8 { pixels: &gray },
        compression: TiffCompression::CcittT6,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let err = encode_tiff(&page).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("CCITT"), "{msg}");
}

#[test]
fn t4_2d_rejects_predictor() {
    let packed = checkerboard(16, 8);
    let page = EncodePage {
        width: 16,
        height: 8,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let err = encode_tiff(&page).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Predictor") || msg.contains("predictor"),
        "{msg}"
    );
}

#[test]
fn t6_rejects_tiled() {
    let packed = checkerboard(16, 16);
    let page = EncodePage {
        width: 16,
        height: 16,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittT6,
        predictor: false,
        planar: false,
        tiling: Some((16, 16)),
        bigtiff: false,
    };
    let err = encode_tiff(&page).unwrap_err();
    let msg = format!("{err:?}");
    // The tile-on-1-bit rejection fires first (with a more specific
    // message), but either error is acceptable.
    assert!(
        msg.contains("tile") || msg.contains("CCITT") || msg.contains("1-bit"),
        "{msg}"
    );
}

// ---------------------------------------------------------------------------
// Multi-page chain: confirm a T.6 + T.4-2D mixed-compression chain
// round-trips. (Each page picks its own compression; this is just
// a sanity check that the IFD-tag emission integrates with the
// existing multi-page writer.)
// ---------------------------------------------------------------------------

#[test]
fn t6_and_t4_2d_in_multi_page_chain() {
    let pa = checkerboard(16, 8);
    let pb = diagonal(24, 24);
    let pages = [
        EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &pa },
            compression: TiffCompression::CcittT6,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
        EncodePage {
            width: 24,
            height: 24,
            kind: EncodePixelFormat::Bilevel { pixels: &pb },
            compression: TiffCompression::CcittT4TwoD {
                eol_byte_aligned: true,
            },
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
    ];
    let bytes = encode_tiff_multi(&pages).expect("encode_tiff_multi");
    let imgs = oxideav_tiff::decode_tiff_all(&bytes).expect("decode_tiff_all");
    assert_eq!(imgs.len(), 2);
    let want_a = bilevel_to_gray8(&pa, 16, 8);
    let want_b = bilevel_to_gray8(&pb, 24, 24);
    assert_eq!(imgs[0].planes[0].data, want_a);
    assert_eq!(imgs[1].planes[0].data, want_b);
}
