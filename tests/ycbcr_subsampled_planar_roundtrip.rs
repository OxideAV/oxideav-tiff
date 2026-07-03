//! Chroma-subsampled YCbCr in `PlanarConfiguration = 2` — encode +
//! decode round-trip (TIFF 6.0 §21 + §"PlanarConfiguration").
//!
//! §21 "Ordering of Component Samples": "For PlanarConfiguration = 2,
//! component samples are stored as 3 separate planes, and the ordering
//! is the same as that used for other PhotometricInterpretation field
//! values" — plain row-major planes, no data-unit packing. The plane
//! *dimensions* come from §21 YCbCrSubSampling: the Y plane is the full
//! luma image; the Cb / Cr planes are the reduced "chroma image"
//! ("ImageWidth of this chroma image is half the ImageWidth of the
//! associated luma image", likewise ImageLength). §21 also constrains
//! RowsPerStrip to an integer multiple of YCbCrSubsampleVert.
//!
//! Oracles:
//!   * planar-vs-chunky cross-check — the planar and chunky subsampled
//!     writers share the same §21 box-filter decimation and the decoder
//!     splats chroma identically, so both encodings of the same pixels
//!     must decode to byte-identical Rgb24;
//!   * raw-byte / IFD checks — 3 strips (Y = w*h, Cb = Cr = (w/sh)*(h/sv)
//!     bytes uncompressed), PlanarConfiguration = 2, tag 530 = [sh, sv],
//!     verbatim Y plane payload;
//!   * a hand-built two-strips-per-plane fixture (RowsPerStrip < height)
//!     decodes without consulting our encoder, exercising the
//!     §"PlanarConfiguration" SamplesPerPixel × StripsPerImage strip
//!     accounting at reduced chroma geometry;
//!   * precise rejections for the still-deferred tiled planar subsampled
//!     shape on both sides.

use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, PageExtras, TiffCompression,
    TiffPixelFormat,
};

/// Full-resolution interleaved (Y, Cb, Cr) with chroma constant over
/// each sh × sv block, so the §21 box-filter decimation and the decode
/// splat are exact inverses and the YCbCr payload round-trips bit-exact.
fn block_constant_ycbcr(w: u32, h: u32, sh: u32, sv: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let (bx, by) = (x / sh, y / sv);
            v.push((x * 7 + y * 13) as u8); // Y varies per pixel
            v.push((bx * 31 + by * 17) as u8); // Cb constant per block
            v.push((bx * 5 + by * 43 + 100) as u8); // Cr constant per block
        }
    }
    v
}

fn planar_page<'a>(
    pixels: &'a [u8],
    w: u32,
    h: u32,
    subsampling: (u16, u16),
    compression: TiffCompression,
    predictor: bool,
) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::YCbCrSubsampled24 {
            pixels,
            subsampling,
        },
        compression,
        predictor,
        planar: true,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
    }
}

const COMPRESSORS: [TiffCompression; 5] = [
    TiffCompression::None,
    TiffCompression::PackBits,
    TiffCompression::Lzw,
    TiffCompression::Deflate,
    TiffCompression::Zstd,
];

#[test]
fn planar_subsampled_matches_chunky_subsampled_decode() {
    for (sh, sv) in [(2u16, 1u16), (2, 2), (4, 1), (4, 2)] {
        let (w, h) = (16u32, 8u32);
        let pixels = block_constant_ycbcr(w, h, sh as u32, sv as u32);
        let chunky = EncodePage {
            planar: false,
            ..planar_page(&pixels, w, h, (sh, sv), TiffCompression::None, false)
        };
        let planar = planar_page(&pixels, w, h, (sh, sv), TiffCompression::None, false);
        let want = decode_tiff(&encode_tiff(&chunky).unwrap()).unwrap();
        let got = decode_tiff(&encode_tiff(&planar).unwrap()).unwrap();
        assert_eq!(got.frame.pixel_format, TiffPixelFormat::Rgb24);
        assert_eq!(
            got.frame.planes[0].data, want.frame.planes[0].data,
            "planar vs chunky decode mismatch at ({sh},{sv})"
        );
    }
}

#[test]
fn planar_subsampled_compressor_predictor_matrix() {
    let (sh, sv) = (2u16, 2u16);
    let (w, h) = (20u32, 12u32);
    let pixels = block_constant_ycbcr(w, h, 2, 2);
    let baseline = decode_tiff(
        &encode_tiff(&planar_page(
            &pixels,
            w,
            h,
            (sh, sv),
            TiffCompression::None,
            false,
        ))
        .unwrap(),
    )
    .unwrap();
    for compression in COMPRESSORS {
        for predictor in [false, true] {
            let page = planar_page(&pixels, w, h, (sh, sv), compression, predictor);
            let img = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
            assert_eq!(
                img.frame.planes[0].data, baseline.frame.planes[0].data,
                "matrix mismatch (compression={compression:?} predictor={predictor})"
            );
        }
    }
}

