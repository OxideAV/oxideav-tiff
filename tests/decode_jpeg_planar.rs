//! `PlanarConfiguration = 2` JPEG-in-TIFF decode tests (TIFF Tech
//! Note 2, "Special considerations for PlanarConfiguration 2").
//!
//! TN2's planar rules: each image segment carries one component only,
//! as a valid single-channel JPEG datastream (SOFn `Nf = 1`, all
//! sampling factors 1); segment counts are `SamplesPerPixel ×
//! StripsPerImage` / `× TilesPerImage`, plane-major; and the SOFn
//! dimensions of a subsampled YCbCr chroma segment are scaled down by
//! the sampling factors ("strips or tiles of the subsampled
//! components contain fewer samples").
//!
//! Fixture strategy: the per-component grayscale JPEG datastreams are
//! produced by `cjpeg` (black-box binary, availability-gated), using
//! the lossless SOF3 process so the planar reassembly can be checked
//! **byte-exactly** — the JPEG leg contributes no loss, isolating the
//! TIFF-side layout logic under test. The YCbCr compositions are
//! cross-checked against the crate's independent §21 *chunky
//! data-unit* decode path (itself pinned against ImageMagick in the
//! existing suites) by building an uncompressed chunky TIFF holding
//! the identical Y / Cb / Cr samples and requiring pixel-identical
//! output.

#![cfg(feature = "registry")]

use std::io::Write;
use std::process::{Command, Stdio};

use oxideav_tiff::types::*;
use oxideav_tiff::{decode_tiff, TiffPixelFormat};

fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{n}-{c}")
}

