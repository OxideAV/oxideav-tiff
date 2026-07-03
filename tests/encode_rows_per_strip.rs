//! Multi-strip *write* (`PageExtras::rows_per_strip`, TIFF 6.0
//! §"RowsPerStrip") — the decoder has read any strip count since the
//! first round; this exercises the write side across the pixel-format
//! × compressor × predictor × planar matrix.
//!
//! Oracles: the multi-strip encode must decode byte-identically to the
//! single-strip encode of the same pixels (the §14 predictors are
//! row-local, so only the compression restart points differ); the IFD
//! must carry `RowsPerStrip = r` with `ceil(height / r)` offsets /
//! byte-counts per plane (plane 0's strips first, §"StripOffsets");
//! and for uncompressed data every strip's byte count must equal its
//! row span exactly. `tiffinfo` (black-box) confirms the Rows/Strip
//! line in the validators file.

use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, PageExtras, TiffCompression,
};

fn page<'a>(
    kind: EncodePixelFormat<'a>,
    w: u32,
    h: u32,
    compression: TiffCompression,
    predictor: bool,
    planar: bool,
    rows_per_strip: Option<u32>,
) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind,
        compression,
        predictor,
        planar,
        tiling: None,
        bigtiff: false,
        extras: PageExtras {
            rows_per_strip,
            ..Default::default()
        },
    }
}

fn gray(w: u32, h: u32) -> Vec<u8> {
    (0..w * h).map(|i| (i * 3 + 11) as u8).collect()
}

fn rgb(w: u32, h: u32) -> Vec<u8> {
    (0..w * h * 3).map(|i| (i * 7 + 5) as u8).collect()
}

const COMPRESSORS: [TiffCompression; 5] = [
    TiffCompression::None,
    TiffCompression::PackBits,
    TiffCompression::Lzw,
    TiffCompression::Deflate,
    TiffCompression::Zstd,
];

/// Encode both single- and multi-strip variants and assert identical
/// decode output; return the multi-strip file for IFD inspection.
fn strip_equiv(single: &EncodePage<'_>, multi: &EncodePage<'_>) -> Vec<u8> {
    let f1 = encode_tiff(single).expect("single-strip encode");
    let fmulti = encode_tiff(multi).expect("multi-strip encode");
    let d1 = decode_tiff(&f1).expect("single-strip decode");
    let dn = decode_tiff(&fmulti).expect("multi-strip decode");
    assert_eq!(d1.frame.pixel_format, dn.frame.pixel_format);
    assert_eq!(
        d1.frame.planes[0].data, dn.frame.planes[0].data,
        "multi-strip decode must match single-strip decode"
    );
    fmulti
}

#[test]
fn gray8_multi_strip_matrix() {
    let (w, h) = (23u32, 13u32);
    let px = gray(w, h);
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            let kind = EncodePixelFormat::Gray8 { pixels: &px };
            let single = page(kind.clone(), w, h, compression, predictor, false, None);
            let multi = page(kind, w, h, compression, predictor, false, Some(5));
            let file = strip_equiv(&single, &multi);
            let hdr = parse_header(&file).unwrap();
            let (entries, _) =
                parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
            let bo = hdr.byte_order;
            assert_eq!(find(&entries, 278).unwrap().as_u32(bo).unwrap(), 5);
            let offs = find(&entries, 273).unwrap().as_u64_vec(bo).unwrap();
            let counts = find(&entries, 279).unwrap().as_u64_vec(bo).unwrap();
            assert_eq!(offs.len(), 3, "ceil(13 / 5) = 3 strips");
            assert_eq!(counts.len(), 3);
            if matches!(compression, TiffCompression::None) {
                assert_eq!(
                    counts,
                    vec![(w * 5) as u64, (w * 5) as u64, (w * 3) as u64],
                    "uncompressed strip byte counts = row spans"
                );
            }
        }
    }
}

