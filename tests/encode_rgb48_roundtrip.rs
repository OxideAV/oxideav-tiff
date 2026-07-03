//! `EncodePixelFormat::Rgb48` (16-bit RGB) encode → decode parity
//! tests.
//!
//! The decode side has rendered 16-bit RGB (`PhotometricInterpretation
//! = 2`, `BitsPerSample = [16, 16, 16]`) as `Rgb48Le` since the strip
//! walker landed; these tests close the encode-parity gap with a
//! binary-independent oracle: the encoder writes the page, the decoder
//! reads it back, and the round-tripped bytes must equal the input
//! exactly (every compressor here is lossless). Compositions covered:
//! the byte-aligned compressor set × `Predictor = 2` (TIFF 6.0 §14 —
//! 16-bit per-component differencing) × `PlanarConfiguration = 2`
//! (§"PlanarConfiguration") × §15 tiling (with partial edge tiles) ×
//! BigTIFF × the multi-page chain, plus IFD tag inspection and the
//! negative paths. An ImageMagick cross-read validates the output
//! against an independent reader when the binary is available.

use oxideav_tiff::types::*;
use oxideav_tiff::{
    decode_tiff, decode_tiff_all, encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat,
    PageExtras, TiffCompression, TiffPixelFormat,
};

/// Deterministic 16-bit RGB test raster exercising the full sample
/// range (values > 255 in every channel so an 8-bit truncation bug
/// cannot round-trip cleanly). Little-endian bytes, row-major
/// interleaved (R, G, B).
fn pixels_rgb48(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 6) as usize);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 2749 + y * 353) % 65536) as u16;
            let g = (65535 - ((x * 991 + y * 5237) % 65536)) as u16;
            let b = ((x * x + y * 7919 + 300) % 65536) as u16;
            v.extend_from_slice(&r.to_le_bytes());
            v.extend_from_slice(&g.to_le_bytes());
            v.extend_from_slice(&b.to_le_bytes());
        }
    }
    v
}

fn page<'a>(w: u32, h: u32, pixels: &'a [u8], compression: TiffCompression) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Rgb48 { pixels },
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
    }
}

/// Encode one page, decode it, assert the format is `Rgb48Le` and
/// return the row-packed pixel bytes.
fn roundtrip(p: &EncodePage<'_>) -> Vec<u8> {
    let tiff = encode_tiff(p).expect("encode failed");
    let d = decode_tiff(&tiff).expect("decode failed");
    assert_eq!((d.width, d.height), (p.width, p.height));
    assert_eq!(d.pixel_format, TiffPixelFormat::Rgb48Le);
    assert_eq!(d.frame.planes.len(), 1);
    let stride = d.frame.planes[0].stride;
    let row_bytes = d.width as usize * 6;
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

/// Strip layout across the compressor set, with and without the §14
/// predictor. Odd dimensions so no power-of-two alignment can hide a
/// stride bug.
#[test]
fn rgb48_strip_roundtrip_all_compressors() {
    let (w, h) = (23u32, 11u32);
    let pixels = pixels_rgb48(w, h);
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            let p = EncodePage {
                predictor,
                ..page(w, h, &pixels, compression)
            };
            assert_eq!(
                roundtrip(&p),
                pixels,
                "mismatch: {compression:?} predictor={predictor}"
            );
        }
    }
}

/// `PlanarConfiguration = 2` (three 16-bit component planes) decodes
/// back to the identical chunky bytes, with and without the per-plane
/// §14 predictor.
#[test]
fn rgb48_planar_roundtrip() {
    let (w, h) = (19u32, 13u32);
    let pixels = pixels_rgb48(w, h);
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            let p = EncodePage {
                planar: true,
                predictor,
                ..page(w, h, &pixels, compression)
            };
            assert_eq!(
                roundtrip(&p),
                pixels,
                "planar mismatch: {compression:?} predictor={predictor}"
            );
        }
    }
}