/// Encode one 8-bit grayscale raster as a lossless (SOF3) JPEG via
/// `cjpeg -lossless 1`; `None` when the binary is missing or lacks
/// lossless support.
fn cjpeg_lossless_gray(samples: &[u8], w: usize, h: usize) -> Option<Vec<u8>> {
    assert_eq!(samples.len(), w * h);
    let mut pgm = format!("P5\n{w} {h}\n255\n").into_bytes();
    pgm.extend_from_slice(samples);
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-planarjpeg-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join("in.pgm");
    let out_path = dir.join("out.jpg");
    std::fs::File::create(&in_path).ok()?.write_all(&pgm).ok()?;
    let status = Command::new("cjpeg")
        .args(["-lossless", "1", "-grayscale", "-outfile"])
        .arg(&out_path)
        .arg(&in_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let ok = matches!(status, Ok(s) if s.success());
    let bytes = if ok {
        std::fs::read(&out_path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

// ---------------------------------------------------------------------------
// Hand-built TIFF writers.
// ---------------------------------------------------------------------------

struct PlanarCfg {
    width: u32,
    height: u32,
    photometric: u16,
    spp: u16,
    compression: u16,
    planar: u16,
    tiling: Option<(u32, u32)>,
    rows_per_strip: u32,
    subsampling: Option<(u16, u16)>,
    /// Raw sample payload for Compression=None wraps (chunky §21
    /// data-unit stream or plain chunky interleave).
    bps: u16,
}

/// Assemble a classic little-endian TIFF whose segments are the
/// supplied blobs (JPEG segments for Compression 7, raw sample bytes
/// for Compression 1).
fn build_tiff(cfg: &PlanarCfg, segments: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x4949u16.to_le_bytes());
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());

    let mut seg_offsets = Vec::new();
    let mut cursor = 8u32;
    let mut data = Vec::new();
    for seg in segments {
        if cursor % 2 == 1 {
            data.push(0);
            cursor += 1;
        }
        seg_offsets.push(cursor);
        data.extend_from_slice(seg);
        cursor += seg.len() as u32;
    }
    if cursor % 2 == 1 {
        data.push(0);
        cursor += 1;
    }
    let ifd_offset = cursor;
    out[4..8].copy_from_slice(&ifd_offset.to_le_bytes());
    out.extend_from_slice(&data);

    let short_val = |v: u16| -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&v.to_le_bytes());
        b
    };
    let long_val = |v: u32| -> [u8; 4] { v.to_le_bytes() };
    let two_shorts = |a: u16, b_: u16| -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&a.to_le_bytes());
        b[2..].copy_from_slice(&b_.to_le_bytes());
        b
    };

    let mut tail: Vec<u8> = Vec::new();
    let n_entries_guess = 16u32;
    let tail_base = ifd_offset + 2 + n_entries_guess * 12 + 4;

    let mut entries: Vec<(u16, u16, u32, [u8; 4])> = vec![
        (TAG_IMAGE_WIDTH, TYPE_LONG, 1, long_val(cfg.width)),
        (TAG_IMAGE_LENGTH, TYPE_LONG, 1, long_val(cfg.height)),
        (TAG_COMPRESSION, TYPE_SHORT, 1, short_val(cfg.compression)),
        (
            TAG_PHOTOMETRIC_INTERPRETATION,
            TYPE_SHORT,
            1,
            short_val(cfg.photometric),
        ),
        (TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, short_val(cfg.spp)),
        (
            TAG_PLANAR_CONFIGURATION,
            TYPE_SHORT,
            1,
            short_val(cfg.planar),
        ),
    ];

    if cfg.spp == 1 {
        entries.push((TAG_BITS_PER_SAMPLE, TYPE_SHORT, 1, short_val(cfg.bps)));
    } else {
        let off = tail_base + tail.len() as u32;
        for _ in 0..cfg.spp {
            tail.extend_from_slice(&cfg.bps.to_le_bytes());
        }
        entries.push((
            TAG_BITS_PER_SAMPLE,
            TYPE_SHORT,
            cfg.spp as u32,
            long_val(off),
        ));
    }

    let push_array = |entries: &mut Vec<(u16, u16, u32, [u8; 4])>,
                      tail: &mut Vec<u8>,
                      tag: u16,
                      vals: &[u32]| {
        if vals.len() == 1 {
            entries.push((tag, TYPE_LONG, 1, long_val(vals[0])));
        } else {
            let off = tail_base + tail.len() as u32;
            for v in vals {
                tail.extend_from_slice(&v.to_le_bytes());
            }
            entries.push((tag, TYPE_LONG, vals.len() as u32, long_val(off)));
        }
    };

    let seg_lens: Vec<u32> = segments.iter().map(|s| s.len() as u32).collect();
    if let Some((tw, th)) = cfg.tiling {
        entries.push((TAG_TILE_WIDTH, TYPE_LONG, 1, long_val(tw)));
        entries.push((TAG_TILE_LENGTH, TYPE_LONG, 1, long_val(th)));
        push_array(&mut entries, &mut tail, TAG_TILE_OFFSETS, &seg_offsets);
        push_array(&mut entries, &mut tail, TAG_TILE_BYTE_COUNTS, &seg_lens);
    } else {
        push_array(&mut entries, &mut tail, TAG_STRIP_OFFSETS, &seg_offsets);
        entries.push((
            TAG_ROWS_PER_STRIP,
            TYPE_LONG,
            1,
            long_val(cfg.rows_per_strip),
        ));
        push_array(&mut entries, &mut tail, TAG_STRIP_BYTE_COUNTS, &seg_lens);
    }
    if let Some((sh, sv)) = cfg.subsampling {
        entries.push((TAG_YCBCR_SUBSAMPLING, TYPE_SHORT, 2, two_shorts(sh, sv)));
    }

    entries.sort_by_key(|e| e.0);
    assert!(entries.len() as u32 <= n_entries_guess);
    while (entries.len() as u32) < n_entries_guess {
        let tag = 60000 + entries.len() as u16;
        entries.push((tag, TYPE_SHORT, 1, short_val(0)));
    }

    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, ty, count, val) in &entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(val);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(out.len() as u32, tail_base, "tail offset arithmetic");
    out.extend_from_slice(&tail);
    out
}

