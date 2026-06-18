//! Read + write coverage for `PhotometricInterpretation = 4`
//! (Transparency Mask), TIFF 6.0 page 37.
//!
//! Spec recap (TIFF 6.0 §"PhotometricInterpretation" value 4):
//!
//! > "This means that the image is used to define an irregularly shaped
//! > region of another image in the same TIFF file. SamplesPerPixel and
//! > BitsPerSample must be 1. PackBits compression is recommended. The
//! > 1-bits define the interior of the region; the 0-bits define the
//! > exterior of the region."
//!
//! Companion field (TIFF 6.0 page 36, NewSubfileType): "Bit 2 is 1 if
//! the image defines a transparency mask for another image in this TIFF
//! file. The PhotometricInterpretation value must be 4."
//!
//! All tests are binary-independent: each one encodes through our
//! writer, decodes through our reader, and asserts on the resulting
//! `Gray8` plane (interior pixels = 0xFF, exterior pixels = 0x00 — the
//! byte layout a downstream compositor multiplies with the main image).

use oxideav_tiff::{
    decode_tiff, decode_tiff_all, encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat,
    TiffCompression, TiffPixelFormat,
};

// A small 16x8 mask: top half = exterior (all 0-bits), bottom half =
// interior (all 1-bits). Width 16 keeps every row exactly two bytes, so
// the byte-packing layout is unambiguous.
fn mask_top_outside_bottom_inside() -> (u32, u32, Vec<u8>) {
    let (w, h) = (16u32, 8u32);
    let row_bytes = (w as usize).div_ceil(8);
    let mut bytes = vec![0u8; row_bytes * h as usize];
    for y in (h as usize / 2)..h as usize {
        for b in 0..row_bytes {
            bytes[y * row_bytes + b] = 0xFF;
        }
    }
    (w, h, bytes)
}

// A 24x4 diagonal stripe (one 1-bit per row at column == row * 3),
// rest are 0-bits — exercises non-aligned widths (24 / 8 = 3 bytes per
// row) and per-bit polarity.
fn mask_diagonal_24x4() -> (u32, u32, Vec<u8>) {
    let (w, h) = (24u32, 4u32);
    let row_bytes = (w as usize).div_ceil(8);
    let mut bytes = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        let col = y * 3;
        let byte = col / 8;
        let bit = 7 - (col % 8);
        bytes[y * row_bytes + byte] |= 1 << bit;
    }
    (w, h, bytes)
}

// Expand a packed 1-bit row buffer into the Gray8 byte plane the
// decoder is expected to produce for a transparency mask: bit value 1
// (interior) -> 0xFF, bit value 0 (exterior) -> 0x00. Matches the
// `build_gray8_from_1bpp(..., invert=false)` path the decoder routes
// through for PhotometricInterpretation = 4.
fn expand_mask_to_gray8(packed: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row_bytes = (w as usize).div_ceil(8);
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let b = packed[y * row_bytes + x / 8];
            let bit = (b >> (7 - (x % 8))) & 1;
            out.push(if bit == 1 { 0xFF } else { 0x00 });
        }
    }
    out
}

fn encode_mask(w: u32, h: u32, bytes: &[u8], compression: TiffCompression) -> Vec<u8> {
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::TransparencyMask { pixels: bytes },
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    encode_tiff(&page).expect("encode_tiff TransparencyMask")
}

fn decode_one_image(buf: &[u8]) -> (u32, u32, Vec<u8>) {
    let d = decode_tiff(buf).expect("decode_tiff");
    assert_eq!(d.pixel_format, TiffPixelFormat::Gray8);
    let plane = d.frame.planes.first().expect("one plane").clone();
    assert_eq!(plane.stride, d.width as usize, "Gray8 stride == width");
    (d.width, d.height, plane.data)
}

