//! `EncodePixelFormat::Gray4` / `Palette4` (4-bit sub-byte) encode →
//! decode parity tests.
//!
//! The decode side has read 4-bit grayscale and 4-bit palette pages
//! (strip and §15 tiled — see `tests/decode_tiled_subbyte.rs`) for
//! many rounds; these tests close the encode-parity gap, including
//! the previously-backlogged **4-bit tile writer** (nibble-granularity
//! §15 edge replication) and the §14 predictor at 4 bits (nibble
//! differencing modulo 16, the exact inverse of the decoder's
//! expand / cumulative-add / repack arm). Oracles: byte-exact decode
//! round-trips (every compressor here is lossless), tiled-vs-strip
//! decode equivalence on identical pixels, IFD tag inspection, and an
//! ImageMagick cross-read (4-bit gray scales to 8-bit as `n * 17`
//! exactly, so the independent reader must agree byte-for-byte).

use oxideav_tiff::types::*;
use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, RgbColor, TiffCompression,
    TiffPixelFormat,
};

/// Deterministic per-pixel nibble values (0..=15).
fn nibbles(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x * 7 + y * 3 + (x & y)) % 16) as u8);
        }
    }
    v
}

/// Pack per-pixel nibbles into the on-disk 4-bit raster: two pixels
/// per byte, high nibble first, each row padded to a byte boundary
/// (TIFF 6.0 §"Compression": "row-starts on byte boundaries").
fn pack4(nibs: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row_bytes = (w as usize).div_ceil(2);
    let mut out = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let v = nibs[y * w as usize + x] & 0x0F;
            let off = y * row_bytes + x / 2;
            if x & 1 == 0 {
                out[off] |= v << 4;
            } else {
                out[off] |= v;
            }
        }
    }
    out
}

/// The decoder's 4-bit grayscale display expansion: nibble n renders
/// as (n << 4) | n = n * 17.
fn expected_gray(nibs: &[u8]) -> Vec<u8> {
    nibs.iter().map(|&n| (n << 4) | n).collect()
}

fn gray_page<'a>(w: u32, h: u32, packed: &'a [u8], compression: TiffCompression) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Gray4 { pixels: packed },
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    }
}

fn decode_plane(tiff: &[u8], expect_pf: TiffPixelFormat, bytes_per_pixel: usize) -> Vec<u8> {
    let d = decode_tiff(tiff).expect("decode failed");
    assert_eq!(d.pixel_format, expect_pf);
    let stride = d.frame.planes[0].stride;
    let row_bytes = d.width as usize * bytes_per_pixel;
    let mut out = Vec::with_capacity(row_bytes * d.height as usize);
    for y in 0..d.height as usize {
        out.extend_from_slice(&d.frame.planes[0].data[y * stride..y * stride + row_bytes]);
    }
    out
}

const COMPRESSORS: [TiffCompression; 5] = [
    TiffCompression::None,
    TiffCompression::PackBits,
    TiffCompression::Lzw,
    TiffCompression::Deflate,
    TiffCompression::Zstd,
];

/// 4-bit grayscale strips across the compressor set × §14 predictor.
/// Odd width so the row padding nibble is exercised.
#[test]
fn gray4_strip_roundtrip() {
    let (w, h) = (21u32, 9u32);
    let nibs = nibbles(w, h);
    let packed = pack4(&nibs, w, h);
    let want = expected_gray(&nibs);
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            let p = EncodePage {
                predictor,
                ..gray_page(w, h, &packed, compression)
            };
            let tiff = encode_tiff(&p).expect("encode failed");
            let got = decode_plane(&tiff, TiffPixelFormat::Gray8, 1);
            assert_eq!(got, want, "gray4 {compression:?} predictor={predictor}");
        }
    }
}

