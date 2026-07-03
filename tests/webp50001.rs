//! Compression=50001 (WebP-in-TIFF) coverage — decode + encode.
//!
//! The scheme is the pixel-codec sibling of the Compression=50000
//! Zstandard registry extension, transcribed in the OxideAV trace doc
//! `docs/image/tiff/tiff-zstd-compression-50000.md` (§1 registers both
//! values; §3: "WebP (50001) follows the identical per-strip /
//! per-tile container discipline: each strip/tile becomes one WebP
//! (VP8 / VP8L) bitstream"). The exact segment framing was pinned
//! against independently produced reference files, committed under
//! `tests/data/webp50001/`:
//!
//! * every strip / tile payload is one complete WebP **file**
//!   (`RIFF….WEBP` container carrying a `VP8L` lossless or `VP8 `
//!   lossy bitstream);
//! * a multi-strip page's final strip carries only the remaining rows
//!   (48×40 at RowsPerStrip=16 → segments of 16 / 16 / 8 rows);
//! * §15 edge tiles stay padded to the full TileWidth × TileLength;
//! * `PhotometricInterpretation = 2`, `BitsPerSample = 8`,
//!   `SamplesPerPixel = 3` (RGB) or `4` (RGBA + ExtraSamples), chunky,
//!   no Predictor.
//!
//! Fixture provenance (black-box): the five `ref_*.tif` files were
//! produced by driving third-party TIFF-writing tooling as an opaque
//! binary over deterministic pixel formulas (reproduced in this file),
//! then verified segment-by-segment with an independent WebP decoder
//! binary. No implementation source was consulted; the files are
//! wire-format samples only.
//!
//! Three layers of validation:
//!
//! 1. **Independent-fixture cross-reads** (run unconditionally): the
//!    committed reference files decode through [`decode_tiff`] and the
//!    pixels match the generating formulas (exactly for the `VP8L`
//!    lossless files; within a small tolerance for the `VP8 ` lossy
//!    file, whose YUV→RGB display conversion is reader-defined).
//!
//! 2. **Self-roundtrips** (run unconditionally): encode the identical
//!    pixels twice — once `Compression = 50001`, once `Compression =
//!    1` — decode both, and assert the pixel planes are byte-identical
//!    (lossless VP8L segments make this exact). Covers Rgb24 / Rgba32,
//!    multi-strip, §15 tiling with partial edge tiles, BigTIFF, and
//!    the multi-page chain. The RGBA alpha channel (which the TIFF
//!    decode drops per the crate's §ExtraSamples policy) is verified
//!    by re-decoding our emitted strip payload through `oxideav-webp`
//!    directly.
//!
//! 3. **Shape rejections**: encoder-side (non-RGB(A) input, predictor,
//!    planar) and decoder-side (a 50001 IFD declaring a Predictor must
//!    be rejected per the TIFF 6.0 §14 reader rule).

use oxideav_tiff::{
    decode_tiff, decode_tiff_all, encode_tiff, encode_tiff_multi, DecodedTiff, EncodePage,
    EncodePixelFormat, ExtraSampleKind, PageExtras, TiffCompression, TiffPixelFormat,
};

// ---- Deterministic pixel formulas (mirror the fixture generators) ----

/// 48×40 RGB ramp used by `ref_rgb_lossless.tif` / `ref_rgb_strips.tif`
/// / `ref_rgba_lossless.tif`: `((x*5) % 256, (y*6) % 256, ((x+y)*3) % 256)`.
fn fixture_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x * 5) & 0xFF) as u8);
            v.push(((y * 6) & 0xFF) as u8);
            v.push((((x + y) * 3) & 0xFF) as u8);
        }
    }
    v
}

/// Alpha plane of `ref_rgba_lossless.tif`: `(x*y) % 256`.
fn fixture_alpha(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x * y) & 0xFF) as u8);
        }
    }
    v
}

/// 48×40 RGB pattern of `ref_rgb_tiled.tif`:
/// `((x*3+y) % 256, (y*7) % 256, (x^y) % 256)`.
fn fixture_rgb_tiled(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x * 3 + y) & 0xFF) as u8);
            v.push(((y * 7) & 0xFF) as u8);
            v.push(((x ^ y) & 0xFF) as u8);
        }
    }
    v
}