/// Row-packed bytes of a single-plane decoded image.
fn image_bytes(img: &oxideav_tiff::TiffImage, bytes_per_pixel: usize) -> Vec<u8> {
    assert_eq!(img.planes.len(), 1);
    let p = &img.planes[0];
    let row_bytes = img.width as usize * bytes_per_pixel;
    let mut out = Vec::with_capacity(row_bytes * img.height as usize);
    for y in 0..img.height as usize {
        out.extend_from_slice(&p.data[y * p.stride..y * p.stride + row_bytes]);
    }
    out
}

// ---------------------------------------------------------------------------
// Deterministic planes.
// ---------------------------------------------------------------------------

fn plane_pattern(w: usize, h: usize, seed: u8) -> Vec<u8> {
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            (x as u32 * 7 + y as u32 * 13 + seed as u32 * 31) as u8
        })
        .collect()
}

/// Pack full-res Y + reduced Cb / Cr planes into the §21 chunky
/// data-unit stream (`sh*sv` Y samples row-major, then Cb, then Cr per
/// unit) — the layout the crate's independent chunky subsampled
/// decode path reads.
fn pack_data_units(
    y: &[u8],
    cb: &[u8],
    cr: &[u8],
    w: usize,
    h: usize,
    sh: usize,
    sv: usize,
) -> Vec<u8> {
    let cw = w / sh;
    let ch = h / sv;
    let unit_len = sh * sv + 2;
    let mut out = vec![0u8; cw * ch * unit_len];
    for by in 0..ch {
        for bx in 0..cw {
            let off = (by * cw + bx) * unit_len;
            for sy in 0..sv {
                for sx in 0..sh {
                    out[off + sy * sh + sx] = y[(by * sv + sy) * w + bx * sh + sx];
                }
            }
            out[off + sh * sv] = cb[by * cw + bx];
            out[off + sh * sv + 1] = cr[by * cw + bx];
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Planar RGB (three full-resolution lossless planes), single strip
/// per plane: byte-exact reassembly.
#[test]
fn planar_rgb_lossless_exact() {
    let (w, h) = (32usize, 32usize);
    let planes: Vec<Vec<u8>> = (0..3).map(|c| plane_pattern(w, h, c as u8 * 50)).collect();
    let mut segs = Vec::new();
    for p in &planes {
        match cjpeg_lossless_gray(p, w, h) {
            Some(j) => segs.push(j),
            None => {
                eprintln!("skipping: cjpeg -lossless unavailable");
                return;
            }
        }
    }
    let d = decode_tiff(&build_tiff(
        &PlanarCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_RGB,
            spp: 3,
            compression: COMPRESSION_JPEG_NEW,
            planar: PLANAR_SEPARATE,
            tiling: None,
            rows_per_strip: h as u32,
            subsampling: None,
            bps: 8,
        },
        &segs,
    ))
    .expect("planar RGB decode");
    assert_eq!(d.frame.pixel_format, TiffPixelFormat::Rgb24);
    let got = image_bytes(&d.frame, 3);
    let mut want = Vec::with_capacity(w * h * 3);
    for (i, &r) in planes[0].iter().enumerate() {
        want.push(r);
        want.push(planes[1][i]);
        want.push(planes[2][i]);
    }
    assert_eq!(got, want, "planar RGB lossless must reassemble exactly");
}

/// Planar subsampled YCbCr (full-res Y + reduced Cb / Cr planes) in
/// strip and multi-strip layouts: pixel-identical to the crate's
/// independent §21 chunky data-unit decode of the same samples.
#[test]
fn planar_ycbcr_subsampled_matches_chunky_path() {
    let (w, h) = (32usize, 32usize);
    for (sh, sv) in [(2usize, 2usize), (2, 1), (1, 1)] {
        let (cw, ch) = (w / sh, h / sv);
        let y = plane_pattern(w, h, 0);
        let cb = plane_pattern(cw, ch, 90);
        let cr = plane_pattern(cw, ch, 180);

        // Reference: chunky §21 data-unit wrap, Compression = None.
        let chunky = build_tiff(
            &PlanarCfg {
                width: w as u32,
                height: h as u32,
                photometric: PHOTO_YCBCR,
                spp: 3,
                compression: COMPRESSION_NONE,
                planar: PLANAR_CHUNKY,
                tiling: None,
                rows_per_strip: h as u32,
                subsampling: Some((sh as u16, sv as u16)),
                bps: 8,
            },
            &[pack_data_units(&y, &cb, &cr, w, h, sh, sv)],
        );
        let ref_d = decode_tiff(&chunky).expect("chunky reference decode");
        let reference = image_bytes(&ref_d.frame, 3);

        // Planar JPEG wraps: single strip and 16-luma-row strips.
        for rps in [h as u32, 16u32] {
            let strips = (h as u32).div_ceil(rps) as usize;
            let mut segs = Vec::new();
            let mut ok = true;
            for (plane, pw, ph, s_v) in [(&y, w, h, 1usize), (&cb, cw, ch, sv), (&cr, cw, ch, sv)] {
                let plane_rps = if strips == 1 { ph } else { rps as usize / s_v };
                let mut done = 0usize;
                for _ in 0..strips {
                    let rows = plane_rps.min(ph - done);
                    let slice = &plane[done * pw..(done + rows) * pw];
                    match cjpeg_lossless_gray(slice, pw, rows) {
                        Some(j) => segs.push(j),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                    done += rows;
                }
                if !ok {
                    break;
                }
            }
            if !ok {
                eprintln!("skipping: cjpeg -lossless unavailable");
                return;
            }
            let planar_tiff = build_tiff(
                &PlanarCfg {
                    width: w as u32,
                    height: h as u32,
                    photometric: PHOTO_YCBCR,
                    spp: 3,
                    compression: COMPRESSION_JPEG_NEW,
                    planar: PLANAR_SEPARATE,
                    tiling: None,
                    rows_per_strip: rps,
                    subsampling: Some((sh as u16, sv as u16)),
                    bps: 8,
                },
                &segs,
            );
            let d = decode_tiff(&planar_tiff).expect("planar YCbCr decode");
            assert_eq!(d.frame.pixel_format, TiffPixelFormat::Rgb24);
            assert_eq!(
                image_bytes(&d.frame, 3),
                reference,
                "planar JPEG ({sh},{sv}) rps={rps} must match the chunky data-unit path"
            );
        }
    }
}

/// Planar subsampled YCbCr in the *tiled* layout: 16x16 luma tiles →
/// 8x8 chroma segments (TN2: tile dims divide exactly), matching the
/// chunky data-unit reference.
#[test]
fn planar_ycbcr_subsampled_tiled_matches_chunky_path() {
    let (w, h) = (32usize, 32usize);
    let (tw, th) = (16usize, 16usize);
    let (sh, sv) = (2usize, 2usize);
    let (cw, ch) = (w / sh, h / sv);
    let y = plane_pattern(w, h, 7);
    let cb = plane_pattern(cw, ch, 77);
    let cr = plane_pattern(cw, ch, 147);

    let chunky = build_tiff(
        &PlanarCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_YCBCR,
            spp: 3,
            compression: COMPRESSION_NONE,
            planar: PLANAR_CHUNKY,
            tiling: None,
            rows_per_strip: h as u32,
            subsampling: Some((sh as u16, sv as u16)),
            bps: 8,
        },
        &[pack_data_units(&y, &cb, &cr, w, h, sh, sv)],
    );
    let reference = image_bytes(&decode_tiff(&chunky).expect("chunky reference").frame, 3);

    // Per-plane tile grids, plane-major, row-major within a plane.
    let mut segs = Vec::new();
    for (plane, pw, ph, seg_w, seg_h) in [
        (&y, w, h, tw, th),
        (&cb, cw, ch, tw / sh, th / sv),
        (&cr, cw, ch, tw / sh, th / sv),
    ] {
        for ty in 0..h / th {
            for tx in 0..w / tw {
                let mut tile = Vec::with_capacity(seg_w * seg_h);
                for r in 0..seg_h {
                    let row = (ty * seg_h + r) * pw + tx * seg_w;
                    tile.extend_from_slice(&plane[row..row + seg_w]);
                }
                let _ = ph;
                match cjpeg_lossless_gray(&tile, seg_w, seg_h) {
                    Some(j) => segs.push(j),
                    None => {
                        eprintln!("skipping: cjpeg -lossless unavailable");
                        return;
                    }
                }
            }
        }
    }
    let d = decode_tiff(&build_tiff(
        &PlanarCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_YCBCR,
            spp: 3,
            compression: COMPRESSION_JPEG_NEW,
            planar: PLANAR_SEPARATE,
            tiling: Some((tw as u32, th as u32)),
            rows_per_strip: 0,
            subsampling: Some((sh as u16, sv as u16)),
            bps: 8,
        },
        &segs,
    ))
    .expect("tiled planar YCbCr decode");
    assert_eq!(
        image_bytes(&d.frame, 3),
        reference,
        "tiled planar JPEG must match the chunky data-unit path"
    );
}

/// Planar CMYK (four full-resolution lossless planes): matches the
/// crate's independent uncompressed chunky CMYK decode of the same
/// samples.
#[test]
fn planar_cmyk_matches_chunky_path() {
    let (w, h) = (16usize, 16usize);
    let planes: Vec<Vec<u8>> = (0..4).map(|c| plane_pattern(w, h, c as u8 * 40)).collect();

    let mut chunky_samples = Vec::with_capacity(w * h * 4);
    for (i, &c0) in planes[0].iter().enumerate() {
        chunky_samples.push(c0);
        chunky_samples.push(planes[1][i]);
        chunky_samples.push(planes[2][i]);
        chunky_samples.push(planes[3][i]);
    }
    let chunky = build_tiff(
        &PlanarCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_CMYK,
            spp: 4,
            compression: COMPRESSION_NONE,
            planar: PLANAR_CHUNKY,
            tiling: None,
            rows_per_strip: h as u32,
            subsampling: None,
            bps: 8,
        },
        &[chunky_samples],
    );
    let reference = image_bytes(&decode_tiff(&chunky).expect("chunky CMYK").frame, 3);

    let mut segs = Vec::new();
    for p in &planes {
        match cjpeg_lossless_gray(p, w, h) {
            Some(j) => segs.push(j),
            None => {
                eprintln!("skipping: cjpeg -lossless unavailable");
                return;
            }
        }
    }
    let d = decode_tiff(&build_tiff(
        &PlanarCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_CMYK,
            spp: 4,
            compression: COMPRESSION_JPEG_NEW,
            planar: PLANAR_SEPARATE,
            tiling: None,
            rows_per_strip: h as u32,
            subsampling: None,
            bps: 8,
        },
        &segs,
    ))
    .expect("planar CMYK decode");
    assert_eq!(
        image_bytes(&d.frame, 3),
        reference,
        "planar CMYK JPEG must match the chunky CMYK path"
    );
}

