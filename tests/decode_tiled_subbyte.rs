//! Decoder coverage for **sub-byte (1-bit and 4-bit) tiled images**
//! (TIFF 6.0 §15 "Tiled Images" combined with the sub-byte sample
//! packing of §"Compression" / §"BitsPerSample").
//!
//! ## Spec basis
//!
//! TIFF 6.0 §15 (page 67) requires `TileWidth` to be "a multiple of
//! 16", and §15 (page 66) states that within a tile "each row of data
//! in a tile is treated as a separate 'scanline'" — i.e. every tile
//! row is independently padded up to a byte boundary, exactly as a
//! strip scanline is. Because `TileWidth` is a multiple of 16, the
//! product `TileWidth · SamplesPerPixel · BitsPerSample` is a multiple
//! of 8 for any `BitsPerSample ∈ {1, 4}` with `SamplesPerPixel = 1`,
//! so every tile-column boundary in the reassembled image row lands on
//! a byte boundary. The decoder can therefore copy each tile's visible
//! row as a whole number of bytes at a byte-aligned destination offset
//! — no cross-byte bit shifting is ever required.
//!
//! §15 "Padding" (page 66): "Boundary tiles are padded to the tile
//! boundaries … good TIFF readers display only the pixels defined by
//! ImageWidth and ImageLength and ignore any padded pixels." The
//! rightmost / bottom tiles in these fixtures are deliberately partial
//! so the padding-removal path is exercised.
//!
//! ## Oracle
//!
//! Every fixture is built twice from the *same* source pixels: once as
//! a tiled classic-II TIFF and once as a single-strip classic-II TIFF.
//! The single-strip sub-byte decode path is long-established and
//! independently validated (it is the same path the bilevel / 4-bit
//! strip fixtures and the CCITT / palette decoders ride). Decoding both
//! layouts through [`decode_tiff`] and asserting the resulting planes
//! are byte-identical is therefore a strong, binary-independent
//! correctness signal for the new tiled sub-byte path: a decoder that
//! mishandled tile ordering, tile-row stride, byte-aligned column
//! offsets, or §15 edge padding would diverge from the strip decode of
//! the identical image.

use oxideav_tiff::{decode_tiff, TiffPixelFormat};

// ---------------------------------------------------------------------
// Bit-packing helpers
// ---------------------------------------------------------------------

/// Pack one logical row of `samples` (each `bps` bits, MSB-first within
/// the byte, lower column => higher-order bits per FillOrder = 1) into a
/// byte vector of length `ceil(samples.len() * bps / 8)`.
fn pack_row(samples: &[u8], bps: usize) -> Vec<u8> {
    let nbytes = (samples.len() * bps).div_ceil(8);
    let mut out = vec![0u8; nbytes];
    for (i, &s) in samples.iter().enumerate() {
        let bit_pos = i * bps; // bit index from the MSB of byte 0
        let byte = bit_pos / 8;
        let shift = 8 - bps - (bit_pos % 8); // MSB-first
        let mask = ((1u16 << bps) - 1) as u8;
        out[byte] |= (s & mask) << shift;
    }
    out
}

/// Pack a full `width × height` image (row-major sample values) into the
/// image-row-stride layout the strip path consumes: each row padded to
/// `ceil(width * bps / 8)` bytes.
fn pack_image(samples: &[u8], width: usize, height: usize, bps: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for y in 0..height {
        let row = &samples[y * width..(y + 1) * width];
        out.extend_from_slice(&pack_row(row, bps));
    }
    out
}

/// Build one tile's packed bytes (tile-row stride =
/// `ceil(tile_w * bps / 8)`), extracting the tile's pixel rectangle from
/// the source samples and replicating the last visible column / row into
/// the padded region (§15 "Padding": "replicating the last column and
/// last row"). The padding value is irrelevant to a conformant reader
/// (it is dropped), but replication mirrors what real writers emit.
#[allow(clippy::too_many_arguments)]
fn build_tile(
    samples: &[u8],
    width: usize,
    height: usize,
    tx: usize,
    ty: usize,
    tile_w: usize,
    tile_h: usize,
    bps: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    for r in 0..tile_h {
        let src_y = (ty * tile_h + r).min(height - 1);
        let mut row = Vec::with_capacity(tile_w);
        for c in 0..tile_w {
            let src_x = (tx * tile_w + c).min(width - 1);
            row.push(samples[src_y * width + src_x]);
        }
        out.extend_from_slice(&pack_row(&row, bps));
    }
    out
}

// ---------------------------------------------------------------------
// Classic-II fixture assembly
// ---------------------------------------------------------------------

fn entry_short(tag: u16, value: u16) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..10].copy_from_slice(&value.to_le_bytes());
    e
}

