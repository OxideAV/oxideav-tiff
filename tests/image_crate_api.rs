//! The image-crate contract surface (`IMAGE_CRATE_API`): root
//! vocabulary, native layouts, `to_rgb8` / `to_rgba8` kernels, limits,
//! metadata round-trips and the deprecated pre-contract wrappers.
//!
//! Everything here builds with `default-features = false`; the
//! registry-gated bridge has its own tests in `src/registry.rs`.

use std::io::Cursor;

use oxideav_tiff::{
    decode, decode_all, decode_from, decode_page, decode_rgb8, decode_rgba8, decode_with, encode,
    encode_page, encode_rgb8, encode_rgba8, encode_to, info, probe, ColorInfo, ColorRange,
    DecodeOptions, EncodeOptions, EncodePage, EncodePixelFormat, Error, ExtraSampleKind, Metadata,
    PageExtras, Palette, PixelFormat, Plane, TiffCompression, TiffImage,
};

fn ramp(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

/// A minimal ICC profile: 128-byte header whose size field matches.
fn icc_blob() -> Vec<u8> {
    let mut p = vec![0u8; 140];
    p[..4].copy_from_slice(&(140u32).to_be_bytes());
    p[36..40].copy_from_slice(b"acsp");
    p
}

/// A hand-built Exif TIFF payload: little-endian classic TIFF, IFD0
/// with the ExifIFDPointer only, Exif IFD with `DateTimeOriginal`
/// (0x9003, ASCII × 20) and `ExposureTime` (0x829A, RATIONAL 1/125).
fn exif_blob() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    // IFD0: one entry → Exif IFD at 26.
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&34665u16.to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&26u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(v.len(), 26);
    // Exif IFD: 2 entries = 2 + 24 + 4 = 30 bytes → data at 56.
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&0x829Au16.to_le_bytes());
    v.extend_from_slice(&5u16.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&56u32.to_le_bytes());
    v.extend_from_slice(&0x9003u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&20u32.to_le_bytes());
    v.extend_from_slice(&64u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(v.len(), 56);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&125u32.to_le_bytes());
    v.extend_from_slice(b"2026:10:04 12:00:00\0");
    v
}

/// A classic little-endian TIFF with its IFD at offset 8 and `tail`
/// appended right after it (so callers know the tail starts at
/// `8 + 2 + 12 × entries + 4`).
fn classic(entries: &[(u16, u16, u32, u32)], tail: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for &(tag, ty, cnt, val) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&cnt.to_le_bytes());
        v.extend_from_slice(&val.to_le_bytes());
    }
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(tail);
    v
}

fn first_ifd_offset(tiff: &[u8]) -> u64 {
    u32::from_le_bytes([tiff[4], tiff[5], tiff[6], tiff[7]]) as u64
}

fn image(format: PixelFormat, w: u32, h: u32) -> TiffImage {
    let bpp = format.bytes_per_pixel();
    let data = ramp(w as usize * h as usize * bpp, 3);
    let mut img = TiffImage::new(w, h, format, vec![Plane::new(w as usize * bpp, data)]).unwrap();
    if format == PixelFormat::Pal8 {
        img.palette = Some(Palette::new(
            (0..=255u8).map(|i| [i, 255 - i, i ^ 0x55, 255]).collect(),
        ));
    }
    img
}

// ---- probe / info -----------------------------------------------------------

#[test]
fn probe_recognises_every_header_and_nothing_else() {
    assert!(probe(b"II\x2a\x00\x08\x00\x00\x00"));
    assert!(probe(b"MM\x00\x2a\x00\x00\x00\x08"));
    assert!(probe(b"II\x2b\x00"));
    assert!(probe(b"MM\x00\x2b"));
    assert!(!probe(b"II\x2a"));
    assert!(!probe(b""));
    assert!(!probe(b"\x89PNG\r\n\x1a\n"));
    assert!(!probe(b"MM\x2a\x00"));
}

