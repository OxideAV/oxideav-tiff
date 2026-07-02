//! TIFF 6.0 §22 old-style JPEG (`Compression = 6`) decode tests.
//!
//! The §22 **interchange-format layout** — `JPEGInterchangeFormat`
//! (tag 513) pointing at a complete SOI..EOI ISO JPEG bitstream, per
//! §22 "Strips and Tiles" ("Compressed images conforming to the
//! syntax of the JPEG interchange format can be converted to TIFF
//! simply by defining a single strip or tile for the entire image and
//! then concatenating the TIFF image description fields to the JPEG
//! compressed image data") — must decode. The **tables-form layout**
//! (raw JPEGQTables / JPEGDCTables / JPEGACTables payloads +
//! entropy-coded strips) must be recognised and rejected with a
//! precise error.
//!
//! Fixture strategy: ImageMagick's `convert` binary produces the JPEG
//! bitstreams (black-box validator per workspace policy — binary in,
//! bytes out, source never consulted); the TIFF wrapping is
//! hand-built here so every §22 field combination is exercised
//! byte-for-byte. Tests needing `convert` skip (with a note) when the
//! binary is missing; the rejection-semantics tests are hand-built
//! only and always run.

#[cfg(feature = "registry")]
use std::io::Write;
#[cfg(feature = "registry")]
use std::process::{Command, Stdio};

use oxideav_tiff::types::*;
use oxideav_tiff::{decode_tiff, TiffError};
#[cfg(feature = "registry")]
use oxideav_tiff::{decode_tiff_all, DecodedTiff};

// ---------------------------------------------------------------------------
// ImageMagick JPEG fixture generation (black-box; only the
// registry-gated decode tests need real bitstreams).
// ---------------------------------------------------------------------------

#[cfg(feature = "registry")]
fn convert_available() -> bool {
    Command::new("convert")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(feature = "registry")]
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

/// Run `convert in.<src_ext> [args...] out.jpg` and return the JPEG
/// bytes.
#[cfg(feature = "registry")]
fn convert_to_jpeg(input_bytes: &[u8], src_ext: &str, args: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-oldjpeg-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join(format!("in.{src_ext}"));
    let out_path = dir.join("out.jpg");
    std::fs::File::create(&in_path)
        .ok()?
        .write_all(input_bytes)
        .ok()?;
    let mut cmd = Command::new("convert");
    cmd.arg(&in_path);
    for a in args {
        cmd.arg(a);
    }
    cmd.arg(&out_path);
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }
    let bytes = std::fs::read(&out_path).ok();
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

#[cfg(feature = "registry")]
fn make_ppm_rgb(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), (w * h * 3) as usize);
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(pixels);
    v
}

#[cfg(feature = "registry")]
fn make_pgm_gray(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), (w * h) as usize);
    let mut v = format!("P5\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(pixels);
    v
}

#[cfg(feature = "registry")]
fn rgb_pattern_64() -> Vec<u8> {
    let mut p = Vec::with_capacity(64 * 64 * 3);
    for y in 0u8..64 {
        for x in 0u8..64 {
            p.push(x.wrapping_mul(4));
            p.push(y.wrapping_mul(4));
            p.push((x ^ y).wrapping_mul(4));
        }
    }
    p
}

#[cfg(feature = "registry")]
fn gray_pattern_64() -> Vec<u8> {
    let mut p = Vec::with_capacity(64 * 64);
    for y in 0u8..64 {
        for x in 0u8..64 {
            p.push(x.wrapping_add(y).wrapping_mul(2));
        }
    }
    p
}

#[cfg(feature = "registry")]
fn frame_bytes(d: &DecodedTiff, bytes_per_pixel: usize) -> Vec<u8> {
    assert_eq!(d.frame.planes.len(), 1);
    let stride = d.frame.planes[0].stride;
    let row_bytes = d.width as usize * bytes_per_pixel;
    let mut out = Vec::with_capacity(row_bytes * d.height as usize);
    for y in 0..d.height as usize {
        out.extend_from_slice(&d.frame.planes[0].data[y * stride..y * stride + row_bytes]);
    }
    out
}

