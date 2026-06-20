//! Decode-side tests for `PlanarConfiguration = 2` (separate component
//! planes) YCbCr images at the §21 4:4:4 ratio (`YCbCrSubSampling =
//! [1, 1]`), per TIFF 6.0 §"PlanarConfiguration" (page 38) and §21
//! "YCbCr Images".
//!
//! At 4:4:4 the §21 "Ordering of Component Samples" data unit collapses
//! to a plain chunky `(Y, Cb, Cr)` triple, so a planar YCbCr page is
//! three full-resolution component planes (Y plane, then Cb plane, then
//! Cr plane) — structurally identical to a planar RGB page. The decoder
//! re-interleaves the planes into chunky order and runs the same §22
//! BT.601 YCbCr→RGB matrix the chunky path uses.
//!
//! The oracle is binary-independent: a hand-built **chunky** YCbCr
//! fixture carrying the identical `(Y, Cb, Cr)` bytes is decoded by the
//! same decoder, and the planar decode must match it pixel-for-pixel.
//! A decoder that mis-ordered the planes, mis-strided a plane, or
//! mis-sized the chroma planes would diverge from the chunky reference.
//!
//! The genuinely-subsampled planar case (Cb/Cr stored at reduced
//! resolution) is rejected with a precise error — only 4:4:4 planar
//! YCbCr decodes — and that rejection is exercised here too.

use oxideav_tiff::decode_tiff;

/// Hand-build a classic-II chunky 4:4:4 YCbCr TIFF (one `(Y, Cb, Cr)`
/// triple per pixel, `PlanarConfiguration = 1`). Carries the
/// §21-required 530 / 531 / 532 fields with the full-range §20 coding
/// values so the decoder's matrix has an explicit reference range.
fn build_chunky_ycbcr_tiff(width: u32, height: u32, ycbcr_chunky: &[u8]) -> Vec<u8> {
    assert_eq!(ycbcr_chunky.len(), (width * height * 3) as usize);
    let strip_bytes = width * height * 3;

    let num_entries: u16 = 12;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset: u32 = ifd_offset + ifd_size;

    let bps_off = blobs_offset; // SHORT[3] = 6
    let mut cursor = bps_off + 6;
    if cursor % 4 != 0 {
        cursor += 4 - (cursor % 4);
    }
    let rbw_off = cursor; // RATIONAL[6] = 48
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
    let short_inline = |v: u16| {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&v.to_le_bytes());
        b
    };

    push(&mut buf, 256, 4, 1, width.to_le_bytes());
    push(&mut buf, 257, 4, 1, height.to_le_bytes());
    push(&mut buf, 258, 3, 3, bps_off.to_le_bytes()); // BitsPerSample
    push(&mut buf, 259, 3, 1, short_inline(1)); // Compression = None
    push(&mut buf, 262, 3, 1, short_inline(6)); // Photometric = YCbCr
    push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes()); // StripOffsets
    push(&mut buf, 277, 3, 1, short_inline(3)); // SamplesPerPixel
    push(&mut buf, 279, 4, 1, strip_bytes.to_le_bytes()); // StripByteCounts
    push(&mut buf, 284, 3, 1, short_inline(1)); // PlanarConfiguration = chunky
                                                // 530 YCbCrSubSampling = [1, 1] (two inline SHORTs)
    let mut ss = [0u8; 4];
    ss[..2].copy_from_slice(&1u16.to_le_bytes());
    ss[2..4].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 530, 3, 2, ss);
    push(&mut buf, 531, 3, 1, short_inline(1)); // YCbCrPositioning = centered
    push(&mut buf, 532, 5, 6, rbw_off.to_le_bytes()); // ReferenceBlackWhite

    buf.extend_from_slice(&0u32.to_le_bytes()); // next-IFD

    // BitsPerSample blob
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
    buf.extend_from_slice(ycbcr_chunky);
    buf
}

