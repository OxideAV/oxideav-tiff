//! Self-roundtrip tests for the read-side tiled-layout decode (TIFF
//! 6.0 §15 "Tiled Images"), the counterpart to the crate's tiled
//! encoder ([`EncodePage::tiling`]).
//!
//! These run unconditionally — they need no external binary — so the
//! tile decode path (TileWidth/TileLength + TileOffsets/TileByteCounts
//! parsing, per-tile decompression, per-tile §14 predictor reversal,
//! and §15 edge-tile boundary-padding removal during reassembly) is
//! always covered in CI.
//!
//! The oracle for every case is the *strip-based* decode of the same
//! source pixels: we encode the identical image twice — once tiled,
//! once strip — through our own writer, decode both through
//! [`decode_tiff`], and assert the resulting pixel planes are
//! byte-identical. A tiled decoder that mishandled tile ordering,
//! row stride within a tile, or the §15 padding of edge tiles would
//! diverge from the strip decode of the same image, so equality
//! across the two layouts is a strong correctness signal that is
//! independent of any external reference.

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, RgbColor, TiffCompression,
};

// ---- Source-pixel generators (deterministic, layout-independent) ----

fn ramp_gray8(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x.wrapping_mul(3)).wrapping_add(y.wrapping_mul(5)) & 0xFF) as u8);
        }
    }
    v
}

fn ramp_gray16le(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 2) as usize);
    for y in 0..h {
        for x in 0..w {
            let s = (x.wrapping_mul(257)).wrapping_add(y.wrapping_mul(1013)) & 0xFFFF;
            v.extend_from_slice(&(s as u16).to_le_bytes());
        }
    }
    v
}

fn pattern_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(7) & 0xFF) as u8);
            v.push((y.wrapping_mul(11) & 0xFF) as u8);
            v.push(((x ^ y).wrapping_mul(13) & 0xFF) as u8);
        }
    }
    v
}

fn palette_indices(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x.wrapping_add(y.wrapping_mul(3))) & 0xFF) as u8);
        }
    }
    v
}

/// A 256-entry palette where each entry is a distinct, recoverable
/// 8-bit triple (the encoder stores it as `(v<<8)|v` SHORTs and the
/// decoder reads back `>>8`, so 8-bit values survive exactly).
fn full_palette() -> Vec<RgbColor> {
    (0..256u32)
        .map(|i| {
            [
                (i & 0xFF) as u8,
                ((i.wrapping_mul(2)) & 0xFF) as u8,
                ((255 - i) & 0xFF) as u8,
            ]
        })
        .collect()
}

/// Decode `page` (with `tiling` already chosen by the caller) and the
/// same source as a strip page, returning the two decoded pixel planes
/// plus the (w, h) the tiled decode reported. The two planes must be
/// equal for a correct tile decode.
fn tiled_vs_strip(page_tiled: &EncodePage<'_>, page_strip: &EncodePage<'_>) -> (Vec<u8>, Vec<u8>) {
    let tiled_bytes = encode_tiff(page_tiled).expect("encode tiled");
    let strip_bytes = encode_tiff(page_strip).expect("encode strip");
    let dt = decode_tiff(&tiled_bytes).expect("decode tiled");
    let ds = decode_tiff(&strip_bytes).expect("decode strip");
    assert_eq!(
        (dt.width, dt.height),
        (ds.width, ds.height),
        "tiled/strip dimension mismatch"
    );
    (
        dt.frame.planes[0].data.clone(),
        ds.frame.planes[0].data.clone(),
    )
}

fn gray8_page<'a>(
    w: u32,
    h: u32,
    px: &'a [u8],
    comp: TiffCompression,
    pred: bool,
    tiling: Option<(u32, u32)>,
) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Gray8 { pixels: px },
        compression: comp,
        predictor: pred,
        planar: false,
        tiling,
    }
}

// ---- Gray8 ----