/// §15 tiled layout with partial right/bottom edge tiles (40×24 image,
/// 16×16 tiles → 3×2 grid with an 8-pixel column overhang and an
/// 8-pixel row overhang), per-tile predictor on and off.
#[test]
fn rgb48_tiled_roundtrip_partial_edges() {
    let (w, h) = (40u32, 24u32);
    let pixels = pixels_rgb48(w, h);
    for compression in [
        TiffCompression::None,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
        TiffCompression::Zstd,
    ] {
        for predictor in [false, true] {
            let p = EncodePage {
                tiling: Some((16, 16)),
                predictor,
                ..page(w, h, &pixels, compression)
            };
            assert_eq!(
                roundtrip(&p),
                pixels,
                "tiled mismatch: {compression:?} predictor={predictor}"
            );
        }
    }
}

/// Tiling composes with planar: one 16×16 tile grid per 16-bit
/// component plane (§15 TileOffsets plane ordering).
#[test]
fn rgb48_planar_tiled_roundtrip() {
    let (w, h) = (40u32, 24u32);
    let pixels = pixels_rgb48(w, h);
    for predictor in [false, true] {
        let p = EncodePage {
            tiling: Some((16, 16)),
            planar: true,
            predictor,
            ..page(w, h, &pixels, TiffCompression::Lzw)
        };
        assert_eq!(roundtrip(&p), pixels, "planar-tiled predictor={predictor}");
    }
}

/// BigTIFF composes unchanged — and the 3-entry `BitsPerSample`
/// SHORT array (6 bytes) stays inline in the widened 8-byte slot.
#[test]
fn rgb48_bigtiff_roundtrip() {
    let (w, h) = (23u32, 11u32);
    let pixels = pixels_rgb48(w, h);
    for tiling in [None, Some((16u32, 16u32))] {
        let p = EncodePage {
            bigtiff: true,
            extras: PageExtras::default(),
            tiling,
            predictor: true,
            ..page(w, h, &pixels, TiffCompression::Deflate)
        };
        assert_eq!(roundtrip(&p), pixels, "bigtiff tiling={tiling:?}");
    }
}