/// Row-packed plane bytes of a [`oxideav_tiff::TiffImage`] (the
/// `decode_tiff_all` page type).
#[cfg(feature = "registry")]
fn image_bytes(img: &oxideav_tiff::TiffImage, bytes_per_pixel: usize) -> Vec<u8> {
    assert_eq!(img.planes.len(), 1);
    let stride = img.planes[0].stride;
    let row_bytes = img.width as usize * bytes_per_pixel;
    let mut out = Vec::with_capacity(row_bytes * img.height as usize);
    for y in 0..img.height as usize {
        out.extend_from_slice(&img.planes[0].data[y * stride..y * stride + row_bytes]);
    }
    out
}

#[cfg(feature = "registry")]
fn mean_squared_error(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "MSE inputs must be the same length");
    let mut acc: u64 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = *x as i32 - *y as i32;
        acc += (d * d) as u64;
    }
    acc as f64 / a.len() as f64
}

// ---------------------------------------------------------------------------
// Hand-built classic-TIFF wrapper around a JPEG bitstream.
// ---------------------------------------------------------------------------

/// Configuration for the hand-built JPEG-carrying TIFF (both the §22
/// old-style layout and its Compression=7 TN2 twin, used as the
/// equivalence oracle).
struct Cfg {
    le: bool,
    width: u32,
    height: u32,
    photometric: u16,
    spp: u16,
    bps: u16,
    compression: u16,
    /// Write StripOffsets / StripByteCounts / RowsPerStrip covering
    /// the JPEG blob. Mandatory for Compression=7; §22
    /// interchange-format writers often omitted them (TN2: "not
    /// writing ... even the basic TIFF strip/tile data pointers").
    strips: bool,
    /// `JPEGProc` (512). §22 says mandatory; interchange-only files
    /// in the wild omit it (the SOF marker carries the process).
    proc: Option<u16>,
    /// Write `JPEGInterchangeFormat` (513) pointing at the blob.
    jif: bool,
    /// Write `JPEGInterchangeFormatLength` (514).
    jif_length: bool,
    /// Trailing pad bytes appended after the JPEG blob and *included*
    /// in the declared length (exercises the trim-to-EOI fallback).
    pad_after_jpeg: usize,
    /// `YCbCrSubSampling` (530).
    subsampling: Option<(u16, u16)>,
    /// `PlanarConfiguration` (284).
    planar: Option<u16>,
    /// Extra single-value SHORT tags (e.g. 517 JPEGLosslessPredictors
    /// at SamplesPerPixel = 1).
    extra_short: Vec<(u16, u16)>,
    /// Extra single-value LONG tags (e.g. 519/520/521 table offsets
    /// at SamplesPerPixel = 1).
    extra_long: Vec<(u16, u32)>,
    /// Override the tag-513 offset (for past-EOF / not-SOI tests).
    jif_offset_override: Option<u32>,
}

impl Cfg {
    fn gray(compression: u16) -> Self {
        Cfg {
            le: true,
            width: 64,
            height: 64,
            photometric: PHOTO_BLACK_IS_ZERO,
            spp: 1,
            bps: 8,
            compression,
            strips: true,
            proc: Some(JPEG_PROC_BASELINE),
            jif: compression == COMPRESSION_JPEG_OLD,
            jif_length: true,
            pad_after_jpeg: 0,
            subsampling: None,
            planar: None,
            extra_short: Vec::new(),
            extra_long: Vec::new(),
            jif_offset_override: None,
        }
    }
    fn ycbcr(compression: u16, sub: (u16, u16)) -> Self {
        Cfg {
            photometric: PHOTO_YCBCR,
            spp: 3,
            subsampling: Some(sub),
            ..Cfg::gray(compression)
        }
    }
    #[cfg(feature = "registry")]
    fn cmyk(compression: u16) -> Self {
        Cfg {
            photometric: PHOTO_CMYK,
            spp: 4,
            ..Cfg::gray(compression)
        }
    }
}

