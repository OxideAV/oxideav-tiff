//! Integration tests for `EncodePixelFormat::CieLab8` /
//! `EncodePixelFormat::CieLabL8` (TIFF 6.0 §23 "CIE L*a*b* Images",
//! PhotometricInterpretation = 8) — binary-independent self-roundtrip
//! suite mirroring `decode_cielab.rs` on the encode side.
//!
//! The encoder writes L*/a*/b* bytes verbatim (the spec's bit
//! interpretation is fixed — L* unsigned 0..255 -> 0..100, a*/b*
//! two's-complement signed bytes), so the test contract is:
//!
//!   1. The encoder must produce a classic-II TIFF the decoder can
//!      parse back to Rgb24 (3-sample) / Gray8 (1-sample).
//!   2. The decoder's output from the encoded file must match a
//!      hand-built classic-TIFF fixture carrying the same L*/a*/b*
//!      bytes — otherwise the encoder is putting the wrong tag,
//!      sample count, or bit depth on disk.
//!   3. The IFD bytes must carry `PhotometricInterpretation = 8`
//!      (tag 262) and `SamplesPerPixel = 3 or 1` (tag 277), inspected
//!      via a byte-level IFD walker independent of our decoder.

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, TiffCompression, TiffPixelFormat,
};

/// Pack a logical (L%, a, b) where L is the perceptual 0..100 scale
/// and a, b are -127..127, into the three on-disk bytes per TIFF 6.0
/// §23.
fn pack_lab(l_pct: f64, a_signed: i32, b_signed: i32) -> [u8; 3] {
    let l_byte = (l_pct * 255.0 / 100.0).round().clamp(0.0, 255.0) as u8;
    [l_byte, (a_signed as i8) as u8, (b_signed as i8) as u8]
}

