//! `TiffCompression::Jpeg` (Compression = 7, TIFF Technical Note 2)
//! encode tests.
//!
//! Every emitted file is decoded by the crate's own decoder and, when
//! the binaries are installed, by black-box validators: ImageMagick
//! (`magick`), libtiff's `tiffcp` (rewritten to `Compression = 1` and
//! then decoded by our reader — i.e. libtiff's JPEG decode of our
//! segments), `tiffinfo` (structural listing) and `djpeg` (on the extracted per-segment datastreams,
//! merged with `JPEGTables` the way TN2 prescribes — the only path that
//! covers the 12-/16-bit and lossless layouts the TIFF-level tools may
//! not handle). DCT layouts report PSNR against the source; lossless
//! layouts must be sample-exact.
//!
//! `ffmpeg` is deliberately not used: its TIFF reader does not decode
//! TN2 baseline JPEG-in-TIFF (checked against an ImageMagick-written
//! reference file — the raw output is unrelated to the image), so it
//! cannot serve as an oracle for `Compression = 7`.

#![cfg(feature = "registry")]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
use oxideav_tiff::jpeg::merge_jpeg_segment;
use oxideav_tiff::types::*;
use oxideav_tiff::{
    decode_tiff, encode_tiff, rgb24_to_ycbcr24, EncodePage, EncodePixelFormat, JpegOptions,
    JpegProcess, JpegTablesLayout, PageExtras, TiffCompression, TiffPixelFormat,
};

// ---------------------------------------------------------------------------
// Black-box plumbing (availability-gated).
// ---------------------------------------------------------------------------

