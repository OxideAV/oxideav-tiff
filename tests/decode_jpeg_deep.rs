//! Deep-precision (9..=16-bit) JPEG-in-TIFF decode tests.
//!
//! TIFF Tech Note 2: "The data precision field of the SOFn marker
//! shall agree with the TIFF BitsPerSample field. ... For SOF0 only
//! precision 8 is permitted; for SOF1, precision 8 or 12 is
//! permitted; for SOF3, precisions 2 to 16 are permitted." This suite
//! exercises the 12-bit SOF1 (extended sequential) and 12-/16-bit
//! SOF3 (lossless) precisions over the Compression = 7 strip and tile
//! layouts, plus the §22 old-style interchange wrap of the same
//! bitstreams. Deep grayscale renders to `Gray16Le` and deep YCbCr /
//! RGB to `Rgb48Le`, each raw code value widened onto the full 16-bit
//! display extent by bit replication (the same display map the 4-bit
//! grayscale path applies at 8 bits).
//!
//! Fixture strategy: `cjpeg` / `djpeg` (libjpeg-turbo binaries,
//! invoked as opaque black-box processes per workspace policy)
//! produce the deep JPEG bitstreams and the reference decodes; the
//! TIFF wrapping is hand-built here. Every test that needs the
//! binaries skips (with a note) when they are missing or lack
//! `-precision` support. The lossless (SOF3) round trips are
//! byte-exact against the synthetic source raster — no tolerance —
//! while the DCT (SOF1) paths compare against `djpeg`'s output with a
//! small per-sample tolerance (two independent IDCT implementations
//! legitimately differ by ±1 code value; ISO 10918-2 compliance is
//! tolerance-based for exactly this reason).

#![cfg(feature = "registry")]

use std::io::Write;
use std::process::{Command, Stdio};

use oxideav_tiff::types::*;
use oxideav_tiff::{decode_tiff, TiffPixelFormat};

// ---------------------------------------------------------------------------
// Black-box binary plumbing (availability-gated).
// ---------------------------------------------------------------------------

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

/// Run `cjpeg [args...] < in_bytes` writing `in_bytes` to a temp file
/// (cjpeg reads PNM) and returning the JPEG bytes, or `None` when the
/// binary is missing / rejects the arguments (e.g. a libjpeg build
/// without `-precision` support).
fn cjpeg(input_pnm: &[u8], args: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-deepjpeg-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join("in.pnm");
    let out_path = dir.join("out.jpg");
    std::fs::File::create(&in_path)
        .ok()?
        .write_all(input_pnm)
        .ok()?;
    let mut cmd = Command::new("cjpeg");
    for a in args {
        cmd.arg(a);
    }
    cmd.arg("-outfile").arg(&out_path).arg(&in_path);
    let status = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
    let ok = matches!(status, Ok(s) if s.success());
    let bytes = if ok {
        std::fs::read(&out_path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

/// Run `djpeg [args...] -outfile out.pnm in.jpg` and return the PNM
/// bytes.
fn djpeg(jpeg: &[u8], args: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-deepjpeg-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join("in.jpg");
    let out_path = dir.join("out.pnm");
    std::fs::File::create(&in_path).ok()?.write_all(jpeg).ok()?;
    let mut cmd = Command::new("djpeg");
    for a in args {
        cmd.arg(a);
    }
    let status = cmd
        .arg("-outfile")
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
// Synthetic deep rasters + PNM encode/decode helpers.
// ---------------------------------------------------------------------------

/// Deterministic `bits`-deep grayscale gradient, row-major u16 raw
/// code values.
fn gray_pattern(w: usize, h: usize, bits: u16) -> Vec<u16> {
    let maxv = (1u32 << bits) - 1;
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            ((x as u32 * 131 + y as u32 * 97) % (maxv + 1)) as u16
        })
        .collect()
}

/// Deterministic `bits`-deep RGB pattern, row-major (r, g, b) u16 raw
/// code values. Smooth (continuous-tone) so DCT compression stays
/// well-behaved.
fn rgb_pattern(w: usize, h: usize, bits: u16) -> Vec<u16> {
    let maxv = (1u32 << bits) - 1;
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            out.push(((x as u32 * maxv) / (w as u32 - 1).max(1)) as u16);
            out.push(((y as u32 * maxv) / (h as u32 - 1).max(1)) as u16);
            out.push((((x + y) as u32 * maxv) / ((w + h - 2) as u32).max(1)) as u16);
        }
    }
    out
}

