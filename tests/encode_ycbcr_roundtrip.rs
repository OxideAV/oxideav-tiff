//! Integration tests for `EncodePixelFormat::YCbCr24`
//! (TIFF 6.0 §21 "YCbCr Images", `PhotometricInterpretation = 6`,
//! `SamplesPerPixel = 3`, `BitsPerSample = [8, 8, 8]`, chunky 4:4:4
//! `YCbCrSubSampling = [1, 1]`) — binary-independent self-roundtrip
//! suite mirroring `encode_cielab_roundtrip.rs` on the encode side.
//!
//! The encoder writes `(Y, Cb, Cr)` bytes verbatim at the §21
//! `PlanarConfiguration = 1` / `YCbCrSubSampling = [1, 1]` data-unit
//! collapse (one Y, one Cb, one Cr per pixel), so the test contract
//! is:
//!
//!   1. The encoder must produce a classic-II TIFF the decoder can
//!      parse back to `Rgb24`.
//!   2. The decoder's output from the encoded file must match a
//!      hand-built classic-TIFF fixture carrying the same
//!      `(Y, Cb, Cr)` bytes and matching tag content — otherwise the
//!      encoder is putting the wrong tag, sample count, or bit depth
//!      on disk.
//!   3. The IFD bytes must carry `PhotometricInterpretation = 6`
//!      (tag 262), `SamplesPerPixel = 3` (tag 277),
//!      `YCbCrSubSampling = [1, 1]` (tag 530),
//!      `YCbCrPositioning = 1` (tag 531), and the §20-page-87
//!      no-headroom `ReferenceBlackWhite = [0/1, 255/1, 128/1, 255/1,
//!      128/1, 255/1]` (tag 532) — inspected via a byte-level IFD
//!      walker independent of our decoder.
//!   4. None / PackBits / LZW / Deflate compression all round-trip to
//!      the same decoded `Rgb24` (lossless byte-aligned compressors).
//!   5. Tiled 4:4:4 layout round-trips (the §21 data unit collapses to
//!      a plain chunky triple, so the generic §15 tile packer applies);
//!      planar / predictor / CCITT and the non-1:1 subsampled tiled
//!      writer remain rejected with a precise error.

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, TiffCompression, TiffPixelFormat,
};