/// True when `name` can be spawned at all (exit status is irrelevant —
/// `tiffcp` / `tiffinfo` exit non-zero on their usage banner).
fn binary_available(name: &str) -> bool {
    Command::new(name)
        .arg("-h")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-jpegenc-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Parse a binary PGM/PPM into (channels, w, h, maxval, samples).
fn parse_pnm(raw: &[u8]) -> (usize, usize, usize, u32, Vec<u16>) {
    let mut fields: Vec<String> = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while raw[i].is_ascii_whitespace() {
            i += 1;
        }
        if raw[i] == b'#' {
            while raw[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while !raw[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&raw[start..i]).into_owned());
    }
    i += 1;
    let channels = match fields[0].as_str() {
        "P5" => 1,
        "P6" => 3,
        other => panic!("unexpected PNM magic {other}"),
    };
    let w: usize = fields[1].parse().unwrap();
    let h: usize = fields[2].parse().unwrap();
    let maxval: u32 = fields[3].parse().unwrap();
    let body = &raw[i..];
    let samples: Vec<u16> = if maxval > 255 {
        body.chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect()
    } else {
        body.iter().map(|&b| b as u16).collect()
    };
    assert_eq!(samples.len(), channels * w * h, "PNM body size");
    (channels, w, h, maxval, samples)
}

/// `magick in.tif[0] -depth D out.(pgm|ppm)` → samples, or None when
/// unavailable / refused.
fn magick_decode_depth(tiff: &[u8], rgb: bool, depth: u32) -> Option<Vec<u16>> {
    if !binary_available("magick") {
        eprintln!("note: `magick` unavailable — skipping ImageMagick check");
        return None;
    }
    let dir = tmp_dir();
    let inp = dir.join("in.tif");
    let out = dir.join(if rgb { "out.ppm" } else { "out.pgm" });
    fs::File::create(&inp).ok()?.write_all(tiff).ok()?;
    let mut first_page = inp.clone().into_os_string();
    first_page.push("[0]");
    let status = Command::new("magick")
        .arg(&first_page)
        .arg("-depth")
        .arg(depth.to_string())
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    let res = if status.success() {
        fs::read(&out).ok().map(|raw| {
            let (c, _, _, maxval, s) = parse_pnm(&raw);
            assert_eq!(c, if rgb { 3 } else { 1 });
            assert_eq!(maxval, if depth == 8 { 255 } else { 65535 });
            s
        })
    } else {
        None
    };
    let _ = fs::remove_dir_all(&dir);
    res
}

fn magick_decode(tiff: &[u8], rgb: bool) -> Option<Vec<u16>> {
    magick_decode_depth(tiff, rgb, 8)
}

/// `tiffcp -c none in.tif out.tif` → the rewritten file bytes.
fn tiffcp_none(tiff: &[u8]) -> Option<Vec<u8>> {
    if !binary_available("tiffcp") {
        eprintln!("note: `tiffcp` unavailable — skipping libtiff check");
        return None;
    }
    let dir = tmp_dir();
    let inp = dir.join("in.tif");
    let out = dir.join("out.tif");
    fs::File::create(&inp).ok()?.write_all(tiff).ok()?;
    let status = Command::new("tiffcp")
        .arg("-c")
        .arg("none")
        .arg(&inp)
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    let res = if status.success() {
        fs::read(&out).ok()
    } else {
        None
    };
    let _ = fs::remove_dir_all(&dir);
    res
}

fn tiffinfo(tiff: &[u8]) -> Option<String> {
    if !binary_available("tiffinfo") {
        return None;
    }
    let dir = tmp_dir();
    let inp = dir.join("in.tif");
    fs::File::create(&inp).ok()?.write_all(tiff).ok()?;
    let out = Command::new("tiffinfo").arg(&inp).output().ok()?;
    let _ = fs::remove_dir_all(&dir);
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// `djpeg [args] in.jpg` → PNM samples.
fn djpeg(jpeg: &[u8], args: &[&str]) -> Option<(usize, usize, usize, Vec<u16>)> {
    let dir = tmp_dir();
    let inp = dir.join("in.jpg");
    let out = dir.join("out.pnm");
    fs::File::create(&inp).ok()?.write_all(jpeg).ok()?;
    let mut cmd = Command::new("djpeg");
    for a in args {
        cmd.arg(a);
    }
    cmd.arg("-outfile").arg(&out).arg(&inp);
    let status = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
    let ok = matches!(status, Ok(s) if s.success());
    let res = if ok {
        fs::read(&out).ok().map(|raw| {
            let (c, w, h, _, s) = parse_pnm(&raw);
            (c, w, h, s)
        })
    } else {
        None
    };
    let _ = fs::remove_dir_all(&dir);
    res
}

/// Pull every segment (strip or tile, in storage order) out of the
/// first IFD, merged with `JPEGTables` when present — each result is
/// a freestanding ISO JPEG datastream.
fn extract_segments(tiff: &[u8]) -> Vec<Vec<u8>> {
    let hdr = parse_header(tiff).unwrap();
    let (entries, _) = parse_ifd(tiff, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let bo = hdr.byte_order;
    let tables = find(&entries, TAG_JPEG_TABLES).map(|e| e.data.clone());
    let (offs, counts) = if let Some(o) = find(&entries, TAG_TILE_OFFSETS) {
        (
            o.as_u64_vec(bo).unwrap(),
            find(&entries, TAG_TILE_BYTE_COUNTS)
                .unwrap()
                .as_u64_vec(bo)
                .unwrap(),
        )
    } else {
        (
            find(&entries, TAG_STRIP_OFFSETS)
                .unwrap()
                .as_u64_vec(bo)
                .unwrap(),
            find(&entries, TAG_STRIP_BYTE_COUNTS)
                .unwrap()
                .as_u64_vec(bo)
                .unwrap(),
        )
    };
    offs.iter()
        .zip(counts.iter())
        .map(|(&o, &c)| {
            let seg = &tiff[o as usize..(o + c) as usize];
            assert_eq!(&seg[..2], &[0xFF, 0xD8], "segment starts with SOI");
            assert_eq!(
                &seg[seg.len() - 2..],
                &[0xFF, 0xD9],
                "segment ends with EOI"
            );
            merge_jpeg_segment(tables.as_deref(), seg).unwrap()
        })
        .collect()
}

fn ifd_entries(tiff: &[u8]) -> Vec<oxideav_tiff::ifd::Entry> {
    let hdr = parse_header(tiff).unwrap();
    parse_ifd(tiff, hdr.byte_order, hdr.variant, hdr.first_ifd_offset)
        .unwrap()
        .0
}

// ---------------------------------------------------------------------------
// Synthetic content + metrics.
// ---------------------------------------------------------------------------

fn psnr_u8(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "PSNR operands differ in length");
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

fn psnr_u16(a: &[u16], b: &[u16], peak: f64) -> f64 {
    assert_eq!(a.len(), b.len(), "PSNR operands differ in length");
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (peak * peak / mse).log10()
    }
}

/// Smooth 8-bit content: a sum of low-frequency gradients.
fn smooth_gray(w: usize, h: usize, seed: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f64 / w.max(2) as f64;
            let fy = y as f64 / h.max(2) as f64;
            let s = 0.5
                + 0.25 * ((fx * 3.0 + seed as f64) * std::f64::consts::PI).sin()
                + 0.25 * ((fy * 2.0 + seed as f64 * 0.5) * std::f64::consts::PI).cos();
            v.push((s.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    v
}

/// Interleaved `(Y, Cb, Cr)` with a textured luma and gently ramping
/// chroma — the kind of content chroma decimation barely touches, so
/// nearest-neighbour and interpolating chroma reconstructions agree
/// closely and the PSNR floors measure the codec, not the upsampler.
fn smooth_ycc(w: usize, h: usize) -> Vec<u8> {
    let y = smooth_gray(w, h, 4);
    let mut v = Vec::with_capacity(w * h * 3);
    for r in 0..h {
        for c in 0..w {
            v.push(y[r * w + c]);
            v.push((96 + (64 * c) / w.max(1)) as u8);
            v.push((100 + (56 * r) / h.max(1)) as u8);
        }
    }
    v
}

fn smooth_rgb(w: usize, h: usize) -> Vec<u8> {
    let r = smooth_gray(w, h, 1);
    let g = smooth_gray(w, h, 2);
    let b = smooth_gray(w, h, 3);
    let mut v = Vec::with_capacity(w * h * 3);
    for i in 0..w * h {
        v.push(r[i]);
        v.push(g[i]);
        v.push(b[i]);
    }
    v
}

fn smooth_u16(w: usize, h: usize, bits: u32, seed: u32) -> Vec<u16> {
    let peak = ((1u32 << bits) - 1) as f64;
    smooth_gray(w, h, seed)
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            (((v as f64 / 255.0) * peak).round() as u32 + (i as u32 % 3)).min(peak as u32) as u16
        })
        .collect()
}

/// The RGB a chroma-splatting reader reconstructs from a subsampled
/// YCbCr page with *no* codec loss: box-decimate Cb / Cr per
/// `sh × sv` block (rounded mean, as the encoder does), splat each
/// block value back, and apply the crate's YCbCr → RGB matrix
/// (BT.601 in Q16, the decoder's own formula).
fn splat_reference_rgb(ycc: &[u8], w: usize, h: usize, sh: usize, sv: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    let bw = w.div_ceil(sh);
    let bh = h.div_ceil(sv);
    for by in 0..bh {
        for bx in 0..bw {
            let (mut cb, mut cr, mut n) = (0u32, 0u32, 0u32);
            for sy in 0..sv {
                for sx in 0..sh {
                    let (x, y) = (bx * sh + sx, by * sv + sy);
                    if x < w && y < h {
                        cb += ycc[(y * w + x) * 3 + 1] as u32;
                        cr += ycc[(y * w + x) * 3 + 2] as u32;
                        n += 1;
                    }
                }
            }
            let cb = ((cb + n / 2) / n) as i32 - 128;
            let cr = ((cr + n / 2) / n) as i32 - 128;
            for sy in 0..sv {
                for sx in 0..sh {
                    let (x, y) = (bx * sh + sx, by * sv + sy);
                    if x < w && y < h {
                        let yy = ycc[(y * w + x) * 3] as i32;
                        let r = yy + ((91881 * cr + 32768) >> 16);
                        let g = yy - ((22554 * cb + 46802 * cr + 32768) >> 16);
                        let b = yy + ((116130 * cb + 32768) >> 16);
                        let o = (y * w + x) * 3;
                        out[o] = r.clamp(0, 255) as u8;
                        out[o + 1] = g.clamp(0, 255) as u8;
                        out[o + 2] = b.clamp(0, 255) as u8;
                    }
                }
            }
        }
    }
    out
}

fn gray_frame_bytes(decoded: &oxideav_tiff::DecodedTiff) -> Vec<u8> {
    assert_eq!(decoded.pixel_format, TiffPixelFormat::Gray8);
    let p = &decoded.frame.planes[0];
    let w = decoded.width as usize;
    (0..decoded.height as usize)
        .flat_map(|y| p.data[y * p.stride..y * p.stride + w].to_vec())
        .collect()
}

fn rgb_frame_bytes(decoded: &oxideav_tiff::DecodedTiff) -> Vec<u8> {
    assert_eq!(decoded.pixel_format, TiffPixelFormat::Rgb24);
    let p = &decoded.frame.planes[0];
    let w = decoded.width as usize;
    (0..decoded.height as usize)
        .flat_map(|y| p.data[y * p.stride..y * p.stride + w * 3].to_vec())
        .collect()
}

fn u16_frame(decoded: &oxideav_tiff::DecodedTiff, spp: usize) -> Vec<u16> {
    let p = &decoded.frame.planes[0];
    let w = decoded.width as usize;
    (0..decoded.height as usize)
        .flat_map(|y| {
            p.data[y * p.stride..y * p.stride + w * spp * 2]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn to_u16(v: &[u8]) -> Vec<u16> {
    v.iter().map(|&b| b as u16).collect()
}

fn jpeg(quality: u8, tables: JpegTablesLayout) -> TiffCompression {
    TiffCompression::Jpeg(JpegOptions {
        quality,
        tables,
        ..JpegOptions::default()
    })
}

fn page<'a>(
    w: u32,
    h: u32,
    kind: EncodePixelFormat<'a>,
    compression: TiffCompression,
) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind,
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
    }
}

/// Shared black-box battery for an 8-bit page: our decode vs source
/// PSNR, then magick / tiffcp agreement with our decode.
/// Returns the number of external validators that ran.
fn validate_8bit(tiff: &[u8], source: &[u8], rgb: bool, floor_db: f64, label: &str) -> usize {
    validate_8bit_agree(tiff, source, rgb, floor_db, 40.0, label)
}

/// Like [`validate_8bit`] with an explicit cross-decoder agreement
/// floor: chroma-subsampled YCbCr legitimately reconstructs
/// differently in every decoder (nearest-neighbour chroma splat in
/// ours vs. interpolating upsamplers elsewhere), so those layouts use
/// a lower `agree_db` and a `source` that is the splat reconstruction.
fn validate_8bit_agree(
    tiff: &[u8],
    source: &[u8],
    rgb: bool,
    floor_db: f64,
    agree_db: f64,
    label: &str,
) -> usize {
    if let Ok(dir) = std::env::var("OXIDEAV_TIFF_DUMP_DIR") {
        // Debugging aid: keep every validated file for manual
        // inspection with tiffinfo / tiffdump / magick.
        let name = label.replace(|c: char| !c.is_ascii_alphanumeric(), "_");
        let _ = fs::write(PathBuf::from(dir).join(format!("{name}.tif")), tiff);
    }
    let dec = decode_tiff(tiff).unwrap();
    assert_eq!(dec.format.compression, Some(COMPRESSION_JPEG_NEW));
    let ours = if rgb {
        rgb_frame_bytes(&dec)
    } else {
        gray_frame_bytes(&dec)
    };
    let p = psnr_u8(source, &ours);
    assert!(
        p >= floor_db,
        "{label}: own decode PSNR {p:.2} dB < {floor_db}"
    );
    eprintln!("{label}: own decode PSNR {p:.2} dB");
    let mut ran = 0;
    if let Some(m) = magick_decode(tiff, rgb) {
        let m8: Vec<u8> = m.iter().map(|&v| v as u8).collect();
        let pm = psnr_u8(source, &m8);
        let ext_floor = floor_db.min(agree_db);
        assert!(
            pm >= ext_floor,
            "{label}: magick PSNR {pm:.2} dB < {ext_floor}"
        );
        let agree = psnr_u8(&ours, &m8);
        assert!(
            agree >= agree_db,
            "{label}: magick vs own decode {agree:.2} dB"
        );
        eprintln!("{label}: magick PSNR {pm:.2} dB (vs ours {agree:.2} dB)");
        ran += 1;
    }
    if let Some(rewritten) = tiffcp_none(tiff) {
        let dec2 = decode_tiff(&rewritten).unwrap();
        assert_eq!(dec2.format.compression, Some(COMPRESSION_NONE));
        assert_eq!((dec2.width, dec2.height), (dec.width, dec.height));
        let theirs = if rgb {
            rgb_frame_bytes(&dec2)
        } else {
            gray_frame_bytes(&dec2)
        };
        let agree = psnr_u8(&ours, &theirs);
        assert!(
            agree >= agree_db,
            "{label}: tiffcp(libtiff) vs own decode {agree:.2} dB"
        );
        let pt = psnr_u8(source, &theirs);
        let ext_floor = floor_db.min(agree_db);
        assert!(pt >= ext_floor, "{label}: tiffcp PSNR {pt:.2} dB");
        eprintln!("{label}: tiffcp -c none PSNR {pt:.2} dB (vs ours {agree:.2} dB)");
        ran += 1;
    }
    if let Some(info) = tiffinfo(tiff) {
        assert!(info.contains("Compression Scheme: JPEG"), "{info}");
        ran += 1;
    }
    ran
}

/// Chroma-subsampled `PlanarConfiguration = 2` JPEG pages are the TN2
/// layout "readers are not required to support": libtiff (and so
/// ImageMagick) refuses or mis-reads them, so the black-box check goes
/// segment by segment instead — every plane segment is a plain
/// single-component JPEG that `djpeg` decodes, compared with the
/// matching window of the (decimated) source plane. Our own decoder
/// must still reconstruct the page against the splat reference.
#[allow(clippy::too_many_arguments)]
fn validate_planar_subsampled(
    tiff: &[u8],
    ycc: &[u8],
    w: usize,
    h: usize,
    sh: usize,
    sv: usize,
    label: &str,
) {
    if let Ok(dir) = std::env::var("OXIDEAV_TIFF_DUMP_DIR") {
        let name = label.replace(|c: char| !c.is_ascii_alphanumeric(), "_");
        let _ = fs::write(PathBuf::from(dir).join(format!("{name}.tif")), tiff);
    }
    let dec = decode_tiff(tiff).unwrap();
    assert_eq!(dec.format.planar_config, Some(PLANAR_SEPARATE));
    let ours = rgb_frame_bytes(&dec);
    let reference = splat_reference_rgb(ycc, w, h, sh, sv);
    let p = psnr_u8(&reference, &ours);
    assert!(p >= 36.0, "{label}: own decode PSNR {p:.2} dB");
    eprintln!("{label}: own decode PSNR {p:.2} dB");
    if let Some(info) = tiffinfo(tiff) {
        assert!(info.contains("separate image planes"), "{info}");
    }
    // Expected planes: Y at full resolution, Cb / Cr decimated.
    let planes: Vec<(Vec<u8>, usize, usize)> = (0..3)
        .map(|c| {
            let (dh, dv) = if c == 0 { (1, 1) } else { (sh, sv) };
            let pw = w.div_ceil(dh);
            let ph = h.div_ceil(dv);
            let mut plane = vec![0u8; pw * ph];
            for by in 0..ph {
                for bx in 0..pw {
                    let (mut sum, mut n) = (0u32, 0u32);
                    for sy in 0..dv {
                        for sx in 0..dh {
                            let (x, y) = (bx * dh + sx, by * dv + sy);
                            if x < w && y < h {
                                sum += ycc[(y * w + x) * 3 + c] as u32;
                                n += 1;
                            }
                        }
                    }
                    plane[by * pw + bx] = ((sum + n / 2) / n) as u8;
                }
            }
            (plane, pw, ph)
        })
        .collect();
    let segs = extract_segments(tiff);
    let per_plane = segs.len() / 3;
    let entries = ifd_entries(tiff);
    let bo = oxideav_tiff::ifd::ByteOrder::Little;
    let tiling = find(&entries, TAG_TILE_WIDTH).map(|e| {
        (
            e.as_u32(bo).unwrap() as usize,
            find(&entries, TAG_TILE_LENGTH).unwrap().as_u32(bo).unwrap() as usize,
        )
    });
    let rps = find(&entries, TAG_ROWS_PER_STRIP)
        .map(|e| e.as_u32(bo).unwrap() as usize)
        .unwrap_or(h);
    let mut checked = 0;
    for (c, (plane, pw, ph)) in planes.iter().enumerate() {
        let (dh, dv) = if c == 0 { (1, 1) } else { (sh, sv) };
        for i in 0..per_plane {
            // Window of this segment on the plane grid.
            let (x0, y0, sw, shh) = match tiling {
                Some((tw, th)) => {
                    let across = w.div_ceil(tw);
                    let (tx, ty) = (i % across, i / across);
                    (tx * (tw / dh), ty * (th / dv), tw / dh, th / dv)
                }
                None => {
                    let luma_rows = rps.min(h - i * rps);
                    (0, i * rps / dv, *pw, luma_rows.div_ceil(dv))
                }
            };
            let Some((ch, dw, dhh, got)) = djpeg(&segs[c * per_plane + i], &["-pnm"]) else {
                continue;
            };
            assert_eq!(
                (ch, dw, dhh),
                (1, sw, shh),
                "{label}: plane {c} segment {i} geometry"
            );
            let mut want = Vec::with_capacity(sw * shh);
            for y in 0..shh {
                for x in 0..sw {
                    let sx = (x0 + x).min(pw - 1);
                    let sy = (y0 + y).min(ph - 1);
                    want.push(plane[sy * pw + sx] as u16);
                }
            }
            let p = psnr_u16(&want, &got, 255.0);
            assert!(
                p >= 38.0,
                "{label}: plane {c} segment {i} djpeg PSNR {p:.2} dB"
            );
            checked += 1;
        }
    }
    if checked > 0 {
        eprintln!("{label}: djpeg validated {checked} plane segments");
    }
}

// ---------------------------------------------------------------------------
// Grayscale strips, both table layouts.
// ---------------------------------------------------------------------------

#[test]
fn gray8_single_strip_both_table_layouts() {
    let (w, h) = (45u32, 29u32);
    let src = smooth_gray(w as usize, h as usize, 4);
    for layout in [JpegTablesLayout::Shared, JpegTablesLayout::PerSegment] {
        let tiff = encode_tiff(&page(
            w,
            h,
            EncodePixelFormat::Gray8 { pixels: &src },
            jpeg(90, layout),
        ))
        .unwrap();
        let entries = ifd_entries(&tiff);
        assert_eq!(
            find(&entries, TAG_JPEG_TABLES).is_some(),
            layout == JpegTablesLayout::Shared,
            "JPEGTables presence follows the layout"
        );
        if let Some(t) = find(&entries, TAG_JPEG_TABLES) {
            assert_eq!(t.field_type, TYPE_UNDEFINED);
            assert_eq!(&t.data[..2], &[0xFF, 0xD8]);
            assert_eq!(&t.data[t.data.len() - 2..], &[0xFF, 0xD9]);
        }
        validate_8bit(&tiff, &src, false, 38.0, &format!("gray8 {layout:?}"));
        // djpeg on the (merged) single segment.
        let segs = extract_segments(&tiff);
        assert_eq!(segs.len(), 1);
        if let Some((c, sw, sh, s)) = djpeg(&segs[0], &["-pnm"]) {
            assert_eq!((c, sw, sh), (1, w as usize, h as usize));
            assert!(psnr_u16(&to_u16(&src), &s, 255.0) >= 38.0);
        }
    }
}

#[test]
fn gray8_multi_strip_rows_per_strip_16() {
    let (w, h) = (33u32, 50u32);
    let src = smooth_gray(w as usize, h as usize, 2);
    let mut p = page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &src },
        jpeg(85, JpegTablesLayout::Shared),
    );
    p.extras.rows_per_strip = Some(16);
    let tiff = encode_tiff(&p).unwrap();
    let segs = extract_segments(&tiff);
    assert_eq!(segs.len(), 4, "ceil(50 / 16) strips");
    validate_8bit(&tiff, &src, false, 36.0, "gray8 4 strips");
    // The last strip's SOF height is the remaining 2 rows (TN2).
    let last = &segs[3];
    let sof = last.windows(2).position(|x| x == [0xFF, 0xC0]).unwrap();
    let lines = u16::from_be_bytes([last[sof + 5], last[sof + 6]]);
    assert_eq!(lines, 2);
    // Quality knob is monotone in size.
    let big = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &src },
        jpeg(100, JpegTablesLayout::PerSegment),
    ))
    .unwrap();
    let small = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &src },
        jpeg(20, JpegTablesLayout::PerSegment),
    ))
    .unwrap();
    assert!(small.len() < big.len());
}