fn entry_long(tag: u16, value: u32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&4u16.to_le_bytes()); // LONG
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..12].copy_from_slice(&value.to_le_bytes());
    e
}

/// SHORT array entry whose values live out-of-line at `offset`.
fn entry_short_array(tag: u16, count: u32, offset: u32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    e[4..8].copy_from_slice(&count.to_le_bytes());
    e[8..12].copy_from_slice(&offset.to_le_bytes());
    e
}

/// LONG array entry: count <= 1 packs inline, otherwise `offset` must
/// point at the array bytes.
fn entry_long_array(tag: u16, count: u32, offset: u32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&4u16.to_le_bytes()); // LONG
    e[4..8].copy_from_slice(&count.to_le_bytes());
    e[8..12].copy_from_slice(&offset.to_le_bytes());
    e
}

// Tags
const T_IMAGE_WIDTH: u16 = 256;
const T_IMAGE_LENGTH: u16 = 257;
const T_BITS_PER_SAMPLE: u16 = 258;
const T_COMPRESSION: u16 = 259;
const T_PHOTOMETRIC: u16 = 262;
const T_SAMPLES_PER_PIXEL: u16 = 277;
const T_TILE_WIDTH: u16 = 322;
const T_TILE_LENGTH: u16 = 323;
const T_TILE_OFFSETS: u16 = 324;
const T_TILE_BYTE_COUNTS: u16 = 325;
const T_COLOR_MAP: u16 = 320;