#[test]
fn info_matches_decode_without_touching_pixels() {
    for format in [
        PixelFormat::Gray8,
        PixelFormat::Gray16Le,
        PixelFormat::Rgb24,
        PixelFormat::Rgb48Le,
        PixelFormat::Rgba,
        PixelFormat::Pal8,
        PixelFormat::Cmyk,
    ] {
        let img = image(format, 5, 3);
        let bytes = encode(&img, &EncodeOptions::default()).unwrap();
        let i = info(&bytes).unwrap();
        let d = decode(&bytes).unwrap();
        assert_eq!((i.width, i.height), (5, 3), "{format:?}");
        assert_eq!(i.format, d.format, "{format:?}");
        assert_eq!(i.frames, 1);
        assert_eq!(i.has_alpha, format == PixelFormat::Rgba, "{format:?}");
        assert_eq!(i.color, d.color);
        assert!(!i.has_icc && !i.has_exif && !i.has_xmp);
        assert_eq!(i.compression, 1);
        assert!(!i.planar && !i.tiled && !i.big_tiff);
    }
    // A page whose strip lies past EOF: `info` succeeds (header + IFD
    // only), `decode` fails — the pixels are never read by `info`.
    let dangling = classic(
        &[
            (256, 4, 1, 1),
            (257, 4, 1, 1),
            (258, 3, 1, 8),
            (259, 3, 1, 1),
            (262, 3, 1, 1),
            (273, 4, 1, 5000),
            (277, 3, 1, 1),
            (278, 4, 1, 1),
            (279, 4, 1, 1),
        ],
        &[],
    );
    let i = info(&dangling).unwrap();
    assert_eq!((i.width, i.height, i.format), (1, 1, PixelFormat::Gray8));
    assert!(matches!(decode(&dangling), Err(Error::InvalidData(_))));
}

#[test]
fn info_counts_pages_and_sees_metadata() {
    let a = image(PixelFormat::Gray8, 4, 4).with_metadata(
        Metadata::new()
            .with_icc(icc_blob())
            .with_xmp(b"<x:xmpmeta/>".to_vec())
            .with_exif(exif_blob()),
    );
    let one = encode(&a, &EncodeOptions::default()).unwrap();
    let i = info(&one).unwrap();
    assert!(i.has_icc && i.has_xmp && i.has_exif);
    // Multi-page through the depth API.
    let px = ramp(16, 1);
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::Gray8 { pixels: &px },
        compression: TiffCompression::Lzw,
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: true,
        extras: PageExtras {
            page_number: Some((0, 2)),
            multi_page: true,
            ..PageExtras::default()
        },
    };
    let two = oxideav_tiff::encode_pages(&[page.clone(), page]).unwrap();
    let i = info(&two).unwrap();
    assert_eq!(i.frames, 2);
    assert!(i.big_tiff);
    assert_eq!(i.compression, 5);
    let frames = decode_all(&two).unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].index, 1);
    assert_eq!(frames[1].page_number, Some((0, 2)));
    assert_eq!(frames[1].new_subfile_type, Some(2));
    assert!(frames.iter().all(|f| f.delay.is_none()));
    assert_eq!(frames[0].image.planes[0].data, px);
}

// ---- decode / convenience paths -----------------------------------------------

#[test]
fn rgb8_and_rgba8_paths_match_to_rgb8() {
    let img = image(PixelFormat::Rgba, 6, 2);
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    let rgb = decode_rgb8(&bytes).unwrap();
    let rgba = decode_rgba8(&bytes).unwrap();
    assert_eq!((rgb.width, rgb.height), (6, 2));
    assert_eq!(rgb.data, img.to_rgb8());
    assert_eq!(rgba.data, img.to_rgba8());
    assert_eq!(rgba.data, img.planes[0].data, "Rgba source copies through");
    assert_eq!(rgb.stride(), 18);
    assert_eq!(rgba.as_bytes().len(), 48);
    let from_reader = decode_from(Cursor::new(&bytes)).unwrap();
    assert_eq!(from_reader, decode(&bytes).unwrap());
}