/// Smooth 48×40 gradient of `ref_rgb_lossy.tif`:
/// `(x*255/47, y*255/39, (x+y)*255/86)`.
fn fixture_rgb_lossy(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x * 255 / (w - 1)) as u8);
            v.push((y * 255 / (h - 1)) as u8);
            v.push(((x + y) * 255 / (w + h - 2)) as u8);
        }
    }
    v
}

/// Extract the decoded RGB rows (dropping any stride padding) as one
/// packed `width * height * 3` buffer.
fn packed_rgb(d: &DecodedTiff) -> Vec<u8> {
    assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
    let stride = d.frame.planes[0].stride;
    let row_bytes = (d.width as usize) * 3;
    let mut out = Vec::with_capacity(row_bytes * d.height as usize);
    for y in 0..d.height as usize {
        out.extend_from_slice(&d.frame.planes[0].data[y * stride..y * stride + row_bytes]);
    }
    out
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/webp50001")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

// ---- 1. Independent-fixture cross-reads ----

#[test]
fn fixture_rgb_lossless_single_strip() {
    let d = decode_tiff(&fixture_bytes("ref_rgb_lossless.tif")).expect("decode");
    assert_eq!((d.width, d.height), (48, 40));
    assert_eq!(packed_rgb(&d), fixture_rgb(48, 40));
}

#[test]
fn fixture_rgb_lossless_multi_strip() {
    // RowsPerStrip = 16 over 40 rows: three WebP segments of 16/16/8
    // rows — exercises the short final strip.
    let d = decode_tiff(&fixture_bytes("ref_rgb_strips.tif")).expect("decode");
    assert_eq!((d.width, d.height), (48, 40));
    assert_eq!(packed_rgb(&d), fixture_rgb(48, 40));
}

#[test]
fn fixture_rgba_lossless() {
    // SamplesPerPixel = 4 + ExtraSamples: the decode drops the alpha
    // (crate §ExtraSamples policy) and must return the RGB part.
    let d = decode_tiff(&fixture_bytes("ref_rgba_lossless.tif")).expect("decode");
    assert_eq!((d.width, d.height), (48, 40));
    assert_eq!(packed_rgb(&d), fixture_rgb(48, 40));
}

#[test]
fn fixture_rgb_tiled_partial_edge_tiles() {
    // 32×32 tiles over 48×40: 2×2 grid where the right column shows
    // 16 px and the bottom row 8 px — the WebP frames stay padded to
    // the full 32×32 tile geometry.
    let d = decode_tiff(&fixture_bytes("ref_rgb_tiled.tif")).expect("decode");
    assert_eq!((d.width, d.height), (48, 40));
    assert_eq!(packed_rgb(&d), fixture_rgb_tiled(48, 40));
}

#[test]
fn fixture_rgb_lossy_vp8_payload() {
    // `VP8 ` (lossy) segment. What this crate owns is the *carriage*:
    // the strip payload must reach the WebP codec byte-identically and
    // its RGBA surface must land in the right strip rows. So the
    // primary assertion is byte-exactness against a direct
    // `oxideav-webp` decode of the extracted RIFF payload; the decoded
    // pixels themselves additionally stay within a loose bound of the
    // generating gradient (the YUV→RGB display conversion of a lossy
    // frame is reader-defined, so cross-reader pixel values differ by
    // design — only gross mis-carriage would blow this bound).
    let bytes = fixture_bytes("ref_rgb_lossy.tif");
    let d = decode_tiff(&bytes).expect("decode");
    assert_eq!((d.width, d.height), (48, 40));
    let got = packed_rgb(&d);

    // Single-strip fixture → exactly one RIFF file inside.
    let riff_at = bytes
        .windows(4)
        .position(|w| w == b"RIFF")
        .expect("RIFF payload present");
    let riff_len = 8 + u32::from_le_bytes([
        bytes[riff_at + 4],
        bytes[riff_at + 5],
        bytes[riff_at + 6],
        bytes[riff_at + 7],
    ]) as usize;
    let direct = oxideav_webp::decode_webp_image(&bytes[riff_at..riff_at + riff_len])
        .expect("payload decodes as WebP");
    assert_eq!((direct.width, direct.height), (48, 40));
    let direct_rgb: Vec<u8> = direct
        .rgba
        .chunks_exact(4)
        .flat_map(|px| px[..3].to_vec())
        .collect();
    assert_eq!(
        got, direct_rgb,
        "TIFF carriage must be byte-exact w.r.t. a direct WebP decode of the payload"
    );

    let want = fixture_rgb_lossy(48, 40);
    let max_diff = got
        .iter()
        .zip(want.iter())
        .map(|(&a, &b)| (i16::from(a) - i16::from(b)).unsigned_abs())
        .max()
        .unwrap();
    assert!(
        max_diff <= 48,
        "lossy WebP-in-TIFF decode strays {max_diff} levels from the source gradient"
    );
}

// ---- 2. Self-roundtrips (webp-encode vs none-encode) ----

fn page<'a>(
    w: u32,
    h: u32,
    kind: EncodePixelFormat<'a>,
    compression: TiffCompression,
    tiling: Option<(u32, u32)>,
    bigtiff: bool,
    extras: PageExtras<'a>,
) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind,
        compression,
        predictor: false,
        planar: false,
        tiling,
        bigtiff,
        extras,
    }
}

