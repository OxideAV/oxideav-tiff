//! Integration tests for BigTIFF write (TIFF 6.0 Adobe Pagemaker 6.0
//! BigTIFF design — 16-byte header with magic 43, 20-byte IFD entries
//! with u64 count + u64 value-or-offset, LONG8 (type 16) for offset
//! fields, 8-byte next-IFD slot, 8-byte inline-value threshold).
//!
//! These tests are deliberately decoder-agnostic to the extent
//! possible: every BigTIFF byte stream is then handed to our own
//! `decode_tiff` / `decode_tiff_all` (which already accept both
//! variants via `parse_header` / `parse_ifd`) and the decoded pixels
//! are compared against the original input.
//!
//! Additionally we make spot assertions on the on-disk byte layout
//! (header bytes, IFD entry width, field types of `StripOffsets` /
//! `StripByteCounts`) to be sure we aren't accidentally producing
//! classic TIFF bytes when `bigtiff = true` is requested.

use oxideav_tiff::{
    decode_tiff, decode_tiff_all, encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat,
    RgbColor, TiffCompression,
};

fn ramp_gray8(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x.wrapping_mul(11)).wrapping_add(y.wrapping_mul(7)) & 0xff) as u8);
        }
    }
    v
}

fn ramp_rgb24(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(13) & 0xff) as u8);
            v.push((y.wrapping_mul(17) & 0xff) as u8);
            v.push((x.wrapping_add(y).wrapping_mul(5) & 0xff) as u8);
        }
    }
    v
}

fn ramp_gray16(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 2) as usize);
    for y in 0..h {
        for x in 0..w {
            let s = (x.wrapping_mul(311)).wrapping_add(y.wrapping_mul(101)) as u16;
            v.extend_from_slice(&s.to_le_bytes());
        }
    }
    v
}

// ---- Header bytes ------------------------------------------------------

#[test]
fn bigtiff_header_is_16_bytes_with_magic_43_off_size_8_reserved_0() {
    let pixels = ramp_gray8(8, 8);
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Gray8 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    // II + 43 + off_size + reserved + first-IFD u64
    assert!(bytes.len() >= 16);
    assert_eq!(&bytes[0..2], b"II");
    assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 43);
    assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 8);
    assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0);
    let first_ifd = u64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    assert!(
        first_ifd >= 16,
        "first-IFD offset {first_ifd} overlaps header"
    );
}

#[test]
fn classic_header_unchanged_when_bigtiff_false() {
    let pixels = ramp_gray8(8, 8);
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Gray8 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    assert_eq!(&bytes[0..2], b"II");
    assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 42);
}

// ---- IFD geometry ------------------------------------------------------

/// Decode the BigTIFF IFD entries from `bytes`, returning the list of
/// `(tag, field_type, count, value_or_offset_u64)` tuples and the
/// next-IFD offset. Walks the on-disk bytes directly so we can assert
/// on field types and counts independent of our decoder.
fn parse_bigtiff_ifd0(bytes: &[u8]) -> (Vec<(u16, u16, u64, u64)>, u64) {
    let first_ifd = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
    let count = u64::from_le_bytes(bytes[first_ifd..first_ifd + 8].try_into().unwrap()) as usize;
    let mut entries = Vec::with_capacity(count);
    let entries_start = first_ifd + 8;
    for i in 0..count {
        let base = entries_start + i * 20;
        let tag = u16::from_le_bytes(bytes[base..base + 2].try_into().unwrap());
        let ft = u16::from_le_bytes(bytes[base + 2..base + 4].try_into().unwrap());
        let cnt = u64::from_le_bytes(bytes[base + 4..base + 12].try_into().unwrap());
        let vo = u64::from_le_bytes(bytes[base + 12..base + 20].try_into().unwrap());
        entries.push((tag, ft, cnt, vo));
    }
    let next = u64::from_le_bytes(
        bytes[entries_start + count * 20..entries_start + count * 20 + 8]
            .try_into()
            .unwrap(),
    );
    (entries, next)
}

