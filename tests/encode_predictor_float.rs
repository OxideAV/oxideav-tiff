//! TIFF floating-point (`SampleFormat = 3`) encode + `Predictor = 3`
//! round-trip.
//!
//! The decoder already renders `SampleFormat = 3` IEEE-float grayscale
//! and RGB to a display plane (and reverses the §14 floating-point
//! predictor); before this round there was no float *encoder*, so the
//! float subsystem could only be validated against externally-written
//! fixtures. The new `EncodePixelFormat::{GrayF32, GrayF64, RgbF32,
//! RgbF64}` formats close that gap.
//!
//! Two complementary, binary-independent oracles run here:
//!
//!   * **Display-plane oracle.** Encode float pixels, decode through the
//!     public `decode_tiff`, and compare against the decoder's display
//!     map computed *directly in the test* from the input floats (a
//!     linear map of the finite sample extent onto 0..=255). A predictor
//!     that did not reverse exactly, or a SampleFormat tag that misrouted
//!     the decode, would diverge. The Predictor = 3 encode and the
//!     Predictor = 1 (no-predictor) encode of identical samples must
//!     decode to byte-identical display planes — the predictor is a
//!     lossless, codec-independent pre-transform.
//!   * **Raw-byte oracle.** Walk the encoded IFD with a tiny independent
//!     reader, confirm `SampleFormat = 3` (tag 339) and the right
//!     `Predictor` value (tag 317), pull the single strip's bytes,
//!     reverse the float predictor with a test-local routine, and assert
//!     the recovered little-endian sample bytes equal the input — so the
//!     encoder's transform is checked without trusting our own decoder.
//!
//! No external image library / binary / decoder source is consulted; the
//! display-map and predictor math are transcribed from TIFF 6.0
//! §SampleFormat and the §14 floating-point predictor.

use oxideav_tiff::{
    decode_tiff, decode_tiff_all, encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat,
    TiffCompression, TiffPixelFormat,
};

// ---------------------------------------------------------------------------
// Display-map oracle (mirrors the decoder's float → display algorithm).
// ---------------------------------------------------------------------------

