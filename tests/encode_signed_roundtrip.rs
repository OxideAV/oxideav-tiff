//! `EncodePixelFormat::GrayI8` / `GrayI16` (SampleFormat = 2 signed
//! two's-complement grayscale) encode → decode parity tests.
//!
//! The decode side has rendered 8-/16-bit signed grayscale through the
//! TIFF 6.0 §SampleFormat order-preserving offset-binary display map
//! (stored signed minimum → display 0, signed maximum → the unsigned
//! ceiling; a sign-bit flip `XOR 0x80` / `XOR 0x8000`) since
//! `tests/decode_sample_format_signed.rs` landed with hand-built
//! fixtures — making the decoder a trusted, binary-independent oracle
//! for the new encode side. Every compressor here is lossless, so the
//! round-trip must reproduce the offset-binary image of the input
//! exactly.

use oxideav_tiff::types::*;
use oxideav_tiff::{
    decode_tiff, decode_tiff_all, encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat,
    PageExtras, TiffCompression, TiffPixelFormat,
};

/// Deterministic signed 8-bit raster spanning the full i8 range.
fn pixels_i8(w: u32, h: u32) -> Vec<i8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((((x * 37 + y * 91) % 256) as i32 - 128) as i8);
        }
    }
    v
}

/// Deterministic signed 16-bit raster spanning the full i16 range.
fn pixels_i16(w: u32, h: u32) -> Vec<i16> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((((x * 2749 + y * 5237) % 65536) as i32 - 32768) as i16);
        }
    }
    v
}

/// The decoder's offset-binary display map at 8 bits: `XOR 0x80`.
fn expected_i8(px: &[i8]) -> Vec<u8> {
    px.iter().map(|&s| (s as u8) ^ 0x80).collect()
}

/// The decoder's offset-binary display map at 16 bits: `XOR 0x8000`,
/// rendered onto the little-endian `Gray16Le` display plane.
fn expected_i16(px: &[i16]) -> Vec<u8> {
    px.iter()
        .flat_map(|&s| ((s as u16) ^ 0x8000).to_le_bytes())
        .collect()
}

fn page_i8<'a>(w: u32, h: u32, pixels: &'a [i8], compression: TiffCompression) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::GrayI8 { pixels },
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
    }
}

fn page_i16<'a>(w: u32, h: u32, pixels: &'a [i16], compression: TiffCompression) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::GrayI16 { pixels },
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
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

/// 8-bit signed strips across the compressor set × §14 predictor
/// (wrapping subtract on the stored two's-complement bytes).
#[test]
fn gray_i8_strip_roundtrip() {
    let (w, h) = (23u32, 11u32);
    let px = pixels_i8(w, h);
    let want = expected_i8(&px);
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            let p = EncodePage {
                predictor,
                ..page_i8(w, h, &px, compression)
            };
            let tiff = encode_tiff(&p).expect("encode failed");
            assert_eq!(
                decode_plane(&tiff, TiffPixelFormat::Gray8, 1),
                want,
                "i8 {compression:?} predictor={predictor}"
            );
        }
    }
}

/// 16-bit signed strips across the compressor set × §14 predictor
/// (16-bit wrapping subtract, sign-agnostic).
#[test]
fn gray_i16_strip_roundtrip() {
    let (w, h) = (23u32, 11u32);
    let px = pixels_i16(w, h);
    let want = expected_i16(&px);
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            let p = EncodePage {
                predictor,
                ..page_i16(w, h, &px, compression)
            };
            let tiff = encode_tiff(&p).expect("encode failed");
            assert_eq!(
                decode_plane(&tiff, TiffPixelFormat::Gray16Le, 2),
                want,
                "i16 {compression:?} predictor={predictor}"
            );
        }
    }
}