/// Assemble a classic (magic 42) TIFF around `jpeg` per `cfg`.
/// Entries are written sorted ascending by tag; inline values are
/// left-justified in the 4-byte value/offset slot per TIFF 6.0 §2.
fn build_jpeg_tiff(cfg: &Cfg, jpeg: &[u8]) -> Vec<u8> {
    let le = cfg.le;
    let p16 = |v: u16| -> [u8; 2] {
        if le {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    };
    let p32 = |v: u32| -> [u8; 4] {
        if le {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        }
    };
    let short_val = |v: u16| -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&p16(v));
        b
    };
    let two_shorts_val = |a: u16, b_: u16| -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&p16(a));
        b[2..].copy_from_slice(&p16(b_));
        b
    };

    // (tag, type, count, 4-byte value/offset slot)
    let mut entries: Vec<(u16, u16, u32, [u8; 4])> = vec![
        (TAG_IMAGE_WIDTH, TYPE_LONG, 1, p32(cfg.width)),
        (TAG_IMAGE_LENGTH, TYPE_LONG, 1, p32(cfg.height)),
        (TAG_COMPRESSION, TYPE_SHORT, 1, short_val(cfg.compression)),
        (
            TAG_PHOTOMETRIC_INTERPRETATION,
            TYPE_SHORT,
            1,
            short_val(cfg.photometric),
        ),
        (TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, short_val(cfg.spp)),
    ];
    if let Some(pl) = cfg.planar {
        entries.push((TAG_PLANAR_CONFIGURATION, TYPE_SHORT, 1, short_val(pl)));
    }
    if let Some(p) = cfg.proc {
        entries.push((TAG_JPEG_PROC, TYPE_SHORT, 1, short_val(p)));
    }
    if let Some((sh, sv)) = cfg.subsampling {
        entries.push((TAG_YCBCR_SUBSAMPLING, TYPE_SHORT, 2, two_shorts_val(sh, sv)));
    }
    for &(tag, v) in &cfg.extra_short {
        entries.push((tag, TYPE_SHORT, 1, short_val(v)));
    }
    for &(tag, v) in &cfg.extra_long {
        entries.push((tag, TYPE_LONG, 1, p32(v)));
    }
    // Placeholder-offset entries appended below need the final entry
    // count first.
    let needs_bps_extra = cfg.spp > 1; // 2 * spp > 4 bytes → out-of-line
    let n = entries.len()
        + 1 // BitsPerSample
        + usize::from(cfg.strips) * 3
        + usize::from(cfg.jif)
        + usize::from(cfg.jif && cfg.jif_length);
    let ifd_bytes = 2 + n * 12 + 4;
    let data_start = 8 + ifd_bytes;
    let bps_extra_len = if needs_bps_extra {
        cfg.spp as usize * 2
    } else {
        0
    };
    let jpeg_off = (data_start + bps_extra_len) as u32;

    if needs_bps_extra {
        entries.push((
            TAG_BITS_PER_SAMPLE,
            TYPE_SHORT,
            cfg.spp as u32,
            p32(data_start as u32),
        ));
    } else {
        entries.push((TAG_BITS_PER_SAMPLE, TYPE_SHORT, 1, short_val(cfg.bps)));
    }
    if cfg.strips {
        entries.push((TAG_STRIP_OFFSETS, TYPE_LONG, 1, p32(jpeg_off)));
        entries.push((TAG_ROWS_PER_STRIP, TYPE_LONG, 1, p32(cfg.height)));
        entries.push((
            TAG_STRIP_BYTE_COUNTS,
            TYPE_LONG,
            1,
            p32((jpeg.len() + cfg.pad_after_jpeg) as u32),
        ));
    }
    if cfg.jif {
        let off = cfg.jif_offset_override.unwrap_or(jpeg_off);
        entries.push((TAG_JPEG_INTERCHANGE_FORMAT, TYPE_LONG, 1, p32(off)));
        if cfg.jif_length {
            entries.push((
                TAG_JPEG_INTERCHANGE_FORMAT_LENGTH,
                TYPE_LONG,
                1,
                p32((jpeg.len() + cfg.pad_after_jpeg) as u32),
            ));
        }
    }
    entries.sort_by_key(|e| e.0);
    assert_eq!(entries.len(), n, "entry count precomputation out of sync");

    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(if le { b"II" } else { b"MM" });
    v.extend_from_slice(&p16(42));
    v.extend_from_slice(&p32(8));
    v.extend_from_slice(&p16(n as u16));
    for (tag, ftype, count, slot) in &entries {
        v.extend_from_slice(&p16(*tag));
        v.extend_from_slice(&p16(*ftype));
        v.extend_from_slice(&p32(*count));
        v.extend_from_slice(slot);
    }
    v.extend_from_slice(&p32(0)); // next IFD
    if needs_bps_extra {
        for _ in 0..cfg.spp {
            v.extend_from_slice(&p16(cfg.bps));
        }
    }
    assert_eq!(v.len() as u32, jpeg_off);
    v.extend_from_slice(jpeg);
    v.extend(vec![0u8; cfg.pad_after_jpeg]);
    v
}