#[test]
fn to_rgb8_kernels_are_exact() {
    // Gray8 / Gray16Le: replicate; 16-bit keeps the high byte.
    let g8 = TiffImage::new(2, 1, PixelFormat::Gray8, vec![Plane::new(2, vec![7, 200])]).unwrap();
    assert_eq!(g8.to_rgb8(), vec![7, 7, 7, 200, 200, 200]);
    assert_eq!(g8.to_rgba8(), vec![7, 7, 7, 255, 200, 200, 200, 255]);
    let g16 = TiffImage::new(
        1,
        1,
        PixelFormat::Gray16Le,
        vec![Plane::new(2, vec![0xFF, 0x12])],
    )
    .unwrap();
    assert_eq!(g16.to_rgb8(), vec![0x12, 0x12, 0x12]);
    // Rgb48Le: high byte per channel.
    let r48 = TiffImage::new(
        1,
        1,
        PixelFormat::Rgb48Le,
        vec![Plane::new(6, vec![0x01, 0xAA, 0x02, 0xBB, 0x03, 0xCC])],
    )
    .unwrap();
    assert_eq!(r48.to_rgb8(), vec![0xAA, 0xBB, 0xCC]);
    // Rgba: alpha dropped / kept.
    let rgba = TiffImage::from_rgba8(1, 1, vec![1, 2, 3, 4]);
    assert_eq!(rgba.to_rgb8(), vec![1, 2, 3]);
    assert_eq!(rgba.to_rgba8(), vec![1, 2, 3, 4]);
    // Pal8: lookup; out-of-range index is black (opaque).
    let pal = TiffImage::new(3, 1, PixelFormat::Pal8, vec![Plane::new(3, vec![1, 0, 9])])
        .unwrap()
        .with_palette(Palette::new(vec![[10, 11, 12, 255], [20, 21, 22, 128]]));
    assert_eq!(pal.to_rgb8(), vec![20, 21, 22, 10, 11, 12, 0, 0, 0]);
    assert_eq!(
        pal.to_rgba8(),
        vec![20, 21, 22, 128, 10, 11, 12, 255, 0, 0, 0, 255]
    );
    assert!(pal.has_alpha());
    // Cmyk: §16 inversion. Pure cyan → (0, 255, 255); K = 255 → black;
    // C = M = Y = K = 0 → white; half ink everywhere → 127·127/255.
    let cmyk = TiffImage::new(
        4,
        1,
        PixelFormat::Cmyk,
        vec![Plane::new(
            16,
            vec![255, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0, 128, 128, 128, 128],
        )],
    )
    .unwrap();
    let v = (127u32 * 127 / 255) as u8;
    assert_eq!(
        cmyk.to_rgb8(),
        vec![0, 255, 255, 0, 0, 0, 255, 255, 255, v, v, v]
    );
    // Row padding is honoured.
    let padded = TiffImage::new(
        1,
        2,
        PixelFormat::Gray8,
        vec![Plane::new(3, vec![1, 9, 9, 2, 9, 9])],
    )
    .unwrap();
    assert_eq!(padded.to_rgb8(), vec![1, 1, 1, 2, 2, 2]);
    assert_eq!(padded.clone().into_raw(), vec![1, 9, 9, 2, 9, 9]);
    assert_eq!(padded.as_bytes().map(|b| b.len()), Some(6));
}

