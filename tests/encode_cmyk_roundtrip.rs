//! Integration tests for [`EncodePixelFormat::Cmyk32`] (TIFF 6.0 §16
//! "CMYK Images", `PhotometricInterpretation = 5`) — binary-independent
//! self-roundtrip suite mirroring the CieLab roundtrip on the encode
//! side.
//!
//! The encoder writes C / M / Y / K bytes verbatim (§16 fixes the on-
//! disk byte interpretation as "amount of ink", 0 = no ink), so the
//! test contract is:
//!
//!   1. The encoder must produce a classic-II TIFF the decoder can
//!      parse back to Rgb24 (via `build_rgb24_from_cmyk`).
//!   2. The decoder's output from the encoded file must match a
//!      hand-built classic-TIFF fixture carrying the same C/M/Y/K
//!      bytes — otherwise the encoder is putting the wrong tag,
//!      sample count, or bit depth on disk.
//!   3. The IFD bytes must carry `PhotometricInterpretation = 5`
//!      (tag 262) and `SamplesPerPixel = 4` (tag 277), inspected via
//!      a byte-level IFD walker independent of our decoder.
//!   4. Predictor=2, PlanarConfiguration=2, tiled §15 layout, and
//!      BigTIFF must each leave the decoded Rgb24 unchanged from the
//!      plain chunky path (§14 / §"PlanarConfiguration" / §15 /
//!      Adobe Pagemaker 6.0 BigTIFF compose with §16 unchanged).

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, TiffCompression, TiffPixelFormat,
};

const PHOTO_CMYK: u16 = 5;

// ---------------------------------------------------------------------------
// Hand-rolled classic-II TIFF builder (oracle reference)
// ---------------------------------------------------------------------------

/// Build a hand-rolled classic-II TIFF for the requested 4-sample 8-bit
/// chunky pixel buffer with photometric=CMYK. The IFD layout is the same
/// minimal 8-entry form `encode_cielab_roundtrip.rs::build_classic_tiff`
/// emits; only the constants differ (SPP=4, BPS array length 4,
/// photometric=5).
fn build_handbuilt_cmyk_tiff(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let samples_per_pixel: u16 = 4;
    let bits_per_sample: u16 = 8;
    let row_bytes = (width as u64) * (samples_per_pixel as u64);
    let strip_bytes = row_bytes * (height as u64);
    assert_eq!(pixels.len() as u64, strip_bytes);

    // BPS array is 4 entries x 2 bytes = 8 bytes, which spills past the
    // 4-byte inline value slot of classic TIFF — write it as an
    // out-of-line blob.
    let bps_blob_bytes: u32 = (samples_per_pixel as u32) * 2;

    let num_entries: u16 = 8;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset: u32 = ifd_offset + ifd_size;
    let bps_off = blobs_offset;
    let pixels_off = bps_off + bps_blob_bytes;

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

    push(&mut buf, 256, 4, 1, width.to_le_bytes());
    push(&mut buf, 257, 4, 1, height.to_le_bytes());
    push(
        &mut buf,
        258,
        3,
        samples_per_pixel as u32,
        bps_off.to_le_bytes(),
    );
    let mut comp = [0u8; 4];
    comp[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 259, 3, 1, comp);
    let mut ph = [0u8; 4];
    ph[..2].copy_from_slice(&PHOTO_CMYK.to_le_bytes());
    push(&mut buf, 262, 3, 1, ph);
    push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes());
    let mut spp = [0u8; 4];
    spp[..2].copy_from_slice(&samples_per_pixel.to_le_bytes());
    push(&mut buf, 277, 3, 1, spp);
    push(&mut buf, 279, 4, 1, (strip_bytes as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // next-IFD
    for _ in 0..samples_per_pixel {
        buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    }
    buf.extend_from_slice(pixels);
    buf
}

// ---------------------------------------------------------------------------
// Byte-level IFD walker (independent of our decoder) — confirms the
// encoder writes the spec-correct photometric / spp tags on the wire.
// ---------------------------------------------------------------------------

/// Parse a classic II TIFF byte-by-byte and return (photometric, spp,
/// bits_per_sample). Used to verify the encoder writes the spec-correct
/// `PhotometricInterpretation = 5` and `SamplesPerPixel = 4` tags
/// without going through our own decoder.
fn read_photometric_spp_bps(bytes: &[u8]) -> (u16, u16, Vec<u16>) {
    assert_eq!(&bytes[0..2], b"II", "expected classic II byte order");
    let magic = u16::from_le_bytes([bytes[2], bytes[3]]);
    assert_eq!(magic, 42, "expected classic-TIFF magic 42");
    let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let n = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]);
    let mut photo: Option<u16> = None;
    let mut spp: Option<u16> = None;
    let mut bps: Vec<u16> = Vec::new();
    for i in 0..n {
        let off = ifd_off + 2 + (i as usize) * 12;
        let tag = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        let ft = u16::from_le_bytes([bytes[off + 2], bytes[off + 3]]);
        let count = u32::from_le_bytes([
            bytes[off + 4],
            bytes[off + 5],
            bytes[off + 6],
            bytes[off + 7],
        ]) as usize;
        let val_off = off + 8;
        match tag {
            262 => {
                photo = Some(u16::from_le_bytes([bytes[val_off], bytes[val_off + 1]]));
            }
            277 => {
                spp = Some(u16::from_le_bytes([bytes[val_off], bytes[val_off + 1]]));
            }
            258 if ft == 3 => {
                let inline_fits = count * 2 <= 4;
                let mut p = if inline_fits {
                    val_off
                } else {
                    u32::from_le_bytes([
                        bytes[val_off],
                        bytes[val_off + 1],
                        bytes[val_off + 2],
                        bytes[val_off + 3],
                    ]) as usize
                };
                for _ in 0..count {
                    bps.push(u16::from_le_bytes([bytes[p], bytes[p + 1]]));
                    p += 2;
                }
            }
            _ => {}
        }
    }
    (photo.expect("photometric"), spp.expect("spp"), bps)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// 4x1 CMYK gradient: pure-K ramp from no-ink to full-ink. The decoded