#[test]
fn bigtiff_strip_fields_use_long8_type_16() {
    // BigTIFF must store StripOffsets / StripByteCounts as LONG8 (type
    // 16, 8 bytes per value); classic TIFF would have used LONG (type
    // 4). Both fit inline in the 8-byte value/offset slot for a
    // single-strip chunky page.
    let pixels = ramp_gray8(8, 8);
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Gray8 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let (entries, next) = parse_bigtiff_ifd0(&bytes);
    assert_eq!(next, 0, "single-page chain ends with next-IFD=0");
    // Tags must be ascending.
    assert!(entries.windows(2).all(|w| w[0].0 <= w[1].0));
    // 273 StripOffsets — LONG8 (16), count 1, value is the strip offset.
    let so = entries.iter().find(|e| e.0 == 273).expect("StripOffsets");
    assert_eq!(so.1, 16, "StripOffsets must be LONG8 (type=16)");
    assert_eq!(so.2, 1);
    assert!(so.3 >= 16, "strip starts past the header");
    // 279 StripByteCounts — LONG8, count 1, value 64 (8x8 gray uncompressed).
    let sbc = entries
        .iter()
        .find(|e| e.0 == 279)
        .expect("StripByteCounts");
    assert_eq!(sbc.1, 16);
    assert_eq!(sbc.2, 1);
    assert_eq!(sbc.3, 64);
}

#[test]
fn bigtiff_bitspersample_rgb_inlines_into_8byte_slot() {
    // Classic TIFF spills BitsPerSample[3] (6 bytes) out-of-line because
    // the value/offset slot is only 4 bytes wide. BigTIFF's 8-byte slot
    // is wide enough to hold all three SHORTs inline, so the encoder
    // should NOT allocate an external blob for it.
    let pixels = ramp_rgb24(8, 8);
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let (entries, _next) = parse_bigtiff_ifd0(&bytes);
    let bps = entries.iter().find(|e| e.0 == 258).expect("BitsPerSample");
    assert_eq!(bps.1, 3, "BitsPerSample is SHORT");
    assert_eq!(bps.2, 3);
    // The three SHORT values 8, 8, 8 packed into the low 6 bytes of the
    // u64 value/offset slot, with the top 2 bytes zero.
    let v = bps.3.to_le_bytes();
    assert_eq!(u16::from_le_bytes([v[0], v[1]]), 8);
    assert_eq!(u16::from_le_bytes([v[2], v[3]]), 8);
    assert_eq!(u16::from_le_bytes([v[4], v[5]]), 8);
}

// ---- Round-trips through our own decoder ------------------------------

#[test]
fn bigtiff_gray8_roundtrip_uncompressed() {
    let pixels = ramp_gray8(40, 24);
    let page = EncodePage {
        width: 40,
        height: 24,
        kind: EncodePixelFormat::Gray8 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (40, 24));
    assert_eq!(d.frame.planes[0].data, pixels);
}

#[test]
fn bigtiff_gray8_roundtrip_lzw_with_predictor() {
    let pixels = ramp_gray8(33, 17);
    let page = EncodePage {
        width: 33,
        height: 17,
        kind: EncodePixelFormat::Gray8 { pixels: &pixels },
        compression: TiffCompression::Lzw,
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (33, 17));
    assert_eq!(d.frame.planes[0].data, pixels);
}

#[test]
fn bigtiff_gray16_roundtrip_deflate() {
    let pixels = ramp_gray16(20, 16);
    let page = EncodePage {
        width: 20,
        height: 16,
        kind: EncodePixelFormat::Gray16Le { pixels: &pixels },
        compression: TiffCompression::Deflate,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (20, 16));
    assert_eq!(d.frame.planes[0].data, pixels);
}

#[test]
fn bigtiff_rgb24_roundtrip_packbits() {
    let pixels = ramp_rgb24(24, 18);
    let page = EncodePage {
        width: 24,
        height: 18,
        kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
        compression: TiffCompression::PackBits,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (24, 18));
    assert_eq!(d.frame.planes[0].data, pixels);
}

#[test]
fn bigtiff_rgb24_planar_lzw_roundtrip() {
    // Planar layout puts 3 strips in the LONG8 array — exercises the
    // out-of-line LONG8 array path (single-strip pages keep the offset
    // inline in the 8-byte value slot).
    let pixels = ramp_rgb24(16, 16);
    let page = EncodePage {
        width: 16,
        height: 16,
        kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
        compression: TiffCompression::Lzw,
        predictor: false,
        planar: true,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (16, 16));
    assert_eq!(d.frame.planes[0].data, pixels);
    // Spot-check: the StripOffsets / StripByteCounts entries are LONG8
    // arrays of length 3, stored out-of-line (offsets point past the
    // strip payloads).
    let (entries, _) = parse_bigtiff_ifd0(&bytes);
    let so = entries.iter().find(|e| e.0 == 273).unwrap();
    assert_eq!(so.1, 16);
    assert_eq!(so.2, 3);
    let sbc = entries.iter().find(|e| e.0 == 279).unwrap();
    assert_eq!(sbc.1, 16);
    assert_eq!(sbc.2, 3);
}

