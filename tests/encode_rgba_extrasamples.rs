//! `EncodePixelFormat::Rgba32` (§ExtraSamples RGBA) encode → decode
//! parity tests.
//!
//! TIFF 6.0 §ExtraSamples (tag 338, pages 31–32): an RGB page may
//! carry extra components as "the 'last components' in each pixel",
//! and "this field must be present if there are extra samples". The
//! decode side has handled all three defined kinds (0 unspecified /
//! 1 associated alpha / 2 unassociated alpha) since
//! `tests/decode_extra_samples.rs` landed — rendering the leading
//! `R G B` triple verbatim and dropping the extra — so the decoder is
//! a trusted oracle for the RGB portion of the round-trip, and an
//! ImageMagick PAM cross-read validates all four channels against an
//! independent reader.

use oxideav_tiff::types::*;
use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, ExtraSampleKind, PageExtras,
    TiffCompression, TiffPixelFormat,
};

/// Deterministic RGBA raster; alpha varies independently of color.
fn pixels_rgba(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x * 11 + y * 3) % 256) as u8);
            v.push(((x * 5 + y * 17) % 256) as u8);
            v.push(((x ^ y) * 7 % 256) as u8);
            v.push(((x * 29 + y * 53 + 100) % 256) as u8);
        }
    }
    v
}

/// The RGB triples of an RGBA raster — the decoder's §ExtraSamples
/// render (leading color components verbatim, extra dropped).
fn rgb_of(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .flat_map(|q| [q[0], q[1], q[2]])
        .collect()
}

fn page<'a>(
    w: u32,
    h: u32,
    pixels: &'a [u8],
    kind: ExtraSampleKind,
    compression: TiffCompression,
) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Rgba32 { pixels, kind },
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
    }
}

