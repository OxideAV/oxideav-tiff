//! Decode-side and round-trip tests for `PhotometricInterpretation = 5`
//! (separated / CMYK, TIFF 6.0 §16) under `PlanarConfiguration = 2`
//! (separate component planes, §"PlanarConfiguration" page 38).
//!
//! A CMYK page is four 8-bit components per pixel (`InkSet = 1`, §16).
//! Under planar layout those four components are stored as four
//! full-resolution single-component planes (C plane, M plane, Y plane,
//! K plane) with `StripOffsets` / `StripByteCounts` carrying
//! `SamplesPerPixel × StripsPerImage = 4` entries in plane order. The
//! decoder de-interleaves the planes back into chunky `C M Y K` order
//! and runs the same §16 `InkSet = 1` additive-RGB conversion the chunky
//! path uses.
//!
//! Two independent oracles:
//!   * Decode: a hand-built **chunky** CMYK fixture carrying the same
//!     `C M Y K` bytes is decoded by the same decoder; the planar decode
//!     must match it pixel-for-pixel. A plane mis-order (e.g. swapping K
//!     for C) or a plane mis-stride would diverge.
//!   * Round-trip: `EncodePixelFormat::Cmyk32` with `EncodePage::planar`
//!     / `EncodePage::predictor` must decode to the same Rgb24 as the
//!     plain chunky encode of the same source.

use oxideav_tiff::{decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, TiffCompression};

/// Hand-build a classic-II chunky CMYK TIFF (`PlanarConfiguration = 1`):
/// one `(C, M, Y, K)` quad per pixel, `PhotometricInterpretation = 5`,
/// `SamplesPerPixel = 4`, `BitsPerSample = [8, 8, 8, 8]`, `InkSet = 1`.
fn build_chunky_cmyk_tiff(width: u32, height: u32, cmyk_chunky: &[u8]) -> Vec<u8> {
    assert_eq!(cmyk_chunky.len(), (width * height * 4) as usize);
    let strip_bytes = width * height * 4;

    let num_entries: u16 = 11;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset: u32 = ifd_offset + ifd_size;

    let bps_off = blobs_offset; // SHORT[4] = 8 bytes
    let pixels_off = bps_off + 8;

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
    let short_inline = |v: u16| {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&v.to_le_bytes());
        b
    };

    push(&mut buf, 256, 4, 1, width.to_le_bytes());
    push(&mut buf, 257, 4, 1, height.to_le_bytes());
    push(&mut buf, 258, 3, 4, bps_off.to_le_bytes()); // BitsPerSample[4]
    push(&mut buf, 259, 3, 1, short_inline(1)); // Compression = None
    push(&mut buf, 262, 3, 1, short_inline(5)); // Photometric = Separated/CMYK
    push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes()); // StripOffsets
    push(&mut buf, 277, 3, 1, short_inline(4)); // SamplesPerPixel
    push(&mut buf, 279, 4, 1, strip_bytes.to_le_bytes()); // StripByteCounts
    push(&mut buf, 284, 3, 1, short_inline(1)); // PlanarConfiguration = chunky
    push(&mut buf, 332, 3, 1, short_inline(1)); // InkSet = 1 (CMYK)
    push(&mut buf, 334, 3, 1, short_inline(4)); // NumberOfInks = 4

    buf.extend_from_slice(&0u32.to_le_bytes()); // next-IFD

    // BitsPerSample SHORT[4]
    for _ in 0..4 {
        buf.extend_from_slice(&8u16.to_le_bytes());
    }
    buf.extend_from_slice(cmyk_chunky);
    buf
}