/// Hand-build a classic-II planar (`PlanarConfiguration = 2`) 4:4:4
/// YCbCr TIFF: three full-resolution component planes (Y, Cb, Cr) in
/// plane order, with `StripOffsets` / `StripByteCounts` as
/// `SamplesPerPixel × StripsPerImage = 3` entries. `YCbCrSubSampling`
/// is set explicitly to `[1, 1]`.
fn build_planar_ycbcr_tiff(width: u32, height: u32, ycbcr_chunky: &[u8]) -> Vec<u8> {
    assert_eq!(ycbcr_chunky.len(), (width * height * 3) as usize);
    let plane_bytes = (width * height) as usize;
    let mut plane_y = Vec::with_capacity(plane_bytes);
    let mut plane_cb = Vec::with_capacity(plane_bytes);
    let mut plane_cr = Vec::with_capacity(plane_bytes);
    for i in 0..plane_bytes {
        plane_y.push(ycbcr_chunky[i * 3]);
        plane_cb.push(ycbcr_chunky[i * 3 + 1]);
        plane_cr.push(ycbcr_chunky[i * 3 + 2]);
    }

    let num_entries: u16 = 13;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
    let blobs_offset: u32 = ifd_offset + ifd_size;

    let bps_off = blobs_offset; // SHORT[3] = 6
    let so_off = bps_off + 6; // LONG[3] = 12
    let sbc_off = so_off + 12; // LONG[3] = 12
    let mut cursor = sbc_off + 12;
    if cursor % 4 != 0 {
        cursor += 4 - (cursor % 4);
    }
    let rbw_off = cursor; // RATIONAL[6] = 48
    let plane_y_off = rbw_off + 48;
    let plane_cb_off = plane_y_off + plane_bytes as u32;
    let plane_cr_off = plane_cb_off + plane_bytes as u32;

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
    push(&mut buf, 258, 3, 3, bps_off.to_le_bytes());
    push(&mut buf, 259, 3, 1, short_inline(1)); // Compression = None
    push(&mut buf, 262, 3, 1, short_inline(6)); // Photometric = YCbCr
    push(&mut buf, 273, 4, 3, so_off.to_le_bytes()); // StripOffsets (3)
    push(&mut buf, 277, 3, 1, short_inline(3)); // SamplesPerPixel
    push(&mut buf, 278, 4, 1, height.to_le_bytes()); // RowsPerStrip = h
    push(&mut buf, 279, 4, 3, sbc_off.to_le_bytes()); // StripByteCounts (3)
    push(&mut buf, 284, 3, 1, short_inline(2)); // PlanarConfiguration = 2
    let mut ss = [0u8; 4];
    ss[..2].copy_from_slice(&1u16.to_le_bytes());
    ss[2..4].copy_from_slice(&1u16.to_le_bytes());
    push(&mut buf, 530, 3, 2, ss); // YCbCrSubSampling = [1, 1]
    push(&mut buf, 531, 3, 1, short_inline(1)); // YCbCrPositioning
    push(&mut buf, 532, 5, 6, rbw_off.to_le_bytes()); // ReferenceBlackWhite

    buf.extend_from_slice(&0u32.to_le_bytes());

    // BitsPerSample SHORT[3]
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    // StripOffsets LONG[3]
    buf.extend_from_slice(&plane_y_off.to_le_bytes());
    buf.extend_from_slice(&plane_cb_off.to_le_bytes());
    buf.extend_from_slice(&plane_cr_off.to_le_bytes());
    // StripByteCounts LONG[3]
    let pb = plane_bytes as u32;
    buf.extend_from_slice(&pb.to_le_bytes());
    buf.extend_from_slice(&pb.to_le_bytes());
    buf.extend_from_slice(&pb.to_le_bytes());
    // Pad to ReferenceBlackWhite alignment.
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
    buf.extend_from_slice(&plane_y);
    buf.extend_from_slice(&plane_cb);
    buf.extend_from_slice(&plane_cr);
    buf
}