#[test]
fn rows_per_strip_must_be_mcu_aligned_for_dct() {
    let (w, h) = (16u32, 40u32);
    let src = smooth_gray(w as usize, h as usize, 1);
    let mut p = page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &src },
        jpeg(75, JpegTablesLayout::Shared),
    );
    p.extras.rows_per_strip = Some(12);
    let err = encode_tiff(&p).unwrap_err().to_string();
    assert!(err.contains("multiple of the MCU height"), "{err}");
    // Lossless is exempt.
    p.compression = TiffCompression::Jpeg(JpegOptions {
        process: JpegProcess::Lossless { predictor: 1 },
        ..JpegOptions::default()
    });
    let tiff = encode_tiff(&p).unwrap();
    let dec = decode_tiff(&tiff).unwrap();
    assert_eq!(gray_frame_bytes(&dec), src);
    // A single strip of any height is fine for DCT.
    let mut p2 = page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &src },
        jpeg(75, JpegTablesLayout::Shared),
    );
    p2.extras.rows_per_strip = Some(40);
    encode_tiff(&p2).unwrap();
}

// ---------------------------------------------------------------------------
// RGB (photometric 2, no colour transform) and YCbCr.
// ---------------------------------------------------------------------------

#[test]
fn rgb24_photometric_rgb_jpeg() {
    let (w, h) = (40u32, 24u32);
    let src = smooth_rgb(w as usize, h as usize);
    for layout in [JpegTablesLayout::Shared, JpegTablesLayout::PerSegment] {
        let tiff = encode_tiff(&page(
            w,
            h,
            EncodePixelFormat::Rgb24 { pixels: &src },
            jpeg(92, layout),
        ))
        .unwrap();
        let dec = decode_tiff(&tiff).unwrap();
        assert_eq!(dec.format.photometric, Some(PHOTO_RGB));
        validate_8bit(&tiff, &src, true, 38.0, &format!("rgb24 {layout:?}"));
    }
}