/// Build a hand-rolled classic-II TIFF for a 3-sample 8-bit chunky
/// YCbCr image at the §21 `YCbCrSubSampling = [1, 1]` data-unit
/// collapse — i.e. the strip carries one `(Y, Cb, Cr)` triple per
/// pixel exactly as the caller supplies them. The fixture additionally
/// emits the three §21-required fields (530 / 531 / 532) so the
/// decoder uses the same coding range our encoder writes.
fn build_classic_ycbcr_tiff(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let row_bytes = (width as u64) * 3;
    let strip_bytes = row_bytes * (height as u64);
    assert_eq!(pixels.len() as u64, strip_bytes);

    // 11 entries: 254, 256, 257, 258, 259, 262, 273, 277, 279, 284,
    // 530, 531, 532. Actually 13 — count below.
    let num_entries: u16 = 13;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset: u32 = ifd_offset + ifd_size;

    // Out-of-line blob layout: BitsPerSample (3 × SHORT = 6 bytes),
    // then ReferenceBlackWhite (6 × RATIONAL = 48 bytes, 4-byte
    // aligned), then the strip pixels.
    let bps_off = blobs_offset;
    let mut cursor = bps_off + 6;
    if cursor % 4 != 0 {
        cursor += 4 - (cursor % 4);
    }
    let rbw_off = cursor;
    let pixels_off = rbw_off + 48;

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&ifd_offset.to_le_bytes());
    buf.extend_from_slice(&num_entries.to_le_bytes());

    let push = |buf: &mut Vec<u8>, tag: u16, ft: u16, count: u32, val: [u8; 4]| {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&ft.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&val);
    };

    // 254 NewSubfileType — LONG inline, 0.
    push(&mut buf, 254, 4, 1, 0u32.to_le_bytes());
    // 256 ImageWidth — LONG inline.
    push(&mut buf, 256, 4, 1, width.to_le_bytes());
    // 257 ImageLength — LONG inline.
    push(&mut buf, 257, 4, 1, height.to_le_bytes());
    // 258 BitsPerSample — SHORT × 3 out-of-line (6 > 4 inline slot).
    push(&mut buf, 258, 3, 3, bps_off.to_le_bytes());
    // 259 Compression — SHORT inline, 1 (None).
    let mut comp = [0u8; 4];
    comp[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 259, 3, 1, comp);
    // 262 PhotometricInterpretation — SHORT inline, 6 (YCbCr).
    let mut ph = [0u8; 4];
    ph[..2].copy_from_slice(&6u16.to_le_bytes());
    push(&mut buf, 262, 3, 1, ph);
    // 273 StripOffsets — LONG inline (1 strip).
    push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes());
    // 277 SamplesPerPixel — SHORT inline.
    let mut spp = [0u8; 4];
    spp[..2].copy_from_slice(&3u16.to_le_bytes());
    push(&mut buf, 277, 3, 1, spp);
    // 279 StripByteCounts — LONG inline.
    push(&mut buf, 279, 4, 1, (strip_bytes as u32).to_le_bytes());
    // 284 PlanarConfiguration — SHORT inline, 1 (chunky).
    let mut pc = [0u8; 4];
    pc[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 284, 3, 1, pc);
    // 530 YCbCrSubSampling — 2 SHORTs inline, [1, 1].
    let mut ss = [0u8; 4];
    ss[..2].copy_from_slice(&1u16.to_le_bytes());
    ss[2..4].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 530, 3, 2, ss);
    // 531 YCbCrPositioning — SHORT inline, 1 (centered).
    let mut pos = [0u8; 4];
    pos[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 531, 3, 1, pos);
    // 532 ReferenceBlackWhite — RATIONAL × 6 out-of-line.
    push(&mut buf, 532, 5, 6, rbw_off.to_le_bytes());

    // next-IFD pointer = 0
    buf.extend_from_slice(&0u32.to_le_bytes());

    // BitsPerSample blob (6 bytes)
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    // Pad to 4-byte alignment for ReferenceBlackWhite.
    while buf.len() < rbw_off as usize {
        buf.push(0);
    }
    // ReferenceBlackWhite (6 RATIONALs = 48 bytes) — full-range YCbCr
    // per §20 page 87 "no headroom/footroom".
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
    // Strip payload
    buf.extend_from_slice(pixels);
    buf
}

// ---------------------------------------------------------------------------
// 3-sample (Y, Cb, Cr) encode tests
// ---------------------------------------------------------------------------

#[test]
fn encoder_ycbcr24_neutral_gradient_matches_handbuilt() {
    // 4x1 neutral-grey ramp: Y varies 0/85/170/255, Cb/Cr fixed at 128
    // (the neutral chroma midpoint). The decoder's BT.601 matrix at
    // Cb = Cr = 128 leaves the chroma offsets at zero, so the output
    // RGB is a perfect Y gradient.
    let pixels: Vec<u8> = vec![0, 128, 128, 85, 128, 128, 170, 128, 128, 255, 128, 128];

    let handbuilt = build_classic_ycbcr_tiff(4, 1, &pixels);
    let want = decode_tiff(&handbuilt).unwrap().frame.planes[0]
        .data
        .clone();

    let page = EncodePage {
        width: 4,
        height: 1,
        kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (4, 1));
    assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
    assert_eq!(d.frame.planes[0].data, want);
}