fn decode_rgb(tiff: &[u8]) -> Vec<u8> {
    let d = decode_tiff(tiff).expect("decode failed");
    assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
    let stride = d.frame.planes[0].stride;
    let row_bytes = d.width as usize * 3;
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

const KINDS: [ExtraSampleKind; 3] = [
    ExtraSampleKind::Unspecified,
    ExtraSampleKind::AssociatedAlpha,
    ExtraSampleKind::UnassociatedAlpha,
];

/// Strip layout: all three §ExtraSamples kinds × the compressor set ×
/// the §14 predictor (offset SamplesPerPixel = 4). The decoder
/// renders the leading RGB verbatim for every kind.
#[test]
fn rgba_strip_roundtrip() {
    let (w, h) = (23u32, 11u32);
    let rgba = pixels_rgba(w, h);
    let want = rgb_of(&rgba);
    for kind in KINDS {
        for compression in COMPRESSORS {
            for predictor in [false, true] {
                let p = EncodePage {
                    predictor,
                    ..page(w, h, &rgba, kind, compression)
                };
                let tiff = encode_tiff(&p).expect("encode failed");
                assert_eq!(
                    decode_rgb(&tiff),
                    want,
                    "rgba {kind:?} {compression:?} predictor={predictor}"
                );
            }
        }
    }
}

/// §15 tiling (partial edges) and `PlanarConfiguration = 2` (four
/// component planes) compose.
#[test]
fn rgba_tiled_and_planar_roundtrip() {
    let (w, h) = (40u32, 24u32);
    let rgba = pixels_rgba(w, h);
    let want = rgb_of(&rgba);
    for predictor in [false, true] {
        let p = EncodePage {
            tiling: Some((16, 16)),
            predictor,
            ..page(
                w,
                h,
                &rgba,
                ExtraSampleKind::UnassociatedAlpha,
                TiffCompression::Lzw,
            )
        };
        assert_eq!(
            decode_rgb(&encode_tiff(&p).expect("tiled encode")),
            want,
            "tiled predictor={predictor}"
        );
        let p = EncodePage {
            planar: true,
            predictor,
            ..page(
                w,
                h,
                &rgba,
                ExtraSampleKind::UnassociatedAlpha,
                TiffCompression::Deflate,
            )
        };
        assert_eq!(
            decode_rgb(&encode_tiff(&p).expect("planar encode")),
            want,
            "planar predictor={predictor}"
        );
    }
}

/// BigTIFF composes; the 4-entry BitsPerSample array (8 bytes) fits
/// exactly at the widened inline threshold.
#[test]
fn rgba_bigtiff_roundtrip() {
    let (w, h) = (23u32, 11u32);
    let rgba = pixels_rgba(w, h);
    let p = EncodePage {
        bigtiff: true,
        extras: PageExtras::default(),
        ..page(
            w,
            h,
            &rgba,
            ExtraSampleKind::AssociatedAlpha,
            TiffCompression::Zstd,
        )
    };
    assert_eq!(decode_rgb(&encode_tiff(&p).expect("encode")), rgb_of(&rgba));
}

/// The written IFD carries the §ExtraSamples field set, and the raw
/// strip stores all four channels verbatim (the alpha bytes survive
/// on disk even though the crate's own render drops them).
#[test]
fn rgba_ifd_tags_and_raw_bytes() {
    use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
    let (w, h) = (8u32, 4u32);
    let rgba = pixels_rgba(w, h);
    for (kind, tag_value) in [
        (ExtraSampleKind::Unspecified, EXTRA_SAMPLE_UNSPECIFIED),
        (
            ExtraSampleKind::AssociatedAlpha,
            EXTRA_SAMPLE_ASSOCIATED_ALPHA,
        ),
        (
            ExtraSampleKind::UnassociatedAlpha,
            EXTRA_SAMPLE_UNASSOCIATED_ALPHA,
        ),
    ] {
        let tiff =
            encode_tiff(&page(w, h, &rgba, kind, TiffCompression::None)).expect("encode failed");
        let hd = parse_header(&tiff).unwrap();
        let (entries, _) =
            parse_ifd(&tiff, hd.byte_order, hd.variant, hd.first_ifd_offset).unwrap();
        let es = find(&entries, TAG_EXTRA_SAMPLES)
            .expect("ExtraSamples present")
            .as_u32_vec(hd.byte_order)
            .unwrap();
        assert_eq!(es, vec![tag_value as u32], "{kind:?}");
        let bps = find(&entries, TAG_BITS_PER_SAMPLE)
            .unwrap()
            .as_u32_vec(hd.byte_order)
            .unwrap();
        assert_eq!(bps, vec![8, 8, 8, 8]);
        let spp = find(&entries, TAG_SAMPLES_PER_PIXEL)
            .unwrap()
            .as_u32(hd.byte_order)
            .unwrap();
        assert_eq!(spp, 4);
        let off = find(&entries, TAG_STRIP_OFFSETS)
            .unwrap()
            .as_u32(hd.byte_order)
            .unwrap() as usize;
        let len = find(&entries, TAG_STRIP_BYTE_COUNTS)
            .unwrap()
            .as_u32(hd.byte_order)
            .unwrap() as usize;
        assert_eq!(&tiff[off..off + len], rgba.as_slice(), "raw RGBA bytes");
    }
}

/// Negative paths: wrong buffer size and CCITT.
#[test]
fn rgba_rejections() {
    let rgba = pixels_rgba(4, 4);
    let e = encode_tiff(&page(
        8,
        8,
        &rgba,
        ExtraSampleKind::UnassociatedAlpha,
        TiffCompression::None,
    ))
    .unwrap_err();
    assert!(format!("{e:?}").contains("Rgba32"), "{e:?}");
    let e = encode_tiff(&page(
        4,
        4,
        &rgba,
        ExtraSampleKind::UnassociatedAlpha,
        TiffCompression::CcittRle,
    ))
    .unwrap_err();
    assert!(format!("{e:?}").contains("Bilevel"), "{e:?}");
}

/// Independent-reader cross-check: ImageMagick transcodes our
/// unassociated-alpha LZW+predictor output to a PAM (P7, RGB_ALPHA)
/// and all four channels must match the input byte-for-byte. Skips
/// when `convert` is unavailable.
#[test]
fn rgba_imagemagick_reads_our_output() {
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
    let rgba = pixels_rgba(w, h);
    let p = EncodePage {
        predictor: true,
        ..page(
            w,
            h,
            &rgba,
            ExtraSampleKind::UnassociatedAlpha,
            TiffCompression::Lzw,
        )
    };
    let tiff = encode_tiff(&p).expect("encode failed");

    let dir = std::env::temp_dir().join(format!("oxideav-tiff-rgba-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let in_path = dir.join("in.tiff");
    let out_path = dir.join("out.pam");
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
    let pam = std::fs::read(&out_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Parse the P7 header: token/value lines until ENDHDR.
    let hdr_end = pam
        .windows(7)
        .position(|w| w == b"ENDHDR\n")
        .expect("PAM ENDHDR");
    let header = std::str::from_utf8(&pam[..hdr_end]).unwrap();
    let field = |name: &str| -> Option<String> {
        header
            .lines()
            .find(|l| l.starts_with(name))
            .map(|l| l[name.len()..].trim().to_string())
    };
    assert!(header.starts_with("P7"), "not a PAM: {header:?}");
    assert_eq!(field("WIDTH").unwrap(), w.to_string());
    assert_eq!(field("HEIGHT").unwrap(), h.to_string());
    assert_eq!(field("DEPTH").unwrap(), "4", "expected 4-channel PAM");
    assert_eq!(field("MAXVAL").unwrap(), "255");
    assert_eq!(field("TUPLTYPE").unwrap(), "RGB_ALPHA");
    let data = &pam[hdr_end + 7..];
    assert_eq!(data.len(), (w * h * 4) as usize);
    assert_eq!(data, rgba.as_slice(), "independent reader disagrees");
}