/// Serialize a deep grayscale raster as a big-endian 16-bit PGM with
/// `maxval = 2^bits - 1` (the PNM layout `cjpeg -precision N` reads).
fn to_pgm16(samples: &[u16], w: usize, h: usize, bits: u16) -> Vec<u8> {
    assert_eq!(samples.len(), w * h);
    let maxv = (1u32 << bits) - 1;
    let mut v = format!("P5\n{w} {h}\n{maxv}\n").into_bytes();
    for s in samples {
        v.extend_from_slice(&s.to_be_bytes());
    }
    v
}

/// Serialize a deep RGB raster as a big-endian 16-bit PPM.
fn to_ppm16(samples: &[u16], w: usize, h: usize, bits: u16) -> Vec<u8> {
    assert_eq!(samples.len(), w * h * 3);
    let maxv = (1u32 << bits) - 1;
    let mut v = format!("P6\n{w} {h}\n{maxv}\n").into_bytes();
    for s in samples {
        v.extend_from_slice(&s.to_be_bytes());
    }
    v
}

/// Parse a binary PGM/PPM with a 2-byte maxval into raw u16 samples.
fn from_pnm16(pnm: &[u8]) -> Option<(Vec<u16>, usize, usize)> {
    let text = pnm;
    // Header: magic, width, height, maxval, single whitespace, data.
    let mut fields = Vec::new();
    let mut i = 0usize;
    while fields.len() < 4 && i < text.len() {
        while i < text.len() && text[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < text.len() && text[i] == b'#' {
            while i < text.len() && text[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < text.len() && !text[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(&text[start..i]);
    }
    if fields.len() < 4 {
        return None;
    }
    i += 1; // single whitespace after maxval
    let w: usize = std::str::from_utf8(fields[1]).ok()?.parse().ok()?;
    let h: usize = std::str::from_utf8(fields[2]).ok()?.parse().ok()?;
    let maxval: u32 = std::str::from_utf8(fields[3]).ok()?.parse().ok()?;
    if maxval < 256 {
        return None;
    }
    let samples = pnm[i..]
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    Some((samples, w, h))
}

/// The decoder's deep display widening: raw `bits`-precision code
/// value → 16-bit extent by bit replication.
fn widen(v: u16, bits: u16) -> u16 {
    if bits >= 16 {
        v
    } else {
        (v << (16 - bits)) | (v >> (2 * bits - 16))
    }
}

// ---------------------------------------------------------------------------
// Hand-built TIFF wrappers (classic, little-endian).
// ---------------------------------------------------------------------------

struct DeepCfg {
    width: u32,
    height: u32,
    photometric: u16,
    spp: u16,
    bps: u16,
    compression: u16,
    /// (tile_w, tile_h) for a tiled layout; strips otherwise.
    tiling: Option<(u32, u32)>,
    rows_per_strip: u32,
    subsampling: Option<(u16, u16)>,
}

/// Assemble a classic little-endian TIFF whose strips/tiles are the
/// supplied JPEG blobs (Compression = 7 shape; or Compression = 6
/// with `JPEGInterchangeFormat` when `compression == 6` and a single
/// segment is given).
fn build_tiff(cfg: &DeepCfg, segments: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x4949u16.to_le_bytes()); // II
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes()); // first IFD offset (patched: IFD after data)

    // Data area starts at offset 8; lay the JPEG segments down first.
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

    // IFD entries, ascending tag order.
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

    // Out-of-line arrays (offsets/bytecounts when > 1 segment, or
    // BitsPerSample when spp = 3).
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
    ];

    // BitsPerSample: inline for spp 1/2, out-of-line for 3+.
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
    if cfg.compression == COMPRESSION_JPEG_OLD {
        assert_eq!(segments.len(), 1, "old-style wrap is single-stream");
        entries.push((TAG_STRIP_OFFSETS, TYPE_LONG, 1, long_val(seg_offsets[0])));
        entries.push((TAG_ROWS_PER_STRIP, TYPE_LONG, 1, long_val(cfg.height)));
        entries.push((TAG_STRIP_BYTE_COUNTS, TYPE_LONG, 1, long_val(seg_lens[0])));
        entries.push((
            TAG_JPEG_INTERCHANGE_FORMAT,
            TYPE_LONG,
            1,
            long_val(seg_offsets[0]),
        ));
        entries.push((
            TAG_JPEG_INTERCHANGE_FORMAT_LENGTH,
            TYPE_LONG,
            1,
            long_val(seg_lens[0]),
        ));
    } else if let Some((tw, th)) = cfg.tiling {
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
    // Pad the entry table to the guessed count with harmless
    // ascending private tags so tail_base stays valid.
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
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD
    assert_eq!(out.len() as u32, tail_base, "tail offset arithmetic");
    out.extend_from_slice(&tail);
    out
}

/// Row-packed u16 samples of a decoded 16-bit plane.
fn plane_u16s(img: &oxideav_tiff::TiffImage, comps: usize) -> Vec<u16> {
    assert_eq!(img.planes.len(), 1);
    let p = &img.planes[0];
    let row_samples = img.width as usize * comps;
    let mut out = Vec::with_capacity(row_samples * img.height as usize);
    for y in 0..img.height as usize {
        for s in 0..row_samples {
            let off = y * p.stride + s * 2;
            out.push(u16::from_le_bytes([p.data[off], p.data[off + 1]]));
        }
    }
    out
}

fn max_abs_diff(a: &[u16], b: &[u16]) -> u32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| (x as i32 - y as i32).unsigned_abs())
        .max()
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// 12-bit SOF1 (extended sequential DCT) grayscale, single strip:
/// decode through the TIFF path and compare with `djpeg`'s reference
/// decode of the identical bitstream, per-sample tolerance ±1 raw
/// code value (independent IDCT implementations).
#[test]
fn deep_gray12_sof1_single_strip_vs_djpeg() {
    let (w, h, bits) = (32usize, 32usize, 12u16);
    let src = gray_pattern(w, h, bits);
    let Some(jpeg) = cjpeg(
        &to_pgm16(&src, w, h, bits),
        &["-precision", "12", "-grayscale"],
    ) else {
        eprintln!("skipping: cjpeg with -precision 12 unavailable");
        return;
    };
    let Some(reference_pnm) = djpeg(&jpeg, &[]) else {
        eprintln!("skipping: djpeg unavailable");
        return;
    };
    let (reference, rw, rh) = from_pnm16(&reference_pnm).expect("djpeg 12-bit PGM");
    assert_eq!((rw, rh), (w, h));

    let tiff = build_tiff(
        &DeepCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_BLACK_IS_ZERO,
            spp: 1,
            bps: bits,
            compression: COMPRESSION_JPEG_NEW,
            tiling: None,
            rows_per_strip: h as u32,
            subsampling: None,
        },
        &[jpeg],
    );
    let d = decode_tiff(&tiff).expect("deep gray decode");
    assert_eq!(d.frame.pixel_format, TiffPixelFormat::Gray16Le);
    let got = plane_u16s(&d.frame, 1);
    let want: Vec<u16> = reference.iter().map(|&v| widen(v, bits)).collect();
    let tol = widen(1, bits) as u32; // ±1 raw code value, widened
    assert!(
        max_abs_diff(&got, &want) <= tol,
        "12-bit SOF1 decode diverges from the djpeg reference by more than ±1 code value"
    );
}