#[test]
fn new_validates_geometry() {
    assert!(matches!(
        TiffImage::new(0, 1, PixelFormat::Gray8, vec![Plane::new(1, vec![0])]),
        Err(Error::InvalidData(_))
    ));
    assert!(matches!(
        TiffImage::new(2, 1, PixelFormat::Rgb24, vec![Plane::new(5, vec![0; 5])]),
        Err(Error::InvalidData(_))
    ));
    assert!(matches!(
        TiffImage::new(2, 2, PixelFormat::Rgb24, vec![Plane::new(6, vec![0; 11])]),
        Err(Error::InvalidData(_))
    ));
    assert!(matches!(
        TiffImage::new(1, 1, PixelFormat::Gray8, vec![]),
        Err(Error::InvalidData(_))
    ));
    assert!(TiffImage::new(2, 2, PixelFormat::Rgb24, vec![Plane::new(8, vec![0; 16])]).is_ok());
    // A short caller-assembled buffer still renders (zeros past the end)
    // and is refused by the encoder rather than panicking.
    let short = TiffImage::from_rgb8(2, 1, vec![1, 2, 3]);
    assert_eq!(short.to_rgb8(), vec![1, 2, 3, 0, 0, 0]);
    assert!(matches!(
        encode(&short, &EncodeOptions::default()),
        Err(Error::InvalidData(_))
    ));
}

// ---- encode -------------------------------------------------------------------