#[test]
fn encoder_ycbcr24_chromatic_primaries_match_handbuilt() {
    // Single-pixel chromatic test cases bracketing the §21 BT.601
    // colour space. Y in mid-range, Cb / Cr swung from 16 to 240
    // (the customary CCIR 601 reference codes) plus the neutral
    // midpoint at 128.
    let cases: &[(u8, u8, u8, &str)] = &[
        (76, 84, 255, "red-ish"),   // R-like
        (149, 43, 21, "green-ish"), // G-like
        (29, 255, 107, "blue-ish"), // B-like
        (128, 128, 128, "neutral"),
    ];
    for (y, cb, cr, label) in cases {
        let pixels = vec![*y, *cb, *cr];
        let handbuilt = build_classic_ycbcr_tiff(1, 1, &pixels);
        let want = decode_tiff(&handbuilt).unwrap().frame.planes[0]
            .data
            .clone();

        let page = EncodePage {
            width: 1,
            height: 1,
            kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!(d.frame.planes[0].data, want, "case {label}");
    }
}

#[test]
fn encoder_ycbcr24_compressors_lossless() {
    // PackBits / LZW / Deflate must round-trip to the same Rgb24 as
    // the uncompressed encode of the same source — they are lossless
    // byte-aligned compressors, photometric-agnostic.
    let pixels: Vec<u8> = (0..(16 * 8))
        .flat_map(|i| {
            let y = ((i % 16) * 16) as u8;
            let cb = 128u8.wrapping_add(((i / 16) * 12) as u8);
            let cr = 128u8.wrapping_sub(((i % 8) * 9) as u8);
            [y, cb, cr]
        })
        .collect();

    let baseline = {
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        decode_tiff(&encode_tiff(&page).unwrap())
            .unwrap()
            .frame
            .planes[0]
            .data
            .clone()
    };

    for c in [
        TiffCompression::PackBits,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
    ] {
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
            compression: c,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "compressor {:?}", c);
    }
}

#[test]
fn encoder_ycbcr24_rejects_planar_predictor_ccitt() {
    // These flags remain deferred — the §14 chroma-difference predictor
    // and the §21 separate-plane layout both change the on-disk shape;
    // CCITT is bilevel-only. (Tiled 4:4:4 is now supported — see the
    // dedicated round-trip test below.)
    let pixels = vec![128u8; 4 * 4 * 3];

    // Predictor = true.
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
        compression: TiffCompression::Lzw,
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err(), "predictor must reject");

    // Planar = true.
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
        compression: TiffCompression::Lzw,
        predictor: false,
        planar: true,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err(), "planar must reject");

    // CCITT (bilevel-only).
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
        compression: TiffCompression::CcittRle,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err(), "CCITT must reject");
}

/// A non-neutral `(Y, Cb, Cr)` raster (varying luma + chroma) so a tile
/// reassembly bug would surface as a colour divergence.
fn ycbcr_ramp(w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let p = (y * w + x) * 3;
            out[p] = ((x * 7 + y * 11) % 256) as u8; // Y
            out[p + 1] = (64 + (x * 3) % 128) as u8; // Cb
            out[p + 2] = (64 + (y * 5) % 128) as u8; // Cr
        }
    }
    out
}