/// Two-page chain: both pages §22 old-style JPEG grayscale (each page
/// its own interchange bitstream), exercising `decode_tiff_all` over
/// the next-IFD chain.
#[cfg(feature = "registry")]
fn build_two_page_oldstyle(jpeg1: &[u8], jpeg2: &[u8], w: u32, h: u32) -> Vec<u8> {
    // Per-IFD entries: 256, 257, 258, 259, 262, 277, 512, 513, 514 → 9.
    let n = 9usize;
    let ifd_bytes = (2 + n * 12 + 4) as u32;
    let ifd1 = 8u32;
    let ifd2 = ifd1 + ifd_bytes;
    let jpeg1_off = ifd2 + ifd_bytes;
    let jpeg2_off = jpeg1_off + jpeg1.len() as u32;

    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    let push_ifd = |v: &mut Vec<u8>, jpeg_off: u32, jpeg_len: u32, next: u32| {
        let short_val = |x: u16| -> [u8; 4] {
            let mut b = [0u8; 4];
            b[..2].copy_from_slice(&x.to_le_bytes());
            b
        };
        let entries: Vec<(u16, u16, u32, [u8; 4])> = vec![
            (TAG_IMAGE_WIDTH, TYPE_LONG, 1, w.to_le_bytes()),
            (TAG_IMAGE_LENGTH, TYPE_LONG, 1, h.to_le_bytes()),
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
            (TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, short_val(1)),
            (TAG_JPEG_PROC, TYPE_SHORT, 1, short_val(JPEG_PROC_BASELINE)),
            (
                TAG_JPEG_INTERCHANGE_FORMAT,
                TYPE_LONG,
                1,
                jpeg_off.to_le_bytes(),
            ),
            (
                TAG_JPEG_INTERCHANGE_FORMAT_LENGTH,
                TYPE_LONG,
                1,
                jpeg_len.to_le_bytes(),
            ),
        ];
        v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, ftype, count, slot) in &entries {
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&ftype.to_le_bytes());
            v.extend_from_slice(&count.to_le_bytes());
            v.extend_from_slice(slot);
        }
        v.extend_from_slice(&next.to_le_bytes());
    };
    push_ifd(&mut v, jpeg1_off, jpeg1.len() as u32, ifd2);
    push_ifd(&mut v, jpeg2_off, jpeg2.len() as u32, 0);
    assert_eq!(v.len() as u32, jpeg1_off);
    v.extend_from_slice(jpeg1);
    v.extend_from_slice(jpeg2);
    v
}

// ---------------------------------------------------------------------------
// Interchange-format decode (ImageMagick-generated JPEG bitstreams).
// ---------------------------------------------------------------------------

/// Grayscale interchange-format §22 file decodes, and produces
/// byte-identical output to the TN2 Compression=7 wrapping of the
/// *same* bitstream (the plumbing-equivalence oracle).
#[cfg(feature = "registry")]
#[test]
fn oldstyle_gray_interchange_decodes() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = gray_pattern_64();
    let Some(jpeg) = convert_to_jpeg(&make_pgm_gray(64, 64, &pixels), "pgm", &["-quality", "95"])
    else {
        eprintln!("skipping: convert failed to produce a grayscale JPEG");
        return;
    };
    let old = build_jpeg_tiff(&Cfg::gray(COMPRESSION_JPEG_OLD), &jpeg);
    let d = decode_tiff(&old).expect("old-style gray decode failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_bytes(&d, 1);
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 200.0,
        "old-style gray reconstruction too far: MSE={mse}"
    );

    // Equivalence: identical bitstream through the TN2 path.
    let new = build_jpeg_tiff(&Cfg::gray(COMPRESSION_JPEG_NEW), &jpeg);
    let d7 = decode_tiff(&new).expect("Compression=7 twin decode failed");
    assert_eq!(
        frame_bytes(&d7, 1),
        got,
        "old-style and TN2 paths disagree on the same bitstream"
    );
}