/// Encode the same pixels as Compression=50001 and Compression=1,
/// decode both, assert identical pixel planes (VP8L is lossless), and
/// check the 50001 file actually carries RIFF/WEBP segment payloads.
fn webp_vs_none(page_webp: &EncodePage<'_>, page_none: &EncodePage<'_>) -> Vec<u8> {
    let w_bytes = encode_tiff(page_webp).expect("encode webp");
    let n_bytes = encode_tiff(page_none).expect("encode none");
    assert!(
        w_bytes.windows(4).any(|w| w == b"WEBP" || w == b"VP8L"),
        "Compression=50001 output carries no WebP container magic"
    );
    let dw = decode_tiff(&w_bytes).expect("decode webp");
    let dn = decode_tiff(&n_bytes).expect("decode none");
    assert_eq!((dw.width, dw.height), (dn.width, dn.height));
    assert_eq!(dw.pixel_format, dn.pixel_format);
    assert_eq!(
        dw.frame.planes[0].data, dn.frame.planes[0].data,
        "webp decode != none decode of the same pixels"
    );
    w_bytes
}

fn pattern_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(7) & 0xFF) as u8);
            v.push((y.wrapping_mul(11) & 0xFF) as u8);
            v.push(((x ^ y).wrapping_mul(13) & 0xFF) as u8);
        }
    }
    v
}

#[test]
fn roundtrip_rgb_single_strip() {
    let px = pattern_rgb(37, 23); // deliberately non-multiple-of-anything
    let kind = || EncodePixelFormat::Rgb24 { pixels: &px };
    webp_vs_none(
        &page(
            37,
            23,
            kind(),
            TiffCompression::Webp,
            None,
            false,
            PageExtras::default(),
        ),
        &page(
            37,
            23,
            kind(),
            TiffCompression::None,
            None,
            false,
            PageExtras::default(),
        ),
    );
}

#[test]
fn roundtrip_rgb_multi_strip() {
    let px = pattern_rgb(64, 41); // 41 rows at RowsPerStrip=16 → 16/16/9
    let kind = || EncodePixelFormat::Rgb24 { pixels: &px };
    let extras = || PageExtras {
        rows_per_strip: Some(16),
        ..PageExtras::default()
    };
    webp_vs_none(
        &page(64, 41, kind(), TiffCompression::Webp, None, false, extras()),
        &page(64, 41, kind(), TiffCompression::None, None, false, extras()),
    );
}

#[test]
fn roundtrip_rgb_tiled_partial_edges() {
    let px = pattern_rgb(70, 50); // 32×32 tiles → 3×2 grid, ragged edges
    let kind = || EncodePixelFormat::Rgb24 { pixels: &px };
    webp_vs_none(
        &page(
            70,
            50,
            kind(),
            TiffCompression::Webp,
            Some((32, 32)),
            false,
            PageExtras::default(),
        ),
        &page(
            70,
            50,
            kind(),
            TiffCompression::None,
            Some((32, 32)),
            false,
            PageExtras::default(),
        ),
    );
}