#[test]
fn lossless_round_trip_every_native_layout_with_metadata() {
    let meta = Metadata::new()
        .with_icc(icc_blob())
        .with_xmp(b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>".to_vec())
        .with_exif(exif_blob());
    for format in [
        PixelFormat::Gray8,
        PixelFormat::Gray16Le,
        PixelFormat::Rgb24,
        PixelFormat::Rgb48Le,
        PixelFormat::Rgba,
        PixelFormat::Pal8,
        PixelFormat::Cmyk,
    ] {
        let img = image(format, 7, 5).with_metadata(meta.clone());
        let single_sample = matches!(
            format,
            PixelFormat::Gray8 | PixelFormat::Gray16Le | PixelFormat::Pal8
        );
        for (label, opts) in [
            ("none", EncodeOptions::default()),
            (
                "lzw+predictor+strips",
                EncodeOptions::default()
                    .with_compression(TiffCompression::Lzw)
                    .with_predictor(true)
                    .with_rows_per_strip(2),
            ),
            (
                "zstd tiled bigtiff",
                EncodeOptions::default()
                    .with_compression(TiffCompression::Zstd)
                    .with_tiling((16, 16))
                    .with_bigtiff(true),
            ),
            (
                "deflate planar",
                EncodeOptions::default()
                    .with_compression(TiffCompression::Deflate)
                    // PlanarConfiguration = 2 is undefined for one
                    // sample per pixel (TIFF 6.0 §PlanarConfiguration).
                    .with_planar(!single_sample),
            ),
        ] {
            let bytes = encode(&img, &opts).unwrap_or_else(|e| panic!("{format:?} {label}: {e}"));
            let back = decode(&bytes).unwrap_or_else(|e| panic!("{format:?} {label}: {e}"));
            assert_eq!(back, img, "{format:?} {label}");
            // Streaming variant writes the same bytes.
            let mut out = Vec::new();
            encode_to(&img, &opts, &mut out).unwrap();
            assert_eq!(out, bytes);
        }
    }
}

#[test]
fn encode_rgb8_and_rgba8_pick_the_natural_layouts() {
    let rgb = ramp(4 * 3 * 3, 9);
    let bytes = encode_rgb8(4, 3, &rgb, &EncodeOptions::default()).unwrap();
    let img = decode(&bytes).unwrap();
    assert_eq!(img.format, PixelFormat::Rgb24);
    assert_eq!(img.planes[0].data, rgb);
    assert_eq!(info(&bytes).unwrap().samples_per_pixel, 3);

    let rgba = ramp(4 * 3 * 4, 11);
    let bytes = encode_rgba8(4, 3, &rgba, &EncodeOptions::default()).unwrap();
    let img = decode(&bytes).unwrap();
    assert_eq!(
        img.format,
        PixelFormat::Rgba,
        "alpha is kept (ExtraSamples = 2)"
    );
    assert_eq!(img.planes[0].data, rgba);
    assert!(info(&bytes).unwrap().has_alpha);
    // The decoded page reports the ExtraSamples tag the writer emitted.
    assert_eq!(decode_page(&bytes).unwrap().layout.samples_per_pixel, 4);
}

#[test]
fn encode_refuses_what_tiff_cannot_carry() {
    // Palette alpha has no TIFF mechanism.
    let mut pal = image(PixelFormat::Pal8, 2, 2);
    pal.palette = Some(Palette::new(vec![[1, 2, 3, 0]; 256]));
    assert!(matches!(
        encode(&pal, &EncodeOptions::default()),
        Err(Error::Unsupported(_))
    ));
    // Pal8 without a palette is an invalid image.
    pal.palette = None;
    assert!(matches!(
        encode(&pal, &EncodeOptions::default()),
        Err(Error::InvalidData(_))
    ));
    // WebP carries 8-bit RGB / RGBA only.
    let g = image(PixelFormat::Gray8, 2, 2);
    assert!(encode(
        &g,
        &EncodeOptions::default().with_compression(TiffCompression::Webp)
    )
    .is_err());
    // Metadata that is not an Exif TIFF payload is InvalidData.
    let bad = g
        .clone()
        .with_metadata(Metadata::new().with_exif(b"Exif\0\0junk".to_vec()));
    assert!(matches!(
        encode(&bad, &EncodeOptions::default()),
        Err(Error::InvalidData(_))
    ));
    // …and is skipped entirely when embedding is off.
    assert!(encode(&bad, &EncodeOptions::default().with_embed_exif(false)).is_ok());
}

#[test]
fn encode_options_metadata_switches_and_software() {
    let img = image(PixelFormat::Rgb24, 3, 3).with_metadata(
        Metadata::new()
            .with_icc(icc_blob())
            .with_xmp(b"<x/>".to_vec())
            .with_exif(exif_blob()),
    );
    let opts = EncodeOptions::default()
        .with_embed_icc(false)
        .with_embed_xmp(false)
        .with_embed_exif(false)
        .with_software(Some("oxideav-tiff contract test".to_string()));
    let bytes = encode(&img, &opts).unwrap();
    let i = info(&bytes).unwrap();
    assert!(!i.has_icc && !i.has_xmp && !i.has_exif);
    let page = decode_page(&bytes).unwrap();
    assert!(page.image.metadata.is_empty());
    assert_eq!(
        page.metadata.software.as_deref(),
        Some("oxideav-tiff contract test")
    );
}

// ---- colour -------------------------------------------------------------------

#[test]
fn color_defaults_and_colorimetry_detection() {
    let bytes = encode(&image(PixelFormat::Rgb24, 2, 2), &EncodeOptions::default()).unwrap();
    let img = decode(&bytes).unwrap();
    assert_eq!(img.color, ColorInfo::tiff_default());
    assert_eq!(img.color.range, ColorRange::Full);
    assert_eq!(img.color.matrix, ColorInfo::MATRIX_IDENTITY);
    assert_eq!(img.color.primaries, ColorInfo::UNSPECIFIED);
    assert!(img.metadata.gamma.is_none());

    // A hand-built page with BT.709 WhitePoint + PrimaryChromaticities
    // (the TIFF 6.0 §20 example values) resolves to primaries 1.
    // IFD: 11 entries → tail starts at 8 + 2 + 132 + 4 = 146.
    let mut tail = Vec::new();
    for (num, den) in [(3127u32, 10000u32), (3290, 10000)] {
        tail.extend_from_slice(&num.to_le_bytes());
        tail.extend_from_slice(&den.to_le_bytes());
    }
    for (num, den) in [
        (640u32, 1000u32),
        (330, 1000),
        (300, 1000),
        (600, 1000),
        (150, 1000),
        (60, 1000),
    ] {
        tail.extend_from_slice(&num.to_le_bytes());
        tail.extend_from_slice(&den.to_le_bytes());
    }
    tail.push(42); // the one Gray8 pixel at 146 + 64 = 210
    let v = classic(
        &[
            (256, 4, 1, 1),
            (257, 4, 1, 1),
            (258, 3, 1, 8),
            (259, 3, 1, 1),
            (262, 3, 1, 1),
            (273, 4, 1, 210),
            (277, 3, 1, 1),
            (278, 4, 1, 1),
            (279, 4, 1, 1),
            (318, 5, 2, 146), // WhitePoint → 2 RATIONALs
            (319, 5, 6, 162), // PrimaryChromaticities → 6 RATIONALs
        ],
        &tail,
    );
    let img = decode(&v).unwrap();
    assert_eq!(img.color.primaries, ColorInfo::PRIMARIES_BT709);
    assert_eq!(img.color.transfer, ColorInfo::UNSPECIFIED);
    assert_eq!(info(&v).unwrap().color, img.color);
    assert_eq!(img.planes[0].data, vec![42]);
}

// ---- limits / strictness --------------------------------------------------------

#[test]
fn decode_options_limits_fire_before_allocation() {
    let img = image(PixelFormat::Rgb24, 8, 4);
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    for (label, opts) in [
        ("width", DecodeOptions::default().with_max_width(7)),
        ("height", DecodeOptions::default().with_max_height(3)),
        ("pixels", DecodeOptions::default().with_max_pixels(31)),
        ("bytes", DecodeOptions::default().with_max_bytes(95)),
    ] {
        assert!(
            matches!(decode_with(&bytes, &opts), Err(Error::LimitExceeded(_))),
            "{label}"
        );
    }
    assert!(decode_with(&bytes, &DecodeOptions::default().with_max_pixels(32)).is_ok());
    assert!(decode_with(&bytes, &DecodeOptions::default().with_max_bytes(96)).is_ok());
    assert!(decode_with(&bytes, &DecodeOptions::default().unlimited()).is_ok());
    assert_eq!(
        DecodeOptions::default().max_pixels,
        Some(DecodeOptions::DEFAULT_MAX_PIXELS)
    );
    assert_eq!(
        DecodeOptions::default().max_bytes,
        Some(DecodeOptions::DEFAULT_MAX_BYTES)
    );
    // The limits are the IFD's claims, so a tiny file is rejected on
    // its declared geometry alone (nothing allocated).
    let hostile = classic(
        &[
            (256, 4, 1, 40000),
            (257, 4, 1, 40000),
            (258, 3, 1, 8),
            (259, 3, 1, 1),
            (262, 3, 1, 1),
            (273, 4, 1, 200),
            (277, 3, 1, 1),
            (278, 4, 1, 40000),
            (279, 4, 1, 1),
        ],
        &[0; 100],
    );
    assert!(matches!(decode(&hostile), Err(Error::LimitExceeded(_))));
    assert!(
        info(&hostile).is_ok(),
        "info does not apply the decode limits"
    );
}

#[test]
fn strict_rejects_the_sample_format_shorthand() {
    // SampleFormat with a single entry on a 3-sample RGB page.
    let img = image(PixelFormat::Rgb24, 2, 2);
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    // Hand-build the page: 10 entries → tail at 8 + 2 + 120 + 4 = 134.
    let mut tail = Vec::new();
    for _ in 0..3 {
        tail.extend_from_slice(&8u16.to_le_bytes());
    }
    tail.extend_from_slice(&ramp(12, 1));
    let v = classic(
        &[
            (256, 4, 1, 2),
            (257, 4, 1, 2),
            (258, 3, 3, 134), // BitsPerSample [8,8,8]
            (259, 3, 1, 1),
            (262, 3, 1, 2),
            (273, 4, 1, 140),
            (277, 3, 1, 3),
            (278, 4, 1, 2),
            (279, 4, 1, 12),
            (339, 3, 1, 1), // SampleFormat shorthand: one entry for three samples
        ],
        &tail,
    );
    assert!(decode(&v).is_ok(), "lenient accepts the shorthand");
    assert!(matches!(
        decode_with(&v, &DecodeOptions::default().with_strict(true)),
        Err(Error::InvalidData(_))
    ));
    // Strict mode changes nothing for a conforming file.
    assert_eq!(
        decode_with(&bytes, &DecodeOptions::default().with_strict(true)).unwrap(),
        img
    );
}

// ---- errors ---------------------------------------------------------------------

#[test]
fn error_shape() {
    let e: Error = std::io::Error::other("boom").into();
    assert!(matches!(e, Error::Io(_)));
    assert!(e.to_string().contains("boom"));
    assert!(std::error::Error::source(&e).is_some());
    struct Failing;
    impl std::io::Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("reader broke"))
        }
    }
    assert!(matches!(decode_from(Failing), Err(Error::Io(_))));
    assert!(matches!(decode(b"not a tiff"), Err(Error::InvalidData(_))));
    assert!(matches!(info(b""), Err(Error::InvalidData(_))));
}