/// Rgb24 should be a black ramp (additive: R = (255 - 0) * (255 - K) /
/// 255, etc.).
fn k_gradient_4x1() -> Vec<u8> {
    let mut v = Vec::with_capacity(4 * 4);
    for &k in &[0u8, 85, 170, 255] {
        v.extend_from_slice(&[0, 0, 0, k]);
    }
    v
}

/// 4x1 CMYK primaries: pure cyan, pure magenta, pure yellow, pure
/// black. Each pixel has exactly one nonzero ink component.
fn cmyk_primaries_4x1() -> Vec<u8> {
    let mut v = Vec::with_capacity(4 * 4);
    v.extend_from_slice(&[255, 0, 0, 0]); // pure cyan
    v.extend_from_slice(&[0, 255, 0, 0]); // pure magenta
    v.extend_from_slice(&[0, 0, 255, 0]); // pure yellow
    v.extend_from_slice(&[0, 0, 0, 255]); // pure black
    v
}

/// 32x32 textured CMYK image: a per-pixel ramp in each component, so
/// every byte covers a wide range of values. Drives the predictor /
/// planar / tile / compressor compose tests.
fn textured_cmyk_32x32() -> Vec<u8> {
    let mut v = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let c = (x * 255 / 31) as u8;
            let m = (y * 255 / 31) as u8;
            let yel = ((x ^ y) * 255 / 31).min(255) as u8;
            let k = (((x + y) % 32) * 255 / 31) as u8;
            v.extend_from_slice(&[c, m, yel, k]);
        }
    }
    v
}

// ---------------------------------------------------------------------------
// Tests: encoder output matches hand-built fixture
// ---------------------------------------------------------------------------