#[test]
fn roundtrip_rgb_bigtiff() {
    let px = pattern_rgb(33, 29);
    let kind = || EncodePixelFormat::Rgb24 { pixels: &px };
    webp_vs_none(
        &page(
            33,
            29,
            kind(),
            TiffCompression::Webp,
            None,
            true,
            PageExtras::default(),
        ),
        &page(
            33,
            29,
            kind(),
            TiffCompression::None,
            None,
            true,
            PageExtras::default(),
        ),
    );
}

#[test]
fn roundtrip_rgba_alpha_carried_in_payload() {
    // The TIFF decode drops the extra sample, so the RGB comparison
    // runs through `webp_vs_none`; the alpha carriage is then verified
    // by decoding our emitted single-strip WebP payload directly
    // through `oxideav-webp`'s public API.
    let (w, h) = (21u32, 17u32);
    let rgb = pattern_rgb(w, h);
    let alpha = fixture_alpha(w, h);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for (px, &a) in rgb.chunks_exact(3).zip(alpha.iter()) {
        rgba.extend_from_slice(px);
        rgba.push(a);
    }
    let kind = || EncodePixelFormat::Rgba32 {
        pixels: &rgba,
        kind: ExtraSampleKind::UnassociatedAlpha,
    };
    let w_bytes = webp_vs_none(
        &page(
            w,
            h,
            kind(),
            TiffCompression::Webp,
            None,
            false,
            PageExtras::default(),
        ),
        &page(
            w,
            h,
            kind(),
            TiffCompression::None,
            None,
            false,
            PageExtras::default(),
        ),
    );
    // Single-strip page → exactly one RIFF file inside; find it and
    // re-decode it as a bare WebP still image.
    let riff_at = w_bytes
        .windows(8)
        .position(|win| &win[..4] == b"RIFF")
        .expect("RIFF payload present");
    let riff_len = 8 + u32::from_le_bytes([
        w_bytes[riff_at + 4],
        w_bytes[riff_at + 5],
        w_bytes[riff_at + 6],
        w_bytes[riff_at + 7],
    ]) as usize;
    let payload = &w_bytes[riff_at..riff_at + riff_len];
    let dec = oxideav_webp::decode_webp_image(payload).expect("strip payload is a WebP file");
    assert_eq!((dec.width, dec.height), (w, h));
    assert_eq!(dec.rgba, rgba, "RGBA (incl. alpha) not carried losslessly");
}

#[test]
fn roundtrip_multi_page() {
    let px0 = pattern_rgb(30, 20);
    let px1 = fixture_rgb(25, 19);
    let pages_of = |c: TiffCompression| {
        vec![
            page(
                30,
                20,
                EncodePixelFormat::Rgb24 { pixels: &px0 },
                c,
                None,
                false,
                PageExtras::default(),
            ),
            page(
                25,
                19,
                EncodePixelFormat::Rgb24 { pixels: &px1 },
                c,
                None,
                false,
                PageExtras::default(),
            ),
        ]
    };
    let w_bytes = encode_tiff_multi(&pages_of(TiffCompression::Webp)).expect("encode webp");
    let n_bytes = encode_tiff_multi(&pages_of(TiffCompression::None)).expect("encode none");
    let dw = decode_tiff_all(&w_bytes).expect("decode webp chain");
    let dn = decode_tiff_all(&n_bytes).expect("decode none chain");
    assert_eq!(dw.len(), 2);
    assert_eq!(dn.len(), 2);
    for (a, b) in dw.iter().zip(dn.iter()) {
        assert_eq!((a.width, a.height), (b.width, b.height));
        assert_eq!(a.planes[0].data, b.planes[0].data);
    }
}

// ---- 3. Shape rejections ----