/// Build a hand-rolled classic-II TIFF for the requested chunky pixel
/// buffer with the given photometric / spp / bps. Mirrors the helper
/// in `decode_cielab.rs` so the roundtrip oracle is a literal
/// byte-identical IFD.
fn build_classic_tiff(
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bits_per_sample: u16,
    photometric: u16,
    pixels: &[u8],
) -> Vec<u8> {
    let row_bytes = (width as u64) * (samples_per_pixel as u64) * (bits_per_sample as u64 / 8);
    let strip_bytes = row_bytes * (height as u64);
    assert_eq!(pixels.len() as u64, strip_bytes);

    let bps_inline = samples_per_pixel == 1;
    let bps_blob_bytes: u32 = if bps_inline {
        0
    } else {
        (samples_per_pixel as u32) * 2
    };

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
    if bps_inline {
        let mut v = [0u8; 4];
        v[..2].copy_from_slice(&bits_per_sample.to_le_bytes());
        push(&mut buf, 258, 3, 1, v);
    } else {
        push(
            &mut buf,
            258,
            3,
            samples_per_pixel as u32,
            bps_off.to_le_bytes(),
        );
    }
    let mut comp = [0u8; 4];
    comp[..2].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 259, 3, 1, comp);
    let mut ph = [0u8; 4];
    ph[..2].copy_from_slice(&photometric.to_le_bytes());
    push(&mut buf, 262, 3, 1, ph);
    push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes());
    let mut spp = [0u8; 4];
    spp[..2].copy_from_slice(&samples_per_pixel.to_le_bytes());
    push(&mut buf, 277, 3, 1, spp);
    push(&mut buf, 279, 4, 1, (strip_bytes as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    if !bps_inline {
        for _ in 0..samples_per_pixel {
            buf.extend_from_slice(&bits_per_sample.to_le_bytes());
        }
    }
    buf.extend_from_slice(pixels);
    buf
}

// ---------------------------------------------------------------------------
// 3-sample (L*, a*, b*) encode tests
// ---------------------------------------------------------------------------

#[test]
fn encoder_cielab8_neutral_gradient_matches_handbuilt() {
    // The same 4x1 neutral L* gradient `decode_cielab.rs` validates on
    // the decode side. Our encoder must produce a file whose decoder
    // output is byte-identical to the hand-built fixture's decode of
    // the same L*/a*/b* bytes.
    let lab_pixels: Vec<u8> = [
        pack_lab(0.0, 0, 0),
        pack_lab(33.0, 0, 0),
        pack_lab(66.0, 0, 0),
        pack_lab(100.0, 0, 0),
    ]
    .concat();

    let handbuilt = build_classic_tiff(4, 1, 3, 8, /* PHOTO_CIELAB */ 8, &lab_pixels);
    let want = decode_tiff(&handbuilt).unwrap().frame.planes[0]
        .data
        .clone();

    let page = EncodePage {
        width: 4,
        height: 1,
        kind: EncodePixelFormat::CieLab8 {
            pixels: &lab_pixels,
        },
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
fn encoder_cielab8_chromatic_primaries_match_handbuilt() {
    // Single-pixel fixtures for each of the four chromatic directions
    // the decode-side suite covers. Each must produce the same Rgb24
    // pixel through our encoder as through the hand-built fixture.
    let cases: &[(f64, i32, i32, &str)] = &[
        (50.0, 120, 0, "red"),
        (50.0, -120, 0, "green"),
        (70.0, 0, 120, "yellow"),
        (30.0, 0, -120, "blue"),
    ];
    for (l, a, b, label) in cases {
        let lab = pack_lab(*l, *a, *b).to_vec();
        let handbuilt = build_classic_tiff(1, 1, 3, 8, 8, &lab);
        let want = decode_tiff(&handbuilt).unwrap().frame.planes[0]
            .data
            .clone();

        let page = EncodePage {
            width: 1,
            height: 1,
            kind: EncodePixelFormat::CieLab8 { pixels: &lab },
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
fn encoder_cielab8_compressors_lossless() {
    // PackBits / LZW / Deflate must all decode to the same Rgb24 as
    // the uncompressed encode of the same source — they are lossless
    // byte-aligned compressors, photometric-agnostic.
    let lab_pixels: Vec<u8> = (0..(16 * 8))
        .flat_map(|i| {
            let l = (i as f64 % 16.0) * 100.0 / 16.0;
            let a = -127 + (i % 8) * 32;
            let b = if i % 2 == 0 { 50 } else { -40 };
            pack_lab(l, a, b)
        })
        .collect();

    let baseline = {
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::CieLab8 {
                pixels: &lab_pixels,
            },
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
            kind: EncodePixelFormat::CieLab8 {
                pixels: &lab_pixels,
            },
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
fn encoder_cielab8_predictor_planar_tiled_all_compose() {
    // Predictor=2, PlanarConfiguration=2, and tiled §15 layout must
    // each leave the decoded Rgb24 unchanged from the plain chunky
    // path.
    let lab_pixels: Vec<u8> = (0..(32 * 32))
        .flat_map(|i| {
            let x = (i % 32) as f64;
            let y = (i / 32) as f64;
            let l = x * 100.0 / 32.0;
            let a = -64 + (y as i32 * 8 - 64);
            let b = 60 - (y as i32 * 4);
            pack_lab(l, a, b)
        })
        .collect();

    let baseline = {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::CieLab8 {
                pixels: &lab_pixels,
            },
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

    // Predictor=2 alone.
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::CieLab8 {
                pixels: &lab_pixels,
            },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "predictor only");
    }

    // Planar alone.
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::CieLab8 {
                pixels: &lab_pixels,
            },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: true,
            tiling: None,
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "planar only");
    }

    // Tiled chunky.
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::CieLab8 {
                pixels: &lab_pixels,
            },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "tiled chunky");
    }

    // Tiled + planar (per-plane tile grid, §15 PlanarConfiguration=2).
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::CieLab8 {
                pixels: &lab_pixels,
            },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: true,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "tiled planar");
    }

    // Predictor + planar + tiled all combined.
    {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::CieLab8 {
                pixels: &lab_pixels,
            },
            compression: TiffCompression::Deflate,
            predictor: true,
            planar: true,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(d.frame.planes[0].data, baseline, "predictor+planar+tiled");
    }
}

// ---------------------------------------------------------------------------
// 1-sample L*-only encode tests
// ---------------------------------------------------------------------------

#[test]
fn encoder_cielab_l8_neutral_ramp_matches_handbuilt() {
    let l_bytes: Vec<u8> = vec![
        ((0.0_f64) * 255.0 / 100.0).round() as u8,
        ((33.0_f64) * 255.0 / 100.0).round() as u8,
        ((66.0_f64) * 255.0 / 100.0).round() as u8,
        ((100.0_f64) * 255.0 / 100.0).round() as u8,
    ];
    let handbuilt = build_classic_tiff(4, 1, 1, 8, 8, &l_bytes);
    let want = decode_tiff(&handbuilt).unwrap().frame.planes[0]
        .data
        .clone();

    let page = EncodePage {
        width: 4,
        height: 1,
        kind: EncodePixelFormat::CieLabL8 { pixels: &l_bytes },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (4, 1));
    assert_eq!(d.pixel_format, TiffPixelFormat::Gray8);
    assert_eq!(d.frame.planes[0].data, want);
}

#[test]
fn encoder_cielab_l8_predictor_and_compressors() {
    // 1-sample chunky path — predictor (SPP=1) and the byte-aligned
    // compressors must all round-trip to the same decoded Gray8.
    let l_bytes: Vec<u8> = (0..(20 * 12)).map(|i| (i & 0xFF) as u8).collect();

    let baseline = {
        let page = EncodePage {
            width: 20,
            height: 12,
            kind: EncodePixelFormat::CieLabL8 { pixels: &l_bytes },
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

    for (c, pred) in [
        (TiffCompression::None, true),
        (TiffCompression::PackBits, false),
        (TiffCompression::Lzw, true),
        (TiffCompression::Deflate, false),
        (TiffCompression::Deflate, true),
    ] {
        let page = EncodePage {
            width: 20,
            height: 12,
            kind: EncodePixelFormat::CieLabL8 { pixels: &l_bytes },
            compression: c,
            predictor: pred,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
        assert_eq!(
            d.frame.planes[0].data, baseline,
            "compressor {:?} pred {}",
            c, pred
        );
    }
}

#[test]
fn encoder_cielab_l8_tiled_composes() {
    // L*-only single-component tiles — byte-aligned chunky tile path,
    // identical to Gray8.
    let l_bytes: Vec<u8> = (0..(32 * 32)).map(|i| (i & 0xFF) as u8).collect();

    let strip = {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::CieLabL8 { pixels: &l_bytes },
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
    let tiled = {
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::CieLabL8 { pixels: &l_bytes },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        decode_tiff(&encode_tiff(&page).unwrap())
            .unwrap()
            .frame
            .planes[0]
            .data
            .clone()
    };
    assert_eq!(strip, tiled);
}

// ---------------------------------------------------------------------------
// IFD inspection (byte-level, independent of our decoder)
// ---------------------------------------------------------------------------

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

#[test]
fn encoder_cielab_writes_photometric_and_samples() {
    // 3-sample.
    let lab: Vec<u8> = pack_lab(50.0, 0, 0).to_vec();
    let p3 = EncodePage {
        width: 1,
        height: 1,
        kind: EncodePixelFormat::CieLab8 { pixels: &lab },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let b3 = encode_tiff(&p3).unwrap();
    assert_eq!(read_ifd_entry_value_short(&b3, 262), Some(8));
    assert_eq!(read_ifd_entry_value_short(&b3, 277), Some(3));

    // 1-sample L*-only.
    let l = vec![127u8];
    let p1 = EncodePage {
        width: 1,
        height: 1,
        kind: EncodePixelFormat::CieLabL8 { pixels: &l },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let b1 = encode_tiff(&p1).unwrap();
    assert_eq!(read_ifd_entry_value_short(&b1, 262), Some(8));
    assert_eq!(read_ifd_entry_value_short(&b1, 277), Some(1));
}

#[test]
fn encoder_cielab_multi_page_chain() {
    // Multi-IFD chain mixing CIELab pages with a Gray8 page must walk
    // cleanly via decode_tiff_all.
    use oxideav_tiff::encode_tiff_multi;
    let lab = pack_lab(50.0, 30, -20).to_vec();
    let gray = vec![10u8, 20, 30, 40];
    let l_only = vec![80u8, 90, 100, 110];
    let pages = vec![
        EncodePage {
            width: 1,
            height: 1,
            kind: EncodePixelFormat::CieLab8 { pixels: &lab },
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
        EncodePage {
            width: 4,
            height: 1,
            kind: EncodePixelFormat::CieLabL8 { pixels: &l_only },
            compression: TiffCompression::Deflate,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
    ];
    let bytes = encode_tiff_multi(&pages).unwrap();
    let imgs = oxideav_tiff::decode_tiff_all(&bytes).unwrap();
    assert_eq!(imgs.len(), 3);
    assert_eq!(imgs[0].pixel_format, TiffPixelFormat::Rgb24);
    assert_eq!(imgs[1].pixel_format, TiffPixelFormat::Gray8);
    assert_eq!(imgs[1].planes[0].data, gray);
    assert_eq!(imgs[2].pixel_format, TiffPixelFormat::Gray8);
}