#[test]
fn ycbcr_444_and_subsampled_strips() {
    let (w, h) = (48u32, 32u32);
    let rgb = smooth_rgb(w as usize, h as usize);
    let ycc = rgb24_to_ycbcr24(&rgb);
    // 4:4:4 via the plain YCbCr24 input.
    let tiff = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::YCbCr24 { pixels: &ycc },
        jpeg(92, JpegTablesLayout::Shared),
    ))
    .unwrap();
    let dec = decode_tiff(&tiff).unwrap();
    assert_eq!(dec.format.photometric, Some(PHOTO_YCBCR));
    validate_8bit(&tiff, &rgb, true, 36.0, "ycbcr 4:4:4");
    let ycc = smooth_ycc(w as usize, h as usize);
    // Subsampled: every §21 pair; the chroma is smooth so the
    // decimation loses little.
    for (sh, sv) in [(2u16, 1u16), (2, 2), (4, 1), (4, 2)] {
        let mut p = page(
            w,
            h,
            EncodePixelFormat::YCbCrSubsampled24 {
                pixels: &ycc,
                subsampling: (sh, sv),
            },
            jpeg(92, JpegTablesLayout::Shared),
        );
        p.extras.rows_per_strip = Some(16);
        let tiff = encode_tiff(&p).unwrap();
        let entries = ifd_entries(&tiff);
        let ss = find(&entries, TAG_YCBCR_SUBSAMPLING)
            .unwrap()
            .as_u32_vec(oxideav_tiff::ifd::ByteOrder::Little)
            .unwrap();
        assert_eq!(ss, vec![sh as u32, sv as u32]);
        let reference = splat_reference_rgb(&ycc, w as usize, h as usize, sh as usize, sv as usize);
        if (sh, sv) == (4, 2) {
            // The crate's own Compression = 7 reader (oxideav-mjpeg)
            // does not decode 4x2 luma sampling, so this legal §21
            // pair is validated black-box only.
            let err = match decode_tiff(&tiff) {
                Ok(_) => String::from("decoded"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains("4x2"), "{err}");
            if let Some(m) = magick_decode(&tiff, true) {
                let m8: Vec<u8> = m.iter().map(|&v| v as u8).collect();
                let pm = psnr_u8(&reference, &m8);
                assert!(pm >= 33.0, "ycbcr 4x2: magick PSNR {pm:.2} dB");
                eprintln!("ycbcr 4x2 strips: magick PSNR {pm:.2} dB (black-box only)");
            }
        } else {
            validate_8bit_agree(
                &tiff,
                &reference,
                true,
                36.0,
                33.0,
                &format!("ycbcr {sh}x{sv} strips"),
            );
        }
        // The SOF sampling factors agree with the TIFF field (TN2).
        let segs = extract_segments(&tiff);
        assert_eq!(segs.len(), 2);
        let sof = segs[0].windows(2).position(|x| x == [0xFF, 0xC0]).unwrap();
        // FF C0 | Lf | P | Y | X | Nf | C1 HV1 Tq1 | C2 HV2 Tq2 | …
        assert_eq!(segs[0][sof + 11], ((sh as u8) << 4) | sv as u8, "luma H/V");
        assert_eq!(segs[0][sof + 14], 0x11, "Cb H/V");
        if let Some((c, dw, dh, _)) = djpeg(&segs[0], &["-pnm"]) {
            assert_eq!((c, dw, dh), (3, w as usize, 16));
        }
    }
}

