//! Integration tests for **tiled** chroma-subsampled YCbCr decode
//! (TIFF 6.0 §21 "YCbCr Images", `PhotometricInterpretation = 6`,
//! `SamplesPerPixel = 3`, `BitsPerSample = [8, 8, 8]`, chunky
//! `PlanarConfiguration = 1`, with a non-1:1 `YCbCrSubSampling`).
//!
//! §21 page 90 admits the subsampled layout under tiling: "TileWidth
//! and TileLength … be integer multiples of YCbCrSubsampleHoriz and
//! YCbCrSubsampleVert respectively." Each tile then holds a whole
//! `tile_w/sh × tile_h/sv` grid of §21 "data units" (each `sh*sv` Y
//! samples followed by one Cb and one Cr), and no data unit straddles a
//! tile boundary.
//!
//! The decoder reassembles those tiles into the same whole-image
//! data-unit buffer the strip path produces, then runs the shared
//! YCbCr→RGB walker. This suite proves that reassembly is correct by
//! comparing the tiled decode against the **strip** decode of the same
//! data units — a binary-independent oracle. Both fixtures are
//! hand-built classic-II TIFFs (no encoder involvement on the tiled
//! side, since the encoder doesn't write tiled YCbCr); the strip path
//! is independently exercised by `encode_ycbcr_subsampled_roundtrip.rs`
//! against the encoder's own output, so a regression in either path
//! would surface as a divergence here or there.

use oxideav_tiff::{decode_tiff, TiffPixelFormat};

// ---------------------------------------------------------------------------
// Reference data-unit packer (mirrors TIFF 6.0 §21 page 93 / 94)
// ---------------------------------------------------------------------------