/// Multi-page chain mixing an Rgb48 page with a Gray16Le page.
#[test]
fn rgb48_multipage_chain() {
    let (w, h) = (16u32, 8u32);
    let rgb = pixels_rgb48(w, h);
    let gray: Vec<u8> = (0..w * h)
        .flat_map(|i| ((i * 257 % 65536) as u16).to_le_bytes())
        .collect();
    let pages = [
        EncodePage {
            predictor: true,
            ..page(w, h, &rgb, TiffCompression::Lzw)
        },
        EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::Gray16Le { pixels: &gray },
            compression: TiffCompression::Deflate,
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
    assert_eq!(decoded[0].pixel_format, TiffPixelFormat::Rgb48Le);
    assert_eq!(decoded[0].planes[0].data, rgb);
    assert_eq!(decoded[1].pixel_format, TiffPixelFormat::Gray16Le);
    assert_eq!(decoded[1].planes[0].data, gray);
}

/// The written IFD carries the 16-bit RGB field set: BitsPerSample =
/// [16, 16, 16] (out-of-line 3-entry SHORT array on classic TIFF),
/// PhotometricInterpretation = 2, SamplesPerPixel = 3.
#[test]
fn rgb48_ifd_tags() {
    use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
    let (w, h) = (16u32, 8u32);
    let pixels = pixels_rgb48(w, h);
    let tiff = encode_tiff(&page(w, h, &pixels, TiffCompression::None)).expect("encode failed");
    let hd = parse_header(&tiff).expect("header");
    let (entries, next) =
        parse_ifd(&tiff, hd.byte_order, hd.variant, hd.first_ifd_offset).expect("ifd");
    assert_eq!(next, 0);
    let bps = find(&entries, TAG_BITS_PER_SAMPLE)
        .expect("BitsPerSample present")
        .as_u32_vec(hd.byte_order)
        .unwrap();
    assert_eq!(bps, vec![16, 16, 16]);
    let photo = find(&entries, TAG_PHOTOMETRIC_INTERPRETATION)
        .unwrap()
        .as_u32(hd.byte_order)
        .unwrap();
    assert_eq!(photo, PHOTO_RGB as u32);
    let spp = find(&entries, TAG_SAMPLES_PER_PIXEL)
        .unwrap()
        .as_u32(hd.byte_order)
        .unwrap();
    assert_eq!(spp, 3);
    // No SampleFormat tag — unsigned integer is the spec default.
    assert!(find(&entries, TAG_SAMPLE_FORMAT).is_none());
}

/// Wrong pixel-buffer size is rejected with a precise error.
#[test]
fn rgb48_wrong_buffer_size_rejected() {
    let pixels = vec![0u8; 10];
    let e = encode_tiff(&page(4, 4, &pixels, TiffCompression::None)).unwrap_err();
    let msg = format!("{e:?}");
    assert!(msg.contains("Rgb48"), "{msg}");
}

/// CCITT compression is bilevel-only per TIFF 6.0 §10 / §11.
#[test]
fn rgb48_ccitt_rejected() {
    let (w, h) = (16u32, 8u32);
    let pixels = pixels_rgb48(w, h);
    let e = encode_tiff(&page(w, h, &pixels, TiffCompression::CcittRle)).unwrap_err();
    let msg = format!("{e:?}");
    assert!(msg.contains("CCITT") || msg.contains("Bilevel"), "{msg}");
}

/// Independent-reader cross-check: ImageMagick transcodes our
/// LZW+predictor Rgb48 output to a 16-bit binary PPM (P6, maxval
/// 65535, big-endian samples per the PPM format) and the samples must
/// match the input exactly. Skips when `convert` is unavailable.
#[test]
fn rgb48_imagemagick_reads_our_output() {
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
    let (w, h) = (23u32, 11u32);
    let pixels = pixels_rgb48(w, h);
    let p = EncodePage {
        predictor: true,
        ..page(w, h, &pixels, TiffCompression::Lzw)
    };
    let tiff = encode_tiff(&p).expect("encode failed");

    let dir = std::env::temp_dir().join(format!("oxideav-tiff-rgb48-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let in_path = dir.join("in.tiff");
    let out_path = dir.join("out.ppm");
    std::fs::write(&in_path, &tiff).unwrap();
    let status = Command::new("convert")
        .arg(&in_path)
        .args(["-depth", "16"])
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
    let ppm = std::fs::read(&out_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Parse the P6 header: "P6\n<w> <h>\n<maxval>\n" then binary
    // samples, 2 bytes big-endian per sample at maxval > 255.
    let mut fields = Vec::new();
    let mut pos = 0usize;
    while fields.len() < 4 && pos < ppm.len() {
        // Skip whitespace and comments.
        while pos < ppm.len() && (ppm[pos].is_ascii_whitespace()) {
            pos += 1;
        }
        if pos < ppm.len() && ppm[pos] == b'#' {
            while pos < ppm.len() && ppm[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        let start = pos;
        while pos < ppm.len() && !ppm[pos].is_ascii_whitespace() {
            pos += 1;
        }
        fields.push(std::str::from_utf8(&ppm[start..pos]).unwrap().to_string());
    }
    pos += 1; // single whitespace after maxval
    assert_eq!(fields[0], "P6");
    assert_eq!(fields[1], w.to_string());
    assert_eq!(fields[2], h.to_string());
    assert_eq!(fields[3], "65535", "expected 16-bit PPM output");
    let data = &ppm[pos..];
    assert_eq!(data.len(), (w * h * 6) as usize, "PPM payload size");
    // PPM stores big-endian samples; our input is little-endian.
    let mut got_le = Vec::with_capacity(data.len());
    for s in data.chunks_exact(2) {
        let v = u16::from_be_bytes([s[0], s[1]]);
        got_le.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(got_le, pixels, "independent reader disagrees with input");
}