#[test]
fn encoder_ycbcr24_tiled_444_roundtrips_against_strip() {
    // At YCbCrSubSampling = [1, 1] the §21 data unit collapses to a plain
    // chunky (Y, Cb, Cr) triple, so the generic §15 tile packer handles
    // it exactly as it does Rgb24. Encode the same pixels strip-based and
    // tiled and assert both decode to the identical Rgb24 plane across the
    // byte-aligned compressors and a spread of tile geometries.
    let cases = [
        (16u32, 16u32, (16u32, 16u32)), // single exact-fit tile
        (32, 32, (16, 16)),             // 2×2 grid
        (48, 32, (16, 16)),             // 3×2 grid, exact fit
        (40, 24, (16, 16)),             // partial right + bottom edges
        (48, 40, (32, 16)),             // non-square tiles, partial edge
    ];
    let compressors = [
        TiffCompression::None,
        TiffCompression::PackBits,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
    ];
    for (w, h, tile) in cases {
        let pixels = ycbcr_ramp(w as usize, h as usize);
        for comp in compressors {
            let strip = EncodePage {
                width: w,
                height: h,
                kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
                compression: comp,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            let tiled = EncodePage {
                width: w,
                height: h,
                kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
                compression: comp,
                predictor: false,
                planar: false,
                tiling: Some(tile),
                bigtiff: false,
            };
            let ds = decode_tiff(&encode_tiff(&strip).unwrap()).unwrap();
            let dt = decode_tiff(&encode_tiff(&tiled).unwrap()).unwrap();
            assert_eq!((dt.width, dt.height), (w, h));
            assert_eq!(dt.pixel_format, TiffPixelFormat::Rgb24);
            assert_eq!(
                dt.frame.planes[0].data, ds.frame.planes[0].data,
                "tiled 4:4:4 YCbCr diverged from strip for {w}x{h} tile {tile:?} comp {comp:?}"
            );
        }
    }
}

#[test]
fn encoder_ycbcr_subsampled_tiled_rejects_non_multiple_tile() {
    // Tiled subsampled YCbCr is now supported (round-trips are covered in
    // encode_ycbcr_subsampled_roundtrip.rs), but §21 page 90 requires the
    // tile dimensions to be integer multiples of the subsampling factors.
    // A TileLength (18) that is not a multiple of sv (2) must reject.
    let pixels = vec![128u8; 16 * 16 * 3];
    let page = EncodePage {
        width: 16,
        height: 16,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (2, 2),
        },
        compression: TiffCompression::Lzw,
        predictor: false,
        planar: false,
        tiling: Some((16, 18)),
        bigtiff: false,
    };
    assert!(
        encode_tiff(&page).is_err(),
        "non-multiple tile geometry must reject"
    );
}

#[test]
fn encoder_ycbcr24_rejects_size_mismatch() {
    // Pixel buffer size must match width * height * 3.
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::YCbCr24 {
            pixels: &[0u8; 47], // one byte short of 48
        },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err());
}

#[test]
fn encoder_ycbcr24_bigtiff_composes() {
    // BigTIFF write must produce a parseable YCbCr file. The only
    // differences are header / IFD geometry; pixel semantics are
    // identical, so the decoded Rgb24 matches the classic encode.
    let pixels: Vec<u8> = (0..(8 * 8))
        .flat_map(|i| {
            let y = (i * 4) as u8;
            let cb = 128u8;
            let cr = 128u8;
            [y, cb, cr]
        })
        .collect();

    let classic = {
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        decode_tiff(&encode_tiff(&page).unwrap())
            .unwrap()
            .frame
            .planes[0]
            .data
            .clone()
    };
    let big = {
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
            compression: TiffCompression::Deflate,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: true,
        };
        decode_tiff(&encode_tiff(&page).unwrap())
            .unwrap()
            .frame
            .planes[0]
            .data
            .clone()
    };
    assert_eq!(classic, big);
}

// ---------------------------------------------------------------------------
// IFD inspection (byte-level, independent of our decoder)
// ---------------------------------------------------------------------------

/// Walk the first IFD of a classic-II TIFF and find the value of a
/// SHORT entry by tag. Returns the first SHORT in the value slot.
fn read_ifd_entry_value_short(bytes: &[u8], tag: u16) -> Option<u16> {
    let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
    for k in 0..count {
        let off = ifd_off + 2 + k * 12;
        let entry_tag = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        if entry_tag == tag {
            return Some(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
        }
    }
    None
}

/// Walk the first IFD and return both SHORTs in a 2-element SHORT
/// inline value slot by tag.
fn read_ifd_entry_value_two_shorts(bytes: &[u8], tag: u16) -> Option<(u16, u16)> {
    let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
    for k in 0..count {
        let off = ifd_off + 2 + k * 12;
        let entry_tag = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        if entry_tag == tag {
            let a = u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]);
            let b = u16::from_le_bytes([bytes[off + 10], bytes[off + 11]]);
            return Some((a, b));
        }
    }
    None
}