#[test]
fn planar_subsampled_ifd_and_verbatim_planes() {
    let (sh, sv) = (2u16, 2u16);
    let (w, h) = (8u32, 4u32);
    let (cw, ch) = (4usize, 2usize);
    let pixels = block_constant_ycbcr(w, h, 2, 2);
    let file = encode_tiff(&planar_page(
        &pixels,
        w,
        h,
        (sh, sv),
        TiffCompression::None,
        false,
    ))
    .unwrap();
    let hdr = parse_header(&file).unwrap();
    let (entries, _) = parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let bo = hdr.byte_order;
    let get1 = |tag: u16| find(&entries, tag).unwrap().as_u32(bo).unwrap();
    assert_eq!(get1(284), 2, "PlanarConfiguration = 2");
    assert_eq!(get1(262), 6, "PhotometricInterpretation = YCbCr");
    let ss = find(&entries, 530).unwrap().as_u32_vec(bo).unwrap();
    assert_eq!(ss, vec![2, 2], "YCbCrSubSampling");
    let offsets = find(&entries, 273).unwrap().as_u64_vec(bo).unwrap();
    let counts = find(&entries, 279).unwrap().as_u64_vec(bo).unwrap();
    assert_eq!(offsets.len(), 3, "one strip per plane");
    assert_eq!(
        counts,
        vec![(w * h) as u64, (cw * ch) as u64, (cw * ch) as u64],
        "uncompressed plane byte counts: full-res Y + reduced chroma"
    );
    // Y plane payload is the input's Y bytes verbatim.
    let y_want: Vec<u8> = pixels.chunks_exact(3).map(|c| c[0]).collect();
    let y_off = offsets[0] as usize;
    assert_eq!(&file[y_off..y_off + y_want.len()], &y_want[..]);
    // Chroma planes are the per-block values (block-constant input, so
    // the box-filter mean is the value itself), row-major at cw × ch.
    let mut cb_want = Vec::with_capacity(cw * ch);
    let mut cr_want = Vec::with_capacity(cw * ch);
    for by in 0..ch {
        for bx in 0..cw {
            let s = ((by * 2) * (w as usize) + bx * 2) * 3;
            cb_want.push(pixels[s + 1]);
            cr_want.push(pixels[s + 2]);
        }
    }
    let cb_off = offsets[1] as usize;
    let cr_off = offsets[2] as usize;
    assert_eq!(&file[cb_off..cb_off + cb_want.len()], &cb_want[..]);
    assert_eq!(&file[cr_off..cr_off + cr_want.len()], &cr_want[..]);
}

// ---------------------------------------------------------------------------
// Hand-built fixture: two strips per plane (RowsPerStrip = 2, height 4)
// ---------------------------------------------------------------------------

/// Build a classic-II planar subsampled YCbCr file by hand: w=4, h=4,
/// subsampling (2,2) so the chroma planes are 2×2, RowsPerStrip=2 so
/// each plane splits into two strips (§21: RowsPerStrip is a multiple
/// of YCbCrSubsampleVert). Strip order per §"StripOffsets": plane 0's
/// strips first, then plane 1's, then plane 2's.
fn hand_built_two_strip_planar(y: &[u8; 16], cb: &[u8; 4], cr: &[u8; 4]) -> Vec<u8> {
    let mut f: Vec<u8> = Vec::new();
    f.extend_from_slice(b"II");
    f.extend_from_slice(&42u16.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes()); // patch later
                                              // Pixel data: 6 strips. Y rows 0-1, Y rows 2-3, Cb row 0, Cb row 1,
                                              // Cr row 0, Cr row 1.
    let strips: [&[u8]; 6] = [
        &y[0..8],
        &y[8..16],
        &cb[0..2],
        &cb[2..4],
        &cr[0..2],
        &cr[2..4],
    ];
    let mut offsets = Vec::new();
    let mut counts = Vec::new();
    for s in strips {
        offsets.push(f.len() as u32);
        counts.push(s.len() as u32);
        f.extend_from_slice(s);
    }
    // Out-of-line arrays: BitsPerSample (3 SHORTs), StripOffsets (6
    // LONGs), StripByteCounts (6 LONGs).
    while f.len() % 2 != 0 {
        f.push(0);
    }
    let bps_off = f.len() as u32;
    for _ in 0..3 {
        f.extend_from_slice(&8u16.to_le_bytes());
    }
    let so_off = f.len() as u32;
    for o in &offsets {
        f.extend_from_slice(&o.to_le_bytes());
    }
    let sc_off = f.len() as u32;
    for c in &counts {
        f.extend_from_slice(&c.to_le_bytes());
    }
    let ifd_off = f.len() as u32;
    // 10 entries, ascending tags.
    let entries: [(u16, u16, u32, u32); 10] = [
        (256, 3, 1, 4),       // ImageWidth = 4 (SHORT)
        (257, 3, 1, 4),       // ImageLength = 4
        (258, 3, 3, bps_off), // BitsPerSample = [8,8,8]
        (259, 3, 1, 1),       // Compression = None
        (262, 3, 1, 6),       // Photometric = YCbCr
        (273, 4, 6, so_off),  // StripOffsets × 6
        (277, 3, 1, 3),       // SamplesPerPixel = 3
        (278, 3, 1, 2),       // RowsPerStrip = 2
        (279, 4, 6, sc_off),  // StripByteCounts × 6
        (284, 3, 1, 2),       // PlanarConfiguration = 2
    ];
    // Tag 530 must follow 284 — build the full list with it appended.
    f.extend_from_slice(&11u16.to_le_bytes());
    for (tag, typ, count, value) in entries {
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&typ.to_le_bytes());
        f.extend_from_slice(&count.to_le_bytes());
        if typ == 3 && count == 1 {
            f.extend_from_slice(&(value as u16).to_le_bytes());
            f.extend_from_slice(&0u16.to_le_bytes());
        } else {
            f.extend_from_slice(&value.to_le_bytes());
        }
    }
    // 530 YCbCrSubSampling = [2, 2] (two SHORTs, inline).
    f.extend_from_slice(&530u16.to_le_bytes());
    f.extend_from_slice(&3u16.to_le_bytes());
    f.extend_from_slice(&2u32.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes()); // next IFD
    f[4..8].copy_from_slice(&ifd_off.to_le_bytes());
    f
}