/// A non-neutral `(Y, Cb, Cr)` raster so a plane mis-order / mis-stride
/// surfaces as a colour divergence rather than passing by coincidence.
fn ycbcr_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            p.push(((x * 7 + y * 11) & 0xFF) as u8); // Y
            p.push((64 + (x * 3) % 128) as u8); // Cb
            p.push((64 + (y * 5) % 128) as u8); // Cr
        }
    }
    p
}

#[test]
fn planar_ycbcr_444_matches_chunky_decode() {
    for (w, h) in [(32u32, 16u32), (8, 8), (17, 5), (1, 1)] {
        let pixels = ycbcr_pattern(w, h);
        let chunky = build_chunky_ycbcr_tiff(w, h, &pixels);
        let planar = build_planar_ycbcr_tiff(w, h, &pixels);

        let dc = decode_tiff(&chunky).expect("chunky 4:4:4 YCbCr decode");
        let dp = decode_tiff(&planar).expect("planar 4:4:4 YCbCr decode");

        assert_eq!((dp.width, dp.height), (w, h));
        assert_eq!(
            dp.frame.planes[0].data, dc.frame.planes[0].data,
            "planar 4:4:4 YCbCr diverged from chunky for {w}x{h}"
        );
    }
}

#[test]
fn planar_ycbcr_444_solid_chroma_preserves_plane_order() {
    // A solid Y=128 / Cb=200 / Cr=60 image. If the Cb and Cr planes were
    // swapped, the BT.601 matrix would push the colour the opposite way
    // on the blue/red axes, so the planar decode would diverge from the
    // chunky reference carrying the same triple.
    let (w, h) = (8u32, 8u32);
    let mut pixels = Vec::with_capacity((w * h * 3) as usize);
    for _ in 0..(w * h) {
        pixels.push(128); // Y
        pixels.push(200); // Cb
        pixels.push(60); // Cr
    }
    let dc = decode_tiff(&build_chunky_ycbcr_tiff(w, h, &pixels)).unwrap();
    let dp = decode_tiff(&build_planar_ycbcr_tiff(w, h, &pixels)).unwrap();
    assert_eq!(
        dp.frame.planes[0].data, dc.frame.planes[0].data,
        "Cb/Cr plane order mismatch under PlanarConfiguration=2"
    );
}

#[test]
fn planar_ycbcr_subsampled_rejected() {
    // Build a planar YCbCr fixture but tag it YCbCrSubSampling = [2, 2].
    // The planar walker sizes every plane at the full image resolution,
    // which is only correct at 4:4:4; under subsampling the Cb/Cr planes
    // are stored at reduced resolution, so the decoder must reject rather
    // than silently mis-read.
    let (w, h) = (8u32, 8u32);
    let pixels = ycbcr_pattern(w, h);
    let mut tiff = build_planar_ycbcr_tiff(w, h, &pixels);

    // Patch the YCbCrSubSampling (tag 530) entry's two inline SHORTs from
    // [1, 1] to [2, 2] in place. The IFD starts at offset 8: 2-byte count
    // then 12-byte entries; tag 530 is the 11th entry (index 10).
    let ifd_start = 8usize;
    let count = u16::from_le_bytes([tiff[ifd_start], tiff[ifd_start + 1]]) as usize;
    let entries_start = ifd_start + 2;
    let mut patched = false;
    for i in 0..count {
        let e = entries_start + i * 12;
        let tag = u16::from_le_bytes([tiff[e], tiff[e + 1]]);
        if tag == 530 {
            // value/offset slot at e+8: two inline SHORTs -> [2, 2]
            tiff[e + 8..e + 10].copy_from_slice(&2u16.to_le_bytes());
            tiff[e + 10..e + 12].copy_from_slice(&2u16.to_le_bytes());
            patched = true;
            break;
        }
    }
    assert!(patched, "test setup: tag 530 not found to patch");

    let err = decode_tiff(&tiff);
    assert!(
        err.is_err(),
        "subsampled (2,2) planar YCbCr must be rejected, not silently mis-decoded"
    );
}