/// Find an out-of-line RATIONAL × 6 blob by tag and decode it as six
/// (numerator, denominator) pairs.
fn read_ifd_rational6(bytes: &[u8], tag: u16) -> Option<[(u32, u32); 6]> {
    let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
    for k in 0..count {
        let off = ifd_off + 2 + k * 12;
        let entry_tag = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        if entry_tag == tag {
            let field_type = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]);
            let cnt = u32::from_le_bytes([
                bytes[off + 4],
                bytes[off + 5],
                bytes[off + 6],
                bytes[off + 7],
            ]);
            assert_eq!(field_type, 5, "tag {tag} must be RATIONAL (5)");
            assert_eq!(cnt, 6, "tag {tag} must have count 6");
            // RATIONAL is 8 bytes per value → 48 bytes total → out of line.
            let blob_off = u32::from_le_bytes([
                bytes[off + 8],
                bytes[off + 9],
                bytes[off + 10],
                bytes[off + 11],
            ]) as usize;
            let mut out = [(0u32, 0u32); 6];
            for (i, slot) in out.iter_mut().enumerate() {
                let base = blob_off + i * 8;
                let n = u32::from_le_bytes([
                    bytes[base],
                    bytes[base + 1],
                    bytes[base + 2],
                    bytes[base + 3],
                ]);
                let d = u32::from_le_bytes([
                    bytes[base + 4],
                    bytes[base + 5],
                    bytes[base + 6],
                    bytes[base + 7],
                ]);
                *slot = (n, d);
            }
            return Some(out);
        }
    }
    None
}

#[test]
fn encoder_ycbcr24_writes_photometric_samples_subsampling_positioning_rbw() {
    let pixels = vec![128u8, 128, 128];
    let page = EncodePage {
        width: 1,
        height: 1,
        kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();

    // 262 PhotometricInterpretation = 6 (YCbCr).
    assert_eq!(read_ifd_entry_value_short(&bytes, 262), Some(6));
    // 277 SamplesPerPixel = 3.
    assert_eq!(read_ifd_entry_value_short(&bytes, 277), Some(3));
    // 284 PlanarConfiguration = 1 (chunky).
    assert_eq!(read_ifd_entry_value_short(&bytes, 284), Some(1));
    // 530 YCbCrSubSampling = [1, 1].
    assert_eq!(read_ifd_entry_value_two_shorts(&bytes, 530), Some((1, 1)));
    // 531 YCbCrPositioning = 1 (centered).
    assert_eq!(read_ifd_entry_value_short(&bytes, 531), Some(1));
    // 532 ReferenceBlackWhite = [0/1, 255/1, 128/1, 255/1, 128/1, 255/1].
    assert_eq!(
        read_ifd_rational6(&bytes, 532),
        Some([(0, 1), (255, 1), (128, 1), (255, 1), (128, 1), (255, 1)])
    );
}

#[test]
fn encoder_ycbcr24_multi_page_chain() {
    // Multi-IFD chain mixing a YCbCr page with a Gray8 page must walk
    // cleanly via decode_tiff_all.
    use oxideav_tiff::encode_tiff_multi;
    let ycbcr = vec![100u8, 128, 128, 150u8, 130, 120];
    let gray = vec![10u8, 20, 30, 40];
    let pages = vec![
        EncodePage {
            width: 2,
            height: 1,
            kind: EncodePixelFormat::YCbCr24 { pixels: &ycbcr },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
        EncodePage {
            width: 4,
            height: 1,
            kind: EncodePixelFormat::Gray8 { pixels: &gray },
            compression: TiffCompression::PackBits,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
    ];
    let bytes = encode_tiff_multi(&pages).unwrap();
    let imgs = oxideav_tiff::decode_tiff_all(&bytes).unwrap();
    assert_eq!(imgs.len(), 2);
    assert_eq!(imgs[0].pixel_format, TiffPixelFormat::Rgb24);
    assert_eq!(imgs[1].pixel_format, TiffPixelFormat::Gray8);
    assert_eq!(imgs[1].planes[0].data, gray);
}