#[test]
fn roundtrip_top_outside_bottom_inside_none() {
    let (w, h, packed) = mask_top_outside_bottom_inside();
    let buf = encode_mask(w, h, &packed, TiffCompression::None);
    let (dw, dh, gray) = decode_one_image(&buf);
    assert_eq!((dw, dh), (w, h));
    assert_eq!(gray, expand_mask_to_gray8(&packed, w, h));
}

#[test]
fn roundtrip_top_outside_bottom_inside_packbits() {
    // Spec note: PackBits is the recommended compressor for
    // transparency masks.
    let (w, h, packed) = mask_top_outside_bottom_inside();
    let buf = encode_mask(w, h, &packed, TiffCompression::PackBits);
    let (dw, dh, gray) = decode_one_image(&buf);
    assert_eq!((dw, dh), (w, h));
    assert_eq!(gray, expand_mask_to_gray8(&packed, w, h));
}

#[test]
fn roundtrip_top_outside_bottom_inside_lzw() {
    let (w, h, packed) = mask_top_outside_bottom_inside();
    let buf = encode_mask(w, h, &packed, TiffCompression::Lzw);
    let (dw, dh, gray) = decode_one_image(&buf);
    assert_eq!((dw, dh), (w, h));
    assert_eq!(gray, expand_mask_to_gray8(&packed, w, h));
}

#[test]
fn roundtrip_top_outside_bottom_inside_deflate() {
    let (w, h, packed) = mask_top_outside_bottom_inside();
    let buf = encode_mask(w, h, &packed, TiffCompression::Deflate);
    let (dw, dh, gray) = decode_one_image(&buf);
    assert_eq!((dw, dh), (w, h));
    assert_eq!(gray, expand_mask_to_gray8(&packed, w, h));
}

#[test]
fn roundtrip_top_outside_bottom_inside_ccitt_mh() {
    let (w, h, packed) = mask_top_outside_bottom_inside();
    let buf = encode_mask(w, h, &packed, TiffCompression::CcittRle);
    let (dw, dh, gray) = decode_one_image(&buf);
    assert_eq!((dw, dh), (w, h));
    assert_eq!(gray, expand_mask_to_gray8(&packed, w, h));
}

#[test]
fn roundtrip_top_outside_bottom_inside_ccitt_t4_1d() {
    let (w, h, packed) = mask_top_outside_bottom_inside();
    let buf = encode_mask(
        w,
        h,
        &packed,
        TiffCompression::CcittT4OneD {
            eol_byte_aligned: false,
        },
    );
    let (dw, dh, gray) = decode_one_image(&buf);
    assert_eq!((dw, dh), (w, h));
    assert_eq!(gray, expand_mask_to_gray8(&packed, w, h));
}

#[test]
fn roundtrip_top_outside_bottom_inside_ccitt_t4_1d_byte_aligned() {
    let (w, h, packed) = mask_top_outside_bottom_inside();
    let buf = encode_mask(
        w,
        h,
        &packed,
        TiffCompression::CcittT4OneD {
            eol_byte_aligned: true,
        },
    );
    let (dw, dh, gray) = decode_one_image(&buf);
    assert_eq!((dw, dh), (w, h));
    assert_eq!(gray, expand_mask_to_gray8(&packed, w, h));
}

#[test]
fn roundtrip_diagonal_24x4_all_compressors() {
    // Smaller non-byte-aligned width (24 px = 3 bytes/row) and
    // pixel-precise bit positions stress the bilevel expander, the
    // CCITT scanner, and the PackBits/LZW/Deflate paths together.
    let (w, h, packed) = mask_diagonal_24x4();
    for compression in [
        TiffCompression::None,
        TiffCompression::PackBits,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
        TiffCompression::CcittRle,
        TiffCompression::CcittT4OneD {
            eol_byte_aligned: false,
        },
        TiffCompression::CcittT4OneD {
            eol_byte_aligned: true,
        },
    ] {
        let buf = encode_mask(w, h, &packed, compression);
        let (dw, dh, gray) = decode_one_image(&buf);
        assert_eq!((dw, dh), (w, h));
        assert_eq!(
            gray,
            expand_mask_to_gray8(&packed, w, h),
            "decode mismatch for compression {compression:?}"
        );
    }
}