#[test]
fn rgb24_planar_multi_strip_plane_major_order() {
    let (w, h) = (10u32, 9u32);
    let px = rgb(w, h);
    let kind = EncodePixelFormat::Rgb24 { pixels: &px };
    let single = page(kind.clone(), w, h, TiffCompression::None, false, true, None);
    let multi = page(kind, w, h, TiffCompression::None, false, true, Some(4));
    let file = strip_equiv(&single, &multi);
    let hdr = parse_header(&file).unwrap();
    let (entries, _) = parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let bo = hdr.byte_order;
    let offs = find(&entries, 273).unwrap().as_u64_vec(bo).unwrap();
    let counts = find(&entries, 279).unwrap().as_u64_vec(bo).unwrap();
    // 3 planes × ceil(9 / 4) = 9 strips, plane-major.
    assert_eq!(offs.len(), 9);
    let want: Vec<u64> = [40u64, 40, 10, 40, 40, 10, 40, 40, 10].to_vec();
    assert_eq!(counts, want, "per-plane 4/4/1-row uncompressed strips");
    // Plane payloads: strip 0 must be the R plane's first 4 rows.
    let r_plane_head: Vec<u8> = px.chunks_exact(3).map(|c| c[0]).take(40).collect();
    let s0 = offs[0] as usize;
    assert_eq!(&file[s0..s0 + 40], &r_plane_head[..]);
}

#[test]
fn planar_predictor_float_multi_strip() {
    // Float RGB planar + Predictor 3 + multi-strip: the float
    // predictor is row-local so per-strip compression must not change
    // the decode.
    let (w, h) = (9u32, 7u32);
    let px: Vec<f32> = (0..w * h * 3).map(|i| i as f32 * 0.35 - 20.0).collect();
    for compression in [TiffCompression::Lzw, TiffCompression::Deflate] {
        let kind = EncodePixelFormat::RgbF32 { pixels: &px };
        let single = page(kind.clone(), w, h, compression, true, true, None);
        let multi = page(kind, w, h, compression, true, true, Some(3));
        strip_equiv(&single, &multi);
    }
}

#[test]
fn f16_gray_multi_strip() {
    use oxideav_tiff::f32_to_f16_bits;
    let (w, h) = (12u32, 11u32);
    let bits: Vec<u16> = (0..w * h)
        .map(|i| f32_to_f16_bits(i as f32 * 0.5 - 8.0))
        .collect();
    let kind = EncodePixelFormat::GrayF16 { pixels: &bits };
    let single = page(kind.clone(), w, h, TiffCompression::Zstd, true, false, None);
    let multi = page(kind, w, h, TiffCompression::Zstd, true, false, Some(4));
    strip_equiv(&single, &multi);
}

#[test]
fn bilevel_ccitt_multi_strip() {
    // CCITT coders restart per strip; the per-strip decoder passes
    // rows_this_strip into the fax decoder, so a 4-row strip split
    // must decode identically to the single-strip file.
    let (w, h) = (40u32, 11u32);
    let row_bytes = (w as usize).div_ceil(8);
    let mut px = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        for xb in 0..row_bytes {
            px[y * row_bytes + xb] = if (y / 2 + xb) % 2 == 0 { 0xF0 } else { 0x0F };
        }
    }
    for compression in [
        TiffCompression::CcittRle,
        TiffCompression::CcittT4OneD {
            eol_byte_aligned: false,
        },
        TiffCompression::CcittT4TwoD {
            eol_byte_aligned: false,
        },
        TiffCompression::CcittT6,
        TiffCompression::PackBits,
    ] {
        let kind = EncodePixelFormat::Bilevel { pixels: &px };
        let single = page(kind.clone(), w, h, compression, false, false, None);
        let multi = page(kind, w, h, compression, false, false, Some(4));
        strip_equiv(&single, &multi);
    }
}

#[test]
fn sub_byte_gray4_multi_strip() {
    let (w, h) = (9u32, 10u32);
    let row_bytes = (w as usize).div_ceil(2);
    let px: Vec<u8> = (0..row_bytes * h as usize)
        .map(|i| ((i * 5) % 251) as u8)
        .collect();
    for predictor in [false, true] {
        let kind = EncodePixelFormat::Gray4 { pixels: &px };
        let single = page(
            kind.clone(),
            w,
            h,
            TiffCompression::Lzw,
            predictor,
            false,
            None,
        );
        let multi = page(kind, w, h, TiffCompression::Lzw, predictor, false, Some(3));
        strip_equiv(&single, &multi);
    }
}