/// Pack a full-resolution `(Y, Cb, Cr)` raster into the §21 whole-image
/// data-unit byte order: for each `sh × sv` block in row-major scan
/// order, `sv` rows of `sh` Y samples, then the rounded mean Cb, then
/// the rounded mean Cr. Identical in shape to the strip-path output the
/// decoder produces internally, so it doubles as the on-disk payload for
/// the strip fixture below.
fn reference_pack(src: &[u8], w: usize, h: usize, sh: usize, sv: usize) -> Vec<u8> {
    let block_w = w.div_ceil(sh);
    let block_h = h.div_ceil(sv);
    let unit = sh * sv + 2;
    let mut out = vec![0u8; block_w * block_h * unit];
    for by in 0..block_h {
        for bx in 0..block_w {
            let uoff = (by * block_w + bx) * unit;
            let mut cb = 0u32;
            let mut cr = 0u32;
            let mut n = 0u32;
            for sy in 0..sv {
                for sx in 0..sh {
                    let px = bx * sh + sx;
                    let py = by * sv + sy;
                    if px >= w || py >= h {
                        continue;
                    }
                    let s = (py * w + px) * 3;
                    out[uoff + sy * sh + sx] = src[s];
                    cb += src[s + 1] as u32;
                    cr += src[s + 2] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            out[uoff + sh * sv] = ((cb + n / 2) / n) as u8;
            out[uoff + sh * sv + 1] = ((cr + n / 2) / n) as u8;
        }
    }
    out
}

/// Full-resolution `(Y, Cb, Cr)` raster whose chroma is constant within
/// each `sh × sv` block (so box-average + splat round-trips bit-exact)
/// while the luma varies per pixel.
fn block_uniform_chroma(w: usize, h: usize, sh: usize, sv: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let bx = x / sh;
            let by = y / sv;
            let p = (y * w + x) * 3;
            out[p] = ((x * 13 + y * 7) % 256) as u8;
            out[p + 1] = (96 + (bx * 17 + by * 5) % 64) as u8;
            out[p + 2] = (96 + (bx * 11 + by * 23) % 64) as u8;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tile slicing: re-tile a whole-image data-unit buffer into a row-major
// grid of fixed-size tiles, each a contiguous data-unit block.
// ---------------------------------------------------------------------------

/// Slice the whole-image data-unit buffer (`du_across × du_down` units)
/// into a row-major grid of tiles, where each tile is
/// `tile_du_across × tile_du_down` whole data units. Edge tiles that
/// overhang the image data-unit grid replicate the last in-image unit as
/// §15 padding (the decoder drops the overhang, so the padding content
/// is decode-irrelevant — it only needs to exist so every tile is the
/// same fixed size). Returns the per-tile payloads in row-major tile
/// order.
#[allow(clippy::too_many_arguments)]
fn slice_into_tiles(
    whole: &[u8],
    du_across: usize,
    du_down: usize,
    tile_du_across: usize,
    tile_du_down: usize,
    unit_bytes: usize,
) -> Vec<Vec<u8>> {
    let tiles_across = du_across.div_ceil(tile_du_across);
    let tiles_down = du_down.div_ceil(tile_du_down);
    let mut tiles = Vec::with_capacity(tiles_across * tiles_down);
    for ty in 0..tiles_down {
        for tx in 0..tiles_across {
            let mut tile = vec![0u8; tile_du_across * tile_du_down * unit_bytes];
            for r in 0..tile_du_down {
                // Clamp source row to the last in-image unit-row (edge pad).
                let src_uy = (ty * tile_du_down + r).min(du_down - 1);
                for c in 0..tile_du_across {
                    let src_ux = (tx * tile_du_across + c).min(du_across - 1);
                    let src = (src_uy * du_across + src_ux) * unit_bytes;
                    let dst = (r * tile_du_across + c) * unit_bytes;
                    tile[dst..dst + unit_bytes].copy_from_slice(&whole[src..src + unit_bytes]);
                }
            }
            tiles.push(tile);
        }
    }
    tiles
}

// ---------------------------------------------------------------------------
// Fixture builders (classic-II, byte-level — independent of our decoder)
// ---------------------------------------------------------------------------

fn push_entry(buf: &mut Vec<u8>, tag: u16, ft: u16, count: u32, val: [u8; 4]) {
    buf.extend_from_slice(&tag.to_le_bytes());
    buf.extend_from_slice(&ft.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&val);
}

fn short_val(v: u16) -> [u8; 4] {
    let mut s = [0u8; 4];
    s[..2].copy_from_slice(&v.to_le_bytes());
    s
}

/// Common trailer: BitsPerSample [8,8,8] + ReferenceBlackWhite (6
/// RATIONALs, §20 no-headroom full range), appended after the IFD.
fn push_trailer(buf: &mut Vec<u8>, bps_off: u32, rbw_off: u32) {
    while buf.len() < bps_off as usize {
        buf.push(0);
    }
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    while buf.len() < rbw_off as usize {
        buf.push(0);
    }
    for (n, d) in [
        (0u32, 1u32),
        (255, 1),
        (128, 1),
        (255, 1),
        (128, 1),
        (255, 1),
    ] {
        buf.extend_from_slice(&n.to_le_bytes());
        buf.extend_from_slice(&d.to_le_bytes());
    }
}

/// Build a strip-organised classic-II subsampled-YCbCr TIFF whose single
/// strip is the whole-image §21 data-unit payload.
fn build_strip(width: u32, height: u32, sh: u16, sv: u16, strip: &[u8]) -> Vec<u8> {
    let num_entries: u16 = 13;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset = ifd_offset + ifd_size;

    let bps_off = blobs_offset;
    let mut cursor = bps_off + 6;
    if cursor % 4 != 0 {
        cursor += 4 - (cursor % 4);
    }
    let rbw_off = cursor;
    let pixels_off = rbw_off + 48;

    let mut buf = Vec::new();
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&ifd_offset.to_le_bytes());
    buf.extend_from_slice(&num_entries.to_le_bytes());

    push_entry(&mut buf, 254, 4, 1, 0u32.to_le_bytes());
    push_entry(&mut buf, 256, 4, 1, width.to_le_bytes());
    push_entry(&mut buf, 257, 4, 1, height.to_le_bytes());
    push_entry(&mut buf, 258, 3, 3, bps_off.to_le_bytes());
    push_entry(&mut buf, 259, 3, 1, short_val(1)); // Compression = None
    push_entry(&mut buf, 262, 3, 1, short_val(6)); // Photometric = YCbCr
    push_entry(&mut buf, 273, 4, 1, pixels_off.to_le_bytes()); // StripOffsets
    push_entry(&mut buf, 277, 3, 1, short_val(3)); // SamplesPerPixel
    push_entry(&mut buf, 279, 4, 1, (strip.len() as u32).to_le_bytes()); // StripByteCounts
    push_entry(&mut buf, 284, 3, 1, short_val(1)); // PlanarConfiguration
    let mut ss = [0u8; 4];
    ss[..2].copy_from_slice(&sh.to_le_bytes());
    ss[2..4].copy_from_slice(&sv.to_le_bytes());
    push_entry(&mut buf, 530, 3, 2, ss); // YCbCrSubSampling
    push_entry(&mut buf, 531, 3, 1, short_val(1)); // YCbCrPositioning
    push_entry(&mut buf, 532, 5, 6, rbw_off.to_le_bytes()); // ReferenceBlackWhite
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD

    push_trailer(&mut buf, bps_off, rbw_off);
    buf.extend_from_slice(strip);
    buf
}

/// Literal-only PackBits encode (TIFF 6.0 §9): emit each ≤128-byte run
/// of source bytes as a literal block (control byte `len - 1`, then the
/// bytes). A valid PackBits stream the decoder's `unpack_packbits`
/// reverses; lets the compressed-tile fixtures exercise the
/// `decompress_block` dispatch without depending on the crate's internal
/// packer.
fn packbits_literal(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in src.chunks(128) {
        out.push((chunk.len() - 1) as u8);
        out.extend_from_slice(chunk);
    }
    out
}

/// Build a tile-organised classic-II subsampled-YCbCr TIFF. The
/// whole-image data-unit buffer is sliced into a `tile_w × tile_h` grid;
/// `TileWidth` / `TileLength` / `TileOffsets` / `TileByteCounts` replace
/// the strip fields. `payloads` are the per-tile on-disk byte strings
/// (already compressed for `compression != 1`); their lengths populate
/// `TileByteCounts`.
#[allow(clippy::too_many_arguments)]
fn build_tiled_comp(
    width: u32,
    height: u32,
    sh: u16,
    sv: u16,
    tile_w: u32,
    tile_h: u32,
    compression: u16,
    payloads: &[Vec<u8>],
) -> Vec<u8> {
    let n = payloads.len() as u32;
    let num_entries: u16 = 15;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let mut cursor = ifd_offset + ifd_size;

    let bps_off = cursor;
    cursor = bps_off + 6;
    if cursor % 4 != 0 {
        cursor += 4 - (cursor % 4);
    }
    let rbw_off = cursor;
    cursor = rbw_off + 48;
    // A single-value LONG array (n == 1) is stored inline in the IFD
    // entry's value slot per TIFF 6.0; only n > 1 spills to an
    // out-of-line array. Reserve out-of-line space only when needed.
    let inline = n == 1;
    let toff_off = cursor; // TileOffsets array (n LONGs), when out-of-line
    if !inline {
        cursor = toff_off + n * 4;
    }
    let tbc_off = cursor; // TileByteCounts array (n LONGs), when out-of-line
    if !inline {
        cursor = tbc_off + n * 4;
    }
    let pixels_off = cursor;

    // Per-tile data offsets and byte counts (payloads may vary in length
    // once compressed).
    let mut tile_offsets = Vec::with_capacity(payloads.len());
    let mut o = pixels_off;
    for p in payloads {
        tile_offsets.push(o);
        o += p.len() as u32;
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&ifd_offset.to_le_bytes());
    buf.extend_from_slice(&num_entries.to_le_bytes());

    push_entry(&mut buf, 254, 4, 1, 0u32.to_le_bytes());
    push_entry(&mut buf, 256, 4, 1, width.to_le_bytes());
    push_entry(&mut buf, 257, 4, 1, height.to_le_bytes());
    push_entry(&mut buf, 258, 3, 3, bps_off.to_le_bytes());
    push_entry(&mut buf, 259, 3, 1, short_val(compression)); // Compression
    push_entry(&mut buf, 262, 3, 1, short_val(6)); // Photometric = YCbCr
    push_entry(&mut buf, 277, 3, 1, short_val(3)); // SamplesPerPixel
    push_entry(&mut buf, 284, 3, 1, short_val(1)); // PlanarConfiguration
    push_entry(&mut buf, 322, 4, 1, tile_w.to_le_bytes()); // TileWidth
    push_entry(&mut buf, 323, 4, 1, tile_h.to_le_bytes()); // TileLength
                                                           // TileOffsets (324) / TileByteCounts (325): inline single value when
                                                           // n == 1, else point at the out-of-line LONG arrays written below.
    if inline {
        push_entry(&mut buf, 324, 4, 1, tile_offsets[0].to_le_bytes());
        push_entry(
            &mut buf,
            325,
            4,
            1,
            (payloads[0].len() as u32).to_le_bytes(),
        );
    } else {
        push_entry(&mut buf, 324, 4, n, toff_off.to_le_bytes());
        push_entry(&mut buf, 325, 4, n, tbc_off.to_le_bytes());
    }
    let mut ss = [0u8; 4];
    ss[..2].copy_from_slice(&sh.to_le_bytes());
    ss[2..4].copy_from_slice(&sv.to_le_bytes());
    push_entry(&mut buf, 530, 3, 2, ss); // YCbCrSubSampling
    push_entry(&mut buf, 531, 3, 1, short_val(1)); // YCbCrPositioning
    push_entry(&mut buf, 532, 5, 6, rbw_off.to_le_bytes()); // ReferenceBlackWhite
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD

    push_trailer(&mut buf, bps_off, rbw_off);
    // TileOffsets / TileByteCounts arrays (only when out-of-line).
    if !inline {
        while buf.len() < toff_off as usize {
            buf.push(0);
        }
        for &off in &tile_offsets {
            buf.extend_from_slice(&off.to_le_bytes());
        }
        while buf.len() < tbc_off as usize {
            buf.push(0);
        }
        for p in payloads {
            buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
        }
    }
    while buf.len() < pixels_off as usize {
        buf.push(0);
    }
    for p in payloads {
        buf.extend_from_slice(p);
    }
    buf
}

/// Uncompressed (`Compression = None`) convenience wrapper.
fn build_tiled(
    width: u32,
    height: u32,
    sh: u16,
    sv: u16,
    tile_w: u32,
    tile_h: u32,
    tiles: &[Vec<u8>],
) -> Vec<u8> {
    build_tiled_comp(width, height, sh, sv, tile_w, tile_h, 1, tiles)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Decode the strip and tiled fixtures of the same data units and assert
/// the rendered Rgb24 planes are byte-identical.
fn assert_tiled_matches_strip(w: u32, h: u32, sh: usize, sv: usize, tile_w: u32, tile_h: u32) {
    let pixels = block_uniform_chroma(w as usize, h as usize, sh, sv);
    let whole = reference_pack(&pixels, w as usize, h as usize, sh, sv);

    let unit_bytes = sh * sv + 2;
    let du_across = (w as usize).div_ceil(sh);
    let du_down = (h as usize).div_ceil(sv);
    let tile_du_across = tile_w as usize / sh;
    let tile_du_down = tile_h as usize / sv;

    let tiles = slice_into_tiles(
        &whole,
        du_across,
        du_down,
        tile_du_across,
        tile_du_down,
        unit_bytes,
    );

    let strip_tiff = build_strip(w, h, sh as u16, sv as u16, &whole);
    let tiled_tiff = build_tiled(w, h, sh as u16, sv as u16, tile_w, tile_h, &tiles);

    let ds = decode_tiff(&strip_tiff).unwrap();
    let dt = decode_tiff(&tiled_tiff).unwrap();

    assert_eq!((dt.width, dt.height), (w, h), "tiled dims");
    assert_eq!(dt.pixel_format, TiffPixelFormat::Rgb24, "tiled format");
    assert_eq!(
        dt.frame.planes[0].data, ds.frame.planes[0].data,
        "tiled vs strip decode diverged for {w}x{h} sub ({sh},{sv}) tile {tile_w}x{tile_h}"
    );
}

#[test]
fn tiled_422_single_tile_exact_fit() {
    // 4:2:2 (sh=2, sv=1). 16×16 image, one 16×16 tile (TileWidth/Length
    // both multiples of 16 per §15 and of (2,1) per §21).
    assert_tiled_matches_strip(16, 16, 2, 1, 16, 16);
}

#[test]
fn tiled_420_multi_tile_exact_fit() {
    // 4:2:0 (sh=2, sv=2). 32×32 image, 2×2 grid of 16×16 tiles.
    assert_tiled_matches_strip(32, 32, 2, 2, 16, 16);
}

#[test]
fn tiled_420_partial_edge_tiles() {
    // 4:2:0, 48×32 image with 16×16 tiles but the image is only 24 data
    // units wide / 16 down at (2,2): exercises right + bottom edge
    // overhang where the last column/row of tiles is partly outside the
    // image data-unit grid.
    assert_tiled_matches_strip(40, 24, 2, 2, 16, 16);
}

#[test]
fn tiled_422_partial_edge_tiles() {
    // 4:2:2 with non-square tiles and edge overhang.
    assert_tiled_matches_strip(48, 40, 2, 1, 32, 16);
}

#[test]
fn tiled_441_multi_tile() {
    // 4:1:1 (sh=4, sv=1): tile width a multiple of 16 (and of 4).
    assert_tiled_matches_strip(32, 16, 4, 1, 16, 16);
}

#[test]
fn tiled_442_partial_edge() {
    // 4:2:... actually (4,2): sh=4, sv=2 with edge overhang both axes.
    assert_tiled_matches_strip(40, 24, 4, 2, 16, 16);
}

#[test]
fn tiled_oversized_single_tile() {
    // A single tile larger than the image in both axes (all but the
    // top-left corner is §15 edge padding the decoder drops).
    assert_tiled_matches_strip(16, 8, 2, 2, 32, 16);
}

#[test]
fn tiled_subsampled_decodes_to_expected_dims() {
    // Sanity on the absolute geometry (not just strip-equality): a 4:2:0
    // 32×16 image decodes to a 32×16 Rgb24 plane.
    let (w, h, sh, sv) = (32u32, 16u32, 2usize, 2usize);
    let pixels = block_uniform_chroma(w as usize, h as usize, sh, sv);
    let whole = reference_pack(&pixels, w as usize, h as usize, sh, sv);
    let unit_bytes = sh * sv + 2;
    let tiles = slice_into_tiles(
        &whole,
        (w as usize).div_ceil(sh),
        (h as usize).div_ceil(sv),
        16 / sh,
        16 / sv,
        unit_bytes,
    );
    let tiff = build_tiled(w, h, sh as u16, sv as u16, 16, 16, &tiles);
    let d = decode_tiff(&tiff).unwrap();
    assert_eq!((d.width, d.height), (w, h));
    assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
    assert_eq!(d.frame.planes[0].data.len(), (w * h * 3) as usize);
}

#[test]
fn tiled_422_packbits_compressed() {
    // PackBits-compressed subsampled-YCbCr tiles exercise the
    // `decompress_block` dispatch through `unpack_packbits` and confirm
    // the data-unit reassembly is identical to the uncompressed path.
    let (w, h, sh, sv, tw, th) = (48u32, 32u32, 2usize, 1usize, 16u32, 16u32);
    let pixels = block_uniform_chroma(w as usize, h as usize, sh, sv);
    let whole = reference_pack(&pixels, w as usize, h as usize, sh, sv);
    let unit_bytes = sh * sv + 2;
    let tiles = slice_into_tiles(
        &whole,
        (w as usize).div_ceil(sh),
        (h as usize).div_ceil(sv),
        tw as usize / sh,
        th as usize / sv,
        unit_bytes,
    );
    let payloads: Vec<Vec<u8>> = tiles.iter().map(|t| packbits_literal(t)).collect();

    let strip_tiff = build_strip(w, h, sh as u16, sv as u16, &whole);
    // Compression = 32773 (PackBits).
    let tiled_tiff = build_tiled_comp(w, h, sh as u16, sv as u16, tw, th, 32773, &payloads);

    let ds = decode_tiff(&strip_tiff).unwrap();
    let dt = decode_tiff(&tiled_tiff).unwrap();
    assert_eq!((dt.width, dt.height), (w, h));
    assert_eq!(dt.frame.planes[0].data, ds.frame.planes[0].data);
}

#[test]
fn tiled_subsampled_rejects_non_multiple_tile_dims() {
    // §21 page 90 requires TileWidth / TileLength to be integer multiples
    // of the subsampling factors. A non-conformant writer that violates
    // this (a data unit straddling a tile boundary) must be rejected, not
    // mis-decoded. Hand-build a fixture whose TileLength (18) is not a
    // multiple of YCbCrSubsampleVert (4); the payload size is irrelevant
    // because the geometry check fires first.
    let (w, h, sh, sv) = (16u32, 16u32, 4usize, 4usize);
    let pixels = block_uniform_chroma(w as usize, h as usize, sh, sv);
    let whole = reference_pack(&pixels, w as usize, h as usize, sh, sv);
    let unit_bytes = sh * sv + 2;
    // One oversized tile covering the whole image but with a bogus
    // TileLength that is not a multiple of sv.
    let tiles = slice_into_tiles(
        &whole,
        (w as usize).div_ceil(sh),
        (h as usize).div_ceil(sv),
        16 / sh,
        16 / sv,
        unit_bytes,
    );
    // TileWidth 16 (mult of 4) but TileLength 18 (NOT a mult of 4).
    let tiff = build_tiled(w, h, sh as u16, sv as u16, 16, 18, &tiles);
    let err = match decode_tiff(&tiff) {
        Ok(_) => panic!("expected §21 multiple-constraint rejection, but decode succeeded"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("integer multiples") || msg.contains("§21"),
        "expected §21 multiple-constraint rejection, got: {msg}"
    );
}