#[test]
fn hand_built_multi_strip_planar_subsampled_decodes() {
    let y: [u8; 16] = [
        10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
    ];
    let cb: [u8; 4] = [128, 90, 200, 60];
    let cr: [u8; 4] = [128, 180, 40, 220];
    let file = hand_built_two_strip_planar(&y, &cb, &cr);
    let got = decode_tiff(&file).expect("hand-built planar subsampled decodes");
    assert_eq!((got.width, got.height), (4, 4));
    assert_eq!(got.frame.pixel_format, TiffPixelFormat::Rgb24);

    // Cross-check against our encoder's single-strip planar write of
    // the equivalent full-resolution pixels (chroma splatted per 2×2
    // block — block-constant, so the encoder's box filter reproduces
    // the same planes and both files must decode identically).
    let mut full = Vec::with_capacity(4 * 4 * 3);
    for py in 0..4usize {
        for px in 0..4usize {
            let ci = (py / 2) * 2 + px / 2;
            full.push(y[py * 4 + px]);
            full.push(cb[ci]);
            full.push(cr[ci]);
        }
    }
    let want = decode_tiff(
        &encode_tiff(&planar_page(
            &full,
            4,
            4,
            (2, 2),
            TiffCompression::None,
            false,
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(got.frame.planes[0].data, want.frame.planes[0].data);
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn tiled_planar_subsampled_encode_rejected() {
    let pixels = block_constant_ycbcr(32, 32, 2, 2);
    let page = EncodePage {
        tiling: Some((16, 16)),
        ..planar_page(&pixels, 32, 32, (2, 2), TiffCompression::None, false)
    };
    let err = encode_tiff(&page).unwrap_err().to_string();
    assert!(
        err.contains("tiled PlanarConfiguration=2"),
        "unexpected error: {err}"
    );
}

#[test]
fn chunky_subsampled_predictor_encode_still_rejected() {
    let pixels = block_constant_ycbcr(8, 8, 2, 2);
    let page = EncodePage {
        planar: false,
        ..planar_page(&pixels, 8, 8, (2, 2), TiffCompression::Lzw, true)
    };
    let err = encode_tiff(&page).unwrap_err().to_string();
    assert!(
        err.contains("chunky chroma-subsampled"),
        "unexpected error: {err}"
    );
}

#[test]
fn planar_subsampled_dimension_constraint_enforced() {
    // §21 page 90: ImageWidth/ImageLength must be multiples of the
    // subsampling factors — also with planar on.
    let pixels = block_constant_ycbcr(7, 4, 1, 1);
    let page = planar_page(&pixels, 7, 4, (2, 2), TiffCompression::None, false);
    assert!(encode_tiff(&page).is_err());
}

#[test]
fn decode_rejects_bad_rows_per_strip_multiple() {
    // A planar subsampled file whose RowsPerStrip is not a multiple of
    // YCbCrSubsampleVert violates §21 page 90 and must be rejected
    // precisely, not mis-sliced. Take the hand-built fixture and patch
    // RowsPerStrip (tag 278) from 2 to 3.
    let y = [0u8; 16];
    let cb = [128u8; 4];
    let cr = [128u8; 4];
    let mut file = hand_built_two_strip_planar(&y, &cb, &cr);
    // Locate tag 278 in the IFD and patch its inline SHORT value.
    let ifd_off = u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize;
    let n = u16::from_le_bytes(file[ifd_off..ifd_off + 2].try_into().unwrap()) as usize;
    let mut patched = false;
    for i in 0..n {
        let e = ifd_off + 2 + i * 12;
        if u16::from_le_bytes(file[e..e + 2].try_into().unwrap()) == 278 {
            file[e + 8..e + 10].copy_from_slice(&3u16.to_le_bytes());
            patched = true;
        }
    }
    assert!(patched);
    let err = match decode_tiff(&file) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("bad RowsPerStrip multiple must not decode"),
    };
    assert!(
        err.contains("RowsPerStrip"),
        "expected RowsPerStrip multiple-of-sv error, got: {err}"
    );
}
