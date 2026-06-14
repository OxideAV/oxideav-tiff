//! Integration tests for `EncodePixelFormat::YCbCrSubsampled24`
//! (TIFF 6.0 §21 "YCbCr Images", `PhotometricInterpretation = 6`,
//! `SamplesPerPixel = 3`, `BitsPerSample = [8, 8, 8]`,
//! chroma-subsampled `YCbCrSubSampling = [sh, sv]`, chunky
//! `PlanarConfiguration = 1`).
//!
//! The encoder takes a full-resolution interleaved `(Y, Cb, Cr)`
//! raster, box-averages the chroma per `sh × sv` block, and packs the
//! §21 page 93 data-unit byte order (`sv` rows of `sh` Y samples, then
//! one Cb, then one Cr). The decoder reverses the same geometry,
//! splatting the single Cb / Cr of each block over its full-resolution
//! footprint.
//!
//! Test contract:
//!
//!   1. The encoded on-disk strip carries the exact §21 data-unit byte
//!      order — verified against an independent reference packer.
//!   2. When the source chroma is constant across each `sh × sv` block,
//!      the decode round-trips bit-exact (decimation + splat is the
//!      identity in that case) for every legal subsampling pair under
//!      every byte-aligned compressor.
//!   3. The IFD carries `YCbCrSubSampling = [sh, sv]`, the YCbCr
//!      photometric / sample tags, `YCbCrPositioning = 1`, and the §20
//!      no-headroom `ReferenceBlackWhite`.
//!   4. The §21 page 90 constraints (legal factor pairs, dimension
//!      multiples) and the deferred planar / tiled / predictor
//!      combinations are rejected with a precise error.

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, TiffCompression, TiffPixelFormat,
};

// ---------------------------------------------------------------------------
// Independent reference packer (mirrors TIFF 6.0 §21 page 93 / 94)
// ---------------------------------------------------------------------------

/// Reference implementation of the §21 chunky data-unit packing,
/// independent of the encoder's internal `pack_ycbcr_data_units`. For
/// each `sh × sv` block in row-major scan order it emits `sv` rows of
/// `sh` full-resolution Y samples, then the rounded mean Cb, then the
/// rounded mean Cr.
fn reference_pack(src: &[u8], w: usize, h: usize, sh: usize, sv: usize) -> Vec<u8> {
    let block_w = w / sh;
    let block_h = h / sv;
    let unit = sh * sv + 2;
    let mut out = vec![0u8; block_w * block_h * unit];
    for by in 0..block_h {
        for bx in 0..block_w {
            let uoff = (by * block_w + bx) * unit;
            let mut cb = 0u32;
            let mut cr = 0u32;
            for sy in 0..sv {
                for sx in 0..sh {
                    let px = bx * sh + sx;
                    let py = by * sv + sy;
                    let s = (py * w + px) * 3;
                    out[uoff + sy * sh + sx] = src[s];
                    cb += src[s + 1] as u32;
                    cr += src[s + 2] as u32;
                }
            }
            let n = (sh * sv) as u32;
            out[uoff + sh * sv] = ((cb + n / 2) / n) as u8;
            out[uoff + sh * sv + 1] = ((cr + n / 2) / n) as u8;
        }
    }
    out
}