#[test]
fn encoder_cmyk32_k_gradient_matches_handbuilt() {
    let cmyk = k_gradient_4x1();

    let handbuilt = build_handbuilt_cmyk_tiff(4, 1, &cmyk);
    let want = decode_tiff(&handbuilt).unwrap().frame.planes[0]
        .data
        .clone();

    let page = EncodePage {
        width: 4,
        height: 1,
        kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
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
fn encoder_cmyk32_primaries_match_handbuilt() {
    let cmyk = cmyk_primaries_4x1();
    let handbuilt = build_handbuilt_cmyk_tiff(4, 1, &cmyk);
    let want = decode_tiff(&handbuilt).unwrap().frame.planes[0]
        .data
        .clone();

    let page = EncodePage {
        width: 4,
        height: 1,
        kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!(d.frame.planes[0].data, want);
}

#[test]
fn encoder_cmyk32_writes_photometric_5_and_spp_4() {
    // Byte-level IFD inspection independent of our own decoder.
    let cmyk = cmyk_primaries_4x1();
    let page = EncodePage {
        width: 4,
        height: 1,
        kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    let (photo, spp, bps) = read_photometric_spp_bps(&bytes);
    assert_eq!(photo, 5, "PhotometricInterpretation must be 5 (CMYK)");
    assert_eq!(spp, 4, "SamplesPerPixel must be 4");
    assert_eq!(bps, vec![8u16, 8, 8, 8], "BitsPerSample must be [8,8,8,8]");
}

// ---------------------------------------------------------------------------
// Tests: compressors / predictor / planar / tiled / BigTIFF compose
// ---------------------------------------------------------------------------

#[test]
fn encoder_cmyk32_compressors_lossless() {
    let cmyk = textured_cmyk_32x32();
    let baseline = {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
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
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
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
fn encoder_cmyk32_predictor_planar_tiled_bigtiff_all_compose() {
    let cmyk = textured_cmyk_32x32();
    let baseline = {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
            compression: TiffCompression::Lzw,
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

    // Predictor = 2 alone (§14 horizontal differencing, offset = SPP = 4).
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "predictor only");
    }

    // PlanarConfiguration = 2 alone (four C/M/Y/K planes).
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: true,
            tiling: None,
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "planar only");
    }

    // Tiled chunky (§15).
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "tiled chunky");
    }

    // Tiled planar (§15 + §"PlanarConfiguration": one tile grid per
    // C / M / Y / K plane, plane-0 tiles emitted first then plane-1's,
    // etc.).
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: true,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "tiled planar");
    }

    // Predictor + planar (per §14: "If PlanarConfiguration is 2,
    // Differencing works the same as it does for grayscale data" — each
    // C/M/Y/K plane is differenced with offset = 1 sample).
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: true,
            tiling: None,
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "predictor+planar");
    }

    // BigTIFF. The 4-entry BitsPerSample SHORT array is 8 bytes — it
    // fits inline in the widened BigTIFF 8-byte value/offset slot
    // (classic TIFF spilled it out-of-line).
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: true,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "bigtiff");
    }
}

// ---------------------------------------------------------------------------
// Tests: negative paths
// ---------------------------------------------------------------------------

#[test]
fn encoder_cmyk32_rejects_ccitt_input() {
    // CCITT is bilevel-only (§10 / §11). Pixel-buffer length is sized
    // for a 4x1 Cmyk32 image but width=4/height=1 keeps the validator
    // satisfied so the gate is reached cleanly.
    let cmyk = vec![0u8; 16];
    let page = EncodePage {
        width: 4,
        height: 1,
        kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
        compression: TiffCompression::CcittRle,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let err = encode_tiff(&page).expect_err("CCITT on Cmyk32 must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("CCITT") || msg.contains("Bilevel"),
        "expected CCITT rejection, got: {msg}"
    );
}

#[test]
fn encoder_cmyk32_short_buffer_rejected() {
    let cmyk = vec![0u8; 15]; // one byte short of 4 * 1 * 4 = 16
    let page = EncodePage {
        width: 4,
        height: 1,
        kind: EncodePixelFormat::Cmyk32 { pixels: &cmyk },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let err = encode_tiff(&page).expect_err("short buffer must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("Cmyk32") || msg.contains("expected"),
        "expected short-buffer rejection, got: {msg}"
    );
}