/// 12-bit SOF3 (lossless) grayscale: byte-exact round trip against
/// the synthetic source raster, single-strip and multi-strip, plus
/// the §22 old-style interchange wrap of the same bitstream.
#[test]
fn deep_gray12_lossless_exact() {
    let (w, h, bits) = (32usize, 32usize, 12u16);
    let src = gray_pattern(w, h, bits);
    let pgm = to_pgm16(&src, w, h, bits);
    let Some(jpeg) = cjpeg(&pgm, &["-precision", "12", "-lossless", "1", "-grayscale"]) else {
        eprintln!("skipping: cjpeg with -precision 12 -lossless unavailable");
        return;
    };
    let want: Vec<u16> = src.iter().map(|&v| widen(v, bits)).collect();

    // Compression = 7 single strip.
    let cfg = DeepCfg {
        width: w as u32,
        height: h as u32,
        photometric: PHOTO_BLACK_IS_ZERO,
        spp: 1,
        bps: bits,
        compression: COMPRESSION_JPEG_NEW,
        tiling: None,
        rows_per_strip: h as u32,
        subsampling: None,
    };
    let d = decode_tiff(&build_tiff(&cfg, std::slice::from_ref(&jpeg))).expect("single strip");
    assert_eq!(d.frame.pixel_format, TiffPixelFormat::Gray16Le);
    assert_eq!(plane_u16s(&d.frame, 1), want, "lossless must be exact");

    // Compression = 7 multi-strip: two independently encoded halves.
    let top = to_pgm16(&src[..w * h / 2], w, h / 2, bits);
    let bottom = to_pgm16(&src[w * h / 2..], w, h / 2, bits);
    let (Some(j_top), Some(j_bottom)) = (
        cjpeg(&top, &["-precision", "12", "-lossless", "1", "-grayscale"]),
        cjpeg(
            &bottom,
            &["-precision", "12", "-lossless", "1", "-grayscale"],
        ),
    ) else {
        eprintln!("skipping multi-strip: cjpeg unavailable");
        return;
    };
    let cfg2 = DeepCfg {
        rows_per_strip: h as u32 / 2,
        ..cfg
    };
    let d2 = decode_tiff(&build_tiff(&cfg2, &[j_top, j_bottom])).expect("multi strip");
    assert_eq!(plane_u16s(&d2.frame, 1), want, "multi-strip lossless exact");

    // §22 old-style interchange wrap of the full-image bitstream.
    let cfg6 = DeepCfg {
        compression: COMPRESSION_JPEG_OLD,
        ..cfg
    };
    let d6 = decode_tiff(&build_tiff(&cfg6, &[jpeg])).expect("old-style deep");
    assert_eq!(d6.frame.pixel_format, TiffPixelFormat::Gray16Le);
    assert_eq!(plane_u16s(&d6.frame, 1), want, "§22 deep lossless exact");
}