/// Assemble a classic little-endian uncompressed **tiled** TIFF for a
/// single-sample sub-byte image.
#[allow(clippy::too_many_arguments)]
fn build_tiled(
    samples: &[u8],
    width: usize,
    height: usize,
    tile_w: usize,
    tile_h: usize,
    bps: usize,
    photometric: u16,
    colormap: Option<&[u16]>,
) -> Vec<u8> {
    assert_eq!(tile_w % 16, 0, "TileWidth must be a multiple of 16");
    assert_eq!(tile_h % 16, 0, "TileLength must be a multiple of 16");

    let tiles_across = width.div_ceil(tile_w);
    let tiles_down = height.div_ceil(tile_h);
    let mut tiles: Vec<Vec<u8>> = Vec::new();
    for ty in 0..tiles_down {
        for tx in 0..tiles_across {
            tiles.push(build_tile(
                samples, width, height, tx, ty, tile_w, tile_h, bps,
            ));
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    let ifd_off_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());

    // Tile data follows the header. Record each tile's offset.
    let mut tile_offsets: Vec<u32> = Vec::new();
    let mut tile_byte_counts: Vec<u32> = Vec::new();
    for t in &tiles {
        if out.len() % 2 != 0 {
            out.push(0);
        }
        tile_offsets.push(out.len() as u32);
        tile_byte_counts.push(t.len() as u32);
        out.extend_from_slice(t);
    }

    // Out-of-line arrays for TileOffsets / TileByteCounts when > 1 tile.
    if out.len() % 2 != 0 {
        out.push(0);
    }
    let off_array_pos = out.len() as u32;
    for &o in &tile_offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    let bc_array_pos = out.len() as u32;
    for &c in &tile_byte_counts {
        out.extend_from_slice(&c.to_le_bytes());
    }

    // Optional ColorMap (palette): 3 * 2^bps SHORTs, R then G then B.
    let cmap_pos = colormap.map(|cm| {
        if out.len() % 2 != 0 {
            out.push(0);
        }
        let pos = out.len() as u32;
        for &v in cm {
            out.extend_from_slice(&v.to_le_bytes());
        }
        (pos, cm.len() as u32)
    });

    // IFD on a word boundary.
    if out.len() % 2 != 0 {
        out.push(0);
    }
    let ifd_off = out.len() as u32;
    out[ifd_off_pos..ifd_off_pos + 4].copy_from_slice(&ifd_off.to_le_bytes());

    let mut entries: Vec<[u8; 12]> = vec![
        entry_long(T_IMAGE_WIDTH, width as u32),
        entry_long(T_IMAGE_LENGTH, height as u32),
        entry_short(T_BITS_PER_SAMPLE, bps as u16),
        entry_short(T_COMPRESSION, 1), // None
        entry_short(T_PHOTOMETRIC, photometric),
    ];
    if let Some((pos, count)) = cmap_pos {
        entries.push(entry_short_array(T_COLOR_MAP, count, pos));
    }
    entries.push(entry_short(T_SAMPLES_PER_PIXEL, 1));
    entries.push(entry_long(T_TILE_WIDTH, tile_w as u32));
    entries.push(entry_long(T_TILE_LENGTH, tile_h as u32));
    if tile_offsets.len() == 1 {
        entries.push(entry_long(T_TILE_OFFSETS, tile_offsets[0]));
        entries.push(entry_long(T_TILE_BYTE_COUNTS, tile_byte_counts[0]));
    } else {
        entries.push(entry_long_array(
            T_TILE_OFFSETS,
            tile_offsets.len() as u32,
            off_array_pos,
        ));
        entries.push(entry_long_array(
            T_TILE_BYTE_COUNTS,
            tile_byte_counts.len() as u32,
            bc_array_pos,
        ));
    }

    // IFD entries must be in ascending tag order.
    entries.sort_by_key(|e| u16::from_le_bytes([e[0], e[1]]));

    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in &entries {
        out.extend_from_slice(e);
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
    out
}

/// Assemble a classic little-endian uncompressed **single-strip** TIFF
/// for the same sub-byte image — the oracle for the tiled decode.
fn build_strip(
    samples: &[u8],
    width: usize,
    height: usize,
    bps: usize,
    photometric: u16,
    colormap: Option<&[u16]>,
) -> Vec<u8> {
    let packed = pack_image(samples, width, height, bps);

    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    let ifd_off_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());

    let strip_off = out.len() as u32;
    let strip_len = packed.len() as u32;
    out.extend_from_slice(&packed);

    let cmap_pos = colormap.map(|cm| {
        if out.len() % 2 != 0 {
            out.push(0);
        }
        let pos = out.len() as u32;
        for &v in cm {
            out.extend_from_slice(&v.to_le_bytes());
        }
        (pos, cm.len() as u32)
    });

    if out.len() % 2 != 0 {
        out.push(0);
    }
    let ifd_off = out.len() as u32;
    out[ifd_off_pos..ifd_off_pos + 4].copy_from_slice(&ifd_off.to_le_bytes());

    let mut entries: Vec<[u8; 12]> = vec![
        entry_long(T_IMAGE_WIDTH, width as u32),
        entry_long(T_IMAGE_LENGTH, height as u32),
        entry_short(T_BITS_PER_SAMPLE, bps as u16),
        entry_short(T_COMPRESSION, 1),
        entry_short(T_PHOTOMETRIC, photometric),
        entry_long(273, strip_off), // StripOffsets
    ];
    if let Some((pos, count)) = cmap_pos {
        entries.push(entry_short_array(T_COLOR_MAP, count, pos));
    }
    entries.push(entry_short(T_SAMPLES_PER_PIXEL, 1));
    entries.push(entry_long(278, height as u32)); // RowsPerStrip = whole image
    entries.push(entry_long(279, strip_len)); // StripByteCounts

    entries.sort_by_key(|e| u16::from_le_bytes([e[0], e[1]]));

    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in &entries {
        out.extend_from_slice(e);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

// ---------------------------------------------------------------------
// Source-pixel generators
// ---------------------------------------------------------------------

/// 1-bit bilevel checker-ish pattern: a deterministic mix of 0 / 1.
fn samples_1bpp(width: usize, height: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            v.push((((x * 3 + y * 5) ^ (x & y)) & 1) as u8);
        }
    }
    v
}

/// 4-bit values in 0..16, deterministic ramp + noise.
fn samples_4bpp(width: usize, height: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            v.push(((x * 2 + y * 7) & 0x0F) as u8);
        }
    }
    v
}

/// A 16-entry palette (for 4-bit palette images): R/G/B SHORTs scaled
/// so the 8-bit value survives `(v << 8) | v` storage and `>> 8` read.
fn palette_16() -> Vec<u16> {
    let mut cm = Vec::new();
    let mk = |v: u8| ((v as u16) << 8) | (v as u16);
    // R plane
    for i in 0..16u16 {
        cm.push(mk((i * 17) as u8));
    }
    // G plane
    for i in 0..16u16 {
        cm.push(mk(((15 - i) * 17) as u8));
    }
    // B plane
    for i in 0..16u16 {
        cm.push(mk(((i * 3) & 0xFF) as u8));
    }
    cm
}

// ---------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------

fn assert_tiled_matches_strip(tiled: &[u8], strip: &[u8], w: u32, h: u32, pf: TiffPixelFormat) {
    let dt = decode_tiff(tiled).expect("tiled decode");
    let ds = decode_tiff(strip).expect("strip decode");
    assert_eq!(dt.width, w, "tiled width");
    assert_eq!(dt.height, h, "tiled height");
    assert_eq!(dt.pixel_format, pf, "tiled pixel format");
    assert_eq!(ds.pixel_format, pf, "strip pixel format");
    assert_eq!(
        dt.frame.planes.len(),
        ds.frame.planes.len(),
        "plane count mismatch"
    );
    for (i, (tp, sp)) in dt
        .frame
        .planes
        .iter()
        .zip(ds.frame.planes.iter())
        .enumerate()
    {
        assert_eq!(
            tp.data, sp.data,
            "tiled vs strip plane {i} byte mismatch ({w}x{h})"
        );
    }
}

