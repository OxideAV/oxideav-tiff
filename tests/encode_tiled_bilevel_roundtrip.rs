//! Encode-side sub-byte (1-bit) tile writing (TIFF 6.0 §15).
//!
//! The decoder already reassembles sub-byte chunky tiles (validated by
//! `decode_tiled_subbyte.rs` against hand-built classic-II fixtures). This
//! suite exercises the *encoder's* new `Bilevel` / `TransparencyMask`
//! tiled writer: it encodes the identical 1-bit pixels both tiled and
//! strip-based, decodes both with the in-tree decoder, and asserts the
//! rendered `Gray8` planes are byte-identical. The strip decode is the
//! trusted oracle — it shares no code with the tile slicer — so a writer
//! that mishandled tile ordering, tile-row bit packing, byte-aligned
//! column offsets, or §15 edge padding would diverge from it.
//!
//! Geometry coverage mirrors the decode suite: exact-fit, partial-edge
//! (right column / bottom row / both), non-square tiles, odd width, and an
//! oversized single tile. The byte-aligned compressors the decoder's
//! sub-byte tile path reads (None / PackBits / LZW / Deflate / ZSTD) are
//! all driven so the round-trip is independent of the per-tile codec.

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, PageExtras, TiffCompression,
};

/// Pack an MSB-first 1-bit bilevel raster with a deterministic pseudo-random
/// pattern (a cheap hash of x, y so runs are uneven and tile boundaries fall
/// mid-run).
fn bilevel_pattern(w: u32, h: u32) -> Vec<u8> {
    let row_bytes = (w as usize).div_ceil(8);
    let mut out = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let v = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) >> 5;
            if v & 1 == 1 {
                out[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    out
}

fn encode_bilevel(
    pixels: &[u8],
    w: u32,
    h: u32,
    comp: TiffCompression,
    tiling: Option<(u32, u32)>,
) -> Vec<u8> {
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Bilevel { pixels },
        compression: comp,
        predictor: false,
        planar: false,
        tiling,
        bigtiff: false,
        extras: PageExtras::default(),
    };
    encode_tiff(&page).expect("encode bilevel")
}

fn encode_mask(
    pixels: &[u8],
    w: u32,
    h: u32,
    comp: TiffCompression,
    tiling: Option<(u32, u32)>,
) -> Vec<u8> {
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::TransparencyMask { pixels },
        compression: comp,
        predictor: false,
        planar: false,
        tiling,
        bigtiff: false,
        extras: PageExtras::default(),
    };
    encode_tiff(&page).expect("encode mask")
}

/// The byte-aligned compressors the decoder's sub-byte tile path reads.
const COMPS: &[TiffCompression] = &[
    TiffCompression::None,
    TiffCompression::PackBits,
    TiffCompression::Lzw,
    TiffCompression::Deflate,
    TiffCompression::Zstd,
];

/// Tiled bilevel decodes byte-identically to the strip-based encode of the
/// same pixels, across the full compressor set, for a given geometry.
fn assert_tiled_matches_strip(w: u32, h: u32, tile: (u32, u32)) {
    let pixels = bilevel_pattern(w, h);
    for &comp in COMPS {
        let strip = encode_bilevel(&pixels, w, h, comp, None);
        let tiled = encode_bilevel(&pixels, w, h, comp, Some(tile));

        let ds = decode_tiff(&strip).expect("decode strip");
        let dt = decode_tiff(&tiled).expect("decode tiled");

        assert_eq!((ds.width, ds.height), (w, h));
        assert_eq!((dt.width, dt.height), (w, h));
        assert_eq!(
            dt.frame.planes[0].data, ds.frame.planes[0].data,
            "tiled != strip for {w}x{h} tile {tile:?} comp {comp:?}"
        );
    }
}

#[test]
fn exact_fit_16x16() {
    // Image is an exact multiple of the tile size — no edge padding.
    assert_tiled_matches_strip(32, 32, (16, 16));
}