// ---- deprecated pre-contract wrappers -------------------------------------------

#[test]
#[allow(deprecated)]
fn deprecated_wrappers_keep_the_historical_shapes() {
    use oxideav_tiff::{
        decode_tiff, decode_tiff_all, decode_tiff_all_pages, decode_tiff_at, encode_tiff,
        encode_tiff_multi, DecodedTiff, TiffPlane,
    };
    let _plane: TiffPlane = Plane::new(1, vec![0]);
    // Palette pages come back flattened to Rgb24 exactly as before.
    let pal = image(PixelFormat::Pal8, 3, 2);
    let bytes = encode(&pal, &EncodeOptions::default()).unwrap();
    let d: DecodedTiff = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (3, 2));
    assert_eq!(d.pixel_format, PixelFormat::Rgb24);
    assert_eq!(d.frame.planes[0].data, pal.to_rgb8());
    assert_eq!(d.format.photometric, Some(3));
    let all = decode_tiff_all(&bytes).unwrap();
    assert_eq!(all[0].format, PixelFormat::Rgb24);
    assert_eq!(decode_tiff_all_pages(&bytes).unwrap().len(), 1);
    assert_eq!(
        decode_tiff_at(&bytes, first_ifd_offset(&bytes))
            .unwrap()
            .frame
            .planes[0]
            .data,
        pal.to_rgb8()
    );
    // Rgba / Cmyk flatten too.
    let rgba = image(PixelFormat::Rgba, 2, 2);
    let d = decode_tiff(&encode(&rgba, &EncodeOptions::default()).unwrap()).unwrap();
    assert_eq!(d.pixel_format, PixelFormat::Rgb24);
    assert_eq!(d.frame.planes[0].data, rgba.to_rgb8());
    let cmyk = image(PixelFormat::Cmyk, 2, 2);
    let d = decode_tiff(&encode(&cmyk, &EncodeOptions::default()).unwrap()).unwrap();
    assert_eq!(d.pixel_format, PixelFormat::Rgb24);
    assert_eq!(d.frame.planes[0].data, cmyk.to_rgb8());
    // The encode wrappers are the depth functions under their old names.
    let px = ramp(4, 0);
    let page = EncodePage {
        width: 2,
        height: 2,
        kind: EncodePixelFormat::Gray8 { pixels: &px },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
    };
    assert_eq!(encode_tiff(&page).unwrap(), encode_page(&page).unwrap());
    assert_eq!(
        encode_tiff_multi(std::slice::from_ref(&page)).unwrap(),
        encode_page(&page).unwrap()
    );
    // Rgba32 with the other extra-sample kinds still flattens to Rgb24
    // in the contract decode as well (no straight-alpha layout exists
    // for pre-multiplied or unspecified extras).
    let rgba_px = ramp(16, 2);
    for kind in [
        ExtraSampleKind::Unspecified,
        ExtraSampleKind::AssociatedAlpha,
    ] {
        let p = EncodePage {
            width: 2,
            height: 2,
            kind: EncodePixelFormat::Rgba32 {
                pixels: &rgba_px,
                kind,
            },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
            extras: PageExtras::default(),
        };
        let img = decode(&encode_page(&p).unwrap()).unwrap();
        assert_eq!(img.format, PixelFormat::Rgb24, "{kind:?}");
    }
}