/// Big-endian (`MM`) §22 wrapping decodes identically to the
/// little-endian one — the JPEG payload is byte-order-independent
/// (multibyte JPEG values are MSB-first regardless of the TIFF byte
/// order) while every IFD field is byte-swapped.
#[cfg(feature = "registry")]
#[test]
fn oldstyle_gray_big_endian_matches_little() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = gray_pattern_64();
    let Some(jpeg) = convert_to_jpeg(&make_pgm_gray(64, 64, &pixels), "pgm", &["-quality", "95"])
    else {
        eprintln!("skipping: convert failed to produce a grayscale JPEG");
        return;
    };
    let le = build_jpeg_tiff(&Cfg::gray(COMPRESSION_JPEG_OLD), &jpeg);
    let be = build_jpeg_tiff(
        &Cfg {
            le: false,
            ..Cfg::gray(COMPRESSION_JPEG_OLD)
        },
        &jpeg,
    );
    let dle = decode_tiff(&le).expect("LE decode failed");
    let dbe = decode_tiff(&be).expect("BE decode failed");
    assert_eq!(frame_bytes(&dle, 1), frame_bytes(&dbe, 1));
}

/// YCbCr 4:2:0 interchange-format file (ImageMagick's default chroma
/// subsampling) decodes through the Yuv420P composite path.
#[cfg(feature = "registry")]
#[test]
fn oldstyle_ycbcr_420_interchange_decodes() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let Some(jpeg) = convert_to_jpeg(
        &make_ppm_rgb(64, 64, &pixels),
        "ppm",
        &["-quality", "92", "-sampling-factor", "2x2"],
    ) else {
        eprintln!("skipping: convert failed to produce a YCbCr JPEG");
        return;
    };
    let old = build_jpeg_tiff(&Cfg::ycbcr(COMPRESSION_JPEG_OLD, (2, 2)), &jpeg);
    let d = decode_tiff(&old).expect("old-style YCbCr 4:2:0 decode failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_bytes(&d, 3);
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 1500.0,
        "old-style 4:2:0 reconstruction too far: MSE={mse}"
    );

    let new = build_jpeg_tiff(&Cfg::ycbcr(COMPRESSION_JPEG_NEW, (2, 2)), &jpeg);
    let d7 = decode_tiff(&new).expect("Compression=7 twin decode failed");
    assert_eq!(frame_bytes(&d7, 3), got);
}

/// YCbCr 4:4:4 (no chroma subsampling) — the Yuv444P composite path.
#[cfg(feature = "registry")]
#[test]
fn oldstyle_ycbcr_444_interchange_decodes() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let Some(jpeg) = convert_to_jpeg(
        &make_ppm_rgb(64, 64, &pixels),
        "ppm",
        &["-quality", "95", "-sampling-factor", "1x1"],
    ) else {
        eprintln!("skipping: convert failed to produce a 4:4:4 JPEG");
        return;
    };
    let old = build_jpeg_tiff(&Cfg::ycbcr(COMPRESSION_JPEG_OLD, (1, 1)), &jpeg);
    let d = decode_tiff(&old).expect("old-style YCbCr 4:4:4 decode failed");
    let got = frame_bytes(&d, 3);
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 300.0,
        "old-style 4:4:4 reconstruction too far: MSE={mse}"
    );
}