/// 4-bit grayscale §15 tiles (the previously-backlogged 4-bit tile
/// writer): partial right/bottom edges, odd width, predictor on/off.
/// The strip encode of the same pixels is the independent oracle.
#[test]
fn gray4_tiled_matches_strip() {
    let (w, h) = (41u32, 25u32); // 16x16 tiles → 3x2 grid, 9- and 9-pixel overhangs
    let nibs = nibbles(w, h);
    let packed = pack4(&nibs, w, h);
    let want = expected_gray(&nibs);
    for compression in [
        TiffCompression::None,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
        TiffCompression::Zstd,
    ] {
        for predictor in [false, true] {
            let tiled = EncodePage {
                tiling: Some((16, 16)),
                predictor,
                ..gray_page(w, h, &packed, compression)
            };
            let strip = EncodePage {
                predictor,
                ..gray_page(w, h, &packed, compression)
            };
            let t = decode_plane(
                &encode_tiff(&tiled).expect("tiled encode"),
                TiffPixelFormat::Gray8,
                1,
            );
            let s = decode_plane(
                &encode_tiff(&strip).expect("strip encode"),
                TiffPixelFormat::Gray8,
                1,
            );
            assert_eq!(
                t, s,
                "tiled != strip: {compression:?} predictor={predictor}"
            );
            assert_eq!(t, want, "tiled != expected: {compression:?}");
        }
    }
}

/// Oversized single tile (tile larger than the whole image) — all
/// padding comes from §15 edge replication.
#[test]
fn gray4_oversized_single_tile() {
    let (w, h) = (10u32, 6u32);
    let nibs = nibbles(w, h);
    let packed = pack4(&nibs, w, h);
    let p = EncodePage {
        tiling: Some((16, 16)),
        ..gray_page(w, h, &packed, TiffCompression::PackBits)
    };
    let tiff = encode_tiff(&p).expect("encode failed");
    assert_eq!(
        decode_plane(&tiff, TiffPixelFormat::Gray8, 1),
        expected_gray(&nibs)
    );
}

/// BigTIFF composes with the 4-bit path unchanged.
#[test]
fn gray4_bigtiff_roundtrip() {
    let (w, h) = (21u32, 9u32);
    let nibs = nibbles(w, h);
    let packed = pack4(&nibs, w, h);
    let p = EncodePage {
        bigtiff: true,
        predictor: true,
        ..gray_page(w, h, &packed, TiffCompression::Deflate)
    };
    let tiff = encode_tiff(&p).expect("encode failed");
    assert_eq!(
        decode_plane(&tiff, TiffPixelFormat::Gray8, 1),
        expected_gray(&nibs)
    );
}

/// 4-bit palette: strip and tiled layouts render through the 48-SHORT
/// ColorMap back to the palette's Rgb24 colors.
#[test]
fn palette4_roundtrip() {
    let (w, h) = (21u32, 9u32);
    // 16 maximally-distinct palette entries.
    let palette: Vec<RgbColor> = (0..16u16)
        .map(|i| [(i * 17) as u8, (255 - i * 13) as u8, ((i * 47) % 256) as u8])
        .collect();
    let nibs = nibbles(w, h);
    let packed = pack4(&nibs, w, h);
    let want: Vec<u8> = nibs.iter().flat_map(|&n| palette[n as usize]).collect();
    for compression in COMPRESSORS {
        for tiling in [None, Some((16u32, 16u32))] {
            let p = EncodePage {
                width: w,
                height: h,
                kind: EncodePixelFormat::Palette4 {
                    indices: &packed,
                    palette: &palette,
                },
                compression,
                predictor: false,
                planar: false,
                tiling,
                bigtiff: false,
            };
            let tiff = encode_tiff(&p).expect("encode failed");
            let got = decode_plane(&tiff, TiffPixelFormat::Rgb24, 3);
            assert_eq!(got, want, "palette4 {compression:?} tiling={tiling:?}");
        }
    }
}

/// The written IFD carries the 4-bit field set: BitsPerSample = 4 and,
/// for palette pages, the §"Palette Color Images" 3 × 2^4 = 48-SHORT
/// ColorMap.
#[test]
fn subbyte4_ifd_tags() {
    use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
    let (w, h) = (16u32, 8u32);
    let nibs = nibbles(w, h);
    let packed = pack4(&nibs, w, h);
    let palette: Vec<RgbColor> = (0..4u8).map(|i| [i * 60, i * 40, i * 20]).collect();
    let p = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Palette4 {
            indices: &packed,
            palette: &palette,
        },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let tiff = encode_tiff(&p).expect("encode failed");
    let hd = parse_header(&tiff).unwrap();
    let (entries, _) = parse_ifd(&tiff, hd.byte_order, hd.variant, hd.first_ifd_offset).unwrap();
    let bps = find(&entries, TAG_BITS_PER_SAMPLE)
        .unwrap()
        .as_u32_vec(hd.byte_order)
        .unwrap();
    assert_eq!(bps, vec![4]);
    let photo = find(&entries, TAG_PHOTOMETRIC_INTERPRETATION)
        .unwrap()
        .as_u32(hd.byte_order)
        .unwrap();
    assert_eq!(photo, PHOTO_PALETTE as u32);
    let cm = find(&entries, TAG_COLOR_MAP).unwrap();
    assert_eq!(cm.count, 48, "ColorMap must be 3 * 2^4 SHORTs");
}