#[test]
fn partial_right_column() {
    // 40 wide / tile 16 -> two full columns + an 8-px partial right column.
    assert_tiled_matches_strip(40, 32, (16, 16));
}

#[test]
fn partial_bottom_row() {
    assert_tiled_matches_strip(32, 40, (16, 16));
}

#[test]
fn partial_both_edges() {
    assert_tiled_matches_strip(40, 44, (16, 16));
}

#[test]
fn non_square_tiles() {
    // tile wider than tall, exercising tile_row_bytes vs tile_h independently.
    assert_tiled_matches_strip(50, 40, (32, 16));
}

#[test]
fn odd_width() {
    // Width not a multiple of 8 -> the image-row trailing byte is partial,
    // but tile_w (16) keeps tile columns byte-aligned.
    assert_tiled_matches_strip(37, 20, (16, 16));
}

#[test]
fn oversized_single_tile() {
    // One tile larger than the whole image -> a single padded tile.
    assert_tiled_matches_strip(20, 12, (32, 16));
}

#[test]
fn one_pixel_overhang() {
    // Image is 1 px past an exact tile multiple in each axis.
    assert_tiled_matches_strip(33, 33, (16, 16));
}

#[test]
fn transparency_mask_tiled_matches_strip() {
    // The mask path uses the identical 1-bit slicer; verify the polarity-
    // pinned mask render (1 -> 0xFF, 0 -> 0x00) survives the tile grid.
    let (w, h, tile) = (40u32, 36u32, (16u32, 16u32));
    let pixels = bilevel_pattern(w, h);
    for &comp in COMPS {
        let strip = encode_mask(&pixels, w, h, comp, None);
        let tiled = encode_mask(&pixels, w, h, comp, Some(tile));
        let ds = decode_tiff(&strip).expect("decode strip mask");
        let dt = decode_tiff(&tiled).expect("decode tiled mask");
        assert_eq!(
            dt.frame.planes[0].data, ds.frame.planes[0].data,
            "mask tiled != strip comp {comp:?}"
        );
    }
}

#[test]
fn bigtiff_tiled_bilevel_matches_strip() {
    // BigTIFF composes with the sub-byte tile writer unchanged.
    let (w, h, tile) = (48u32, 40u32, (16u32, 16u32));
    let pixels = bilevel_pattern(w, h);
    let page_strip = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Bilevel { pixels: &pixels },
        compression: TiffCompression::Lzw,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
        extras: PageExtras::default(),
    };
    let page_tiled = EncodePage {
        tiling: Some(tile),
        ..page_strip.clone()
    };
    let strip = encode_tiff(&page_strip).expect("encode strip bigtiff");
    let tiled = encode_tiff(&page_tiled).expect("encode tiled bigtiff");
    let ds = decode_tiff(&strip).expect("decode strip bigtiff");
    let dt = decode_tiff(&tiled).expect("decode tiled bigtiff");
    assert_eq!(dt.frame.planes[0].data, ds.frame.planes[0].data);
}

#[test]
fn tile_dims_must_be_multiple_of_16() {
    // §15 TileWidth / TileLength must be non-zero multiples of 16.
    let pixels = bilevel_pattern(32, 32);
    let page = EncodePage {
        width: 32,
        height: 32,
        kind: EncodePixelFormat::Bilevel { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: Some((8, 16)),
        bigtiff: false,
        extras: PageExtras::default(),
    };
    assert!(encode_tiff(&page).is_err(), "tile_w=8 must be rejected");
}

#[test]
fn ccitt_tiling_still_rejected() {
    // CCITT remains strip-oriented on the encode side.
    let pixels = bilevel_pattern(32, 32);
    let page = EncodePage {
        width: 32,
        height: 32,
        kind: EncodePixelFormat::Bilevel { pixels: &pixels },
        compression: TiffCompression::CcittRle,
        predictor: false,
        planar: false,
        tiling: Some((16, 16)),
        bigtiff: false,
        extras: PageExtras::default(),
    };
    assert!(
        encode_tiff(&page).is_err(),
        "CCITT + tiling must be rejected"
    );
}