/// Multi-page TIFF: main image (Gray8) + mask page (TransparencyMask),
/// the layout TIFF 6.0 page 37 envisions ("define an irregularly
/// shaped region of *another image in the same TIFF file*"). The two
/// IFDs share the same `width × height` here, but the spec also allows
/// the mask to be at a higher resolution ("The image mask is typically
/// at a higher resolution than the main image" — page 37); we exercise
/// the equal-resolution case because that's what a compositor most
/// commonly produces.
#[test]
fn multipage_main_image_plus_mask() {
    let (w, h, packed_mask) = mask_top_outside_bottom_inside();
    // Main image: a vertical ramp greyscale, same dimensions.
    let mut gray = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for _x in 0..w {
            gray.push((y * (255 / (h - 1).max(1))) as u8);
        }
    }
    let pages = [
        EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::Gray8 { pixels: &gray },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
        EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::TransparencyMask {
                pixels: &packed_mask,
            },
            compression: TiffCompression::PackBits,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        },
    ];
    let buf = encode_tiff_multi(&pages).expect("multipage encode");
    let images = decode_tiff_all(&buf).expect("multipage decode");
    assert_eq!(images.len(), 2);
    // Page 0: greyscale plane is the original ramp.
    assert_eq!(images[0].pixel_format, TiffPixelFormat::Gray8);
    assert_eq!(images[0].planes[0].data, gray);
    // Page 1: mask page decodes as Gray8 with interior=0xFF /
    // exterior=0x00, matching the canonical mask polarity.
    assert_eq!(images[1].pixel_format, TiffPixelFormat::Gray8);
    assert_eq!(
        images[1].planes[0].data,
        expand_mask_to_gray8(&packed_mask, w, h)
    );
}

/// IFD-level inspection: confirm the encoder writes
/// `PhotometricInterpretation = 4` (tag 262) and sets bit 2 of
/// `NewSubfileType` (tag 254) on a TransparencyMask page. Reads the
/// IFD directly via the public `ifd::parse_ifd` surface to avoid
/// re-implementing the byte-walker.
#[test]
fn transparency_mask_ifd_tags() {
    use oxideav_tiff::ifd::{parse_header, parse_ifd};

    let (w, h, packed) = mask_top_outside_bottom_inside();
    let buf = encode_mask(w, h, &packed, TiffCompression::PackBits);

    let header = parse_header(&buf).expect("parse_header");
    let (entries, _next) = parse_ifd(
        &buf,
        header.byte_order,
        header.variant,
        header.first_ifd_offset,
    )
    .expect("parse_ifd");

    let mut photometric: Option<u32> = None;
    let mut new_subfile_type: Option<u32> = None;
    let mut samples_per_pixel: Option<u32> = None;
    let mut bits_per_sample: Option<u32> = None;
    for e in &entries {
        // Tag numbers per types.rs.
        match e.tag {
            254 => new_subfile_type = Some(e.as_u32(header.byte_order).unwrap()),
            258 => bits_per_sample = Some(e.as_u32(header.byte_order).unwrap()),
            262 => photometric = Some(e.as_u32(header.byte_order).unwrap()),
            277 => samples_per_pixel = Some(e.as_u32(header.byte_order).unwrap()),
            _ => {}
        }
    }
    assert_eq!(photometric, Some(4), "PhotometricInterpretation = 4");
    assert_eq!(samples_per_pixel, Some(1), "SamplesPerPixel = 1");
    assert_eq!(bits_per_sample, Some(1), "BitsPerSample = 1");
    let nst = new_subfile_type.expect("NewSubfileType tag present");
    assert_eq!(nst & (1 << 2), 1 << 2, "NewSubfileType bit 2 (mask) set");
    // Other bits not asserted clear — additive flags are independent
    // (page 36: "These values are defined as bit flags because they
    // are independent of each other"), so a future round could safely
    // set bit 1 (page-of-multi-page) without breaking this assertion.
}