/// Hand-build a classic-II planar (`PlanarConfiguration = 2`) CMYK TIFF:
/// four full-resolution component planes (C, M, Y, K) in plane order,
/// `StripOffsets` / `StripByteCounts` as `SamplesPerPixel ×
/// StripsPerImage = 4` entries.
fn build_planar_cmyk_tiff(width: u32, height: u32, cmyk_chunky: &[u8]) -> Vec<u8> {
    assert_eq!(cmyk_chunky.len(), (width * height * 4) as usize);
    let plane_bytes = (width * height) as usize;
    let mut planes: Vec<Vec<u8>> = (0..4).map(|_| Vec::with_capacity(plane_bytes)).collect();
    for i in 0..plane_bytes {
        for (c, plane) in planes.iter_mut().enumerate() {
            plane.push(cmyk_chunky[i * 4 + c]);
        }
    }

    let num_entries: u16 = 12;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset: u32 = ifd_offset + ifd_size;

    let bps_off = blobs_offset; // SHORT[4] = 8
    let so_off = bps_off + 8; // LONG[4] = 16
    let sbc_off = so_off + 16; // LONG[4] = 16
    let planes_off = sbc_off + 16;

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
    let short_inline = |v: u16| {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&v.to_le_bytes());
        b
    };

    push(&mut buf, 256, 4, 1, width.to_le_bytes());
    push(&mut buf, 257, 4, 1, height.to_le_bytes());
    push(&mut buf, 258, 3, 4, bps_off.to_le_bytes()); // BitsPerSample[4]
    push(&mut buf, 259, 3, 1, short_inline(1)); // Compression = None
    push(&mut buf, 262, 3, 1, short_inline(5)); // Photometric = CMYK
    push(&mut buf, 273, 4, 4, so_off.to_le_bytes()); // StripOffsets (4)
    push(&mut buf, 277, 3, 1, short_inline(4)); // SamplesPerPixel
    push(&mut buf, 278, 4, 1, height.to_le_bytes()); // RowsPerStrip = h
    push(&mut buf, 279, 4, 4, sbc_off.to_le_bytes()); // StripByteCounts (4)
    push(&mut buf, 284, 3, 1, short_inline(2)); // PlanarConfiguration = 2
    push(&mut buf, 332, 3, 1, short_inline(1)); // InkSet = 1
    push(&mut buf, 334, 3, 1, short_inline(4)); // NumberOfInks = 4

    buf.extend_from_slice(&0u32.to_le_bytes());

    // BitsPerSample SHORT[4]
    for _ in 0..4 {
        buf.extend_from_slice(&8u16.to_le_bytes());
    }
    // StripOffsets LONG[4]
    let pb = plane_bytes as u32;
    for c in 0..4u32 {
        buf.extend_from_slice(&(planes_off + c * pb).to_le_bytes());
    }
    // StripByteCounts LONG[4]
    for _ in 0..4 {
        buf.extend_from_slice(&pb.to_le_bytes());
    }
    for plane in &planes {
        buf.extend_from_slice(plane);
    }
    buf
}

/// A `(C, M, Y, K)` raster with each component on a distinct ramp so a
/// cross-plane swap surfaces as a colour divergence.
fn cmyk_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            p.push(((x * 9) & 0xFF) as u8); // C
            p.push(((y * 13) & 0xFF) as u8); // M
            p.push((((x + y) * 5) & 0xFF) as u8); // Y
            p.push(((x ^ y).wrapping_mul(3) & 0xFF) as u8); // K
        }
    }
    p
}

#[test]
fn planar_cmyk_matches_chunky_decode() {
    for (w, h) in [(16u32, 8u32), (8, 8), (13, 7), (1, 1)] {
        let pixels = cmyk_pattern(w, h);
        let chunky = build_chunky_cmyk_tiff(w, h, &pixels);
        let planar = build_planar_cmyk_tiff(w, h, &pixels);

        let dc = decode_tiff(&chunky).expect("chunky CMYK decode");
        let dp = decode_tiff(&planar).expect("planar CMYK decode");

        assert_eq!((dp.width, dp.height), (w, h));
        assert_eq!(
            dp.frame.planes[0].data, dc.frame.planes[0].data,
            "planar CMYK diverged from chunky for {w}x{h}"
        );
    }
}

#[test]
fn planar_cmyk_solid_ink_preserves_plane_order() {
    // Solid C=200 / M=10 / Y=10 / K=20. If the C and K planes were
    // swapped the additive-RGB result would change markedly, so the
    // planar decode would diverge from the chunky reference.
    let (w, h) = (8u32, 8u32);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        pixels.extend_from_slice(&[200, 10, 10, 20]);
    }
    let dc = decode_tiff(&build_chunky_cmyk_tiff(w, h, &pixels)).unwrap();
    let dp = decode_tiff(&build_planar_cmyk_tiff(w, h, &pixels)).unwrap();
    assert_eq!(
        dp.frame.planes[0].data, dc.frame.planes[0].data,
        "CMYK plane order mismatch under PlanarConfiguration=2"
    );
}

#[test]
fn encoder_cmyk32_planar_predictor_roundtrips_against_chunky() {
    // EncodePixelFormat::Cmyk32 + EncodePage::planar / ::predictor must
    // decode to the same Rgb24 as the plain chunky encode — the chunky
    // encode (validated against the hand-built fixtures above) is the
    // independent oracle. Strip + tiled, with and without predictor,
    // across the byte-aligned compressors.
    let (w, h) = (16u32, 16u32);
    let pixels = cmyk_pattern(w, h);
    let oracle = {
        let page = EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
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

    for comp in [
        TiffCompression::None,
        TiffCompression::PackBits,
        TiffCompression::Lzw,
        TiffCompression::Deflate,
    ] {
        for predictor in [false, true] {
            for tiling in [None, Some((16u32, 16u32))] {
                let page = EncodePage {
                    width: w,
                    height: h,
                    kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                    compression: comp,
                    predictor,
                    planar: true,
                    tiling,
                    bigtiff: false,
                };
                let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
                assert_eq!(
                    d.frame.planes[0].data, oracle,
                    "planar CMYK diverged for comp {comp:?} predictor={predictor} tiling={tiling:?}"
                );
            }
        }
    }
}