/// CMYK 4-component interchange stream (Adobe APP14-marked JPEG) —
/// composited to Rgb24 through the same additive-RGB conversion as
/// the uncompressed CMYK path.
#[cfg(feature = "registry")]
#[test]
fn oldstyle_cmyk_interchange_decodes() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let Some(jpeg) = convert_to_jpeg(
        &make_ppm_rgb(64, 64, &pixels),
        "ppm",
        &["-colorspace", "CMYK", "-quality", "90"],
    ) else {
        eprintln!("skipping: convert failed to produce a CMYK JPEG");
        return;
    };
    let old = build_jpeg_tiff(&Cfg::cmyk(COMPRESSION_JPEG_OLD), &jpeg);
    let d = decode_tiff(&old).expect("old-style CMYK decode failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_bytes(&d, 3);
    // ImageMagick writes a *standalone* CMYK JPEG as YCCK (Adobe
    // APP14 transform = 2), so absolute color fidelity against the
    // RGB source depends on the JPEG codec's YCCK handling and the
    // additive CMYK→RGB display conversion — not on the §22 plumbing
    // under test. Assert structural sanity plus byte-exact
    // equivalence with the TN2 (Compression=7) wrapping of the same
    // bitstream, which shares every step after the §22 field walk.
    let nonzero = got.iter().filter(|&&b| b != 0).count();
    assert!(nonzero > got.len() / 10, "suspiciously many zeros");
    let unsat = got.iter().filter(|&&b| b != 255).count();
    assert!(unsat > got.len() / 10, "suspiciously many 255s");

    let new = build_jpeg_tiff(&Cfg::cmyk(COMPRESSION_JPEG_NEW), &jpeg);
    let d7 = decode_tiff(&new).expect("Compression=7 twin decode failed");
    assert_eq!(frame_bytes(&d7, 3), got);
}

/// The TN2-documented "interchange datastream dumped into the file"
/// shape: no strip pointers at all. §22's interchange layout does not
/// need them — tag 513/514 fully locate the bitstream.
#[cfg(feature = "registry")]
#[test]
fn oldstyle_without_strip_pointers_decodes() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = gray_pattern_64();
    let Some(jpeg) = convert_to_jpeg(&make_pgm_gray(64, 64, &pixels), "pgm", &["-quality", "95"])
    else {
        eprintln!("skipping: convert failed to produce a grayscale JPEG");
        return;
    };
    let with_strips = build_jpeg_tiff(&Cfg::gray(COMPRESSION_JPEG_OLD), &jpeg);
    let without_strips = build_jpeg_tiff(
        &Cfg {
            strips: false,
            ..Cfg::gray(COMPRESSION_JPEG_OLD)
        },
        &jpeg,
    );
    let a = decode_tiff(&with_strips).expect("with-strips decode failed");
    let b = decode_tiff(&without_strips).expect("stripless decode failed");
    assert_eq!(frame_bytes(&a, 1), frame_bytes(&b, 1));
}

/// `JPEGInterchangeFormatLength` (tag 514) absent: §22 marks it
/// "useful", not mandatory. The stream is taken to run to the last
/// EOI before end-of-file.
#[cfg(feature = "registry")]
#[test]
fn oldstyle_without_length_tag_decodes() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = gray_pattern_64();
    let Some(jpeg) = convert_to_jpeg(&make_pgm_gray(64, 64, &pixels), "pgm", &["-quality", "95"])
    else {
        eprintln!("skipping: convert failed to produce a grayscale JPEG");
        return;
    };
    let base = build_jpeg_tiff(&Cfg::gray(COMPRESSION_JPEG_OLD), &jpeg);
    let no_len = build_jpeg_tiff(
        &Cfg {
            jif_length: false,
            ..Cfg::gray(COMPRESSION_JPEG_OLD)
        },
        &jpeg,
    );
    let a = decode_tiff(&base).expect("with-length decode failed");
    let b = decode_tiff(&no_len).expect("length-less decode failed");
    assert_eq!(frame_bytes(&a, 1), frame_bytes(&b, 1));
}

/// A declared length that includes trailing padding after the EOI:
/// the decoder trims to the EOI marker instead of feeding pad bytes
/// to the JPEG codec.
#[cfg(feature = "registry")]
#[test]
fn oldstyle_padded_length_trims_to_eoi() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = gray_pattern_64();
    let Some(jpeg) = convert_to_jpeg(&make_pgm_gray(64, 64, &pixels), "pgm", &["-quality", "95"])
    else {
        eprintln!("skipping: convert failed to produce a grayscale JPEG");
        return;
    };
    let padded = build_jpeg_tiff(
        &Cfg {
            pad_after_jpeg: 16,
            ..Cfg::gray(COMPRESSION_JPEG_OLD)
        },
        &jpeg,
    );
    let d = decode_tiff(&padded).expect("padded-length decode failed");
    let mse = mean_squared_error(&frame_bytes(&d, 1), &pixels);
    assert!(
        mse < 200.0,
        "padded-length reconstruction too far: MSE={mse}"
    );
}