/// IFD-level inspection: a non-mask page (Gray8) must leave bit 2 of
/// NewSubfileType clear. Catches accidental fallthrough where every
/// page would otherwise get tagged as a mask.
#[test]
fn gray8_page_does_not_set_mask_bit() {
    use oxideav_tiff::ifd::{parse_header, parse_ifd};

    let w = 8u32;
    let h = 4u32;
    let gray = vec![0u8; (w * h) as usize];
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Gray8 { pixels: &gray },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let buf = encode_tiff(&page).expect("encode Gray8");
    let header = parse_header(&buf).expect("parse_header");
    let (entries, _next) = parse_ifd(
        &buf,
        header.byte_order,
        header.variant,
        header.first_ifd_offset,
    )
    .expect("parse_ifd");
    for e in &entries {
        if e.tag == 254 {
            let v = e.as_u32(header.byte_order).unwrap();
            assert_eq!(
                v & (1 << 2),
                0,
                "Gray8 page must not have NewSubfileType bit 2 (mask) set; got 0x{v:x}"
            );
        }
    }
}

/// Sub-byte (1-bit) tile writing (TIFF 6.0 §15) now supports the mask
/// path: the tiled encode decodes to the same polarity-pinned Gray8
/// plane (1 -> 0xFF, 0 -> 0x00) as the strip encode of the identical
/// 1-bit pixels. The strip path is the independent oracle.
#[test]
fn transparency_mask_tiled_matches_strip() {
    let (w, h, packed) = mask_diagonal_24x4();
    let strip = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::TransparencyMask { pixels: &packed },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let tiled = EncodePage {
        tiling: Some((16, 16)),
        ..strip.clone()
    };
    let ds = decode_tiff(&encode_tiff(&strip).expect("encode strip mask")).expect("decode strip");
    let dt = decode_tiff(&encode_tiff(&tiled).expect("encode tiled mask")).expect("decode tiled");
    assert_eq!(dt.frame.planes[0].data, ds.frame.planes[0].data);
}

/// `PlanarConfiguration = 2` is irrelevant for a single-sample page;
/// the encoder must reject the combination with a precise error so a
/// caller's bug surfaces immediately rather than producing a file
/// libtiff / our own decoder can't read.
#[test]
fn transparency_mask_planar_rejected() {
    let (w, h, packed) = (16u32, 8u32, vec![0u8; 2 * 8]);
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::TransparencyMask { pixels: &packed },
        compression: TiffCompression::None,
        predictor: false,
        planar: true,
        tiling: None,
        bigtiff: false,
    };
    let err = encode_tiff(&page).expect_err("planar TransparencyMask must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("PlanarConfiguration") || msg.contains("multi-sample"),
        "unexpected error message: {msg}"
    );
}

/// Predictor=2 differences whole sample components — undefined for
/// 1-bit data, so the encoder must reject the combination.
#[test]
fn transparency_mask_predictor_rejected() {
    let (w, h, packed) = (16u32, 8u32, vec![0u8; 2 * 8]);
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::TransparencyMask { pixels: &packed },
        compression: TiffCompression::Lzw,
        predictor: true,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let err = encode_tiff(&page).expect_err("predictor + TransparencyMask must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("Predictor") || msg.contains("1-bit"),
        "unexpected error message: {msg}"
    );
}

/// Pixel-buffer size validation: the encoder must reject a
/// `pixels.len()` that doesn't match `ceil(width/8) * height`.
#[test]
fn transparency_mask_wrong_buffer_size_rejected() {
    let page = EncodePage {
        width: 16,
        height: 8,
        // Expected 16 bytes (2 per row × 8 rows); we deliberately
        // pass 10 bytes.
        kind: EncodePixelFormat::TransparencyMask { pixels: &[0u8; 10] },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    let err = encode_tiff(&page).expect_err("size-mismatch TransparencyMask must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("TransparencyMask") && msg.contains("16"),
        "unexpected error message: {msg}"
    );
}