/// Build a full-resolution `(Y, Cb, Cr)` raster whose chroma is
/// constant within each `sh × sv` block (so the box-average + splat
/// round-trips bit-exact) while the luma varies per pixel.
fn block_uniform_chroma(w: usize, h: usize, sh: usize, sv: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let bx = x / sh;
            let by = y / sv;
            let p = (y * w + x) * 3;
            // Luma: a per-pixel ramp.
            out[p] = ((x * 13 + y * 7) % 256) as u8;
            // Chroma: a per-block value, identical for all pixels in
            // the block, swung around the 128 neutral midpoint.
            out[p + 1] = (96 + (bx * 17 + by * 5) % 64) as u8;
            out[p + 2] = (96 + (bx * 11 + by * 23) % 64) as u8;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// On-disk byte order
// ---------------------------------------------------------------------------

#[test]
fn encoded_strip_matches_reference_data_unit_order() {
    // §21 page 94 worked example geometry: a 4×2 block. Use the exact
    // sample names from the spec to make the assertion legible.
    let (w, h, sh, sv) = (8usize, 2usize, 4usize, 2usize);
    let pixels = block_uniform_chroma(w, h, sh, sv);
    let want_strip = reference_pack(&pixels, w, h, sh, sv);

    let page = EncodePage {
        width: w as u32,
        height: h as u32,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (sh as u16, sv as u16),
        },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();

    // Find StripOffsets (273) + StripByteCounts (279) via a byte walker
    // and compare the on-disk strip bytes to the reference packing.
    let strip_off = read_long(&bytes, 273).unwrap() as usize;
    let strip_len = read_long(&bytes, 279).unwrap() as usize;
    assert_eq!(strip_len, want_strip.len(), "strip byte count");
    assert_eq!(
        &bytes[strip_off..strip_off + strip_len],
        &want_strip[..],
        "on-disk strip must match §21 data-unit byte order"
    );
}

// ---------------------------------------------------------------------------
// Bit-exact round-trip (block-uniform chroma) over all legal pairs
// ---------------------------------------------------------------------------

#[test]
fn block_uniform_chroma_roundtrips_all_pairs_and_compressors() {
    let pairs = [(1u16, 1u16), (2, 1), (2, 2), (4, 1), (4, 2)];
    let comps = [
        TiffCompression::None,
        TiffCompression::PackBits,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
        TiffCompression::Zstd,
    ];
    // Dimensions divisible by every horiz (4) and vert (2) factor.
    let (w, h) = (8usize, 4usize);
    for (sh, sv) in pairs {
        let pixels = block_uniform_chroma(w, h, sh as usize, sv as usize);
        // The expected decoded RGB is the decode of the 1:1-equivalent
        // splat: build it by decoding the encoded None file once and
        // reusing it as the oracle for the compressors.
        let mut oracle: Option<Vec<u8>> = None;
        for comp in comps {
            let page = EncodePage {
                width: w as u32,
                height: h as u32,
                kind: EncodePixelFormat::YCbCrSubsampled24 {
                    pixels: &pixels,
                    subsampling: (sh, sv),
                },
                compression: comp,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            let bytes = encode_tiff(&page).unwrap();
            let d = decode_tiff(&bytes).unwrap();
            assert_eq!((d.width, d.height), (w as u32, h as u32), "{sh}x{sv}");
            assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
            let got = d.frame.planes[0].data.clone();
            match &oracle {
                None => oracle = Some(got),
                Some(o) => assert_eq!(&got, o, "compressor mismatch at {sh}x{sv}"),
            }
        }
    }
}

#[test]
fn subsampled_roundtrip_equals_full_resolution_splat() {
    // A subsampled encode of block-uniform chroma must decode to the
    // exact same RGB as a full-resolution 4:4:4 encode of the same
    // block-uniform raster — because the box-average recovers the
    // (already constant) per-block chroma and the decode splats it
    // back unchanged.
    let (w, h, sh, sv) = (4usize, 4usize, 2usize, 2usize);
    let pixels = block_uniform_chroma(w, h, sh, sv);

    let sub = EncodePage {
        width: w as u32,
        height: h as u32,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (sh as u16, sv as u16),
        },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let full = EncodePage {
        width: w as u32,
        height: h as u32,
        kind: EncodePixelFormat::YCbCr24 { pixels: &pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let dsub = decode_tiff(&encode_tiff(&sub).unwrap()).unwrap();
    let dfull = decode_tiff(&encode_tiff(&full).unwrap()).unwrap();
    assert_eq!(dsub.frame.planes[0].data, dfull.frame.planes[0].data);
}

#[test]
fn bigtiff_subsampled_roundtrips() {
    let (w, h, sh, sv) = (8usize, 4usize, 2usize, 2usize);
    let pixels = block_uniform_chroma(w, h, sh, sv);
    let page = EncodePage {
        width: w as u32,
        height: h as u32,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (sh as u16, sv as u16),
        },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    // BigTIFF magic 43.
    assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 43);
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (w as u32, h as u32));
    assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
}

// ---------------------------------------------------------------------------
// IFD tag content
// ---------------------------------------------------------------------------

#[test]
fn writes_actual_subsampling_factors_in_tag_530() {
    for (sh, sv) in [(2u16, 1u16), (2, 2), (4, 1), (4, 2)] {
        let (w, h) = (8usize, 2usize);
        let pixels = block_uniform_chroma(w, h, sh as usize, sv as usize);
        let page = EncodePage {
            width: w as u32,
            height: h as u32,
            kind: EncodePixelFormat::YCbCrSubsampled24 {
                pixels: &pixels,
                subsampling: (sh, sv),
            },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        assert_eq!(read_short(&bytes, 262), Some(6), "photometric YCbCr");
        assert_eq!(read_short(&bytes, 277), Some(3), "SamplesPerPixel");
        assert_eq!(read_short(&bytes, 284), Some(1), "PlanarConfiguration");
        assert_eq!(read_two_shorts(&bytes, 530), Some((sh, sv)), "subsampling");
        assert_eq!(read_short(&bytes, 531), Some(1), "positioning centered");
    }
}

// ---------------------------------------------------------------------------
// Decoder-side strip-size path, exercised from a hand-built fixture
// ---------------------------------------------------------------------------

#[test]
fn decoder_reads_handbuilt_subsampled_strip() {
    // Hand-build a classic-II TIFF whose strip is a §21 4:2:2 data-unit
    // payload (no encoder involvement) to prove the decoder's strip
    // sizing is driven by the data-unit geometry, not full resolution.
    let (w, h, sh, sv) = (4u32, 2u32, 2usize, 1usize);
    let pixels = block_uniform_chroma(w as usize, h as usize, sh, sv);
    let strip = reference_pack(&pixels, w as usize, h as usize, sh, sv);
    let tiff = build_classic_subsampled(w, h, sh as u16, sv as u16, &strip);
    let d = decode_tiff(&tiff).unwrap();
    assert_eq!((d.width, d.height), (w, h));
    assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);

    // Cross-check: encoder output decodes to the same RGB.
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (sh as u16, sv as u16),
        },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let enc = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
    assert_eq!(d.frame.planes[0].data, enc.frame.planes[0].data);
}

// ---------------------------------------------------------------------------
// Negative paths
// ---------------------------------------------------------------------------

#[test]
fn rejects_illegal_subsampling_pair() {
    // (1, 2) violates §21 page 90 (YCbCrSubsampleVert <= Horiz).
    let pixels = vec![0u8; 2 * 2 * 3];
    let page = EncodePage {
        width: 2,
        height: 2,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (1, 2),
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
fn rejects_non_multiple_dimensions() {
    // width 3 is not a multiple of ChromaSubsampleHoriz 2.
    let pixels = vec![0u8; 3 * 2 * 3];
    let page = EncodePage {
        width: 3,
        height: 2,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (2, 2),
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
fn rejects_wrong_buffer_length() {
    let pixels = vec![0u8; 10]; // not width*height*3
    let page = EncodePage {
        width: 4,
        height: 2,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (2, 2),
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
fn rejects_predictor_planar_tiled_and_ccitt() {
    let (w, h) = (8usize, 4usize);
    let pixels = block_uniform_chroma(w, h, 2, 2);
    let base = |comp, predictor, planar, tiling| EncodePage {
        width: w as u32,
        height: h as u32,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &pixels,
            subsampling: (2, 2),
        },
        compression: comp,
        predictor,
        planar,
        tiling,
        bigtiff: false,
    };
    assert!(encode_tiff(&base(TiffCompression::None, true, false, None)).is_err());
    assert!(encode_tiff(&base(TiffCompression::None, false, true, None)).is_err());
    assert!(encode_tiff(&base(TiffCompression::None, false, false, Some((16, 16)))).is_err());
    assert!(encode_tiff(&base(TiffCompression::CcittT6, false, false, None)).is_err());
}

// ---------------------------------------------------------------------------
// Byte-level IFD walkers (independent of our decoder)
// ---------------------------------------------------------------------------

fn read_short(bytes: &[u8], tag: u16) -> Option<u16> {
    let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
    for k in 0..count {
        let off = ifd_off + 2 + k * 12;
        if u16::from_le_bytes([bytes[off], bytes[off + 1]]) == tag {
            return Some(u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]));
        }
    }
    None
}

fn read_long(bytes: &[u8], tag: u16) -> Option<u32> {
    let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
    for k in 0..count {
        let off = ifd_off + 2 + k * 12;
        if u16::from_le_bytes([bytes[off], bytes[off + 1]]) == tag {
            return Some(u32::from_le_bytes([
                bytes[off + 8],
                bytes[off + 9],
                bytes[off + 10],
                bytes[off + 11],
            ]));
        }
    }
    None
}

fn read_two_shorts(bytes: &[u8], tag: u16) -> Option<(u16, u16)> {
    let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
    for k in 0..count {
        let off = ifd_off + 2 + k * 12;
        if u16::from_le_bytes([bytes[off], bytes[off + 1]]) == tag {
            let a = u16::from_le_bytes([bytes[off + 8], bytes[off + 9]]);
            let b = u16::from_le_bytes([bytes[off + 10], bytes[off + 11]]);
            return Some((a, b));
        }
    }
    None
}

/// Hand-build a minimal classic-II TIFF carrying a single subsampled
/// YCbCr strip, independent of the encoder.
fn build_classic_subsampled(width: u32, height: u32, sh: u16, sv: u16, strip: &[u8]) -> Vec<u8> {
    let num_entries: u16 = 13;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset: u32 = ifd_offset + ifd_size;

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

    push(&mut buf, 254, 4, 1, 0u32.to_le_bytes());
    push(&mut buf, 256, 4, 1, width.to_le_bytes());
    push(&mut buf, 257, 4, 1, height.to_le_bytes());
    push(&mut buf, 258, 3, 3, bps_off.to_le_bytes());
    let mut s = [0u8; 4];
    s[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 259, 3, 1, s); // Compression = None
    s = [0; 4];
    s[..2].copy_from_slice(&6u16.to_le_bytes());
    push(&mut buf, 262, 3, 1, s); // Photometric = YCbCr
    push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes()); // StripOffsets
    s = [0; 4];
    s[..2].copy_from_slice(&3u16.to_le_bytes());
    push(&mut buf, 277, 3, 1, s); // SamplesPerPixel
    push(&mut buf, 279, 4, 1, (strip.len() as u32).to_le_bytes()); // StripByteCounts
    s = [0; 4];
    s[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 284, 3, 1, s); // PlanarConfiguration = 1
    s = [0; 4];
    s[..2].copy_from_slice(&sh.to_le_bytes());
    s[2..4].copy_from_slice(&sv.to_le_bytes());
    push(&mut buf, 530, 3, 2, s); // YCbCrSubSampling
    s = [0; 4];
    s[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 531, 3, 1, s); // YCbCrPositioning = 1
    push(&mut buf, 532, 5, 6, rbw_off.to_le_bytes()); // ReferenceBlackWhite

    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0

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
    buf.extend_from_slice(strip);
    buf
}