/// §15 tiling with partial edges composes for both signed widths.
#[test]
fn gray_signed_tiled_roundtrip() {
    let (w, h) = (40u32, 24u32);
    let px8 = pixels_i8(w, h);
    let px16 = pixels_i16(w, h);
    for predictor in [false, true] {
        let p = EncodePage {
            tiling: Some((16, 16)),
            predictor,
            ..page_i8(w, h, &px8, TiffCompression::Lzw)
        };
        let tiff = encode_tiff(&p).expect("i8 tiled encode failed");
        assert_eq!(
            decode_plane(&tiff, TiffPixelFormat::Gray8, 1),
            expected_i8(&px8),
            "i8 tiled predictor={predictor}"
        );
        let p = EncodePage {
            tiling: Some((16, 16)),
            predictor,
            ..page_i16(w, h, &px16, TiffCompression::Deflate)
        };
        let tiff = encode_tiff(&p).expect("i16 tiled encode failed");
        assert_eq!(
            decode_plane(&tiff, TiffPixelFormat::Gray16Le, 2),
            expected_i16(&px16),
            "i16 tiled predictor={predictor}"
        );
    }
}

/// BigTIFF composes unchanged.
#[test]
fn gray_signed_bigtiff_roundtrip() {
    let (w, h) = (23u32, 11u32);
    let px = pixels_i16(w, h);
    let p = EncodePage {
        bigtiff: true,
        extras: PageExtras::default(),
        predictor: true,
        ..page_i16(w, h, &px, TiffCompression::Zstd)
    };
    let tiff = encode_tiff(&p).expect("encode failed");
    assert_eq!(
        decode_plane(&tiff, TiffPixelFormat::Gray16Le, 2),
        expected_i16(&px)
    );
}

/// Multi-page chain mixing a signed page with an unsigned one.
#[test]
fn gray_signed_multipage_chain() {
    let (w, h) = (16u32, 8u32);
    let px = pixels_i8(w, h);
    let unsigned: Vec<u8> = (0..w * h).map(|i| (i % 256) as u8).collect();
    let pages = [
        page_i8(w, h, &px, TiffCompression::Lzw),
        EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::Gray8 { pixels: &unsigned },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
            extras: PageExtras::default(),
        },
    ];
    let tiff = encode_tiff_multi(&pages).expect("multi-page encode failed");
    let decoded = decode_tiff_all(&tiff).expect("multi-page decode failed");
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].planes[0].data, expected_i8(&px));
    assert_eq!(decoded[1].planes[0].data, unsigned);
}

/// The written IFD carries `SampleFormat = 2` (inline single SHORT)
/// and the raw strip stores the two's-complement bytes verbatim.
#[test]
fn gray_signed_ifd_tags_and_raw_bytes() {
    use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
    let (w, h) = (8u32, 4u32);
    let px = pixels_i8(w, h);
    let tiff = encode_tiff(&page_i8(w, h, &px, TiffCompression::None)).expect("encode failed");
    let hd = parse_header(&tiff).unwrap();
    let (entries, _) = parse_ifd(&tiff, hd.byte_order, hd.variant, hd.first_ifd_offset).unwrap();
    let sf = find(&entries, TAG_SAMPLE_FORMAT)
        .expect("SampleFormat present")
        .as_u32_vec(hd.byte_order)
        .unwrap();
    assert_eq!(sf, vec![SAMPLE_FORMAT_SINT as u32]);
    // Raw strip bytes are the two's-complement samples, unmapped.
    let off = find(&entries, TAG_STRIP_OFFSETS)
        .unwrap()
        .as_u32(hd.byte_order)
        .unwrap() as usize;
    let len = find(&entries, TAG_STRIP_BYTE_COUNTS)
        .unwrap()
        .as_u32(hd.byte_order)
        .unwrap() as usize;
    let raw: Vec<u8> = px.iter().map(|&s| s as u8).collect();
    assert_eq!(&tiff[off..off + len], raw.as_slice());
}

/// Negative paths: wrong buffer size, CCITT, planar.
#[test]
fn gray_signed_rejections() {
    let px = pixels_i8(8, 4);
    let e = encode_tiff(&page_i8(16, 16, &px, TiffCompression::None)).unwrap_err();
    assert!(format!("{e:?}").contains("GrayI8"), "{e:?}");
    let e = encode_tiff(&page_i8(8, 4, &px, TiffCompression::CcittRle)).unwrap_err();
    assert!(format!("{e:?}").contains("Bilevel"), "{e:?}");
    let p = EncodePage {
        planar: true,
        ..page_i8(8, 4, &px, TiffCompression::None)
    };
    let e = encode_tiff(&p).unwrap_err();
    assert!(format!("{e:?}").contains("PlanarConfiguration"), "{e:?}");
}