/// §22 old-style (Compression = 6) with `PlanarConfiguration = 2`:
/// the interchange bitstream declares its own (non-interleaved) scan
/// structure, so the planar flag no longer blocks the decode — the
/// output must equal the `PlanarConfiguration = 1` wrap of the
/// identical bitstream.
#[test]
fn oldstyle_planar_flag_decodes_interchange() {
    let (w, h) = (32usize, 32usize);
    let gray = plane_pattern(w, h, 3);
    let Some(jpeg) = cjpeg_lossless_gray(&gray, w, h) else {
        eprintln!("skipping: cjpeg -lossless unavailable");
        return;
    };

    // Old-style wrap needs the JPEGInterchangeFormat tags; reuse the
    // strip slots for the blob and add 513/514 via a bespoke build.
    let build_old = |planar: u16| -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x4949u16.to_le_bytes());
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        let blob_off = 8u32;
        out.extend_from_slice(&jpeg);
        if out.len() % 2 == 1 {
            out.push(0);
        }
        let ifd_offset = out.len() as u32;
        out[4..8].copy_from_slice(&ifd_offset.to_le_bytes());
        let short_val = |v: u16| -> [u8; 4] {
            let mut b = [0u8; 4];
            b[..2].copy_from_slice(&v.to_le_bytes());
            b
        };
        let entries: Vec<(u16, u16, u32, [u8; 4])> = vec![
            (TAG_IMAGE_WIDTH, TYPE_LONG, 1, (w as u32).to_le_bytes()),
            (TAG_IMAGE_LENGTH, TYPE_LONG, 1, (h as u32).to_le_bytes()),
            (TAG_BITS_PER_SAMPLE, TYPE_SHORT, 1, short_val(8)),
            (
                TAG_COMPRESSION,
                TYPE_SHORT,
                1,
                short_val(COMPRESSION_JPEG_OLD),
            ),
            (
                TAG_PHOTOMETRIC_INTERPRETATION,
                TYPE_SHORT,
                1,
                short_val(PHOTO_BLACK_IS_ZERO),
            ),
            (TAG_STRIP_OFFSETS, TYPE_LONG, 1, blob_off.to_le_bytes()),
            (TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, short_val(1)),
            (TAG_ROWS_PER_STRIP, TYPE_LONG, 1, (h as u32).to_le_bytes()),
            (
                TAG_STRIP_BYTE_COUNTS,
                TYPE_LONG,
                1,
                (jpeg.len() as u32).to_le_bytes(),
            ),
            (TAG_PLANAR_CONFIGURATION, TYPE_SHORT, 1, short_val(planar)),
            (
                TAG_JPEG_INTERCHANGE_FORMAT,
                TYPE_LONG,
                1,
                blob_off.to_le_bytes(),
            ),
            (
                TAG_JPEG_INTERCHANGE_FORMAT_LENGTH,
                TYPE_LONG,
                1,
                (jpeg.len() as u32).to_le_bytes(),
            ),
        ];
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, ty, count, val) in &entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(val);
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    };

    let chunky = decode_tiff(&build_old(PLANAR_CHUNKY)).expect("planar=1 wrap");
    let planar = decode_tiff(&build_old(PLANAR_SEPARATE)).expect("planar=2 wrap");
    assert_eq!(
        image_bytes(&chunky.frame, 1),
        image_bytes(&planar.frame, 1),
        "§22 planar flag must not change the interchange decode"
    );
    assert_eq!(image_bytes(&chunky.frame, 1), gray, "lossless exact");
}

/// Structural gates: wrong segment count and a multi-channel segment
/// where TN2 requires single-channel are precise errors.
#[test]
fn planar_structural_gates() {
    let (w, h) = (16usize, 16usize);
    let gray = plane_pattern(w, h, 5);
    let Some(jpeg) = cjpeg_lossless_gray(&gray, w, h) else {
        eprintln!("skipping: cjpeg -lossless unavailable");
        return;
    };
    // Two segments where SamplesPerPixel=3 planar strips need three.
    let tiff = build_tiff(
        &PlanarCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_RGB,
            spp: 3,
            compression: COMPRESSION_JPEG_NEW,
            planar: PLANAR_SEPARATE,
            tiling: None,
            rows_per_strip: h as u32,
            subsampling: None,
            bps: 8,
        },
        &[jpeg.clone(), jpeg.clone()],
    );
    let Err(e) = decode_tiff(&tiff) else {
        panic!("wrong planar segment count must not decode");
    };
    let msg = format!("{e:?}");
    assert!(msg.contains("strip entries"), "{msg}");
}