// ---------------------------------------------------------------------------
// Tiles.
// ---------------------------------------------------------------------------

#[test]
fn tiled_gray_rgb_and_ycbcr420() {
    let (w, h) = (70u32, 45u32);
    let gray = smooth_gray(w as usize, h as usize, 6);
    let rgb = smooth_rgb(w as usize, h as usize);
    let ycc = rgb24_to_ycbcr24(&rgb);
    let mut p = page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &gray },
        jpeg(90, JpegTablesLayout::Shared),
    );
    p.tiling = Some((32, 16));
    let tiff = encode_tiff(&p).unwrap();
    assert_eq!(extract_segments(&tiff).len(), 3 * 3);
    let dec = decode_tiff(&tiff).unwrap();
    assert!(dec.format.tiled);
    validate_8bit(&tiff, &gray, false, 37.0, "gray8 tiles 32x16");

    let mut p = page(
        w,
        h,
        EncodePixelFormat::Rgb24 { pixels: &rgb },
        jpeg(90, JpegTablesLayout::PerSegment),
    );
    p.tiling = Some((16, 32));
    let tiff = encode_tiff(&p).unwrap();
    validate_8bit(&tiff, &rgb, true, 37.0, "rgb24 tiles 16x32");

    let mut p = page(
        w,
        h,
        EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &ycc,
            subsampling: (2, 2),
        },
        jpeg(90, JpegTablesLayout::Shared),
    );
    // 16x16 tiles are exactly one 4:2:0 MCU.
    p.tiling = Some((16, 16));
    // ImageWidth / ImageLength must be multiples of the factors (§21).
    p.width = 70;
    p.height = 44;
    let ycc2 = smooth_ycc(70, 44);
    p.kind = EncodePixelFormat::YCbCrSubsampled24 {
        pixels: &ycc2,
        subsampling: (2, 2),
    };
    let tiff = encode_tiff(&p).unwrap();
    let reference = splat_reference_rgb(&ycc2, 70, 44, 2, 2);
    validate_8bit_agree(
        &tiff,
        &reference,
        true,
        36.0,
        33.0,
        "ycbcr 4:2:0 tiles 16x16",
    );
    // 4:1:1 needs 32-wide tiles (8 × Hmax = 32).
    let ycc411 = smooth_ycc(64, 32);
    let mut p = page(
        64,
        32,
        EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &ycc411,
            subsampling: (4, 1),
        },
        jpeg(90, JpegTablesLayout::Shared),
    );
    p.tiling = Some((16, 16));
    let err = encode_tiff(&p).unwrap_err().to_string();
    assert!(err.contains("MCU size"), "{err}");
    p.tiling = Some((32, 16));
    let tiff = encode_tiff(&p).unwrap();
    let reference = splat_reference_rgb(&ycc411, 64, 32, 4, 1);
    validate_8bit_agree(
        &tiff,
        &reference,
        true,
        36.0,
        33.0,
        "ycbcr 4:1:1 tiles 32x16",
    );
}