#[test]
fn tiled_gray8_partial_edges_match_strip() {
    // 50x30 over 16x16 tiles => 4 across (last col 2 px) x 2 down
    // (last row 14 px): both right-column and bottom-row tiles carry
    // §15 boundary padding the decoder must drop on reassembly.
    let px = ramp_gray8(50, 30);
    let (t, s) = tiled_vs_strip(
        &gray8_page(50, 30, &px, TiffCompression::None, false, Some((16, 16))),
        &gray8_page(50, 30, &px, TiffCompression::None, false, None),
    );
    assert_eq!(t, s, "tiled gray8 != strip gray8");
    assert_eq!(t, px, "tiled gray8 != source");
}

#[test]
fn tiled_gray8_exact_grid_match_strip() {
    // 32x32 over 16x16 => a clean 2x2 grid with no edge padding.
    let px = ramp_gray8(32, 32);
    let (t, s) = tiled_vs_strip(
        &gray8_page(32, 32, &px, TiffCompression::None, false, Some((16, 16))),
        &gray8_page(32, 32, &px, TiffCompression::None, false, None),
    );
    assert_eq!(t, s);
    assert_eq!(t, px);
}

#[test]
fn tiled_gray8_tile_bigger_than_image_match_strip() {
    // 20x12 image, 32x16 tile => one tile padded on BOTH axes; the
    // decoder must crop a single oversized tile down to the image.
    let px = ramp_gray8(20, 12);
    let (t, s) = tiled_vs_strip(
        &gray8_page(20, 12, &px, TiffCompression::None, false, Some((32, 16))),
        &gray8_page(20, 12, &px, TiffCompression::None, false, None),
    );
    assert_eq!(t, s);
    assert_eq!(t, px);
}

#[test]
fn tiled_gray8_all_compressions_match_strip() {
    // The decode must be layout- and codec-independent: every
    // supported tile compression decodes to the same pixels.
    let px = ramp_gray8(50, 30);
    for comp in [
        TiffCompression::None,
        TiffCompression::PackBits,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
    ] {
        let (t, s) = tiled_vs_strip(
            &gray8_page(50, 30, &px, comp, false, Some((16, 16))),
            &gray8_page(50, 30, &px, comp, false, None),
        );
        assert_eq!(t, s, "tiled gray8 mismatch under {comp:?}");
        assert_eq!(t, px, "tiled gray8 != source under {comp:?}");
    }
}

// ---- Gray16Le (multi-byte samples) ----

#[test]
fn tiled_gray16le_partial_edges_match_strip() {
    let px = ramp_gray16le(50, 30);
    let page_t = EncodePage {
        width: 50,
        height: 30,
        kind: EncodePixelFormat::Gray16Le { pixels: &px },
        compression: TiffCompression::Lzw,
        predictor: false,
        planar: false,
        tiling: Some((16, 16)),
    };
    let page_s = EncodePage {
        tiling: None,
        ..page_t.clone()
    };
    let (t, s) = tiled_vs_strip(&page_t, &page_s);
    assert_eq!(t, s, "tiled gray16 != strip gray16");
}

#[test]
fn tiled_gray16le_predictor_match_strip() {
    // 16-bit + Predictor=2: the decoder reverses the §14 horizontal
    // differencing per tile-row using 16-bit sample arithmetic.
    let px = ramp_gray16le(48, 32);
    let page_t = EncodePage {
        width: 48,
        height: 32,
        kind: EncodePixelFormat::Gray16Le { pixels: &px },
        compression: TiffCompression::Deflate,
        predictor: true,
        planar: false,
        tiling: Some((16, 16)),
    };
    let page_s = EncodePage {
        tiling: None,
        ..page_t.clone()
    };
    let (t, s) = tiled_vs_strip(&page_t, &page_s);
    assert_eq!(t, s, "tiled gray16 predictor != strip");
}

// ---- Rgb24 (multi-sample chunky) ----

#[test]
fn tiled_rgb24_partial_edges_match_strip() {
    let px = pattern_rgb(50, 30);
    let page_t = EncodePage {
        width: 50,
        height: 30,
        kind: EncodePixelFormat::Rgb24 { pixels: &px },
        compression: TiffCompression::Lzw,
        predictor: false,
        planar: false,
        tiling: Some((16, 16)),
    };
    let page_s = EncodePage {
        tiling: None,
        ..page_t.clone()
    };
    let (t, s) = tiled_vs_strip(&page_t, &page_s);
    assert_eq!(t, s, "tiled rgb24 != strip rgb24");
    assert_eq!(t, px, "tiled rgb24 != source");
}