/// 16-bit SOF3 (lossless) grayscale — the top of the §22 / SOF3
/// precision range. Byte-exact; the widening is the identity at 16
/// bits.
#[test]
fn deep_gray16_lossless_exact() {
    let (w, h, bits) = (32usize, 32usize, 16u16);
    let src = gray_pattern(w, h, bits);
    let Some(jpeg) = cjpeg(
        &to_pgm16(&src, w, h, bits),
        &["-precision", "16", "-lossless", "1", "-grayscale"],
    ) else {
        eprintln!("skipping: cjpeg with -precision 16 -lossless unavailable");
        return;
    };
    let d = decode_tiff(&build_tiff(
        &DeepCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_BLACK_IS_ZERO,
            spp: 1,
            bps: bits,
            compression: COMPRESSION_JPEG_NEW,
            tiling: None,
            rows_per_strip: h as u32,
            subsampling: None,
        },
        &[jpeg],
    ))
    .expect("16-bit lossless decode");
    assert_eq!(d.frame.pixel_format, TiffPixelFormat::Gray16Le);
    assert_eq!(plane_u16s(&d.frame, 1), src, "16-bit lossless exact");
}

/// 12-bit lossless grayscale under WhiteIsZero: the polarity
/// inversion applies after the (monotone) widening, so every output
/// sample is the bitwise complement of the BlackIsZero render.
#[test]
fn deep_gray12_white_is_zero_inverts() {
    let (w, h, bits) = (16usize, 16usize, 12u16);
    let src = gray_pattern(w, h, bits);
    let Some(jpeg) = cjpeg(
        &to_pgm16(&src, w, h, bits),
        &["-precision", "12", "-lossless", "1", "-grayscale"],
    ) else {
        eprintln!("skipping: cjpeg with -precision 12 unavailable");
        return;
    };
    let cfg = DeepCfg {
        width: w as u32,
        height: h as u32,
        photometric: PHOTO_WHITE_IS_ZERO,
        spp: 1,
        bps: bits,
        compression: COMPRESSION_JPEG_NEW,
        tiling: None,
        rows_per_strip: h as u32,
        subsampling: None,
    };
    let d = decode_tiff(&build_tiff(&cfg, &[jpeg])).expect("WhiteIsZero deep decode");
    let got = plane_u16s(&d.frame, 1);
    let want: Vec<u16> = src.iter().map(|&v| 0xFFFF - widen(v, bits)).collect();
    assert_eq!(got, want, "WhiteIsZero must complement the widened value");
}