// ---------------------------------------------------------------------------
// PlanarConfiguration = 2.
// ---------------------------------------------------------------------------

#[test]
fn planar_rgb_and_subsampled_ycbcr_strips_and_tiles() {
    let (w, h) = (40u32, 36u32);
    let rgb = smooth_rgb(w as usize, h as usize);
    let ycc = smooth_ycc(w as usize, h as usize);
    // RGB planes, 3 strips each.
    let mut p = page(
        w,
        h,
        EncodePixelFormat::Rgb24 { pixels: &rgb },
        jpeg(92, JpegTablesLayout::Shared),
    );
    p.planar = true;
    p.extras.rows_per_strip = Some(16);
    let tiff = encode_tiff(&p).unwrap();
    let segs = extract_segments(&tiff);
    assert_eq!(segs.len(), 3 * 3, "SamplesPerPixel × StripsPerImage");
    let dec = decode_tiff(&tiff).unwrap();
    assert_eq!(dec.format.planar_config, Some(PLANAR_SEPARATE));
    validate_8bit(&tiff, &rgb, true, 38.0, "planar rgb strips");
    // Every plane segment is a single-component frame with 1x1
    // factors (TN2 PlanarConfiguration 2).
    for s in &segs {
        let sof = s.windows(2).position(|x| x == [0xFF, 0xC0]).unwrap();
        assert_eq!(s[sof + 9], 1, "Nf");
        assert_eq!(s[sof + 11], 0x11);
    }

    // YCbCr 4:2:2 planes: chroma strips are 20 wide, 16 tall (the
    // luma strip is 40 x 16).
    let mut p = page(
        w,
        h,
        EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &ycc,
            subsampling: (2, 1),
        },
        jpeg(92, JpegTablesLayout::Shared),
    );
    p.planar = true;
    p.extras.rows_per_strip = Some(16);
    let tiff = encode_tiff(&p).unwrap();
    let segs = extract_segments(&tiff);
    assert_eq!(segs.len(), 9);
    let sof = segs[3].windows(2).position(|x| x == [0xFF, 0xC0]).unwrap();
    let cw = u16::from_be_bytes([segs[3][sof + 7], segs[3][sof + 8]]);
    assert_eq!(cw, 20, "Cb plane strip width scaled by 2");
    validate_planar_subsampled(
        &tiff,
        &ycc,
        w as usize,
        h as usize,
        2,
        1,
        "planar ycbcr 4:2:2 strips",
    );

    // 4:2:0 planar tiles: chroma tiles are 16x8 for 32x16 luma tiles.
    let mut p = page(
        w,
        h,
        EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &ycc,
            subsampling: (2, 2),
        },
        jpeg(92, JpegTablesLayout::PerSegment),
    );
    p.planar = true;
    p.tiling = Some((32, 16));
    let tiff = encode_tiff(&p).unwrap();
    let segs = extract_segments(&tiff);
    assert_eq!(segs.len(), 3 * 2 * 3);
    let sof = segs[6].windows(2).position(|x| x == [0xFF, 0xC0]).unwrap();
    let ch = u16::from_be_bytes([segs[6][sof + 5], segs[6][sof + 6]]);
    let cw = u16::from_be_bytes([segs[6][sof + 7], segs[6][sof + 8]]);
    assert_eq!((cw, ch), (16, 8));
    validate_planar_subsampled(
        &tiff,
        &ycc,
        w as usize,
        h as usize,
        2,
        2,
        "planar ycbcr 4:2:0 tiles",
    );
}

// ---------------------------------------------------------------------------
// CMYK.
// ---------------------------------------------------------------------------