#[test]
fn ycbcr_subsampled_chunky_multi_strip() {
    // Chroma-subsampled chunky data-unit stream: one strip covers a
    // whole number of data-unit rows (rows_per_strip multiple of sv).
    let (w, h) = (16u32, 12u32);
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            px.push((x * 9 + y * 4) as u8);
            px.push(((x / 2) * 30) as u8);
            px.push(((y / 2) * 25 + 40) as u8);
        }
    }
    for (sh, sv) in [(2u16, 2u16), (2, 1), (4, 2)] {
        let kind = EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &px,
            subsampling: (sh, sv),
        };
        let single = page(
            kind.clone(),
            w,
            h,
            TiffCompression::Deflate,
            false,
            false,
            None,
        );
        let multi = page(
            kind.clone(),
            w,
            h,
            TiffCompression::Deflate,
            false,
            false,
            Some(4),
        );
        strip_equiv(&single, &multi);
        // Non-multiple of sv rejected (§21 page 90) — only when sv > 1.
        if sv > 1 {
            let bad = page(kind, w, h, TiffCompression::Deflate, false, false, Some(3));
            assert!(encode_tiff(&bad).is_err(), "rps=3 with sv={sv} must reject");
        }
    }
}

#[test]
fn ycbcr_subsampled_planar_multi_strip() {
    let (w, h) = (16u32, 8u32);
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            px.push((x * 3 + y * 17) as u8);
            px.push(((x / 2) * 22) as u8);
            px.push(((y / 2) * 33 + 10) as u8);
        }
    }
    let kind = EncodePixelFormat::YCbCrSubsampled24 {
        pixels: &px,
        subsampling: (2, 2),
    };
    for predictor in [false, true] {
        let single = page(
            kind.clone(),
            w,
            h,
            TiffCompression::Lzw,
            predictor,
            true,
            None,
        );
        let multi = page(
            kind.clone(),
            w,
            h,
            TiffCompression::Lzw,
            predictor,
            true,
            Some(4),
        );
        let file = strip_equiv(&single, &multi);
        let hdr = parse_header(&file).unwrap();
        let (entries, _) =
            parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
        let offs = find(&entries, 273)
            .unwrap()
            .as_u64_vec(hdr.byte_order)
            .unwrap();
        // 3 planes × ceil(8 / 4) = 6 strips (chroma strips carry
        // 4 / sv = 2 reduced rows each).
        assert_eq!(offs.len(), 6, "predictor={predictor}");
    }
}

#[test]
fn rows_per_strip_validation() {
    let px = gray(8, 8);
    let kind = EncodePixelFormat::Gray8 { pixels: &px };
    // Zero rejected.
    assert!(encode_tiff(&page(
        kind.clone(),
        8,
        8,
        TiffCompression::None,
        false,
        false,
        Some(0)
    ))
    .is_err());
    // Tiling + rows_per_strip rejected.
    let mut tiled = page(
        kind.clone(),
        8,
        8,
        TiffCompression::None,
        false,
        false,
        Some(4),
    );
    tiled.tiling = Some((16, 16));
    assert!(encode_tiff(&tiled).is_err());
    // rows_per_strip >= height behaves exactly like None (single strip).
    let a = encode_tiff(&page(
        kind.clone(),
        8,
        8,
        TiffCompression::None,
        false,
        false,
        Some(100),
    ))
    .unwrap();
    let b = encode_tiff(&page(kind, 8, 8, TiffCompression::None, false, false, None)).unwrap();
    assert_eq!(a, b, "clamped rows_per_strip is byte-identical to None");
}

#[test]
fn multi_strip_bigtiff_composes() {
    let (w, h) = (10u32, 10u32);
    let px = gray(w, h);
    let kind = EncodePixelFormat::Gray8 { pixels: &px };
    let mut single = page(
        kind.clone(),
        w,
        h,
        TiffCompression::Deflate,
        false,
        false,
        None,
    );
    single.bigtiff = true;
    let mut multi = page(kind, w, h, TiffCompression::Deflate, false, false, Some(3));
    multi.bigtiff = true;
    let file = strip_equiv(&single, &multi);
    let hdr = parse_header(&file).unwrap();
    let (entries, _) = parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let offs = find(&entries, 273)
        .unwrap()
        .as_u64_vec(hdr.byte_order)
        .unwrap();
    assert_eq!(offs.len(), 4, "ceil(10 / 3) = 4 LONG8 strip offsets");
}