#[test]
fn bigtiff_palette8_roundtrip() {
    let indices: Vec<u8> = (0..(12 * 8)).map(|i| (i % 7) as u8).collect();
    let palette: Vec<RgbColor> = (0..7)
        .map(|i: u32| {
            [
                (i * 36 + 5) as u8,
                (i * 73 + 11) as u8,
                (i * 109 + 17) as u8,
            ]
        })
        .collect();
    let page = EncodePage {
        width: 12,
        height: 8,
        kind: EncodePixelFormat::Palette8 {
            indices: &indices,
            palette: &palette,
        },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (12, 8));
    // Decoded palette pixels are RGB24 — verify a few colours match.
    let plane = &d.frame.planes[0].data;
    assert_eq!(plane.len(), (12 * 8 * 3) as usize);
    for (i, &idx) in indices.iter().enumerate() {
        let dr = plane[i * 3];
        let dg = plane[i * 3 + 1];
        let db = plane[i * 3 + 2];
        let exp = palette[idx as usize];
        assert_eq!((dr, dg, db), (exp[0], exp[1], exp[2]), "pixel {i}");
    }
}

#[test]
fn bigtiff_rgb24_tiled_roundtrip() {
    // Tiled layout in BigTIFF — single tile keeps TileOffsets /
    // TileByteCounts inline (LONG8 value fits in 8-byte slot).
    let pixels = ramp_rgb24(32, 32);
    let page = EncodePage {
        width: 32,
        height: 32,
        kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: Some((16, 16)),
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (32, 32));
    assert_eq!(d.frame.planes[0].data, pixels);
    let (entries, _) = parse_bigtiff_ifd0(&bytes);
    let to = entries.iter().find(|e| e.0 == 324).expect("TileOffsets");
    assert_eq!(to.1, 16, "TileOffsets must be LONG8 in BigTIFF");
    // 32x32 image over 16x16 tiles = 4 tiles in the grid.
    assert_eq!(to.2, 4);
}

// ---- Multi-page chain --------------------------------------------------

#[test]
fn bigtiff_multipage_chain() {
    let p1 = ramp_gray8(16, 12);
    let p2 = ramp_rgb24(8, 8);
    let pages = vec![
        EncodePage {
            width: 16,
            height: 12,
            kind: EncodePixelFormat::Gray8 { pixels: &p1 },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: true,
        },
        EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Rgb24 { pixels: &p2 },
            compression: TiffCompression::Deflate,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: true,
        },
    ];
    let bytes = encode_tiff_multi(&pages).unwrap();
    // Header is BigTIFF, first IFD chain valid.
    assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 43);
    let imgs = decode_tiff_all(&bytes).unwrap();
    assert_eq!(imgs.len(), 2);
    assert_eq!((imgs[0].width, imgs[0].height), (16, 12));
    assert_eq!(imgs[0].planes[0].data, p1);
    assert_eq!((imgs[1].width, imgs[1].height), (8, 8));
    assert_eq!(imgs[1].planes[0].data, p2);
}

#[test]
fn bigtiff_multipage_must_agree_on_variant() {
    // Mixing classic + BigTIFF pages is rejected — the on-disk IFD
    // layouts are incompatible, so the encoder can't produce a
    // coherent chain.
    let p1 = ramp_gray8(8, 8);
    let p2 = ramp_gray8(8, 8);
    let pages = vec![
        EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &p1 },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
        EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &p2 },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: true,
        },
    ];
    let err = encode_tiff_multi(&pages).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("bigtiff"), "error mentions bigtiff: {msg}");
}

// ---- CCITT bilevel composes with BigTIFF -------------------------------

#[test]
fn bigtiff_bilevel_ccitt_mh_roundtrip() {
    // 24x8 zebra pattern. CCITT MH compresses, then the LONG8
    // StripOffsets/StripByteCounts get written; round-trip through
    // our own decoder.
    let mut pixels = vec![0u8; (24usize.div_ceil(8)) * 8];
    for (row_idx, chunk) in pixels.chunks_exact_mut(3).enumerate() {
        if row_idx % 2 == 0 {
            chunk[0] = 0xFF;
            chunk[1] = 0xFF;
            chunk[2] = 0xFF;
        }
    }
    let page = EncodePage {
        width: 24,
        height: 8,
        kind: EncodePixelFormat::Bilevel { pixels: &pixels },
        compression: TiffCompression::CcittRle,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (24, 8));
    // The decoded plane is Gray8; alternating rows of 0xFF and 0x00
    // following the WhiteIsZero photometric the bilevel encoder writes.
    let plane = &d.frame.planes[0].data;
    assert_eq!(plane.len(), 24 * 8);
    for y in 0..8 {
        let want = if y % 2 == 0 { 0x00 } else { 0xFF };
        for x in 0..24 {
            assert_eq!(plane[y * 24 + x], want, "pixel ({x},{y})");
        }
    }
}