/// `JPEGProc` (512) absent but a complete interchange stream present:
/// tolerated — the bitstream's SOF marker declares the process (TN2
/// records writers that omitted every §22 auxiliary field).
#[cfg(feature = "registry")]
#[test]
fn oldstyle_without_proc_tag_decodes() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = gray_pattern_64();
    let Some(jpeg) = convert_to_jpeg(&make_pgm_gray(64, 64, &pixels), "pgm", &["-quality", "95"])
    else {
        eprintln!("skipping: convert failed to produce a grayscale JPEG");
        return;
    };
    let no_proc = build_jpeg_tiff(
        &Cfg {
            proc: None,
            ..Cfg::gray(COMPRESSION_JPEG_OLD)
        },
        &jpeg,
    );
    let d = decode_tiff(&no_proc).expect("proc-less interchange decode failed");
    let mse = mean_squared_error(&frame_bytes(&d, 1), &pixels);
    assert!(mse < 200.0, "proc-less reconstruction too far: MSE={mse}");
}

/// Two-page §22 chain via `decode_tiff_all`: each page carries its
/// own interchange bitstream, and each decodes to its own pixels.
#[cfg(feature = "registry")]
#[test]
fn oldstyle_two_page_chain_decodes() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let px1 = gray_pattern_64();
    let px2: Vec<u8> = px1.iter().map(|&v| 255 - v).collect();
    let Some(j1) = convert_to_jpeg(&make_pgm_gray(64, 64, &px1), "pgm", &["-quality", "95"]) else {
        eprintln!("skipping: convert failed (page 1)");
        return;
    };
    let Some(j2) = convert_to_jpeg(&make_pgm_gray(64, 64, &px2), "pgm", &["-quality", "95"]) else {
        eprintln!("skipping: convert failed (page 2)");
        return;
    };
    let tiff = build_two_page_oldstyle(&j1, &j2, 64, 64);
    let pages = decode_tiff_all(&tiff).expect("two-page old-style decode failed");
    assert_eq!(pages.len(), 2);
    let mse1 = mean_squared_error(&image_bytes(&pages[0], 1), &px1);
    let mse2 = mean_squared_error(&image_bytes(&pages[1], 1), &px2);
    assert!(mse1 < 200.0, "page 1 too far: MSE={mse1}");
    assert!(mse2 < 200.0, "page 2 too far: MSE={mse2}");
}

// ---------------------------------------------------------------------------
// Recognition / rejection semantics (hand-built, no ImageMagick, run
// in both registry and standalone builds — every rejection below
// fires before the JPEG codec is reached).
// ---------------------------------------------------------------------------

/// A minimal fake bitstream for tests that must not reach the JPEG
/// codec: valid SOI..EOI framing, garbage in between.
fn fake_jif() -> Vec<u8> {
    vec![0xFF, 0xD8, 0x00, 0x11, 0x22, 0xFF, 0xD9]
}

fn expect_err(tiff: &[u8], needle: &str) -> TiffError {
    match decode_tiff(tiff) {
        Ok(_) => panic!("decode unexpectedly succeeded (wanted error containing {needle:?})"),
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                msg.contains(needle),
                "error {msg:?} does not mention {needle:?}"
            );
            e
        }
    }
}