/// 12-bit SOF1 YCbCr 4:2:0 and 4:4:4 (Compression = 7): decode to
/// `Rgb48Le` and compare with `djpeg`'s own 12-bit RGB reconstruction
/// of the identical bitstream. Two independent IDCTs *and* two
/// independent (identically-specified BT.601 / JFIF) color converters
/// are in play, so the tolerance is a few raw code values.
#[test]
fn deep_ycbcr12_sof1_vs_djpeg() {
    let (w, h, bits) = (32usize, 32usize, 12u16);
    let src = rgb_pattern(w, h, bits);
    let ppm = to_ppm16(&src, w, h, bits);
    for (sample_arg, sub) in [("2x2", (2u16, 2u16)), ("1x1", (1u16, 1u16))] {
        let Some(jpeg) = cjpeg(&ppm, &["-precision", "12", "-sample", sample_arg]) else {
            eprintln!("skipping: cjpeg with -precision 12 unavailable");
            return;
        };
        // `-nosmooth` selects djpeg's replication upsampler — the
        // same §21-style chroma replication this crate's compositor
        // applies — so the comparison isolates IDCT + color-convert
        // rounding instead of upsampling policy.
        let Some(reference_pnm) = djpeg(&jpeg, &["-nosmooth"]) else {
            eprintln!("skipping: djpeg unavailable");
            return;
        };
        let (reference, rw, rh) = from_pnm16(&reference_pnm).expect("djpeg 12-bit PPM");
        assert_eq!((rw, rh), (w, h));

        let tiff = build_tiff(
            &DeepCfg {
                width: w as u32,
                height: h as u32,
                photometric: PHOTO_YCBCR,
                spp: 3,
                bps: bits,
                compression: COMPRESSION_JPEG_NEW,
                tiling: None,
                rows_per_strip: h as u32,
                subsampling: Some(sub),
            },
            &[jpeg],
        );
        let d = decode_tiff(&tiff).expect("deep YCbCr decode");
        assert_eq!(d.frame.pixel_format, TiffPixelFormat::Rgb48Le);
        let got = plane_u16s(&d.frame, 3);
        let want: Vec<u16> = reference.iter().map(|&v| widen(v, bits)).collect();
        let tol = 4 * widen(1, bits) as u32; // ±4 raw code values at 12 bits
        let diff = max_abs_diff(&got, &want);
        assert!(
            diff <= tol,
            "12-bit YCbCr {sample_arg} decode diverges from djpeg by {diff} (> {tol})"
        );
    }
}

/// 12-bit lossless, tiled Compression = 7 layout: four 16x16 tiles,
/// each an independently encoded SOF3 bitstream, byte-exact
/// reassembly.
#[test]
fn deep_gray12_lossless_tiled_exact() {
    let (w, h, bits) = (32usize, 32usize, 12u16);
    let (tw, th) = (16usize, 16usize);
    let src = gray_pattern(w, h, bits);
    let mut tiles = Vec::new();
    for ty in 0..h / th {
        for tx in 0..w / tw {
            let mut tile = Vec::with_capacity(tw * th);
            for r in 0..th {
                let row = (ty * th + r) * w + tx * tw;
                tile.extend_from_slice(&src[row..row + tw]);
            }
            match cjpeg(
                &to_pgm16(&tile, tw, th, bits),
                &["-precision", "12", "-lossless", "1", "-grayscale"],
            ) {
                Some(j) => tiles.push(j),
                None => {
                    eprintln!("skipping: cjpeg with -precision 12 unavailable");
                    return;
                }
            }
        }
    }
    let d = decode_tiff(&build_tiff(
        &DeepCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_BLACK_IS_ZERO,
            spp: 1,
            bps: bits,
            compression: COMPRESSION_JPEG_NEW,
            tiling: Some((tw as u32, th as u32)),
            rows_per_strip: 0,
            subsampling: None,
        },
        &tiles,
    ))
    .expect("tiled deep decode");
    assert_eq!(d.frame.pixel_format, TiffPixelFormat::Gray16Le);
    let want: Vec<u16> = src.iter().map(|&v| widen(v, bits)).collect();
    assert_eq!(plane_u16s(&d.frame, 1), want, "tiled lossless exact");
}

/// Depth gates that must stay precise errors: sub-8-bit precisions
/// and deep CMYK.
#[test]
fn deep_gates_precise_errors() {
    let cfg = DeepCfg {
        width: 8,
        height: 8,
        photometric: PHOTO_BLACK_IS_ZERO,
        spp: 1,
        bps: 4,
        compression: COMPRESSION_JPEG_NEW,
        tiling: None,
        rows_per_strip: 8,
        subsampling: None,
    };
    // A well-formed-enough wrapper with junk segment bytes: the depth
    // gate fires before the JPEG bytes are touched.
    let tiff = build_tiff(&cfg, &[vec![0u8; 8]]);
    let Err(e) = decode_tiff(&tiff) else {
        panic!("sub-8-bit JPEG-in-TIFF must not decode");
    };
    let msg = format!("{e:?}");
    assert!(msg.contains("BitsPerSample"), "{msg}");

    let cfg = DeepCfg {
        photometric: PHOTO_CMYK,
        spp: 4,
        bps: 12,
        ..cfg
    };
    let tiff = build_tiff(&cfg, &[vec![0u8; 8]]);
    let Err(e) = decode_tiff(&tiff) else {
        panic!("deep CMYK JPEG-in-TIFF must not decode");
    };
    let msg = format!("{e:?}");
    assert!(msg.contains("CMYK"), "{msg}");
}