#[test]
fn cmyk_jpeg_roundtrips_through_own_decoder() {
    let (w, h) = (24u32, 20u32);
    let rgb = smooth_rgb(w as usize, h as usize);
    // Simple CMY(K=0) so the decoder's §16 additive mapping inverts.
    let cmyk: Vec<u8> = rgb
        .chunks_exact(3)
        .flat_map(|p| [255 - p[0], 255 - p[1], 255 - p[2], 0])
        .collect();
    for planar in [false, true] {
        let mut p = page(
            w,
            h,
            EncodePixelFormat::Cmyk32 { pixels: &cmyk },
            jpeg(95, JpegTablesLayout::Shared),
        );
        p.planar = planar;
        let tiff = encode_tiff(&p).unwrap();
        let dec = decode_tiff(&tiff).unwrap();
        assert_eq!(dec.format.photometric, Some(PHOTO_CMYK));
        let ours = rgb_frame_bytes(&dec);
        let p = psnr_u8(&rgb, &ours);
        assert!(p >= 36.0, "cmyk planar={planar}: PSNR {p:.2}");
        if let Some(info) = tiffinfo(&tiff) {
            assert!(info.contains("separated"), "{info}");
        }
        let segs = extract_segments(&tiff);
        if !planar {
            if let Some((c, dw, dh, _)) = djpeg(&segs[0], &["-pnm"]) {
                // djpeg emits CMYK as a 4-channel PAM only with -pnm
                // refusing; accept either a refusal (None) or RGB.
                assert!(c == 3 || c == 1);
                assert_eq!((dw, dh), (w as usize, h as usize));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Deep precisions: 12-bit SOF1, lossless SOF3 (8 / 12 / 16 bits).
// ---------------------------------------------------------------------------

fn widen12(v: u16) -> u16 {
    (v << 4) | (v >> 8)
}

#[test]
fn gray12_and_rgb36_dct_sof1() {
    let (w, h) = (30u32, 21u32);
    let g = smooth_u16(w as usize, h as usize, 12, 1);
    let tiff = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Gray12 { pixels: &g },
        jpeg(95, JpegTablesLayout::Shared),
    ))
    .unwrap();
    let dec = decode_tiff(&tiff).unwrap();
    assert_eq!(dec.pixel_format, TiffPixelFormat::Gray16Le);
    assert_eq!(dec.format.bits_per_sample, vec![12]);
    let got = u16_frame(&dec, 1);
    let want: Vec<u16> = g.iter().map(|&v| widen12(v)).collect();
    let p = psnr_u16(&want, &got, 65535.0);
    assert!(p >= 42.0, "gray12: PSNR {p:.2}");
    let segs = extract_segments(&tiff);
    assert!(segs[0].windows(2).any(|x| x == [0xFF, 0xC1]), "SOF1");
    if let Some((c, dw, dh, s)) = djpeg(&segs[0], &["-pnm", "-precision", "12"]) {
        assert_eq!((c, dw, dh), (1, w as usize, h as usize));
        let p = psnr_u16(&g, &s, 4095.0);
        assert!(p >= 42.0, "gray12 djpeg: PSNR {p:.2}");
        eprintln!("gray12: djpeg PSNR {p:.2} dB");
    }
    if let Some(info) = tiffinfo(&tiff) {
        assert!(info.contains("Bits/Sample: 12"), "{info}");
    }
    if let Some(m) = magick_decode_depth(&tiff, false, 16) {
        let p = psnr_u16(&want, &m, 65535.0);
        assert!(p >= 40.0, "gray12 magick(16-bit): PSNR {p:.2}");
        eprintln!("gray12: magick 16-bit PSNR {p:.2} dB");
    }

    let r = smooth_u16(w as usize, h as usize, 12, 1);
    let gg = smooth_u16(w as usize, h as usize, 12, 2);
    let b = smooth_u16(w as usize, h as usize, 12, 3);
    let mut rgb = Vec::with_capacity(r.len() * 3);
    for i in 0..r.len() {
        rgb.extend_from_slice(&[r[i], gg[i], b[i]]);
    }
    for planar in [false, true] {
        let mut p = page(
            w,
            h,
            EncodePixelFormat::Rgb36 { pixels: &rgb },
            jpeg(95, JpegTablesLayout::PerSegment),
        );
        p.planar = planar;
        let tiff = encode_tiff(&p).unwrap();
        let dec = decode_tiff(&tiff).unwrap();
        assert_eq!(dec.pixel_format, TiffPixelFormat::Rgb48Le);
        let got = u16_frame(&dec, 3);
        let want: Vec<u16> = rgb.iter().map(|&v| widen12(v)).collect();
        let p = psnr_u16(&want, &got, 65535.0);
        assert!(p >= 42.0, "rgb36 planar={planar}: PSNR {p:.2}");
        if !planar {
            let segs = extract_segments(&tiff);
            // Outside the TIFF container a bare three-component
            // stream is read as YCbCr (T.872 §6.1: the container is
            // what says otherwise), so djpeg only confirms the
            // stream decodes with the right geometry here; the
            // colour check goes through magick with the container.
            if let Some((c, dw, dh, _)) = djpeg(&segs[0], &["-pnm", "-precision", "12"]) {
                assert_eq!((c, dw, dh), (3, w as usize, h as usize));
            }
            if let Some(m) = magick_decode_depth(&tiff, true, 16) {
                let p = psnr_u16(&want, &m, 65535.0);
                assert!(p >= 40.0, "rgb36 magick(16-bit): PSNR {p:.2}");
                eprintln!("rgb36: magick 16-bit PSNR {p:.2} dB");
            }
        }
    }
    // 12-bit input under a byte-run compressor is rejected precisely.
    let err = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Gray12 { pixels: &g },
        TiffCompression::Lzw,
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("Compression=7"), "{err}");
}

#[test]
fn lossless_sof3_is_sample_exact_at_8_12_and_16_bits() {
    let (w, h) = (27u32, 19u32);
    let g8 = smooth_gray(w as usize, h as usize, 3);
    let g12 = smooth_u16(w as usize, h as usize, 12, 4);
    let g16 = smooth_u16(w as usize, h as usize, 16, 5);
    let g16le: Vec<u8> = g16.iter().flat_map(|v| v.to_le_bytes()).collect();
    for predictor in [1u8, 4, 7] {
        let comp = TiffCompression::Jpeg(JpegOptions {
            process: JpegProcess::Lossless { predictor },
            tables: if predictor == 4 {
                JpegTablesLayout::PerSegment
            } else {
                JpegTablesLayout::Shared
            },
            ..JpegOptions::default()
        });
        // 8-bit.
        let tiff =
            encode_tiff(&page(w, h, EncodePixelFormat::Gray8 { pixels: &g8 }, comp)).unwrap();
        let dec = decode_tiff(&tiff).unwrap();
        assert_eq!(gray_frame_bytes(&dec), g8, "8-bit predictor {predictor}");
        let segs = extract_segments(&tiff);
        assert!(segs[0].windows(2).any(|x| x == [0xFF, 0xC3]), "SOF3");
        if let Some((_, _, _, s)) = djpeg(&segs[0], &["-pnm"]) {
            assert_eq!(s, to_u16(&g8), "djpeg 8-bit predictor {predictor}");
        }
        if let Some(rewritten) = tiffcp_none(&tiff) {
            let dec2 = decode_tiff(&rewritten).unwrap();
            assert_eq!(
                gray_frame_bytes(&dec2),
                g8,
                "tiffcp 8-bit lossless predictor {predictor}"
            );
        }
        if let Some(m) = magick_decode(&tiff, false) {
            assert_eq!(
                m,
                to_u16(&g8),
                "magick 8-bit lossless predictor {predictor}"
            );
        }
        // 12-bit.
        let tiff = encode_tiff(&page(
            w,
            h,
            EncodePixelFormat::Gray12 { pixels: &g12 },
            comp,
        ))
        .unwrap();
        let dec = decode_tiff(&tiff).unwrap();
        let want: Vec<u16> = g12.iter().map(|&v| widen12(v)).collect();
        assert_eq!(u16_frame(&dec, 1), want, "12-bit predictor {predictor}");
        let segs = extract_segments(&tiff);
        if let Some((_, _, _, s)) = djpeg(&segs[0], &["-pnm", "-precision", "12"]) {
            assert_eq!(s, g12, "djpeg 12-bit predictor {predictor}");
        }
        // 16-bit.
        let tiff = encode_tiff(&page(
            w,
            h,
            EncodePixelFormat::Gray16Le { pixels: &g16le },
            comp,
        ))
        .unwrap();
        let dec = decode_tiff(&tiff).unwrap();
        assert_eq!(dec.format.bits_per_sample, vec![16]);
        assert_eq!(u16_frame(&dec, 1), g16, "16-bit predictor {predictor}");
        let segs = extract_segments(&tiff);
        if let Some((_, _, _, s)) = djpeg(&segs[0], &["-pnm", "-precision", "16"]) {
            assert_eq!(s, g16, "djpeg 16-bit predictor {predictor}");
        }
    }
    // 16-bit RGB lossless, chunky + planar + tiled.
    let r = smooth_u16(w as usize, h as usize, 16, 1);
    let g = smooth_u16(w as usize, h as usize, 16, 2);
    let b = smooth_u16(w as usize, h as usize, 16, 3);
    let mut rgb48 = Vec::with_capacity(r.len() * 6);
    for i in 0..r.len() {
        for v in [r[i], g[i], b[i]] {
            rgb48.extend_from_slice(&v.to_le_bytes());
        }
    }
    let want: Vec<u16> = (0..r.len()).flat_map(|i| [r[i], g[i], b[i]]).collect();
    let comp = TiffCompression::Jpeg(JpegOptions {
        process: JpegProcess::Lossless { predictor: 6 },
        ..JpegOptions::default()
    });
    for (planar, tiling) in [
        (false, None),
        (true, None),
        (false, Some((16, 16))),
        (true, Some((16, 16))),
    ] {
        let mut p = page(w, h, EncodePixelFormat::Rgb48 { pixels: &rgb48 }, comp);
        p.planar = planar;
        p.tiling = tiling;
        let tiff = encode_tiff(&p).unwrap();
        let dec = decode_tiff(&tiff).unwrap();
        assert_eq!(dec.pixel_format, TiffPixelFormat::Rgb48Le);
        assert_eq!(
            u16_frame(&dec, 3),
            want,
            "rgb48 planar={planar} tiling={tiling:?}"
        );
    }
    // 16-bit input needs the lossless process.
    let err = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Gray16Le { pixels: &g16le },
        jpeg(75, JpegTablesLayout::Shared),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("lossless"), "{err}");
}