/// Linear-map a slice of `f64` samples onto 0..=255 using the finite
/// extent (min/max of the finite samples) shared across the whole slice,
/// matching `decoder::build_*_from_float` when SMin/SMax are absent.
/// Non-finite samples render at the floor (0); a degenerate extent
/// renders a flat 0 plane.
fn display_map(samples: &[f64]) -> Vec<u8> {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &s in samples {
        if s.is_finite() {
            if s < lo {
                lo = s;
            }
            if s > hi {
                hi = s;
            }
        }
    }
    let span = if lo.is_finite() && hi.is_finite() {
        hi - lo
    } else {
        0.0
    };
    samples
        .iter()
        .map(|&s| {
            if !s.is_finite() || span <= 0.0 {
                0u8
            } else {
                let t = ((s - lo) / span).clamp(0.0, 1.0);
                (t * 255.0 + 0.5) as u8
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Raw-byte oracle: minimal classic-II IFD walk + predictor reversal.
// ---------------------------------------------------------------------------

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Read tag `tag` from the first IFD of a classic little-endian TIFF.
/// Returns `(field_type, count, value_or_offset)`.
fn find_tag(tiff: &[u8], tag: u16) -> Option<(u16, u32, u32)> {
    assert_eq!(&tiff[0..2], b"II", "test only walks little-endian output");
    assert_eq!(rd_u16(tiff, 2), 42, "test only walks classic TIFF");
    let ifd = rd_u32(tiff, 4) as usize;
    let n = rd_u16(tiff, ifd) as usize;
    for i in 0..n {
        let e = ifd + 2 + i * 12;
        if rd_u16(tiff, e) == tag {
            return Some((
                rd_u16(tiff, e + 2),
                rd_u32(tiff, e + 4),
                rd_u32(tiff, e + 8),
            ));
        }
    }
    None
}

/// Reverse the float predictor over one strip: inclusive prefix sum, then
/// scatter the significance-ordered planes back to little-endian sample
/// bytes. `width` is samples per row, `rows` the scan-lines.
fn reverse_float_predictor(buf: &mut [u8], width: usize, rows: usize, bytes: usize) {
    let row_len = width * bytes;
    let mut plane = vec![0u8; row_len];
    for r in 0..rows {
        let row = &mut buf[r * row_len..r * row_len + row_len];
        for i in 1..row_len {
            row[i] = row[i].wrapping_add(row[i - 1]);
        }
        for p in 0..bytes {
            // Little-endian: significance rank p -> in-file byte bytes-1-p.
            let dst = bytes - 1 - p;
            for k in 0..width {
                plane[k * bytes + dst] = row[p * width + k];
            }
        }
        row.copy_from_slice(&plane);
    }
}

/// Pull the single uncompressed strip bytes from a classic-II TIFF.
fn single_strip(tiff: &[u8]) -> Vec<u8> {
    let (_, soc, so) = find_tag(tiff, 273).expect("StripOffsets");
    assert_eq!(soc, 1, "test fixtures use a single strip");
    let (_, sbc_c, sbc) = find_tag(tiff, 279).expect("StripByteCounts");
    assert_eq!(sbc_c, 1);
    tiff[so as usize..so as usize + sbc as usize].to_vec()
}

// ---------------------------------------------------------------------------
// GrayF32 / GrayF64
// ---------------------------------------------------------------------------

fn gray_samples_f32() -> (u32, u32, Vec<f32>) {
    // 4x3 with a wide dynamic range, a couple of negatives, and an exact
    // endpoint repeat so the extent is unambiguous.
    let v: Vec<f32> = vec![
        -2.5, 0.0, 1.0, 3.5, //
        -1.0, 0.25, 2.0, 3.5, //
        -2.5, 0.5, 1.5, 0.75,
    ];
    (4, 3, v)
}

#[test]
fn grayf32_predictor_roundtrip_matches_unpredicted() {
    let (w, h, fpix) = gray_samples_f32();
    let samples64: Vec<f64> = fpix.iter().map(|&x| x as f64).collect();
    let want = display_map(&samples64);

    for comp in [
        TiffCompression::None,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
        TiffCompression::PackBits,
        TiffCompression::Zstd,
    ] {
        for predictor in [false, true] {
            let page = EncodePage {
                width: w,
                height: h,
                kind: EncodePixelFormat::GrayF32 { pixels: &fpix },
                compression: comp,
                predictor,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            let bytes = encode_tiff(&page).expect("encode GrayF32");
            let dec = decode_tiff(&bytes).expect("decode GrayF32");
            assert_eq!(dec.frame.pixel_format, TiffPixelFormat::Gray8);
            assert_eq!(
                dec.frame.planes[0].data, want,
                "GrayF32 comp={comp:?} predictor={predictor} display plane mismatch"
            );
        }
    }
}

#[test]
fn grayf32_emits_sampleformat_and_predictor_tags() {
    let (w, h, fpix) = gray_samples_f32();

    // No predictor: SampleFormat=3 present, Predictor tag absent.
    let p1 = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::GrayF32 { pixels: &fpix },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let t1 = encode_tiff(&p1).unwrap();
    let (sf_type, sf_count, sf_val) = find_tag(&t1, 339).expect("SampleFormat tag present");
    assert_eq!(sf_type, 3, "SampleFormat field type SHORT");
    assert_eq!(sf_count, 1, "one SampleFormat value for grayscale");
    assert_eq!(sf_val & 0xFFFF, 3, "SampleFormat = 3 (IEEE float)");
    assert!(find_tag(&t1, 317).is_none(), "no Predictor tag when off");

    // With predictor: Predictor = 3.
    let p2 = EncodePage {
        predictor: true,
        ..p1.clone()
    };
    let t2 = encode_tiff(&p2).unwrap();
    let (_, _, pv) = find_tag(&t2, 317).expect("Predictor tag present");
    assert_eq!(pv & 0xFFFF, 3, "Predictor = 3 (float predictor)");
}

#[test]
fn grayf32_raw_predictor_reverses_to_input() {
    let (w, h, fpix) = gray_samples_f32();
    let mut input_bytes = Vec::new();
    for s in &fpix {
        input_bytes.extend_from_slice(&s.to_le_bytes());
    }
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::GrayF32 { pixels: &fpix },
        compression: TiffCompression::None,
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let tiff = encode_tiff(&page).unwrap();
    let mut strip = single_strip(&tiff);
    reverse_float_predictor(&mut strip, w as usize, h as usize, 4);
    assert_eq!(
        strip, input_bytes,
        "reversing the encoder's float predictor must recover the input sample bytes"
    );
}

#[test]
fn grayf64_predictor_roundtrip_matches_unpredicted() {
    let w = 3u32;
    let h = 2u32;
    let fpix: Vec<f64> = vec![1.0e-3, -4.0, 2.0, 0.0, 8.0, -8.0];
    let want = display_map(&fpix);
    for comp in [TiffCompression::None, TiffCompression::Deflate] {
        for predictor in [false, true] {
            let page = EncodePage {
                width: w,
                height: h,
                kind: EncodePixelFormat::GrayF64 { pixels: &fpix },
                compression: comp,
                predictor,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            let bytes = encode_tiff(&page).unwrap();
            let dec = decode_tiff(&bytes).unwrap();
            assert_eq!(
                dec.frame.planes[0].data, want,
                "GrayF64 comp={comp:?} pred={predictor}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// RgbF32 / RgbF64
// ---------------------------------------------------------------------------

fn rgb_samples_f32() -> (u32, u32, Vec<f32>) {
    // 3x2 RGB, interleaved. Shared extent across all three channels.
    let v: Vec<f32> = vec![
        0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 5.5, 4.5, // row 0 (3 px)
        -1.0, 0.5, 7.0, 2.5, 3.5, 1.5, 6.5, 0.25, 7.0, // row 1
    ];
    (3, 2, v)
}

#[test]
fn rgbf32_predictor_roundtrip_matches_unpredicted() {
    let (w, h, fpix) = rgb_samples_f32();
    let samples64: Vec<f64> = fpix.iter().map(|&x| x as f64).collect();
    let want = display_map(&samples64); // shared extent across R/G/B

    for comp in [
        TiffCompression::None,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
        TiffCompression::Zstd,
    ] {
        for predictor in [false, true] {
            let page = EncodePage {
                width: w,
                height: h,
                kind: EncodePixelFormat::RgbF32 { pixels: &fpix },
                compression: comp,
                predictor,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            let bytes = encode_tiff(&page).unwrap();
            let dec = decode_tiff(&bytes).unwrap();
            assert_eq!(dec.frame.pixel_format, TiffPixelFormat::Rgb24);
            assert_eq!(
                dec.frame.planes[0].data, want,
                "RgbF32 comp={comp:?} predictor={predictor} display plane mismatch"
            );
        }
    }
}

#[test]
fn rgbf32_sampleformat_is_three_values() {
    let (w, h, fpix) = rgb_samples_f32();
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::RgbF32 { pixels: &fpix },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let tiff = encode_tiff(&page).unwrap();
    let (sf_type, sf_count, _off) = find_tag(&tiff, 339).expect("SampleFormat present");
    assert_eq!(sf_type, 3);
    assert_eq!(sf_count, 3, "one SampleFormat value per RGB component");
    // 3 SHORTs (6 bytes) spill out-of-line on classic TIFF; the value
    // slot is an offset. Read the three values and confirm all are 3.
    let off = _off as usize;
    for i in 0..3 {
        assert_eq!(rd_u16(&tiff, off + i * 2), 3, "SampleFormat[{i}] = 3");
    }
}

#[test]
fn rgbf32_raw_predictor_reverses_to_input() {
    let (w, h, fpix) = rgb_samples_f32();
    let mut input_bytes = Vec::new();
    for s in &fpix {
        input_bytes.extend_from_slice(&s.to_le_bytes());
    }
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::RgbF32 { pixels: &fpix },
        compression: TiffCompression::None,
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let tiff = encode_tiff(&page).unwrap();
    let mut strip = single_strip(&tiff);
    // Width in samples = image width * 3 components.
    reverse_float_predictor(&mut strip, w as usize * 3, h as usize, 4);
    assert_eq!(strip, input_bytes);
}

#[test]
fn rgbf64_predictor_roundtrip_matches_unpredicted() {
    let (w, h, f32pix) = rgb_samples_f32();
    let fpix: Vec<f64> = f32pix.iter().map(|&x| x as f64).collect();
    let want = display_map(&fpix);
    for predictor in [false, true] {
        let page = EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::RgbF64 { pixels: &fpix },
            compression: TiffCompression::Deflate,
            predictor,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let dec = decode_tiff(&bytes).unwrap();
        assert_eq!(dec.frame.planes[0].data, want, "RgbF64 pred={predictor}");
    }
}

// ---------------------------------------------------------------------------
// Tiled + BigTIFF + multi-page compositions, and negative paths.
// ---------------------------------------------------------------------------

#[test]
fn grayf32_tiled_matches_strip() {
    // 40x32 ramp; 16x16 tiles (partial right/bottom edges).
    let (w, h) = (40u32, 32u32);
    let fpix: Vec<f32> = (0..(w * h)).map(|i| (i as f32) * 0.5 - 100.0).collect();
    let samples64: Vec<f64> = fpix.iter().map(|&x| x as f64).collect();
    let want = display_map(&samples64);

    for predictor in [false, true] {
        let page = EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::GrayF32 { pixels: &fpix },
            compression: TiffCompression::Lzw,
            predictor,
            planar: false,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let dec = decode_tiff(&bytes).unwrap();
        assert_eq!(
            dec.frame.planes[0].data, want,
            "GrayF32 tiled predictor={predictor} must match the strip display map"
        );
    }
}

#[test]
fn rgbf32_bigtiff_roundtrip() {
    let (w, h, fpix) = rgb_samples_f32();
    let samples64: Vec<f64> = fpix.iter().map(|&x| x as f64).collect();
    let want = display_map(&samples64);
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::RgbF32 { pixels: &fpix },
        compression: TiffCompression::Deflate,
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: true,
    };
    let bytes = encode_tiff(&page).unwrap();
    assert_eq!(&bytes[0..2], b"II");
    assert_eq!(
        u16::from_le_bytes([bytes[2], bytes[3]]),
        43,
        "BigTIFF magic 43"
    );
    let dec = decode_tiff(&bytes).unwrap();
    assert_eq!(dec.frame.planes[0].data, want);
}

#[test]
fn rgbf32_planar_matches_chunky() {
    // PlanarConfiguration=2 float RGB: the encoder de-interleaves into
    // three Y/Cb/Cr-style planes and applies the §14 float predictor
    // per-plane; the decoder reverses it per-plane and re-interleaves.
    // The planar encode of identical samples must decode to the same
    // display plane as the chunky encode.
    let (w, h, fpix) = rgb_samples_f32();
    let samples64: Vec<f64> = fpix.iter().map(|&x| x as f64).collect();
    let want = display_map(&samples64);

    for comp in [
        TiffCompression::None,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
        TiffCompression::Zstd,
    ] {
        for predictor in [false, true] {
            for tiling in [None, Some((16u32, 16u32))] {
                let page = EncodePage {
                    width: w,
                    height: h,
                    kind: EncodePixelFormat::RgbF32 { pixels: &fpix },
                    compression: comp,
                    predictor,
                    planar: true,
                    tiling,
                    bigtiff: false,
                };
                let bytes = encode_tiff(&page).unwrap();
                let dec = decode_tiff(&bytes).unwrap();
                assert_eq!(
                    dec.frame.planes[0].data, want,
                    "RgbF32 planar comp={comp:?} predictor={predictor} tiling={tiling:?} \
                     must match the chunky display map"
                );
            }
        }
    }
}

#[test]
fn rgbf64_planar_predictor_matches_chunky() {
    let (w, h, f32pix) = rgb_samples_f32();
    let fpix: Vec<f64> = f32pix.iter().map(|&x| x as f64).collect();
    let want = display_map(&fpix);
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::RgbF64 { pixels: &fpix },
        compression: TiffCompression::Deflate,
        predictor: true,
        planar: true,
        tiling: None,
        bigtiff: false,
    };
    let bytes = encode_tiff(&page).unwrap();
    let dec = decode_tiff(&bytes).unwrap();
    assert_eq!(dec.frame.planes[0].data, want);
}

#[test]
fn float_rejects_grayscale_planar_and_ccitt() {
    let (w, h, fpix) = gray_samples_f32();
    // PlanarConfiguration=2 is irrelevant for a single sample -> rejected.
    let planar = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::GrayF32 { pixels: &fpix },
        compression: TiffCompression::None,
        predictor: false,
        planar: true,
        tiling: None,
        bigtiff: false,
    };
    assert!(
        encode_tiff(&planar).is_err(),
        "single-sample float planar must be rejected (PlanarConfiguration irrelevant)"
    );

    // CCITT with float is rejected (bilevel-only).
    let ccitt = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::GrayF32 { pixels: &fpix },
        compression: TiffCompression::CcittRle,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&ccitt).is_err(), "CCITT float must be rejected");
}

#[test]
fn float_multipage_chain_roundtrips() {
    // A multi-IFD float chain (encode_tiff_multi): a GrayF32 page with the
    // float predictor, then an RgbF64 page, then a GrayF64 tiled page.
    // Each must decode (via decode_tiff_all) to its own display plane.
    let (gw, gh, gpix) = gray_samples_f32();
    let gwant = display_map(&gpix.iter().map(|&x| x as f64).collect::<Vec<_>>());

    let (rw, rh, rpix32) = rgb_samples_f32();
    let rpix: Vec<f64> = rpix32.iter().map(|&x| x as f64).collect();
    let rwant = display_map(&rpix);

    let (tw, th) = (32u32, 16u32);
    let tpix: Vec<f64> = (0..(tw * th)).map(|i| (i as f64) * 0.25 - 50.0).collect();
    let twant = display_map(&tpix);

    let pages = vec![
        EncodePage {
            width: gw,
            height: gh,
            kind: EncodePixelFormat::GrayF32 { pixels: &gpix },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
        EncodePage {
            width: rw,
            height: rh,
            kind: EncodePixelFormat::RgbF64 { pixels: &rpix },
            compression: TiffCompression::Deflate,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
        EncodePage {
            width: tw,
            height: th,
            kind: EncodePixelFormat::GrayF64 { pixels: &tpix },
            compression: TiffCompression::Zstd,
            predictor: true,
            planar: false,
            tiling: Some((16, 16)),
            bigtiff: false,
        },
    ];
    let bytes = encode_tiff_multi(&pages).unwrap();
    let frames = decode_tiff_all(&bytes).unwrap();
    assert_eq!(frames.len(), 3, "three-page float chain");
    assert_eq!(frames[0].planes[0].data, gwant, "page 0 GrayF32");
    assert_eq!(frames[1].planes[0].data, rwant, "page 1 RgbF64");
    assert_eq!(frames[2].planes[0].data, twant, "page 2 GrayF64 tiled");
}

#[test]
fn float_wrong_buffer_size_rejected() {
    let fpix = [1.0f32, 2.0, 3.0];
    let page = EncodePage {
        width: 2,
        height: 2, // expects 4 samples, got 3
        kind: EncodePixelFormat::GrayF32 { pixels: &fpix },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    assert!(encode_tiff(&page).is_err());
}