#[test]
fn tiled_rgb24_predictor_match_strip() {
    // RGB + Predictor=2 reverses the per-component (stride-3) §14
    // differencing within every tile row.
    let px = pattern_rgb(48, 32);
    let page_t = EncodePage {
        width: 48,
        height: 32,
        kind: EncodePixelFormat::Rgb24 { pixels: &px },
        compression: TiffCompression::Deflate,
        predictor: true,
        planar: false,
        tiling: Some((16, 16)),
    };
    let page_s = EncodePage {
        tiling: None,
        ..page_t.clone()
    };
    let (t, s) = tiled_vs_strip(&page_t, &page_s);
    assert_eq!(t, s, "tiled rgb24 predictor != strip");
    assert_eq!(t, px, "tiled rgb24 predictor != source");
}

#[test]
fn tiled_rgb24_nonsquare_tiles_match_strip() {
    // 32x16 (non-square) tiles over a 50x30 image: width and height
    // crops differ, exercising independent x/y boundary handling.
    let px = pattern_rgb(50, 30);
    let page_t = EncodePage {
        width: 50,
        height: 30,
        kind: EncodePixelFormat::Rgb24 { pixels: &px },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: Some((32, 16)),
    };
    let page_s = EncodePage {
        tiling: None,
        ..page_t.clone()
    };
    let (t, s) = tiled_vs_strip(&page_t, &page_s);
    assert_eq!(t, s);
    assert_eq!(t, px);
}

// ---- Palette8 (index -> colormap path through tiles) ----

#[test]
fn tiled_palette8_match_strip() {
    // Palette indices route through the same chunky tile walker as
    // Gray8 (1 sample/pixel, 8-bit) before colormap expansion; the
    // decode expands both layouts to Rgb24, so they must match.
    let idx = palette_indices(50, 30);
    let pal = full_palette();
    let page_t = EncodePage {
        width: 50,
        height: 30,
        kind: EncodePixelFormat::Palette8 {
            indices: &idx,
            palette: &pal,
        },
        compression: TiffCompression::Lzw,
        predictor: false,
        planar: false,
        tiling: Some((16, 16)),
    };
    let page_s = EncodePage {
        tiling: None,
        ..page_t.clone()
    };
    let (t, s) = tiled_vs_strip(&page_t, &page_s);
    assert_eq!(t, s, "tiled palette8 != strip palette8");
    // Every output pixel must equal its source index mapped through
    // the palette (proves the index survived tiling + reassembly).
    assert_eq!(t.len(), (50 * 30 * 3) as usize);
    for (i, &ix) in idx.iter().enumerate() {
        let c = pal[ix as usize];
        assert_eq!(&t[i * 3..i * 3 + 3], &c[..], "palette pixel {i} wrong");
    }
}

// ---- Single-row / single-column edge cases ----

#[test]
fn tiled_gray8_single_tile_covers_image_match_strip() {
    // Image smaller than one tile in both dims: a single tile holds the
    // whole image plus padding the decoder strips.
    let px = ramp_gray8(10, 7);
    let (t, s) = tiled_vs_strip(
        &gray8_page(10, 7, &px, TiffCompression::Lzw, false, Some((16, 16))),
        &gray8_page(10, 7, &px, TiffCompression::Lzw, false, None),
    );
    assert_eq!(t, s);
    assert_eq!(t, px);
}

#[test]
fn tiled_gray8_one_pixel_overhang_match_strip() {
    // 33x33 over 32x32 tiles => a 1-px overhang on each axis (3x3 grid
    // whose right column and bottom row are 1 px wide/tall): the most
    // aggressive §15 padding case.
    let px = ramp_gray8(33, 33);
    let (t, s) = tiled_vs_strip(
        &gray8_page(33, 33, &px, TiffCompression::Deflate, false, Some((32, 32))),
        &gray8_page(33, 33, &px, TiffCompression::Deflate, false, None),
    );
    assert_eq!(t, s);
    assert_eq!(t, px);
}