/// Negative paths: wrong buffer size, oversized palette, CCITT
/// (1-bit facsimile coding), and planar (irrelevant at SPP = 1).
#[test]
fn subbyte4_rejections() {
    let nibs = nibbles(8, 4);
    let packed = pack4(&nibs, 8, 4);
    // Wrong buffer size.
    let e = encode_tiff(&gray_page(16, 16, &packed, TiffCompression::None)).unwrap_err();
    assert!(format!("{e:?}").contains("Gray4"), "{e:?}");
    // Palette with 17 entries.
    let palette: Vec<RgbColor> = (0..17u8).map(|i| [i, i, i]).collect();
    let p = EncodePage {
        width: 8,
        height: 4,
        kind: EncodePixelFormat::Palette4 {
            indices: &packed,
            palette: &palette,
        },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let e = encode_tiff(&p).unwrap_err();
    assert!(format!("{e:?}").contains("1..=16"), "{e:?}");
    // CCITT requires Bilevel / TransparencyMask input.
    let e = encode_tiff(&gray_page(8, 4, &packed, TiffCompression::CcittRle)).unwrap_err();
    assert!(format!("{e:?}").contains("Bilevel"), "{e:?}");
    // Planar is irrelevant at SamplesPerPixel = 1.
    let p = EncodePage {
        planar: true,
        ..gray_page(8, 4, &packed, TiffCompression::None)
    };
    let e = encode_tiff(&p).unwrap_err();
    assert!(format!("{e:?}").contains("PlanarConfiguration"), "{e:?}");
}

/// Independent-reader cross-check: ImageMagick transcodes our tiled
/// LZW+predictor Gray4 output to an 8-bit binary PGM. A 4-bit sample
/// n scales to 8 bits as n * 255 / 15 = n * 17 — exactly the
/// decoder's (n << 4) | n expansion — so the reader must agree
/// byte-for-byte. Skips when `convert` is unavailable.
#[test]
fn gray4_imagemagick_reads_our_output() {
    use std::process::{Command, Stdio};
    if !Command::new("convert")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let (w, h) = (41u32, 25u32);
    let nibs = nibbles(w, h);
    let packed = pack4(&nibs, w, h);
    let p = EncodePage {
        tiling: Some((16, 16)),
        predictor: true,
        ..gray_page(w, h, &packed, TiffCompression::Lzw)
    };
    let tiff = encode_tiff(&p).expect("encode failed");

    let dir = std::env::temp_dir().join(format!("oxideav-tiff-gray4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let in_path = dir.join("in.tiff");
    let out_path = dir.join("out.pgm");
    std::fs::write(&in_path, &tiff).unwrap();
    let status = Command::new("convert")
        .arg(&in_path)
        .args(["-depth", "8"])
        .arg(&out_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let ok = matches!(status, Ok(s) if s.success());
    if !ok {
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("skipping: convert could not transcode the TIFF");
        return;
    }
    let pgm = std::fs::read(&out_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Parse "P5\n<w> <h>\n255\n" + binary bytes.
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while fields.len() < 4 && pos < pgm.len() {
        while pos < pgm.len() && pgm[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos < pgm.len() && pgm[pos] == b'#' {
            while pos < pgm.len() && pgm[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        let start = pos;
        while pos < pgm.len() && !pgm[pos].is_ascii_whitespace() {
            pos += 1;
        }
        fields.push(std::str::from_utf8(&pgm[start..pos]).unwrap().to_string());
    }
    pos += 1;
    assert_eq!(fields[0], "P5");
    assert_eq!(fields[1], w.to_string());
    assert_eq!(fields[2], h.to_string());
    assert_eq!(fields[3], "255");
    let data = &pgm[pos..];
    assert_eq!(data.len(), (w * h) as usize);
    assert_eq!(
        data,
        expected_gray(&nibs).as_slice(),
        "independent reader disagrees"
    );
}