// ---------------------------------------------------------------------------
// Layout composition + rejections.
// ---------------------------------------------------------------------------

#[test]
fn bigtiff_and_multipage_compose_with_jpeg() {
    let (w, h) = (20u32, 18u32);
    let a = smooth_gray(w as usize, h as usize, 1);
    let b = smooth_rgb(w as usize, h as usize);
    let mut p1 = page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &a },
        jpeg(90, JpegTablesLayout::Shared),
    );
    p1.bigtiff = true;
    let mut p2 = page(
        w,
        h,
        EncodePixelFormat::Rgb24 { pixels: &b },
        jpeg(90, JpegTablesLayout::PerSegment),
    );
    p2.bigtiff = true;
    let tiff = oxideav_tiff::encode_tiff_multi(&[p1, p2]).unwrap();
    assert_eq!(u16::from_le_bytes([tiff[2], tiff[3]]), 43, "BigTIFF magic");
    let pages = oxideav_tiff::decode_tiff_all_pages(&tiff).unwrap();
    assert_eq!(pages.len(), 2);
    assert!(psnr_u8(&a, &gray_frame_bytes(&pages[0])) >= 38.0);
    assert!(psnr_u8(&b, &rgb_frame_bytes(&pages[1])) >= 38.0);
    if let Some(m) = magick_decode(&tiff, false) {
        // magick reads the first page.
        let m8: Vec<u8> = m.iter().map(|&v| v as u8).collect();
        assert!(psnr_u8(&a, &m8) >= 38.0);
    }
}

#[test]
fn jpeg_rejections_are_precise() {
    let (w, h) = (16u32, 16u32);
    let g = smooth_gray(16, 16, 1);
    let comp = jpeg(75, JpegTablesLayout::Shared);
    let mut p = page(w, h, EncodePixelFormat::Gray8 { pixels: &g }, comp);
    p.predictor = true;
    assert!(encode_tiff(&p)
        .unwrap_err()
        .to_string()
        .contains("Predictor"));
    let bits = vec![0u8; 2 * 16];
    let err = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Bilevel { pixels: &bits },
        comp,
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("Bilevel"), "{err}");
    let pal = vec![[0u8, 0, 0]; 256];
    let err = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Palette8 {
            indices: &g,
            palette: &pal,
        },
        comp,
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("Palette8"), "{err}");
    let f = vec![0f32; 256];
    let err = encode_tiff(&page(w, h, EncodePixelFormat::GrayF32 { pixels: &f }, comp))
        .unwrap_err()
        .to_string();
    assert!(err.contains("float"), "{err}");
    let err = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &g },
        jpeg(0, JpegTablesLayout::Shared),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("quality"), "{err}");
    let bad = TiffCompression::Jpeg(JpegOptions {
        process: JpegProcess::Lossless { predictor: 8 },
        ..JpegOptions::default()
    });
    let err = encode_tiff(&page(w, h, EncodePixelFormat::Gray8 { pixels: &g }, bad))
        .unwrap_err()
        .to_string();
    assert!(err.contains("Table H.1"), "{err}");
    let ycc = vec![128u8; 16 * 16 * 3];
    let err = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &ycc,
            subsampling: (2, 2),
        },
        TiffCompression::Jpeg(JpegOptions {
            process: JpegProcess::Lossless { predictor: 1 },
            ..JpegOptions::default()
        }),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("non-subsampled"), "{err}");
    let big = vec![4096u16; 256];
    let err = encode_tiff(&page(
        w,
        h,
        EncodePixelFormat::Gray12 { pixels: &big },
        comp,
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("12-bit range"), "{err}");
}