#[test]
fn encode_rejects_non_rgb_input() {
    let px = vec![0u8; 16 * 16];
    let p = page(
        16,
        16,
        EncodePixelFormat::Gray8 { pixels: &px },
        TiffCompression::Webp,
        None,
        false,
        PageExtras::default(),
    );
    assert!(encode_tiff(&p).is_err(), "Gray8 + WebP must be rejected");
}

#[test]
fn encode_rejects_predictor_and_planar() {
    let px = pattern_rgb(16, 16);
    let mut p = page(
        16,
        16,
        EncodePixelFormat::Rgb24 { pixels: &px },
        TiffCompression::Webp,
        None,
        false,
        PageExtras::default(),
    );
    p.predictor = true;
    assert!(
        encode_tiff(&p).is_err(),
        "Predictor + WebP must be rejected"
    );
    p.predictor = false;
    p.planar = true;
    assert!(encode_tiff(&p).is_err(), "Planar + WebP must be rejected");
}

#[test]
fn decode_rejects_declared_predictor() {
    // TIFF 6.0 §14 reader rule: a Compression=50001 file declaring a
    // Predictor has no reversible byte-stream meaning — must be
    // rejected, not mis-decoded. Build the hostile file by patching a
    // Predictor=2 tag into a valid fixture's IFD: the fixture writer
    // emitted no tag 317, so instead re-purpose our own encoder's
    // output is impossible (the encoder refuses the combo) — patch the
    // ExtraSamples-free RGB fixture by rewriting its Software tag
    // (305, count ≥ 3 SHORT-equivalent space) is fragile too; the
    // robust route: flip the ResolutionUnit tag (296, SHORT count 1)
    // of `ref_rgb_lossless.tif` into Predictor=2 (317, SHORT count 1).
    let mut bytes = fixture_bytes("ref_rgb_lossless.tif");
    // Classic little-endian TIFF: IFD offset at byte 4.
    assert_eq!(&bytes[..4], b"II\x2a\x00");
    let ifd = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let n = u16::from_le_bytes([bytes[ifd], bytes[ifd + 1]]) as usize;
    let mut patched = false;
    for i in 0..n {
        let e = ifd + 2 + i * 12;
        let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
        if tag == 296 {
            // tag := 317 (Predictor), type SHORT, count 1, value 2.
            bytes[e..e + 2].copy_from_slice(&317u16.to_le_bytes());
            bytes[e + 8..e + 12].copy_from_slice(&2u32.to_le_bytes());
            patched = true;
            break;
        }
    }
    assert!(patched, "fixture lost its ResolutionUnit tag");
    // Tag order stays ascending (296 → 317 slot sits between 284 and
    // 305? no — entries must be ascending; 317 lands after 305/306 in
    // the original order, so re-sort the entry table to keep the IFD
    // well-formed).
    let mut entries: Vec<[u8; 12]> = (0..n)
        .map(|i| {
            let e = ifd + 2 + i * 12;
            let mut a = [0u8; 12];
            a.copy_from_slice(&bytes[e..e + 12]);
            a
        })
        .collect();
    entries.sort_by_key(|a| u16::from_le_bytes([a[0], a[1]]));
    for (i, a) in entries.iter().enumerate() {
        let e = ifd + 2 + i * 12;
        bytes[e..e + 12].copy_from_slice(a);
    }
    let err = decode_tiff(&bytes)
        .err()
        .expect("Predictor=2 + 50001 must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Predictor"),
        "rejection should name the Predictor; got: {msg}"
    );
}

#[test]
fn decode_rejects_wrong_frame_geometry() {
    // A strip payload whose embedded WebP frame does not match the IFD
    // geometry must fail loudly. Shrink the fixture's ImageWidth tag:
    // the (valid) 48-wide WebP frame then mismatches the declared 32.
    let mut bytes = fixture_bytes("ref_rgb_lossless.tif");
    let ifd = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    let n = u16::from_le_bytes([bytes[ifd], bytes[ifd + 1]]) as usize;
    for i in 0..n {
        let e = ifd + 2 + i * 12;
        let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
        if tag == 256 {
            bytes[e + 8..e + 12].copy_from_slice(&32u32.to_le_bytes());
        }
    }
    assert!(
        decode_tiff(&bytes).is_err(),
        "frame/IFD geometry mismatch must be rejected"
    );
}