/// §22 tables-form baseline layout (all three table fields present,
/// entropy-coded strips, no interchange stream) → precise Unsupported.
#[test]
fn reject_tables_form_baseline() {
    let cfg = Cfg {
        jif: false,
        extra_long: vec![
            (TAG_JPEG_Q_TABLES, 8),
            (TAG_JPEG_DC_TABLES, 8),
            (TAG_JPEG_AC_TABLES, 8),
        ],
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    let e = expect_err(&tiff, "tables-form");
    assert!(matches!(e, TiffError::Unsupported(_)), "{e:?}");
}

/// Baseline tables-form with a *missing* mandatory table field →
/// invalid-data naming the gap (§22: JPEGACTables "is mandatory
/// whenever the JPEGProc Field specifies a DCT-based process").
#[test]
fn reject_tables_form_missing_ac_tables() {
    let cfg = Cfg {
        jif: false,
        extra_long: vec![(TAG_JPEG_Q_TABLES, 8), (TAG_JPEG_DC_TABLES, 8)],
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    expect_err(&tiff, "JPEGACTables");
}

/// Lossless (JPEGProc=14) tables-form → precise Unsupported.
#[test]
fn reject_tables_form_lossless() {
    let cfg = Cfg {
        jif: false,
        proc: Some(JPEG_PROC_LOSSLESS),
        extra_short: vec![(TAG_JPEG_LOSSLESS_PREDICTORS, 1)],
        extra_long: vec![(TAG_JPEG_DC_TABLES, 8)],
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    let e = expect_err(&tiff, "lossless");
    assert!(matches!(e, TiffError::Unsupported(_)), "{e:?}");
}

/// Lossless without its mandatory JPEGLosslessPredictors → invalid.
#[test]
fn reject_lossless_missing_predictors() {
    let cfg = Cfg {
        jif: false,
        proc: Some(JPEG_PROC_LOSSLESS),
        extra_long: vec![(TAG_JPEG_DC_TABLES, 8)],
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    expect_err(&tiff, "JPEGLosslessPredictors");
}

/// JPEGProc values other than 1 / 14 are undefined by §22 ("will be
/// defined in the future") → Unsupported, even when an interchange
/// stream is present.
#[test]
fn reject_unknown_proc() {
    let cfg = Cfg {
        proc: Some(2),
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    let e = expect_err(&tiff, "JPEGProc=2");
    assert!(matches!(e, TiffError::Unsupported(_)), "{e:?}");
}

/// Tag 513 pointing past EOF → invalid-data, no panic.
#[test]
fn reject_jif_offset_past_eof() {
    let cfg = Cfg {
        jif_offset_override: Some(1 << 24),
        jif_length: false,
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    expect_err(&tiff, "past EOF");
}

/// Tag 513 pointing at bytes that are not an SOI marker → invalid.
#[test]
fn reject_jif_not_soi() {
    let cfg = Cfg {
        jif_offset_override: Some(8), // points into the IFD itself
        jif_length: false,
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    expect_err(&tiff, "SOI");
}

/// PlanarConfiguration=2 ("not interleaved") old-style JPEG →
/// Unsupported, as for Compression=7.
#[test]
fn reject_planar_separate() {
    let cfg = Cfg {
        planar: Some(PLANAR_SEPARATE),
        ..Cfg::ycbcr(COMPRESSION_JPEG_OLD, (1, 1))
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    let e = expect_err(&tiff, "PlanarConfiguration");
    assert!(matches!(e, TiffError::Unsupported(_)), "{e:?}");
}

/// Non-8-bit precision → Unsupported (§22 baseline "accepts as input
/// only those images having 8 bits per component"; this build's JPEG
/// codec renders 8-bit planes only). 16-bit is used because it passes
/// the decoder's generic per-width gate and reaches the §22 branch.
#[test]
fn reject_non_8bit_precision() {
    let cfg = Cfg {
        bps: 16,
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    let e = expect_err(&tiff, "BitsPerSample=16");
    assert!(matches!(e, TiffError::Unsupported(_)), "{e:?}");
}

/// Palette photometric is not a §22 continuous-tone color space →
/// invalid.
#[test]
fn reject_palette_photometric() {
    let cfg = Cfg {
        photometric: PHOTO_PALETTE,
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    expect_err(&tiff, "continuous-tone");
}

/// JPEGLosslessPredictors selection-value outside 1..=7 → invalid,
/// even before the tables-form / interchange split.
#[test]
fn reject_bad_lossless_predictor_value() {
    let cfg = Cfg {
        proc: Some(JPEG_PROC_LOSSLESS),
        jif: false,
        extra_short: vec![(TAG_JPEG_LOSSLESS_PREDICTORS, 9)],
        extra_long: vec![(TAG_JPEG_DC_TABLES, 8)],
        ..Cfg::gray(COMPRESSION_JPEG_OLD)
    };
    let tiff = build_jpeg_tiff(&cfg, &fake_jif());
    expect_err(&tiff, "selection-value");
}