// ---------------------------------------------------------------------
// 1-bit bilevel
// ---------------------------------------------------------------------

#[test]
fn bilevel_1bpp_exact_fit_blackiszero() {
    let (w, h) = (32usize, 32usize);
    let s = samples_1bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 16, 1, 1, None); // photometric 1 = BlackIsZero
    let strip = build_strip(&s, w, h, 1, 1, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

#[test]
fn bilevel_1bpp_exact_fit_whiteiszero() {
    let (w, h) = (48usize, 32usize);
    let s = samples_1bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 16, 1, 0, None); // photometric 0 = WhiteIsZero
    let strip = build_strip(&s, w, h, 1, 0, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

#[test]
fn bilevel_1bpp_partial_edges() {
    // Width / height NOT multiples of the tile size: exercises §15
    // boundary padding and the rightmost-byte sub-byte trim.
    let (w, h) = (40usize, 36usize);
    let s = samples_1bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 16, 1, 1, None);
    let strip = build_strip(&s, w, h, 1, 1, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

#[test]
fn bilevel_1bpp_width_not_multiple_of_8() {
    // Image width with a partial trailing byte in the image row (35
    // bits => 5 bytes, last byte holds 3 valid bits + 5 padding).
    let (w, h) = (35usize, 20usize);
    let s = samples_1bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 16, 1, 1, None);
    let strip = build_strip(&s, w, h, 1, 1, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

#[test]
fn bilevel_1bpp_nonsquare_tiles() {
    let (w, h) = (50usize, 50usize);
    let s = samples_1bpp(w, h);
    let tiled = build_tiled(&s, w, h, 32, 16, 1, 1, None);
    let strip = build_strip(&s, w, h, 1, 1, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

#[test]
fn bilevel_1bpp_single_oversized_tile() {
    // ImageWidth < TileWidth (§15 explicitly permits this): one tile
    // covering the whole image with heavy padding.
    let (w, h) = (10usize, 12usize);
    let s = samples_1bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 16, 1, 1, None);
    let strip = build_strip(&s, w, h, 1, 1, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

// ---------------------------------------------------------------------
// 4-bit grayscale
// ---------------------------------------------------------------------

#[test]
fn gray_4bpp_exact_fit() {
    let (w, h) = (32usize, 32usize);
    let s = samples_4bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 16, 4, 1, None);
    let strip = build_strip(&s, w, h, 4, 1, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

#[test]
fn gray_4bpp_partial_edges() {
    let (w, h) = (44usize, 30usize);
    let s = samples_4bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 16, 4, 1, None);
    let strip = build_strip(&s, w, h, 4, 1, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

#[test]
fn gray_4bpp_odd_width() {
    // 4-bit, odd width => the image row has a trailing nibble of
    // padding (ceil(w/2) bytes).
    let (w, h) = (33usize, 18usize);
    let s = samples_4bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 16, 4, 1, None);
    let strip = build_strip(&s, w, h, 4, 1, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

#[test]
fn gray_4bpp_whiteiszero() {
    let (w, h) = (40usize, 40usize);
    let s = samples_4bpp(w, h);
    let tiled = build_tiled(&s, w, h, 16, 32, 4, 0, None);
    let strip = build_strip(&s, w, h, 4, 0, None);
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Gray8);
}

// ---------------------------------------------------------------------
// 4-bit palette
// ---------------------------------------------------------------------

#[test]
fn palette_4bpp_exact_fit() {
    let (w, h) = (32usize, 32usize);
    // Indices must be in 0..16 for a 4-bit palette.
    let s: Vec<u8> = samples_4bpp(w, h);
    let cm = palette_16();
    let tiled = build_tiled(&s, w, h, 16, 16, 4, 3, Some(&cm)); // photometric 3 = Palette
    let strip = build_strip(&s, w, h, 4, 3, Some(&cm));
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Rgb24);
}

#[test]
fn palette_4bpp_partial_edges() {
    let (w, h) = (46usize, 34usize);
    let s: Vec<u8> = samples_4bpp(w, h);
    let cm = palette_16();
    let tiled = build_tiled(&s, w, h, 16, 16, 4, 3, Some(&cm));
    let strip = build_strip(&s, w, h, 4, 3, Some(&cm));
    assert_tiled_matches_strip(&tiled, &strip, w as u32, h as u32, TiffPixelFormat::Rgb24);
}
