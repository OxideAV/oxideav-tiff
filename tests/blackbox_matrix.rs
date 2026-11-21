//! Live black-box matrix, direction "foreign → ours".
//!
//! Outside TIFF writers (`magick`, `tiffcp`, `raw2tiff`, `tiffset`,
//! `ffmpeg`, `sips` — executables only, run for their output) generate
//! a wide sweep of files in a temp directory: every compression they
//! can write, predictors, strip and tile geometries, chunky / separate
//! planes, bit depths and sample formats, photometrics, both byte
//! orders, classic + BigTIFF, multi-page, orientations. Each file is
//! decoded by this crate and by a black-box reader and the two results
//! are compared sample-exact (lossless layouts) or within the stated
//! tolerance (JPEG / YCbCr / CIELab, whose reconstruction TIFF 6.0
//! leaves to the reader).
//!
//! Nothing is committed: the whole matrix is generated at run time,
//! and each test SKIPS when its tools are absent. A file the writer
//! tool refuses to produce is not counted. The `KNOWN_GAPS` table is
//! the honest ledger of what this crate does not decode; a case that
//! fails without being on it fails the test, and so does a ledger
//! entry that starts passing (the ledger must then be shortened).

#![allow(clippy::too_many_arguments)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_tiff::ifd::{Entry, find, parse_header, parse_ifd};
use oxideav_tiff::{DecodeOptions, Page, TiffImage, TiffPixelFormat, decode_pages};

// ---------------------------------------------------------------------
// Tool plumbing
// ---------------------------------------------------------------------

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|_| true)
        .unwrap_or(false)
}

/// `true` when the matrix can run: every tool present, and the crate
/// built with its JPEG path (`registry`), which the sweeps exercise.
fn ready(tools: &[&str]) -> bool {
    if !cfg!(feature = "registry") {
        eprintln!("skip: the matrix needs the `registry` feature (JPEG-in-TIFF)");
        return false;
    }
    // The reference paths also lean on these three everywhere.
    let always = ["tiffinfo", "tiff2rgba", "tiffcrop"];
    if let Some(missing) = tools.iter().chain(&always).find(|t| !have(t)) {
        eprintln!("skip: `{missing}` is not installed");
        return false;
    }
    true
}

fn run(tool: &str, args: &[String]) -> Result<Vec<u8>, String> {
    let out = Command::new(tool)
        .args(args)
        .output()
        .map_err(|e| format!("{tool}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{tool} {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("")
        ));
    }
    Ok(out.stdout)
}

fn s(x: &str) -> String {
    x.to_string()
}

fn p(x: &Path) -> String {
    x.to_string_lossy().into_owned()
}

fn sv(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| x.to_string()).collect()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("oxideav-tiff-bbm-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("temp dir");
        Self(d)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------
// Deterministic source rasters
// ---------------------------------------------------------------------

/// One 16-bit sample of the test pattern: flat blocks (runs), a smooth
/// ramp and a noise band, so every coder sees literals, runs and
/// matches. Channels differ.
fn sample16(x: u32, y: u32, c: u32, w: u32, h: u32) -> u16 {
    let band = (y * 4) / h.max(1);
    match band {
        0 => (((x / 7) * 9000 + c * 21000) & 0xFFFF) as u16,
        1 => ((x * 65535 / w.max(1)) as u16).wrapping_add((c * 5000 + y * 37) as u16),
        2 => {
            let mut v = x
                .wrapping_mul(0x9E37_79B9)
                .wrapping_add(y.wrapping_mul(0x85EB_CA6B))
                .wrapping_add(c.wrapping_mul(0xC2B2_AE35));
            v ^= v >> 15;
            v = v.wrapping_mul(0x2C1B_3C6D);
            v ^= v >> 12;
            (v & 0xFFFF) as u16
        }
        _ => {
            if (x / 3 + y + c).is_multiple_of(2) {
                0xFFFF
            } else {
                0
            }
        }
    }
}

fn raster8(w: u32, h: u32, ch: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * ch) as usize);
    for y in 0..h {
        for x in 0..w {
            for c in 0..ch {
                v.push((sample16(x, y, c, w, h) >> 8) as u8);
            }
        }
    }
    v
}

fn raster16(w: u32, h: u32, ch: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * ch * 2) as usize);
    for y in 0..h {
        for x in 0..w {
            for c in 0..ch {
                v.extend_from_slice(&sample16(x, y, c, w, h).to_le_bytes());
            }
        }
    }
    v
}

/// Bilevel pattern as 8-bit gray (0 / 255): long runs, short runs, an
/// all-white band, a noise band and vertical structure for 2-D coding.
fn raster_bw(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            let band = (y * 5) / h.max(1);
            let on = match band {
                0 => (x / 9 + y / 2).is_multiple_of(2),
                1 => true,
                2 => sample16(x, y, 0, w, h) & 0x100 != 0,
                3 => x.is_multiple_of(2),
                _ => (x + y * 2) % 13 < 6,
            };
            v.push(if on { 255 } else { 0 });
        }
    }
    v
}

/// Low-frequency RGB(A) ramp: lossy and chroma-subsampled layouts are
/// compared on it, where two readers' reconstructions stay close.
fn raster_smooth(w: u32, h: u32, ch: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * ch) as usize);
    for y in 0..h {
        for x in 0..w {
            let px = [
                (x * 255 / (w - 1).max(1)) as u8,
                (y * 255 / (h - 1).max(1)) as u8,
                ((x + y) * 255 / (w + h - 2).max(1)) as u8,
                255,
            ];
            v.extend_from_slice(&px[..ch as usize]);
        }
    }
    v
}

#[derive(Clone)]
struct Src {
    /// `magick` arguments that load this raster.
    load: Vec<String>,
    /// Raw sample file (for `raw2tiff` / `ffmpeg`).
    raw: PathBuf,
}

/// `kind`: gray / rgb / rgba / graya / cmyk / cmyka / bw; `bits` 8 or 16.
fn make_src(dir: &TempDir, kind: &str, bits: u32, w: u32, h: u32) -> Src {
    let (ch, fmt) = match kind {
        "gray" | "bw" => (1, "gray"),
        "rgb" | "smooth" => (3, "rgb"),
        "rgba" | "smootha" => (4, "rgba"),
        "graya" => (2, "graya"),
        "cmyk" => (4, "cmyk"),
        "cmyka" => (5, "cmyka"),
        _ => unreachable!(),
    };
    let data = if kind == "bw" {
        raster_bw(w, h)
    } else if kind.starts_with("smooth") {
        raster_smooth(w, h, ch)
    } else if bits == 16 {
        raster16(w, h, ch)
    } else {
        raster8(w, h, ch)
    };
    let raw = dir.path(&format!("src-{kind}{bits}-{w}x{h}.raw"));
    if !raw.exists() {
        fs::write(&raw, data).expect("write src");
    }
    Src {
        load: vec![
            s("-size"),
            format!("{w}x{h}"),
            s("-depth"),
            bits.to_string(),
            s("-endian"),
            s("LSB"),
            format!("{fmt}:{}", p(&raw)),
        ],
        raw,
    }
}

// ---------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------

/// Largest per-sample difference (8-bit units) allowed between this
/// crate and a black-box reader on DCT-coded or chroma-subsampled data
/// of the smooth ramp: two conforming readers differ by the inverse-DCT
/// accuracy T.81 permits plus their own chroma upsampling filter
/// (TIFF 6.0 §21 names none).
const LOSSY_TOL: u32 = 12;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tol {
    /// Lossless layout: every sample equal.
    Exact,
    /// Reader-defined reconstruction (JPEG IDCT, YCbCr upsampling,
    /// CIELab rendering): largest per-sample difference allowed, in
    /// 8-bit units.
    Within(u32),
}

struct Case {
    name: String,
    /// `(tool, args)` run in order; the last one writes `out`.
    cmds: Vec<(String, Vec<String>)>,
    out: PathBuf,
    tol: Tol,
    /// Number of pages expected (0 = whatever the file holds).
    pages: usize,
}

enum Outcome {
    Pass,
    /// The writer tool refused the combination: not counted.
    NotWritten,
    Refused(String),
    Mismatch(String),
    RefFail(String),
    /// Decoded here, but no black-box reader on hand reads the file
    /// (each one refuses it): nothing to compare against.
    Unverified,
}

fn tiff_tag_u32(bytes: &[u8], tag: u16) -> Option<Vec<u32>> {
    let hdr = parse_header(bytes).ok()?;
    let (entries, _) = parse_ifd(bytes, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).ok()?;
    find(&entries, tag)?.as_u32_vec(hdr.byte_order).ok()
}

/// First-IFD facts used to describe a case in the report.
fn describe(bytes: &[u8]) -> String {
    let g = |t: u16| {
        tiff_tag_u32(bytes, t)
            .map(|v| {
                v.iter()
                    .take(5)
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_else(|| s("-"))
    };
    format!(
        "comp={} photo={} bps={} spp={} planar={} pred={} sf={} extra={} tiled={} fill={} sub={}",
        g(259),
        g(262),
        g(258),
        g(277),
        g(284),
        g(317),
        g(339),
        g(338),
        tiff_tag_u32(bytes, 322).is_some(),
        g(266),
        g(530),
    )
}

/// The reference reader's rendering of page `idx`: 8-bit RGBA, 16-bit
/// RGBA (little-endian) or 8-bit CMYK, matching our native layout.
fn magick_ref(
    path: &Path,
    idx: usize,
    fmt: &str,
    depth: u32,
    extra: &[&str],
) -> Result<Vec<u8>, String> {
    let args = |fmt: &str| {
        let mut a = vec![format!("{}[{idx}]", p(path)), s("-auto-orient")];
        a.extend(extra.iter().map(|x| x.to_string()));
        a.extend(sv(&["-depth", &depth.to_string(), "-endian", "LSB"]));
        a.push(format!("{fmt}:-"));
        a
    };
    match run("magick", &args(fmt)) {
        Ok(v) => Ok(v),
        // The tool refuses `RGBA` output for some alpha-less pages:
        // take `RGB` and add the opaque alpha.
        Err(_) if fmt == "RGBA" => {
            let rgb = run("magick", &args("RGB"))?;
            let n = (depth / 8) as usize;
            let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
            for px in rgb.chunks_exact(3 * n) {
                out.extend_from_slice(px);
                out.extend(std::iter::repeat_n(0xFF, n));
            }
            Ok(out)
        }
        Err(e) => Err(e),
    }
}

fn max_diff8(a: &[u8], b: &[u8]) -> (u32, usize) {
    let mut worst = 0u32;
    let mut at = 0usize;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let d = (*x as i32 - *y as i32).unsigned_abs();
        if d > worst {
            worst = d;
            at = i;
        }
    }
    (worst, at)
}

/// Uncompressed samples of page `idx` as a black-box reader hands them
/// back: `tiffcp -c none` (or `tiffcrop`) re-writes the page as
/// uncompressed chunky data; the whole-byte samples are then assembled
/// here straight from the offset / byte-count tags into row-major
/// little-endian order. Returns
/// `(samples, bits, spp, sample_format, photometric)`.
fn raw_ref(
    path: &Path,
    idx: usize,
    scratch: &Path,
) -> Result<(Vec<u8>, u32, u32, u32, u32), String> {
    let mut a = sv(&["-c", "none", "-L", "-p", "contig", "-s", "-r", "1000000"]);
    a.push(format!("{},{idx}", p(path)));
    a.push(p(scratch));
    if run("tiffcp", &a).is_err() {
        // `tiffcp` cannot merge separate planes at every sample width
        // (and corrupts separate-plane tiles wider than 8 bits when
        // asked to keep them); `tiffcrop` re-writes those as chunky
        // strips.
        let _ = fs::remove_file(scratch);
        let one = scratch.with_extension("page.tif");
        let src = if idx == 0 {
            path.to_path_buf()
        } else {
            run("tiffcp", &[format!("{},{idx}", p(path)), p(&one)])?;
            one.clone()
        };
        let r = run(
            "tiffcrop",
            &[
                s("-c"),
                s("none"),
                s("-s"),
                s("-p"),
                s("contig"),
                p(&src),
                p(scratch),
            ],
        );
        let _ = fs::remove_file(&one);
        r?;
    }
    let bytes = fs::read(scratch).map_err(|e| e.to_string())?;
    let hdr = parse_header(&bytes).map_err(|e| e.to_string())?;
    let bo = hdr.byte_order;
    let (entries, _) =
        parse_ifd(&bytes, bo, hdr.variant, hdr.first_ifd_offset).map_err(|e| e.to_string())?;
    let get = |t: u16, d: u32| -> u32 {
        find(&entries, t)
            .and_then(|e: &Entry| e.as_u32(bo).ok())
            .unwrap_or(d)
    };
    let list = |t: u16| -> Result<Vec<u64>, String> {
        find(&entries, t)
            .ok_or(format!("tag {t} missing"))?
            .as_u64_vec(bo)
            .map_err(|e| e.to_string())
    };
    let (w, h) = (get(256, 0) as usize, get(257, 0) as usize);
    let (bits, spp) = (get(258, 1), get(277, 1));
    // FillOrder = 2 output keeps every stored byte bit-reversed.
    let unfill = |b: &[u8]| -> Vec<u8> {
        if get(266, 1) == 2 {
            b.iter().map(|x| x.reverse_bits()).collect()
        } else {
            b.to_vec()
        }
    };
    if bits == 1 && spp == 1 && find(&entries, 322).is_none() {
        // Bilevel strips: one byte per pixel (0 / 1) out of the packed
        // rows.
        let (w, h) = (get(256, 0) as usize, get(257, 0) as usize);
        let row = w.div_ceil(8);
        let mut packed = Vec::new();
        for (o, c) in list(273)?.iter().zip(&list(279)?) {
            packed.extend_from_slice(&unfill(&bytes[*o as usize..(*o + *c) as usize]));
        }
        let mut data = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                data.push((packed[y * row + x / 8] >> (7 - x % 8)) & 1);
            }
        }
        return Ok((data, 8, 1, get(339, 1), get(262, 1)));
    }
    if bits % 8 != 0 {
        return Err(format!("raw reference at {bits} bits not modelled"));
    }
    let bytes_per = (bits / 8) as usize;
    let planar = get(284, 1) == 2 && spp > 1;
    let planes = if planar { spp as usize } else { 1 };
    let comps = if planar { 1 } else { spp as usize };
    let tiled = find(&entries, 322).is_some();
    let (seg_w, seg_h, offs, cnts) = if tiled {
        (
            get(322, 0) as usize,
            get(323, 0) as usize,
            list(324)?,
            list(325)?,
        )
    } else {
        (
            w,
            (get(278, u32::MAX) as usize).min(h),
            list(273)?,
            list(279)?,
        )
    };
    let (across, down) = (w.div_ceil(seg_w), h.div_ceil(seg_h));
    let px = spp as usize * bytes_per;
    let mut data = vec![0u8; w * h * px];
    for plane in 0..planes {
        for sy in 0..down {
            for sx in 0..across {
                let i = plane * across * down + sy * across + sx;
                let seg = unfill(&bytes[offs[i] as usize..(offs[i] + cnts[i]) as usize]);
                let rows = seg_h.min(h - sy * seg_h);
                let cols = seg_w.min(w - sx * seg_w);
                for r in 0..rows {
                    for x in 0..cols {
                        let src = (r * seg_w + x) * comps * bytes_per;
                        let dst = ((sy * seg_h + r) * w + sx * seg_w + x) * px + plane * bytes_per;
                        data[dst..dst + comps * bytes_per]
                            .copy_from_slice(&seg[src..src + comps * bytes_per]);
                    }
                }
            }
        }
    }
    if matches!(bo, oxideav_tiff::ifd::ByteOrder::Big) {
        for sample in data.chunks_exact_mut(bytes_per) {
            sample.reverse();
        }
    }
    Ok((data, bits, spp, get(339, 1), get(262, 1)))
}

fn f16_to_f32(h: u16) -> f32 {
    oxideav_tiff::f16_bits_to_f32(h)
}

/// Raw black-box samples of a page widened to `u32`, plus
/// `(bits, spp, sample_format, photometric)`.
fn raw_values(
    path: &Path,
    idx: usize,
    scratch: &Path,
) -> Result<(Vec<u32>, u32, u32, u32, u32), String> {
    let (raw, bits, spp, sf, photo) = raw_ref(path, idx, scratch)?;
    let vals = match bits {
        8 => raw.iter().map(|&b| b as u32).collect(),
        16 => raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]) as u32)
            .collect(),
        32 => raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        // 64-bit samples: the top 32 bits (what the sample matrix of
        // the crate keeps too).
        64 => raw
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| (u64::from_le_bytes(*c) >> 32) as u32)
            .collect(),
        b => return Err(format!("raw reference at {b} bits not modelled")),
    };
    Ok((vals, bits.min(32), spp, sf, photo))
}

/// Our plane as one integer per sample (8- or 16-bit layouts).
fn our_values(img: &TiffImage) -> Vec<u32> {
    let d = img.as_bytes().unwrap_or(&[]);
    if img.format.bits_per_sample() == 16 {
        d.as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]) as u32)
            .collect()
    } else {
        d.iter().map(|&b| b as u32).collect()
    }
}

/// Compare one decoded page with the black-box reader(s).
fn check_page(
    path: &Path,
    idx: usize,
    page: &Page,
    tol: Tol,
    scratch: &Path,
) -> Result<(), String> {
    use TiffPixelFormat as F;
    let img: &TiffImage = &page.image;
    let (w, h) = (img.width as usize, img.height as usize);
    let mut allow = match tol {
        Tol::Exact => 0,
        Tol::Within(t) => t,
    };
    let lay = &page.layout;
    let bits = lay.bits_per_sample.first().copied().unwrap_or(1) as u32;
    let photo = lay.photometric.unwrap_or(1);
    let lab = photo == 8;
    let lab_args: &[&str] = if lab { &["-colorspace", "sRGB"] } else { &[] };
    let signed = lay.sample_format.first() == Some(&2);
    let depth = img.format.bits_per_sample() as u32;
    // A sample narrower or wider than the layout that carries it is
    // rescaled by the reader, and TIFF 6.0 fixes no rescaling rule
    // (this crate replicates bits / keeps the top bits, the reference
    // rounds): one step of the output unit is allowed there. The same
    // holds for the 16-bit ColorMap words of a palette page reduced
    // to 8 bits (two black-box readers differ by one there).
    let exact_width = matches!((bits, depth), (1 | 2 | 4 | 8, 8) | (16, 16));
    if !exact_width || photo == 3 {
        allow = allow.max(1);
    }
    let associated = lay.samples_per_pixel > 1 && extra_kind(path, idx) == Some(1);
    if associated {
        // Dividing the alpha back out rounds.
        allow = allow.max(1);
    }

    if photo == 6 && lay.compression != Some(7) && lay.compression != Some(6) && idx == 0 {
        // Byte-stream-coded YCbCr: the primary reference reader does
        // not interpret these pages (it hands back the stored samples
        // unconverted), so the `tiff2rgba` rendering is the reference.
        return rgba_reader(path, idx, page, Tol::Within(allow.max(4)), scratch)
            .map_err(|e| format!("YCbCr vs tiff2rgba: {e}"));
    }
    if signed && !img.format.is_float() {
        // Signed integers: the black-box raw samples through the
        // documented offset-binary map (sign bit flipped, then the
        // unsigned rules).
        let (raw, rbits, rspp, _, rphoto) = raw_values(path, idx, scratch)?;
        let ours = our_values(img);
        let ch = img.format.bytes_per_pixel() / (depth as usize / 8);
        let rspp = rspp as usize;
        if raw.len() / rspp != ours.len() / ch || ch > rspp {
            return Err(format!(
                "signed: {} pixels of {ch}, reference {} of {rspp}",
                ours.len() / ch,
                raw.len() / rspp
            ));
        }
        let max = (1u32 << depth) - 1;
        // Colour samples only (extras are dropped or kept as alpha by
        // the unsigned rules, checked elsewhere).
        let colour = if img.format.has_alpha() { ch - 1 } else { ch };
        for (i, (o, r)) in ours
            .chunks_exact(ch)
            .zip(raw.chunks_exact(rspp))
            .enumerate()
        {
            for c in 0..colour {
                let u = r[c] ^ (1u32 << (rbits - 1));
                let u = if rbits > depth {
                    u >> (rbits - depth)
                } else {
                    u
                };
                let e = if rphoto == 0 { max - u } else { u };
                if o[c] != e {
                    return Err(format!(
                        "signed pixel {i} channel {c}: ours {} expected {e}",
                        o[c]
                    ));
                }
            }
        }
        return Ok(());
    }

    if photo == 4 {
        // Transparency mask: "the 1-bits define the interior" — this
        // crate hands interior back as 0xFF. Raw bits are the reference.
        let (raw, _, _, _, _) = raw_values(path, idx, scratch)?;
        let ours = our_values(img);
        if raw.len() != ours.len() {
            return Err(format!(
                "mask: {} pixels, reference {}",
                ours.len(),
                raw.len()
            ));
        }
        if let Some(i) = ours.iter().zip(&raw).position(|(o, r)| *o != r * 255) {
            return Err(format!(
                "mask pixel {i}: ours {} raw bit {}",
                ours[i], raw[i]
            ));
        }
        return Ok(());
    }

    match img.format {
        F::Gray8 | F::Rgb24 | F::Rgba | F::Pal8 | F::Ya8 => {
            let reference = match magick_ref(path, idx, "RGBA", 8, lab_args) {
                Ok(r) => r,
                // Second black-box reader for a plain 8-bit RGB page.
                Err(e) if photo == 2 && bits == 8 && img.format == F::Rgb24 => {
                    let (raw, _, spp, _, _) = raw_values(path, idx, scratch).map_err(|_| e)?;
                    raw.chunks_exact(spp as usize)
                        .flat_map(|px| [px[0] as u8, px[1] as u8, px[2] as u8, 255])
                        .collect()
                }
                Err(e) => return Err(e),
            };
            let ours = img.to_rgba8();
            if reference.len() != w * h * 4 {
                return Err(format!(
                    "geometry: ours {w}x{h}, reference has {} bytes",
                    reference.len()
                ));
            }
            for (i, (o, r)) in ours
                .as_chunks::<4>()
                .0
                .iter()
                .zip(reference.as_chunks::<4>().0.iter())
                .enumerate()
            {
                // A pre-multiplied colour under zero alpha has no
                // straight value.
                let skip_colour = associated && o[3] == 0 && r[3] == 0;
                for c in 0..4 {
                    if skip_colour && c < 3 {
                        continue;
                    }
                    let d = (o[c] as i32 - r[c] as i32).unsigned_abs();
                    if d > allow {
                        return Err(format!(
                            "8-bit RGBA: diff {d} at pixel {i} channel {c}: ours {} ref {}",
                            o[c], r[c]
                        ));
                    }
                }
            }
        }
        F::Cmyk => {
            let ours = img.as_bytes().ok_or("no plane")?;
            let reference = match magick_ref(path, idx, "CMYK", 8, &[]) {
                Ok(r) => r,
                Err(_) => {
                    // Second black-box reader: raw samples.
                    let (raw, rbits, spp, _, _) = raw_values(path, idx, scratch)?;
                    raw.chunks_exact(spp as usize)
                        .flat_map(|px| px[..4].iter().map(move |&v| (v >> (rbits - 8)) as u8))
                        .collect()
                }
            };
            if reference.len() != ours.len() {
                return Err(format!(
                    "geometry: ours {} bytes, reference {}",
                    ours.len(),
                    reference.len()
                ));
            }
            let (d, at) = max_diff8(ours, &reference);
            if d > allow {
                return Err(format!("CMYK: max diff {d} at byte {at}"));
            }
        }
        F::Gray16Le | F::Rgb48Le | F::Ya16Le | F::Rgba64Le => {
            let ours_raw = img.as_bytes().ok_or("no plane")?;
            let reference = magick_ref(path, idx, "RGBA", 16, lab_args)?;
            if reference.len() != w * h * 8 {
                return Err(format!(
                    "geometry: ours {w}x{h}, reference has {} bytes",
                    reference.len()
                ));
            }
            let mut ours = Vec::with_capacity(w * h * 8);
            match img.format {
                F::Gray16Le => {
                    for g in ours_raw.as_chunks::<2>().0.iter() {
                        ours.extend_from_slice(&[g[0], g[1], g[0], g[1], g[0], g[1], 0xFF, 0xFF]);
                    }
                }
                F::Ya16Le => {
                    for g in ours_raw.as_chunks::<4>().0.iter() {
                        ours.extend_from_slice(&[g[0], g[1], g[0], g[1], g[0], g[1], g[2], g[3]]);
                    }
                }
                F::Rgb48Le => {
                    for px in ours_raw.as_chunks::<6>().0.iter() {
                        ours.extend_from_slice(px);
                        ours.extend_from_slice(&[0xFF, 0xFF]);
                    }
                }
                _ => ours.extend_from_slice(ours_raw),
            }
            let allow16 = if tol == Tol::Exact {
                allow
            } else {
                allow * 257
            };
            for (i, (o, r)) in ours
                .as_chunks::<8>()
                .0
                .iter()
                .zip(reference.as_chunks::<8>().0.iter())
                .enumerate()
            {
                let skip_colour = associated && o[6..8] == [0, 0] && r[6..8] == [0, 0];
                for c in 0..4 {
                    if skip_colour && c < 3 {
                        continue;
                    }
                    let ov = u16::from_le_bytes([o[c * 2], o[c * 2 + 1]]) as i32;
                    let rv = u16::from_le_bytes([r[c * 2], r[c * 2 + 1]]) as i32;
                    let d = (ov - rv).unsigned_abs();
                    if d > allow16 {
                        return Err(format!(
                            "16-bit RGBA: diff {d} at pixel {i} channel {c}: ours {ov} ref {rv}"
                        ));
                    }
                }
            }
        }
        F::GrayF32Le | F::RgbF32Le | F::RgbaF32Le => {
            let (raw, rbits, spp, sf, rphoto) = raw_ref(path, idx, scratch)?;
            if sf != 3 {
                return Err(format!("float output for SampleFormat {sf}"));
            }
            let vals: Vec<f32> = match rbits {
                16 => raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| f16_to_f32(u16::from_le_bytes([c[0], c[1]])))
                    .collect(),
                32 => raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
                64 => raw
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| f64::from_le_bytes(*c) as f32)
                    .collect(),
                b => return Err(format!("float width {b}")),
            };
            let ours: Vec<f32> = img
                .as_bytes()
                .ok_or("no plane")?
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let ch = img.format.bytes_per_pixel() / 4;
            if ours.len() / ch != vals.len() / spp as usize {
                return Err(format!(
                    "float count: ours {} reference {}",
                    ours.len(),
                    vals.len()
                ));
            }
            for (i, (o, r)) in ours
                .chunks_exact(ch)
                .zip(vals.chunks_exact(spp as usize))
                .enumerate()
            {
                for c in 0..ch {
                    // Associated alpha: the colour is divided back out.
                    let stored = if associated && c < 3 && ch == 4 {
                        if r[3] > 0.0 { r[c] / r[3] } else { 0.0 }
                    } else {
                        r[c]
                    };
                    let b = if rphoto == 0 { 1.0 - stored } else { stored };
                    if o[c].to_bits() != b.to_bits() && !(o[c].is_nan() && b.is_nan()) {
                        return Err(format!(
                            "float pixel {i} channel {c}: ours {} reference {b}",
                            o[c]
                        ));
                    }
                }
            }
        }
        other => return Err(format!("layout {other:?} has no comparison rule")),
    }
    Ok(())
}

/// First `ExtraSamples` value of page `idx`.
fn extra_kind(path: &Path, idx: usize) -> Option<u32> {
    let bytes = fs::read(path).ok()?;
    let hdr = parse_header(&bytes).ok()?;
    let mut off = hdr.first_ifd_offset;
    for _ in 0..idx {
        off = parse_ifd(&bytes, hdr.byte_order, hdr.variant, off).ok()?.1;
    }
    let (entries, _) = parse_ifd(&bytes, hdr.byte_order, hdr.variant, off).ok()?;
    find(&entries, 338)?.as_u32(hdr.byte_order).ok()
}

/// Third opinion for a first-directory 8-bit page: the `tiff2rgba`
/// rendering (opaque pages only), within the case tolerance.
fn rgba_reader(
    path: &Path,
    idx: usize,
    page: &Page,
    tol: Tol,
    scratch: &Path,
) -> Result<(), String> {
    use TiffPixelFormat as F;
    if idx != 0 || !matches!(page.image.format, F::Gray8 | F::Rgb24 | F::Pal8 | F::Cmyk) {
        return Err(s("not applicable"));
    }
    let rgba = scratch.with_extension("rgba3.tif");
    run("tiff2rgba", &[s("-c"), s("none"), p(path), p(&rgba)])?;
    let r = raw_values(&rgba, 0, scratch);
    let _ = fs::remove_file(&rgba);
    let (raw, _, spp, _, _) = r?;
    let ours = page.image.to_rgba8();
    if spp != 4 || raw.len() != ours.len() {
        return Err(s("geometry"));
    }
    let allow = match tol {
        Tol::Exact => 0,
        Tol::Within(t) => t,
    };
    for (i, (o, r)) in ours.iter().zip(&raw).enumerate() {
        let d = (*o as i32 - *r as i32).unsigned_abs();
        if d > allow {
            return Err(format!("diff {d} at byte {i}: ours {o} theirs {r}"));
        }
    }
    Ok(())
}

fn run_case(c: &Case, scratch: &Path) -> (Outcome, String) {
    for (tool, args) in &c.cmds {
        if run(tool, args).is_err() {
            return (Outcome::NotWritten, String::new());
        }
    }
    let Ok(bytes) = fs::read(&c.out) else {
        return (Outcome::NotWritten, String::new());
    };
    let desc = describe(&bytes);
    let pages = match decode_pages(&bytes) {
        Ok(pg) => pg,
        Err(e) => {
            // A file the black-box reader refuses as well is not a
            // decodable TIFF (the writer produced something its own
            // reader rejects): refusing it is the right answer.
            if magick_ref(&c.out, 0, "RGB", 8, &[]).is_err() {
                let _ = fs::remove_file(&c.out);
                return (Outcome::Pass, desc);
            }
            return (Outcome::Refused(e.to_string()), desc);
        }
    };
    // A granted thread budget must hand back the very same pages.
    match oxideav_tiff::decode_pages_with(&bytes, &DecodeOptions::default().with_threads(3)) {
        Ok(threaded) if threaded == pages => {}
        Ok(_) => {
            return (
                Outcome::Mismatch(s("3-thread decode differs from serial")),
                desc,
            );
        }
        Err(e) => {
            return (
                Outcome::Mismatch(format!("3-thread decode failed: {e}")),
                desc,
            );
        }
    }
    // The header-only probe must describe what decode returns.
    match oxideav_tiff::info(&bytes) {
        Ok(i) => {
            let first = &pages[0].image;
            if (i.width, i.height, i.format) != (first.width, first.height, first.format)
                || i.frames as usize != pages.len()
            {
                return (
                    Outcome::Mismatch(format!(
                        "info() says {}x{} {:?} ({} pages), decode {}x{} {:?} ({} pages)",
                        i.width,
                        i.height,
                        i.format,
                        i.frames,
                        first.width,
                        first.height,
                        first.format,
                        pages.len()
                    )),
                    desc,
                );
            }
        }
        Err(e) => return (Outcome::Mismatch(format!("info() failed: {e}")), desc),
    }
    if c.pages != 0 && pages.len() != c.pages {
        return (
            Outcome::Mismatch(format!(
                "{} pages decoded, {} expected",
                pages.len(),
                c.pages
            )),
            desc,
        );
    }
    for (i, page) in pages.iter().enumerate() {
        if let Err(e) = check_page(&c.out, i, page, c.tol, scratch) {
            // The primary reference reader mis-reads or refuses a few
            // layouts (separate gray + alpha planes, separate-plane
            // JPEG): a second black-box reader that agrees with this
            // crate settles those.
            if matches!(second_reader(&c.out, i, page, scratch), Ok(true)) {
                continue;
            }
            let third = rgba_reader(&c.out, i, page, c.tol, scratch);
            if third.is_ok() {
                continue;
            }
            let tool_refused = |m: &str| m.starts_with("magick") || m.starts_with("tiff");
            if tool_refused(&e) && third.as_ref().is_err_and(|m| tool_refused(m)) {
                return (Outcome::Unverified, desc);
            }
            let o = if e.starts_with("magick") || e.starts_with("tiffcp") {
                Outcome::RefFail(e)
            } else {
                Outcome::Mismatch(format!("page {i}: {e}"))
            };
            return (o, desc);
        }
    }
    let _ = fs::remove_file(&c.out);
    (Outcome::Pass, desc)
}

/// Known decode gaps: `(needles, excluded, reason)`. A case whose
/// `name [facts]` line contains every needle and no excluded string is
/// expected to be REFUSED with an error (never mis-decoded).
const KNOWN_GAPS: &[(&[&str], &[&str], &str)] = &[(
    &["comp=34925"],
    &[],
    "Compression 34925 (LZMA2): no description of the code or its stream framing is staged",
)];

fn known_gap(line: &str) -> Option<&'static str> {
    KNOWN_GAPS
        .iter()
        .find(|(need, not, _)| {
            need.iter().all(|n| line.contains(n)) && !not.iter().any(|n| line.contains(n))
        })
        .map(|g| g.2)
}

struct Tally {
    written: usize,
    passed: usize,
    known: usize,
    unverified: usize,
}

fn run_matrix(tag: &str, dir: &TempDir, cases: Vec<Case>) -> Tally {
    let next = AtomicUsize::new(0);
    let results: Mutex<BTreeMap<usize, (Outcome, String)>> = Mutex::new(BTreeMap::new());
    std::thread::scope(|sc| {
        for t in 0..4 {
            let (next, results, cases) = (&next, &results, &cases);
            let scratch = dir.path(&format!("ref-{tag}-{t}.tif"));
            sc.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= cases.len() {
                        break;
                    }
                    let r = run_case(&cases[i], &scratch);
                    results.lock().unwrap().insert(i, r);
                }
            });
        }
    });
    let results = results.into_inner().unwrap();
    let mut tally = Tally {
        written: 0,
        passed: 0,
        known: 0,
        unverified: 0,
    };
    let mut bad = Vec::new();
    let mut stale = Vec::new();
    for (i, (o, desc)) in &results {
        let name = &cases[*i].name;
        let gap = known_gap(&format!("{name} [{desc}]"));
        match o {
            Outcome::NotWritten => {}
            Outcome::Unverified => {
                tally.written += 1;
                tally.unverified += 1;
            }
            Outcome::Pass => {
                tally.written += 1;
                tally.passed += 1;
                if gap.is_some() {
                    stale.push(name.clone());
                }
            }
            Outcome::Refused(_) if gap.is_some() => {
                tally.written += 1;
                tally.known += 1;
            }
            Outcome::Refused(e) => {
                tally.written += 1;
                bad.push(format!("{name}: refused: {e}  [{desc}]"));
            }
            Outcome::Mismatch(e) => {
                tally.written += 1;
                bad.push(format!("{name}: mismatch: {e}  [{desc}]"));
            }
            Outcome::RefFail(e) => {
                tally.written += 1;
                bad.push(format!("{name}: reference reader failed: {e}  [{desc}]"));
            }
        }
    }
    eprintln!(
        "blackbox matrix [{tag}]: {} files written, {} pass, {} known gaps, {} read by no \
         black-box reader, {} failing",
        tally.written,
        tally.passed,
        tally.known,
        tally.unverified,
        bad.len()
    );
    for b in &bad {
        eprintln!("  FAIL {b}");
    }
    for st in &stale {
        eprintln!("  STALE ledger entry now passes: {st}");
    }
    assert!(
        bad.is_empty() && stale.is_empty(),
        "{} failing / {} stale cases in [{tag}]",
        bad.len(),
        stale.len()
    );
    tally
}

// ---------------------------------------------------------------------
// magick-written matrix
// ---------------------------------------------------------------------

struct Kind {
    tag: &'static str,
    src: (&'static str, u32),
    args: &'static [&'static str],
    spp: u32,
    tol: Tol,
    core: bool,
}

const KINDS: &[Kind] = &[
    Kind {
        tag: "gray8",
        src: ("gray", 8),
        args: &[],
        spp: 1,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "gray8w",
        src: ("gray", 8),
        args: &["-define", "quantum:polarity=min-is-white"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "bilevel",
        src: ("bw", 8),
        args: &["-type", "Bilevel"],
        spp: 1,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "bilevelw",
        src: ("bw", 8),
        args: &[
            "-type",
            "Bilevel",
            "-define",
            "quantum:polarity=min-is-white",
        ],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray2",
        src: ("gray", 8),
        args: &["-depth", "2"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray4",
        src: ("gray", 8),
        args: &["-depth", "4"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray12",
        src: ("gray", 16),
        args: &["-depth", "12"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray16",
        src: ("gray", 16),
        args: &[],
        spp: 1,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "gray32",
        src: ("gray", 16),
        args: &["-depth", "32"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb8",
        src: ("rgb", 8),
        args: &[],
        spp: 3,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "rgb8s",
        src: ("smooth", 8),
        args: &[],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb2",
        src: ("rgb", 8),
        args: &["-depth", "2"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb4",
        src: ("rgb", 8),
        args: &["-depth", "4"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb12",
        src: ("rgb", 16),
        args: &["-depth", "12"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb16",
        src: ("rgb", 16),
        args: &[],
        spp: 3,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "rgb32",
        src: ("rgb", 16),
        args: &["-depth", "32"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "pal8",
        src: ("rgb", 8),
        args: &["-colors", "256", "-type", "Palette"],
        spp: 1,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "pal4",
        src: ("rgb", 8),
        args: &["-colors", "16", "-type", "Palette"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "pal2",
        src: ("rgb", 8),
        args: &["-colors", "4", "-type", "Palette"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "pal1",
        src: ("rgb", 8),
        args: &["-colors", "2", "-type", "Palette"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "graya8",
        src: ("graya", 8),
        args: &[],
        spp: 2,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "graya16",
        src: ("graya", 16),
        args: &[],
        spp: 2,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgba8",
        src: ("rgba", 8),
        args: &[],
        spp: 4,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "rgba8assoc",
        src: ("rgba", 8),
        args: &["-define", "tiff:alpha=associated"],
        spp: 4,
        tol: Tol::Within(1),
        core: false,
    },
    Kind {
        tag: "rgba8unspec",
        src: ("rgba", 8),
        args: &["-define", "tiff:alpha=unspecified"],
        spp: 4,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgba16",
        src: ("rgba", 16),
        args: &[],
        spp: 4,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "cmyk8",
        src: ("cmyk", 8),
        args: &[],
        spp: 4,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "cmyk16",
        src: ("cmyk", 16),
        args: &[],
        spp: 4,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "cmyka8",
        src: ("cmyka", 8),
        args: &[],
        spp: 5,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "ycbcr8",
        src: ("rgb", 8),
        args: &["-colorspace", "YCbCr"],
        spp: 3,
        tol: Tol::Within(2),
        core: false,
    },
    // CIELab → RGB is the reader's conversion (TIFF 6.0 section 23:
    // "some conversion to RGB will be required"); two black-box readers
    // differ from each other by up to 42 on this raster's saturated
    // colours, which bounds what can be pinned.
    Kind {
        tag: "lab8",
        src: ("rgb", 8),
        args: &["-colorspace", "Lab"],
        spp: 3,
        tol: Tol::Within(42),
        core: false,
    },
    Kind {
        tag: "lab16",
        src: ("rgb", 16),
        args: &["-colorspace", "Lab"],
        spp: 3,
        tol: Tol::Within(42),
        core: false,
    },
    Kind {
        tag: "grayf16",
        src: ("gray", 16),
        args: &["-define", "quantum:format=floating-point", "-depth", "16"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "grayf32",
        src: ("gray", 16),
        args: &["-define", "quantum:format=floating-point", "-depth", "32"],
        spp: 1,
        tol: Tol::Exact,
        core: true,
    },
    Kind {
        tag: "grayf64",
        src: ("gray", 16),
        args: &["-define", "quantum:format=floating-point", "-depth", "64"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgbf16",
        src: ("rgb", 16),
        args: &["-define", "quantum:format=floating-point", "-depth", "16"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgbf32",
        src: ("rgb", 16),
        args: &["-define", "quantum:format=floating-point", "-depth", "32"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgbf64",
        src: ("rgb", 16),
        args: &["-define", "quantum:format=floating-point", "-depth", "64"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgbaf32",
        src: ("rgba", 16),
        args: &["-define", "quantum:format=floating-point", "-depth", "32"],
        spp: 4,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "grays8",
        src: ("gray", 8),
        args: &["-define", "quantum:format=signed"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "grays16",
        src: ("gray", 16),
        args: &["-define", "quantum:format=signed"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "grays32",
        src: ("gray", 16),
        args: &["-define", "quantum:format=signed", "-depth", "32"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgbs8",
        src: ("rgb", 8),
        args: &["-define", "quantum:format=signed"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgbs16",
        src: ("rgb", 16),
        args: &["-define", "quantum:format=signed"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    // Every other sample width the writer takes (bit-packed below 8
    // and between 8 and 16, bit strings or whole bytes above). 24 bits
    // is left out: TIFF 6.0 states the byte order for 16- and 32-bit
    // integers only, and this writer stores a 24-bit sample in the
    // byte order opposite to the file's (0xF618F5 goes to disk as
    // F5 18 F6 under `MM` and F6 18 F5 under `II`), so its files are
    // no reference for the plain reading (file byte order) this crate
    // applies; the 24-bit unpacker is pinned by the pack / unpack
    // round trip in `generic`.
    Kind {
        tag: "gray3",
        src: ("gray", 16),
        args: &["-depth", "3"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray5",
        src: ("gray", 16),
        args: &["-depth", "5"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray6",
        src: ("gray", 16),
        args: &["-depth", "6"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray7",
        src: ("gray", 16),
        args: &["-depth", "7"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray9",
        src: ("gray", 16),
        args: &["-depth", "9"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray10",
        src: ("gray", 16),
        args: &["-depth", "10"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray11",
        src: ("gray", 16),
        args: &["-depth", "11"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray14",
        src: ("gray", 16),
        args: &["-depth", "14"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray15",
        src: ("gray", 16),
        args: &["-depth", "15"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray31",
        src: ("gray", 16),
        args: &["-depth", "31"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "gray64",
        src: ("gray", 16),
        args: &["-depth", "64"],
        spp: 1,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb3",
        src: ("rgb", 16),
        args: &["-depth", "3"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb5",
        src: ("rgb", 16),
        args: &["-depth", "5"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb10",
        src: ("rgb", 16),
        args: &["-depth", "10"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb14",
        src: ("rgb", 16),
        args: &["-depth", "14"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
    Kind {
        tag: "rgb64",
        src: ("rgb", 16),
        args: &["-depth", "64"],
        spp: 3,
        tol: Tol::Exact,
        core: false,
    },
];

const LAYOUTS: &[(&str, &[&str])] = &[
    ("strip", &[]),
    ("rps1", &["-define", "tiff:rows-per-strip=1"]),
    ("rps7", &["-define", "tiff:rows-per-strip=7"]),
    ("tile16", &["-define", "tiff:tile-geometry=16x16"]),
    ("tile32x48", &["-define", "tiff:tile-geometry=32x48"]),
];

fn magick_case(
    dir: &TempDir,
    name: String,
    src: &Src,
    kind: &Kind,
    extra: &[&str],
    tol: Tol,
    bigtiff: bool,
) -> Case {
    let out = dir.path(&format!("{name}.tif"));
    let mut a = src.load.clone();
    a.extend(kind.args.iter().map(|x| x.to_string()));
    a.extend(extra.iter().map(|x| x.to_string()));
    a.push(if bigtiff {
        format!("tiff64:{}", p(&out))
    } else {
        p(&out)
    });
    Case {
        name,
        cmds: vec![(s("magick"), a)],
        out,
        tol,
        pages: 1,
    }
}

/// Which `-compress` values make sense for a kind (others fall back
/// to a different scheme inside the writer and only repeat a case).
fn compressions(kind: &Kind) -> Vec<(&'static str, Tol)> {
    let mut v = vec![
        ("None", kind.tol),
        ("RLE", kind.tol),
        ("LZW", kind.tol),
        ("Zip", kind.tol),
        ("Zstd", kind.tol),
        ("LZMA", kind.tol),
    ];
    if matches!(kind.tag, "rgb8" | "rgba8") {
        // Lossy VP8: the YUV → RGB reconstruction is the reader's, so
        // only geometry is pinned; `WebPL` (quality 100) is lossless.
        v.push(("WebP", Tol::Within(255)));
        v.push(("WebPL", kind.tol));
    }
    match kind.tag {
        "gray8" | "rgb8" | "cmyk8" | "ycbcr8" => v.push(("JPEG", Tol::Within(8))),
        _ => {}
    }
    if kind.tag.starts_with("bilevel") {
        v.push(("Fax", kind.tol));
        v.push(("Group4", kind.tol));
    }
    v
}

fn magick_cases(dir: &TempDir) -> Vec<Case> {
    let (w, h) = (67, 41);
    let mut out = Vec::new();
    for kind in KINDS {
        let src = make_src(dir, kind.src.0, kind.src.1, w, h);
        let is_float = kind.args.contains(&"quantum:format=floating-point");
        for (comp, tol) in compressions(kind) {
            let layouts: Vec<_> = if kind.core {
                LAYOUTS.to_vec()
            } else {
                vec![LAYOUTS[0], LAYOUTS[3]]
            };
            if !kind.core && !matches!(comp, "None" | "LZW" | "Zip" | "JPEG") {
                continue;
            }
            let (comp_name, comp) = match comp {
                "WebPL" => ("webpl", "WebP"),
                c => (c, c),
            };
            for (lname, largs) in &layouts {
                for planar in [false, true] {
                    // (The writer tags its plane-separated CIELab data
                    // as chunky — not a file its tags describe.)
                    if planar && (kind.spp == 1 || kind.tag.starts_with("lab")) {
                        continue;
                    }
                    let preds: &[u32] = match comp {
                        "LZW" | "Zip" | "Zstd" | "LZMA" if is_float => &[1, 2, 3],
                        "LZW" | "Zip" | "Zstd" | "LZMA" => &[1, 2],
                        _ => &[1],
                    };
                    for pred in preds {
                        // Byte order alternates across the sweep (both
                        // orders for the core kinds' strip layout).
                        // (`tiff:endian` selects the file's byte order;
                        // the writer's default is big-endian.)
                        let endians: &[&str] = if kind.core && *lname == "strip" {
                            &["tiff:endian=lsb", "tiff:endian=msb"]
                        } else if out.len() % 2 == 0 {
                            &["tiff:endian=lsb"]
                        } else {
                            &["tiff:endian=msb"]
                        };
                        for endian in endians {
                            let name = format!(
                                "magick-{}-{}-{}{}{}-{}",
                                kind.tag,
                                comp_name.to_lowercase(),
                                lname,
                                if planar { "-planar" } else { "" },
                                if *pred > 1 {
                                    format!("-pred{pred}")
                                } else {
                                    String::new()
                                },
                                &endian[endian.len() - 3..]
                            );
                            let pdef = format!("tiff:predictor={pred}");
                            let mut extra: Vec<&str> = vec!["-compress", comp, "-define", endian];
                            if comp_name == "webpl" {
                                extra.extend_from_slice(&["-quality", "100"]);
                            }
                            extra.extend_from_slice(largs);
                            if planar {
                                extra.extend_from_slice(&["-interlace", "Plane"]);
                            }
                            if *pred > 1 {
                                extra.extend_from_slice(&["-define", &pdef]);
                            }
                            out.push(magick_case(dir, name, &src, kind, &extra, tol, false));
                        }
                    }
                }
            }
        }
        // Big-endian + BigTIFF + bit-order variants of every kind.
        for (vname, vargs, big) in [
            (
                "lsb-lzw2",
                &[
                    "-define",
                    "tiff:endian=lsb",
                    "-compress",
                    "LZW",
                    "-define",
                    "tiff:predictor=2",
                ][..],
                false,
            ),
            (
                "big-lsb-tile",
                &[
                    "-define",
                    "tiff:endian=lsb",
                    "-define",
                    "tiff:tile-geometry=16x16",
                ][..],
                true,
            ),
            (
                "msb-lzw2",
                &[
                    "-define",
                    "tiff:endian=msb",
                    "-compress",
                    "LZW",
                    "-define",
                    "tiff:predictor=2",
                ][..],
                false,
            ),
            (
                "msb-tile",
                &[
                    "-define",
                    "tiff:endian=msb",
                    "-compress",
                    "Zip",
                    "-define",
                    "tiff:tile-geometry=16x16",
                ][..],
                false,
            ),
            (
                "big-lsb",
                &["-define", "tiff:endian=lsb", "-compress", "LZW"][..],
                true,
            ),
            (
                "big-msb-tile",
                &[
                    "-define",
                    "tiff:endian=msb",
                    "-define",
                    "tiff:tile-geometry=16x16",
                ][..],
                true,
            ),
            ("fill-lsb", &["-define", "tiff:fill-order=lsb"][..], false),
        ] {
            let name = format!("magick-{}-{vname}", kind.tag);
            out.push(magick_case(dir, name, &src, kind, vargs, kind.tol, big));
        }
    }
    // JPEG sampling / quality sweep.
    for tag in ["rgb8", "gray8"] {
        let kind = KINDS.iter().find(|k| k.tag == tag).unwrap();
        // Chroma-subsampled layouts are compared on the smooth ramp.
        let src = make_src(
            dir,
            if tag == "rgb8" { "smooth" } else { kind.src.0 },
            8,
            w,
            h,
        );
        for samp in ["1x1", "2x1", "2x2", "1x2", "4x1", "4x2"] {
            for q in ["30", "75", "100"] {
                for (lname, largs) in LAYOUTS {
                    let name = format!("magick-{tag}-jpeg-{samp}-q{q}-{lname}");
                    let mut extra =
                        vec!["-compress", "JPEG", "-quality", q, "-sampling-factor", samp];
                    extra.extend_from_slice(largs);
                    // T.81 bounds the accuracy of the inverse DCT, it
                    // does not fix one result, and TIFF 6.0 §21 leaves
                    // the chroma upsampling filter to the reader.
                    let tol = Tol::Within(LOSSY_TOL);
                    out.push(magick_case(dir, name, &src, kind, &extra, tol, false));
                }
            }
        }
    }
    // Sizes: degenerate and large.
    for (sw, sh) in [
        (1u32, 1u32),
        (1, 70),
        (70, 1),
        (257, 3),
        (16, 16),
        (640, 400),
    ] {
        for tag in ["gray8", "bilevel", "gray16", "rgb8", "pal8"] {
            let kind = KINDS.iter().find(|k| k.tag == tag).unwrap();
            // JPEG (2 × 2 chroma for RGB) is compared on the smooth ramp.
            let smooth = make_src(dir, "smooth", 8, sw, sh);
            let plain = make_src(dir, kind.src.0, kind.src.1, sw, sh);
            for comp in ["None", "RLE", "LZW", "Zip", "Group4", "Fax", "JPEG"] {
                if matches!(comp, "Group4" | "Fax") != (tag == "bilevel")
                    && comp != "None"
                    && comp != "LZW"
                {
                    continue;
                }
                if comp == "JPEG" && !matches!(tag, "gray8" | "rgb8") {
                    continue;
                }
                let (src, tol) = if comp == "JPEG" {
                    (
                        if tag == "rgb8" { &smooth } else { &plain },
                        Tol::Within(LOSSY_TOL),
                    )
                } else {
                    (&plain, Tol::Exact)
                };
                for (lname, largs) in [LAYOUTS[0], LAYOUTS[3]] {
                    let name = format!("magick-{tag}-{sw}x{sh}-{}-{lname}", comp.to_lowercase());
                    let mut extra = vec!["-compress", comp];
                    extra.extend_from_slice(largs);
                    out.push(magick_case(dir, name, src, kind, &extra, tol, false));
                }
            }
        }
    }
    out
}

#[test]
fn foreign_magick_matrix() {
    if !ready(&["magick", "tiffcp"]) {
        return;
    }
    let dir = TempDir::new("magick");
    let cases = magick_cases(&dir);
    run_matrix("magick", &dir, cases);
}

// ---------------------------------------------------------------------
// tiffcp-written matrix
// ---------------------------------------------------------------------

/// An uncompressed little-endian base file for the re-writers.
fn base_file(dir: &TempDir, tag: &str, w: u32, h: u32) -> Option<PathBuf> {
    let kind = KINDS.iter().find(|k| k.tag == tag)?;
    let src = make_src(dir, kind.src.0, kind.src.1, w, h);
    let out = dir.path(&format!("base-{tag}-{w}x{h}.tif"));
    if !out.exists() {
        let mut a = src.load.clone();
        a.extend(kind.args.iter().map(|x| x.to_string()));
        a.extend(sv(&["-compress", "None", "-define", "tiff:endian=lsb"]));
        a.push(p(&out));
        run("magick", &a).ok()?;
    }
    Some(out)
}

fn tiffcp_cases(dir: &TempDir) -> Vec<Case> {
    let mut out = Vec::new();
    let layouts: &[(&str, &[&str])] = &[
        ("default", &[]),
        ("r1", &["-r", "1"]),
        ("r5", &["-r", "5"]),
        ("r16", &["-r", "16"]),
        ("t16", &["-t", "-w", "16", "-l", "16"]),
        ("t32x16", &["-t", "-w", "32", "-l", "16"]),
        ("t128", &["-t", "-w", "128", "-l", "128"]),
    ];
    for (tag, spp, w, h) in [
        ("gray8", 1, 67u32, 41u32),
        ("bilevel", 1, 67, 41),
        ("bilevel", 1, 300, 200),
        ("gray4", 1, 67, 41),
        ("gray16", 1, 67, 41),
        ("rgb8", 3, 67, 41),
        ("rgb8", 3, 200, 120),
        ("rgb8s", 3, 67, 41),
        ("rgb8s", 3, 200, 120),
        ("rgb16", 3, 67, 41),
        ("rgba8", 4, 67, 41),
        ("graya8", 2, 67, 41),
        ("cmyk8", 4, 67, 41),
        ("pal8", 1, 67, 41),
        ("grayf32", 1, 67, 41),
        ("rgbf32", 3, 67, 41),
    ] {
        let Some(base) = base_file(dir, tag, w, h) else {
            continue;
        };
        let kind = KINDS.iter().find(|k| k.tag == tag).unwrap();
        let float = tag.contains("f32");
        let mut comps: Vec<(&str, Tol)> = vec![
            ("none", kind.tol),
            ("packbits", kind.tol),
            ("lzw", kind.tol),
            ("zip", kind.tol),
            ("zip:1:p9", kind.tol),
            ("zstd", kind.tol),
            ("lzma", kind.tol),
        ];
        if float {
            comps.extend([
                ("lzw:3", kind.tol),
                ("zip:3", kind.tol),
                ("zstd:3", kind.tol),
            ]);
        } else if tag != "bilevel" {
            comps.extend([
                ("lzw:2", kind.tol),
                ("zip:2", kind.tol),
                ("zstd:2", kind.tol),
            ]);
        }
        if tag == "bilevel" {
            for c in [
                "g3",
                "g3:1d",
                "g3:2d",
                "g3:fill",
                "g3:1d:fill",
                "g3:2d:fill",
                "g4",
            ] {
                comps.push((c, kind.tol));
            }
        }
        if matches!(tag, "rgb8" | "rgba8") {
            comps.push(("webp", Tol::Within(255)));
        }
        if matches!(tag, "rgb8" | "gray8" | "cmyk8") {
            comps.push(("jpeg:r:90", Tol::Within(LOSSY_TOL)));
        }
        if tag == "rgb8s" {
            // YCbCr JPEG with 2 × 2 chroma on the smooth ramp only.
            comps.retain(|c| c.0 == "none");
            // Edge tiles are padded; a reader that interpolates chroma
            // pulls the padding into the last image rows / columns
            // (observed: up to 46 away from the source there, where
            // this crate stays within 3), so the bound is wide.
            let _ = w;
            let ycc = Tol::Within(48);
            comps.push(("jpeg", ycc));
            comps.push(("jpeg:95", ycc));
            comps.push(("jpeg:r:50", Tol::Within(LOSSY_TOL)));
        }
        for (comp, tol) in comps {
            for (lname, largs) in layouts {
                for planar in [false, true] {
                    if planar && spp == 1 {
                        continue;
                    }
                    let n = out.len();
                    let mut variants: Vec<(&str, Vec<&str>)> = vec![(
                        if n.is_multiple_of(2) { "le" } else { "be" },
                        vec![if n.is_multiple_of(2) { "-L" } else { "-B" }],
                    )];
                    if *lname == "default" || *lname == "t16" {
                        variants.push(("big", vec!["-8"]));
                    }
                    if tag == "bilevel" && *lname != "t128" {
                        variants.push(("lsb2msb", vec!["-f", "lsb2msb"]));
                    }
                    for (vname, vargs) in variants {
                        let name = format!(
                            "tiffcp-{tag}-{w}x{h}-{}-{lname}{}-{vname}",
                            comp.replace(':', "_"),
                            if planar { "-sep" } else { "" }
                        );
                        let o = dir.path(&format!("{name}.tif"));
                        let mut a = sv(&["-c", comp]);
                        a.extend(largs.iter().map(|x| x.to_string()));
                        a.extend(sv(&["-p", if planar { "separate" } else { "contig" }]));
                        a.extend(vargs.iter().map(|x| x.to_string()));
                        a.push(p(&base));
                        a.push(p(&o));
                        out.push(Case {
                            name,
                            cmds: vec![(s("tiffcp"), a)],
                            out: o,
                            tol,
                            pages: 1,
                        });
                    }
                }
            }
        }
    }
    out
}

#[test]
fn foreign_tiffcp_matrix() {
    if !ready(&["magick", "tiffcp"]) {
        return;
    }
    let dir = TempDir::new("tiffcp");
    let cases = tiffcp_cases(&dir);
    run_matrix("tiffcp", &dir, cases);
}

// ---------------------------------------------------------------------
// raw2tiff-written matrix (every sample type the tool takes)
// ---------------------------------------------------------------------

fn raw2tiff_cases(dir: &TempDir) -> Vec<Case> {
    let (w, h) = (53u32, 29u32);
    let mut out = Vec::new();
    for (dtype, bytes) in [
        ("byte", 1u32),
        ("short", 2),
        ("long", 4),
        ("sbyte", 1),
        ("sshort", 2),
        ("slong", 4),
        ("float", 4),
        ("double", 8),
    ] {
        for (photo, bands) in [
            ("minisblack", 1u32),
            ("miniswhite", 1),
            ("rgb", 3),
            ("cmyk", 4),
            ("minisblack", 2),
            ("rgb", 4),
            ("cielab", 3),
        ] {
            // One raw file per (type, bands): floats in a sane range,
            // integers from the shared pattern.
            let lab = photo == "cielab";
            let raw = dir.path(&format!(
                "r2t-{dtype}-{bands}{}.raw",
                if lab { "-lab" } else { "" }
            ));
            if !raw.exists() {
                let mut data = Vec::new();
                for y in 0..h {
                    for x in 0..w {
                        for c in 0..bands {
                            let v = sample16(x, y, c, w, h);
                            if lab {
                                // Near-neutral, mid-lightness Lab: L*
                                // unsigned, a* / b* small and signed
                                // (× 256 at 16 bits).
                                let s8: i32 = if c == 0 {
                                    64 + (v >> 9) as i32
                                } else {
                                    (v >> 12) as i32 - 8
                                };
                                if bytes == 1 {
                                    data.push(s8 as u8);
                                } else if c == 0 {
                                    data.extend_from_slice(&((s8 * 257) as u16).to_le_bytes());
                                } else {
                                    data.extend_from_slice(&((s8 * 256) as i16).to_le_bytes());
                                }
                                continue;
                            }
                            match dtype {
                                "float" => data.extend_from_slice(
                                    &((v as f32 / 65535.0) * 3.0 - 1.0).to_le_bytes(),
                                ),
                                "double" => data.extend_from_slice(
                                    &((v as f64 / 65535.0) * 3.0 - 1.0).to_le_bytes(),
                                ),
                                _ => {
                                    let wide =
                                        (v as u32) << 16 | sample16(y, x, c + 7, h, w) as u32;
                                    data.extend_from_slice(
                                        &wide.to_le_bytes()[4 - bytes as usize..],
                                    );
                                }
                            }
                        }
                    }
                }
                fs::write(&raw, data).expect("raw");
            }
            // Only combinations with a defined meaning: the Lab
            // encodings exist for 8- and 16-bit unsigned samples, ink
            // coverage for unsigned integers.
            if photo == "cielab" && !matches!(dtype, "byte" | "short") {
                continue;
            }
            if photo == "cmyk" && !matches!(dtype, "byte" | "short" | "long") {
                continue;
            }
            let tol = if photo == "cielab" {
                Tol::Within(24)
            } else {
                Tol::Exact
            };
            for inter in ["pixel", "band"] {
                // (Lab data is generated pixel-interleaved: read as
                // band-major it would be arbitrary out-of-gamut triples.)
                if inter == "band" && (bands == 1 || lab) {
                    continue;
                }
                for comp in ["none", "packbits", "lzw", "lzw:2", "zip", "zip:2"] {
                    for rows in ["1000", "3"] {
                        let name = format!(
                            "raw2tiff-{dtype}-{photo}{bands}-{inter}-{}-r{rows}",
                            comp.replace(':', "_")
                        );
                        let o = dir.path(&format!("{name}.tif"));
                        // Band-interleaved input is band-major on disk.
                        let a = sv(&[
                            "-w",
                            &w.to_string(),
                            "-l",
                            &h.to_string(),
                            "-b",
                            &bands.to_string(),
                            "-d",
                            dtype,
                            "-p",
                            photo,
                            "-i",
                            inter,
                            "-c",
                            comp,
                            "-r",
                            rows,
                            &p(&raw),
                            &p(&o),
                        ]);
                        out.push(Case {
                            name,
                            cmds: vec![(s("raw2tiff"), a)],
                            out: o,
                            tol,
                            pages: 1,
                        });
                    }
                }
            }
        }
    }
    out
}

#[test]
fn foreign_raw2tiff_matrix() {
    if !ready(&["magick", "tiffcp", "raw2tiff"]) {
        return;
    }
    let dir = TempDir::new("raw2tiff");
    let cases = raw2tiff_cases(&dir);
    run_matrix("raw2tiff", &dir, cases);
}

// ---------------------------------------------------------------------
// ffmpeg- and sips-written matrices (two unrelated writers)
// ---------------------------------------------------------------------

fn ffmpeg_cases(dir: &TempDir) -> Vec<Case> {
    let mut out = Vec::new();
    for (w, h) in [(64u32, 48u32), (67, 41)] {
        let noisy = make_src(dir, "rgba", 8, w, h);
        let smooth = make_src(dir, "smootha", 8, w, h);
        for pix in [
            "rgb24", "rgb48le", "pal8", "rgba", "rgba64le", "gray", "ya8", "gray16le", "ya16le",
            "monob", "monow", "yuv420p", "yuv422p", "yuv440p", "yuv444p", "yuv410p", "yuv411p",
        ] {
            // §21: "ImageWidth and ImageLength are constrained to be
            // integer multiples of YCbCrSubsampleHoriz and
            // YCbCrSubsampleVert" — subsampled layouts only at 64 × 48.
            if pix.starts_with("yuv") && pix != "yuv444p" && (w, h) != (64, 48) {
                continue;
            }
            let src = if pix.starts_with("yuv") {
                &smooth
            } else {
                &noisy
            };
            for algo in ["raw", "packbits", "lzw", "deflate"] {
                let name = format!("ffmpeg-{pix}-{algo}-{w}x{h}");
                let o = dir.path(&format!("{name}.tif"));
                let a = sv(&[
                    "-v",
                    "error",
                    "-f",
                    "rawvideo",
                    "-pixel_format",
                    "rgba",
                    "-video_size",
                    &format!("{w}x{h}"),
                    "-i",
                    &p(&src.raw),
                    "-frames:v",
                    "1",
                    "-pix_fmt",
                    pix,
                    "-compression_algo",
                    algo,
                    "-y",
                    &p(&o),
                ]);
                // YCbCr: TIFF 6.0 section 21 leaves chroma upsampling
                // to the reader (compared on the smooth ramp).
                let tol = if pix.starts_with("yuv") {
                    Tol::Within(LOSSY_TOL)
                } else {
                    Tol::Exact
                };
                out.push(Case {
                    name,
                    cmds: vec![(s("ffmpeg"), a)],
                    out: o,
                    tol,
                    pages: 1,
                });
            }
        }
    }
    out
}

#[test]
fn foreign_ffmpeg_matrix() {
    if !ready(&["magick", "tiffcp", "ffmpeg"]) {
        return;
    }
    let dir = TempDir::new("ffmpeg");
    let cases = ffmpeg_cases(&dir);
    run_matrix("ffmpeg", &dir, cases);
}

fn sips_cases(dir: &TempDir) -> Vec<Case> {
    let mut out = Vec::new();
    for (tag, kind, bits) in [
        ("rgb8", "rgb", 8u32),
        ("rgba8", "rgba", 8),
        ("gray8", "gray", 8),
        ("graya8", "graya", 8),
        ("rgb16", "rgb", 16),
        ("rgba16", "rgba", 16),
        ("gray16", "gray", 16),
    ] {
        let src = make_src(dir, kind, bits, 67, 41);
        let png = dir.path(&format!("sips-src-{tag}.png"));
        let mut a = src.load.clone();
        a.push(p(&png));
        for opt in ["default", "lzw", "packbits"] {
            let name = format!("sips-{tag}-{opt}");
            let o = dir.path(&format!("{name}.tif"));
            out.push(Case {
                name,
                cmds: vec![
                    (s("magick"), a.clone()),
                    (
                        s("sips"),
                        sv(&[
                            "-s",
                            "format",
                            "tiff",
                            "-s",
                            "formatOptions",
                            opt,
                            &p(&png),
                            "--out",
                            &p(&o),
                        ]),
                    ),
                ],
                out: o,
                tol: Tol::Exact,
                pages: 1,
            });
        }
    }
    out
}

#[test]
fn foreign_sips_matrix() {
    if !ready(&["magick", "tiffcp", "sips"]) {
        return;
    }
    let dir = TempDir::new("sips");
    let cases = sips_cases(&dir);
    run_matrix("sips", &dir, cases);
}

// ---------------------------------------------------------------------
// Orientation, multi-page, pyramid
// ---------------------------------------------------------------------

fn structure_cases(dir: &TempDir) -> Vec<Case> {
    let mut out = Vec::new();
    // All eight orientations, set on an existing file by `tiffset`
    // (which appends a rewritten directory).
    for tag in ["rgb8", "bilevel", "gray16", "rgba8", "pal8", "gray4"] {
        let Some(base) = base_file(dir, tag, 67, 41) else {
            continue;
        };
        for orient in 1..=8u32 {
            let name = format!("orient-{tag}-{orient}");
            let o = dir.path(&format!("{name}.tif"));
            out.push(Case {
                name,
                cmds: vec![
                    (s("cp"), vec![p(&base), p(&o)]),
                    (
                        s("tiffset"),
                        sv(&["-s", "274", &orient.to_string(), &p(&o)]),
                    ),
                ],
                out: o,
                tol: Tol::Exact,
                pages: 1,
            });
        }
    }
    // Multi-page chains: mixed kinds and sizes, both byte orders,
    // classic and BigTIFF, plus a reduced-resolution pyramid.
    let pages: Vec<PathBuf> = [
        ("rgb8", 67, 41),
        ("bilevel", 300, 200),
        ("gray16", 67, 41),
        ("rgba8", 67, 41),
        ("pal8", 67, 41),
    ]
    .iter()
    .filter_map(|(t, w, h)| base_file(dir, t, *w, *h))
    .collect();
    if pages.len() == 5 {
        for (vname, vargs) in [
            ("le", vec!["-L", "-c", "lzw"]),
            ("be", vec!["-B", "-c", "zip"]),
            ("big", vec!["-8", "-c", "packbits"]),
            (
                "big-be-tiled",
                vec!["-8", "-B", "-t", "-w", "32", "-l", "32", "-c", "lzw"],
            ),
        ] {
            let name = format!("multipage-tiffcp-{vname}");
            let o = dir.path(&format!("{name}.tif"));
            let mut a: Vec<String> = vargs.iter().map(|x| x.to_string()).collect();
            a.extend(pages.iter().map(|x| p(x)));
            a.push(p(&o));
            out.push(Case {
                name,
                cmds: vec![(s("tiffcp"), a)],
                out: o,
                tol: Tol::Exact,
                pages: 5,
            });
        }
        let name = s("multipage-magick-adjoin");
        let o = dir.path("multipage-magick-adjoin.tif");
        let mut a: Vec<String> = pages.iter().map(|x| p(x)).collect();
        a.extend(sv(&["-adjoin", "-compress", "Zip", &p(&o)]));
        out.push(Case {
            name,
            cmds: vec![(s("magick"), a)],
            out: o,
            tol: Tol::Exact,
            pages: 5,
        });
    }
    let src = make_src(dir, "rgb", 8, 640, 400);
    for (vname, vargs) in [
        ("strip", vec![]),
        ("tile", vec!["-define", "tiff:tile-geometry=128x128"]),
        ("lzw", vec!["-compress", "LZW"]),
    ] {
        let name = format!("pyramid-{vname}");
        let o = dir.path(&format!("{name}.tif"));
        let mut a = src.load.clone();
        a.extend(vargs.iter().map(|x| x.to_string()));
        a.push(format!("ptif:{}", p(&o)));
        out.push(Case {
            name,
            cmds: vec![(s("magick"), a)],
            out: o,
            tol: Tol::Exact,
            pages: 0,
        });
    }
    out
}

#[test]
fn foreign_structure_matrix() {
    if !ready(&["magick", "tiffcp", "tiffset"]) {
        return;
    }
    let dir = TempDir::new("structure");
    let cases = structure_cases(&dir);
    let t = run_matrix("structure", &dir, cases);
    assert!(t.passed >= 40, "structure matrix shrank: {} pass", t.passed);
}

// ---------------------------------------------------------------------
// Direction "ours → foreign": every layout this crate's writer emits
// ---------------------------------------------------------------------

use oxideav_tiff::{
    EncodePage, EncodePixelFormat, ExtraSampleKind, JpegOptions, JpegProcess, JpegTablesLayout,
    PageExtras, TiffCompression, encode_page,
};

/// Owned rasters for every writer input kind at one size.
struct Rasters {
    w: u32,
    h: u32,
    bw: Vec<u8>,
    gray4: Vec<u8>,
    gray8: Vec<u8>,
    gray16: Vec<u8>,
    rgb8: Vec<u8>,
    rgb16: Vec<u8>,
    rgba8: Vec<u8>,
    ya8: Vec<u8>,
    ya16: Vec<u8>,
    rgba16: Vec<u8>,
    pal_idx: Vec<u8>,
    pal4_idx: Vec<u8>,
    palette: Vec<[u8; 3]>,
    gi8: Vec<i8>,
    gi16: Vec<i16>,
    f16: Vec<u16>,
    f32s: Vec<f32>,
    f64s: Vec<f64>,
    rgb_f16: Vec<u16>,
    rgb_f32: Vec<f32>,
    rgb_f64: Vec<f64>,
    rgba_f32: Vec<f32>,
    cmyk: Vec<u8>,
    /// The smooth ramp as full-range YCbCr triples.
    ycc: Vec<u8>,
    /// Near-neutral CIELab triples (L* ramp, small a* / b*).
    lab: Vec<u8>,
    gray12: Vec<u16>,
    rgb36: Vec<u16>,
}

impl Rasters {
    fn new(w: u32, h: u32) -> Self {
        let n = (w * h) as usize;
        let s16 = |ch: u32| -> Vec<u16> {
            let mut v = Vec::with_capacity(n * ch as usize);
            for y in 0..h {
                for x in 0..w {
                    for c in 0..ch {
                        v.push(sample16(x, y, c, w, h));
                    }
                }
            }
            v
        };
        let flt = |ch: u32| -> Vec<f32> {
            s16(ch)
                .iter()
                .map(|&v| v as f32 / 65535.0 * 2.5 - 0.5)
                .collect()
        };
        let row1 = (w as usize).div_ceil(8);
        let mut bw = vec![0u8; row1 * h as usize];
        for (i, &v) in raster_bw(w, h).iter().enumerate() {
            if v == 0 {
                let (y, x) = (i / w as usize, i % w as usize);
                bw[y * row1 + x / 8] |= 0x80 >> (x % 8);
            }
        }
        let row4 = (w as usize).div_ceil(2);
        let g8 = raster8(w, h, 1);
        let mut gray4 = vec![0u8; row4 * h as usize];
        for (i, &v) in g8.iter().enumerate() {
            let (y, x) = (i / w as usize, i % w as usize);
            gray4[y * row4 + x / 2] |= (v >> 4) << (if x % 2 == 0 { 4 } else { 0 });
        }
        Self {
            w,
            h,
            bw,
            gray4,
            gray16: raster16(w, h, 1),
            rgb8: raster8(w, h, 3),
            rgb16: raster16(w, h, 3),
            rgba8: raster8(w, h, 4),
            ya8: raster8(w, h, 2),
            ya16: raster16(w, h, 2),
            rgba16: raster16(w, h, 4),
            pal_idx: g8.clone(),
            pal4_idx: gray4_indices(&g8, w, h),
            palette: (0..256u32)
                .map(|i| [(i * 7 % 256) as u8, (255 - i) as u8, (i * 13 % 256) as u8])
                .collect(),
            gi8: g8.iter().map(|&v| v as i8).collect(),
            gi16: s16(1).iter().map(|&v| v as i16).collect(),
            f16: flt(1)
                .iter()
                .map(|&v| oxideav_tiff::f32_to_f16_bits(v))
                .collect(),
            f32s: flt(1),
            f64s: flt(1).iter().map(|&v| v as f64 * 1.000_000_1).collect(),
            rgb_f16: flt(3)
                .iter()
                .map(|&v| oxideav_tiff::f32_to_f16_bits(v))
                .collect(),
            rgb_f32: flt(3),
            rgb_f64: flt(3).iter().map(|&v| v as f64 * 1.000_000_1).collect(),
            rgba_f32: flt(4),
            cmyk: raster8(w, h, 4),
            ycc: oxideav_tiff::rgb24_to_ycbcr24(&raster_smooth(w, h, 3)),
            lab: raster_smooth(w, h, 3)
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| {
                    [
                        64 + p[0] / 2,
                        ((p[1] as i32 - 128) / 12) as i8 as u8,
                        ((p[2] as i32 - 128) / 12) as i8 as u8,
                    ]
                })
                .collect(),
            gray12: s16(1).iter().map(|&v| v >> 4).collect(),
            rgb36: s16(3).iter().map(|&v| v >> 4).collect(),
            gray8: g8,
        }
    }

    fn kinds(&self) -> Vec<(&'static str, EncodePixelFormat<'_>, Tol)> {
        use EncodePixelFormat as E;
        let x = Tol::Exact;
        let mut v = vec![
            ("bilevel", E::Bilevel { pixels: &self.bw }, x),
            ("mask", E::TransparencyMask { pixels: &self.bw }, x),
            (
                "gray4",
                E::Gray4 {
                    pixels: &self.gray4,
                },
                x,
            ),
            (
                "gray8",
                E::Gray8 {
                    pixels: &self.gray8,
                },
                x,
            ),
            (
                "gray16",
                E::Gray16Le {
                    pixels: &self.gray16,
                },
                x,
            ),
            ("rgb8", E::Rgb24 { pixels: &self.rgb8 }, x),
            (
                "rgb16",
                E::Rgb48 {
                    pixels: &self.rgb16,
                },
                x,
            ),
            (
                "pal8",
                E::Palette8 {
                    indices: &self.pal_idx,
                    palette: &self.palette,
                },
                x,
            ),
            (
                "pal4",
                E::Palette4 {
                    indices: &self.pal4_idx,
                    palette: &self.palette[..16],
                },
                x,
            ),
            ("gi8", E::GrayI8 { pixels: &self.gi8 }, x),
            ("gi16", E::GrayI16 { pixels: &self.gi16 }, x),
            ("gf16", E::GrayF16 { pixels: &self.f16 }, x),
            ("gf32", E::GrayF32 { pixels: &self.f32s }, x),
            ("gf64", E::GrayF64 { pixels: &self.f64s }, x),
            (
                "rgbf16",
                E::RgbF16 {
                    pixels: &self.rgb_f16,
                },
                x,
            ),
            (
                "rgbf32",
                E::RgbF32 {
                    pixels: &self.rgb_f32,
                },
                x,
            ),
            (
                "rgbf64",
                E::RgbF64 {
                    pixels: &self.rgb_f64,
                },
                x,
            ),
            // CIELab → RGB is the reader's conversion and the readers
            // do not agree on the reference white of an untagged page
            // (D50 per the Adobe Lab update here): near-neutral colours
            // bound the difference.
            ("lab8", E::CieLab8 { pixels: &self.lab }, Tol::Within(24)),
            (
                "labl8",
                E::CieLabL8 {
                    pixels: &self.gray8,
                },
                Tol::Within(4),
            ),
            ("cmyk8", E::Cmyk32 { pixels: &self.cmyk }, x),
            ("ycbcr8", E::YCbCr24 { pixels: &self.ycc }, Tol::Within(3)),
            (
                "gray12",
                E::Gray12 {
                    pixels: &self.gray12,
                },
                x,
            ),
            (
                "rgb36",
                E::Rgb36 {
                    pixels: &self.rgb36,
                },
                x,
            ),
        ];
        for (name, kind) in [
            ("unassoc", ExtraSampleKind::UnassociatedAlpha),
            ("assoc", ExtraSampleKind::AssociatedAlpha),
            ("unspec", ExtraSampleKind::Unspecified),
        ] {
            let k = |tag: &'static str| -> &'static str {
                Box::leak(format!("{tag}-{name}").into_boxed_str())
            };
            v.push((
                k("rgba8"),
                E::Rgba32 {
                    pixels: &self.rgba8,
                    kind,
                },
                x,
            ));
            v.push((
                k("ya8"),
                E::GrayAlpha8 {
                    pixels: &self.ya8,
                    kind,
                },
                x,
            ));
            v.push((
                k("ya16"),
                E::GrayAlpha16 {
                    pixels: &self.ya16,
                    kind,
                },
                x,
            ));
            v.push((
                k("rgba16"),
                E::Rgba64 {
                    pixels: &self.rgba16,
                    kind,
                },
                x,
            ));
            v.push((
                k("rgbaf32"),
                E::RgbaF32 {
                    pixels: &self.rgba_f32,
                    kind,
                },
                x,
            ));
        }
        // §21 chroma subsampling: reconstruction is the reader's.
        if self.w.is_multiple_of(4) && self.h.is_multiple_of(2) {
            for (name, sub) in [
                ("ycc21", (2u16, 1u16)),
                ("ycc22", (2, 2)),
                ("ycc12", (1, 2)),
                ("ycc41", (4, 1)),
                ("ycc42", (4, 2)),
            ] {
                v.push((
                    name,
                    E::YCbCrSubsampled24 {
                        pixels: &self.ycc,
                        subsampling: sub,
                    },
                    Tol::Within(LOSSY_TOL),
                ));
            }
        }
        v
    }
}

/// One palette index per byte (0..=15), then packed two per byte.
fn gray4_indices(g8: &[u8], w: u32, h: u32) -> Vec<u8> {
    let row = (w as usize).div_ceil(2);
    let mut out = vec![0u8; row * h as usize];
    for (i, &v) in g8.iter().enumerate() {
        let (y, x) = (i / w as usize, i % w as usize);
        out[y * row + x / 2] |= (v >> 4) << (if x % 2 == 0 { 4 } else { 0 });
    }
    out
}

fn writer_compressions() -> Vec<(String, TiffCompression, bool)> {
    use TiffCompression as C;
    let mut v: Vec<(String, C, bool)> = vec![
        (s("none"), C::None, false),
        (s("packbits"), C::PackBits, false),
        (s("lzw"), C::Lzw, false),
        (s("deflate"), C::Deflate, false),
        (s("zstd"), C::Zstd, false),
        (s("webp"), C::Webp, false),
        (s("rle"), C::CcittRle, false),
        (
            s("g3"),
            C::CcittT4OneD {
                eol_byte_aligned: false,
            },
            false,
        ),
        (
            s("g3-fill"),
            C::CcittT4OneD {
                eol_byte_aligned: true,
            },
            false,
        ),
        (
            s("g3-2d"),
            C::CcittT4TwoD {
                eol_byte_aligned: false,
                uncompressed: false,
            },
            false,
        ),
        (
            s("g3-2d-fill"),
            C::CcittT4TwoD {
                eol_byte_aligned: true,
                uncompressed: false,
            },
            false,
        ),
        (
            s("g4"),
            C::CcittT6 {
                uncompressed: false,
            },
            false,
        ),
    ];
    let j = JpegOptions::default();
    for (name, o) in [
        ("jpeg-q75", j),
        ("jpeg-q30", j.with_quality(30)),
        (
            "jpeg-q95-perseg",
            j.with_quality(95).with_tables(JpegTablesLayout::PerSegment),
        ),
        (
            "jpeg-q90-opt",
            j.with_quality(90).with_optimize_huffman(true),
        ),
        ("jpeg-q90-rst", j.with_quality(90).with_restart_interval(4)),
    ] {
        v.push((s(name), C::Jpeg(o), true));
    }
    for pred in 1..=7u8 {
        v.push((
            format!("jpeg-lossless{pred}"),
            C::Jpeg(j.with_process(JpegProcess::Lossless { predictor: pred })),
            false,
        ));
    }
    v
}

/// `tiffinfo -D` (directory + a full data read) must exit clean and
/// print nothing on stderr.
fn tiffinfo_clean(path: &Path, read_data: bool) -> Result<(), String> {
    let mut cmd = Command::new("tiffinfo");
    if read_data {
        cmd.arg("-D");
    }
    let out = cmd.arg(path).output().map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || !err.trim().is_empty() {
        return Err(format!(
            "tiffinfo: {}",
            err.lines().next().unwrap_or("non-zero exit")
        ));
    }
    Ok(())
}

/// Second black-box reader for a lossless page: the raw samples
/// (`tiffcp` / `tiffcrop`), or the `tiff2rgba` rendering where the raw
/// samples are not whole bytes. `Ok(false)` = neither applies.
fn second_reader(path: &Path, idx: usize, page: &Page, scratch: &Path) -> Result<bool, String> {
    use TiffPixelFormat as F;
    let img = &page.image;
    let lay = &page.layout;
    let bits = lay.bits_per_sample.first().copied().unwrap_or(1) as u32;
    let photo = lay.photometric.unwrap_or(1);
    let uint = lay.sample_format.first().is_none_or(|&f| f == 1);
    let spp = lay.samples_per_pixel as usize;
    let (colour, has_alpha) = match img.format {
        F::Gray8 | F::Gray16Le => (1, false),
        F::Ya8 | F::Ya16Le => (1, true),
        F::Rgb24 | F::Rgb48Le => (3, false),
        F::Rgba | F::Rgba64Le => (3, true),
        F::Cmyk => (4, false),
        _ => (0, false),
    };
    let depth = img.format.bits_per_sample() as u32;
    if uint
        && colour != 0
        && bits >= depth
        && bits.is_multiple_of(8)
        && spp >= colour
        && matches!(photo, 0 | 1 | 2 | 5)
    {
        // Raw samples, mapped by the documented rules: extra samples
        // past the alpha are dropped, associated alpha is divided out
        // (rounded, clamped; zero alpha gives zero colour), WhiteIsZero
        // colour is inverted.
        let kind = extra_kind(path, idx);
        if has_alpha != matches!(kind, Some(1 | 2)) {
            return Err(format!(
                "alpha layout {:?} for ExtraSamples {kind:?}",
                img.format
            ));
        }
        let (mut raw, rbits, rspp, _, rphoto) = raw_values(path, idx, scratch)?;
        // A sample wider than the layout keeps its top bits.
        for v in raw.iter_mut() {
            *v >>= rbits - depth;
        }
        if rspp as usize != spp {
            return Err(format!(
                "reader 2: {rspp} samples per pixel, tags say {spp}"
            ));
        }
        let ours = our_values(img);
        let max = (1u32 << depth) - 1;
        let ch = colour + has_alpha as usize;
        if raw.len() / spp != ours.len() / ch {
            return Err(format!(
                "reader 2: {} pixels, ours {}",
                raw.len() / spp,
                ours.len() / ch
            ));
        }
        for (i, (o, r)) in ours.chunks_exact(ch).zip(raw.chunks_exact(spp)).enumerate() {
            for c in 0..ch {
                let mut e = r[c];
                if c < colour {
                    if kind == Some(1) {
                        let a = r[colour] as u64;
                        e = if a == 0 {
                            0
                        } else if a >= max as u64 {
                            e
                        } else {
                            ((e as u64 * max as u64 + a / 2) / a).min(max as u64) as u32
                        };
                    }
                    if rphoto == 0 {
                        e = max - e;
                    }
                }
                if o[c] != e {
                    return Err(format!(
                        "reader 2: pixel {i} channel {c} ours {} raw-derived {e}",
                        o[c]
                    ));
                }
            }
        }
        return Ok(true);
    }
    let plain = spp == if photo == 2 { 3 } else { 1 };
    if idx == 0
        && plain
        && matches!(img.format, F::Gray8 | F::Rgb24 | F::Pal8)
        && matches!(photo, 0..=3)
        && uint
    {
        let rgba = scratch.with_extension("rgba.tif");
        run("tiff2rgba", &[s("-c"), s("none"), p(path), p(&rgba)])?;
        let r = raw_values(&rgba, 0, scratch);
        let _ = fs::remove_file(&rgba);
        let (raw, _, spp, _, _) = r?;
        if spp != 4 {
            return Err(format!("tiff2rgba wrote {spp} samples per pixel"));
        }
        let ours = img.to_rgba8();
        if raw.len() != ours.len() {
            return Err(format!(
                "reader 2: {} samples, ours {}",
                raw.len(),
                ours.len()
            ));
        }
        // 2-bit and odd widths widen with a reader-chosen rule.
        let allow = if matches!(bits, 1 | 4 | 8) { 0 } else { 1 };
        for (i, (&o, &r)) in ours.iter().zip(&raw).enumerate() {
            if (o as i32 - r as i32).unsigned_abs() > allow {
                return Err(format!("reader 2 (rgba): sample {i} ours {o} theirs {r}"));
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// `(name, tiling, RowsPerStrip, BigTIFF)`.
type WriterLayout = (&'static str, Option<(u32, u32)>, Option<u32>, bool);

struct WriterTally {
    files: usize,
    two_readers: usize,
    one_reader: usize,
    unreadable_outside: usize,
}

fn writer_matrix(tag: &str, w: u32, h: u32, full: bool) -> WriterTally {
    let dir = TempDir::new(tag);
    let r = Rasters::new(w, h);
    let comps = writer_compressions();
    // (name, bytes, tol)
    let mut files: Vec<(String, PathBuf, Tol)> = Vec::new();
    for (kname, kind, ktol) in r.kinds() {
        for (cname, comp, lossy) in &comps {
            for predictor in [false, true] {
                for planar in [false, true] {
                    let layouts: &[WriterLayout] = if full {
                        &[
                            ("strip", None, None, false),
                            ("rps1", None, Some(1), false),
                            ("rps7", None, Some(7), false),
                            ("rps16", None, Some(16), false),
                            ("t16", Some((16, 16)), None, false),
                            ("t32x48", Some((32, 48)), None, false),
                            ("big", None, None, true),
                            ("big-t16", Some((16, 16)), None, true),
                        ]
                    } else {
                        &[
                            ("strip", None, None, false),
                            ("t16", Some((16, 16)), None, false),
                        ]
                    };
                    for (lname, tiling, rps, big) in layouts {
                        let page = EncodePage::new(
                            w,
                            h,
                            kind.clone(),
                            *comp,
                            predictor,
                            planar,
                            *tiling,
                            *big,
                            PageExtras::default().with_rows_per_strip(*rps),
                        );
                        let Ok(bytes) = encode_page(&page) else {
                            continue;
                        };
                        let name = format!(
                            "ours-{kname}-{cname}-{lname}{}{}-{w}x{h}",
                            if predictor { "-pred" } else { "" },
                            if planar { "-sep" } else { "" }
                        );
                        let path = dir.path(&format!("{name}.tif"));
                        fs::write(&path, bytes).expect("write");
                        let tol = if *lossy {
                            // DCT: the two readers run different
                            // inverse transforms (T.81 fixes accuracy
                            // bounds, not one result); subsampled
                            // chroma is additionally reader-upsampled.
                            Tol::Within(LOSSY_TOL)
                        } else {
                            ktol
                        };
                        files.push((name, path, tol));
                    }
                }
            }
        }
    }
    let next = AtomicUsize::new(0);
    let bad: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let counts = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    std::thread::scope(|sc| {
        for t in 0..4 {
            let (next, bad, files, counts) = (&next, &bad, &files, &counts);
            let scratch = dir.path(&format!("ref-{t}.tif"));
            sc.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= files.len() {
                        break;
                    }
                    let (name, path, tol) = &files[i];
                    // Ok(n) = number of black-box readers that
                    // confirmed the page (0 = none can read the layout).
                    let verdict = (|| -> Result<usize, String> {
                        let bytes = fs::read(path).map_err(|e| e.to_string())?;
                        let desc = describe(&bytes);
                        let pages = decode_pages(&bytes).map_err(|e| format!("own decode: {e}"))?;
                        if reader_limited(&desc).is_some() {
                            return Ok(0);
                        }
                        // `tiffinfo -D` cannot walk subsampled tiles
                        // ("Cannot display data", also on files the
                        // black-box tools write): directory check only.
                        let subsampled_tiles = desc.contains("photo=6")
                            && desc.contains("tiled=true")
                            && !desc.contains("sub=1,1");
                        tiffinfo_clean(path, !subsampled_tiles)?;
                        let first = check_page(path, 0, &pages[0], *tol, &scratch);
                        let second = if *tol == Tol::Exact {
                            second_reader(path, 0, &pages[0], &scratch)
                        } else {
                            Ok(false)
                        };
                        match (first, second) {
                            (Ok(()), Ok(true)) => Ok(2),
                            (Ok(()), Ok(false)) => Ok(1),
                            (Ok(()), Err(e)) => Err(e),
                            // The primary reader mis-reads a few
                            // layouts (see `run_case`).
                            (Err(_), Ok(true)) => Ok(1),
                            (Err(e), _) => match rgba_reader(path, 0, &pages[0], *tol, &scratch) {
                                Ok(()) => Ok(1),
                                // Every reader refuses the page.
                                Err(m)
                                    if (e.starts_with("magick") || e.starts_with("tiff"))
                                        && (m.starts_with("tiff") || m == "not applicable") =>
                                {
                                    Ok(0)
                                }
                                Err(_) => Err(e),
                            },
                        }
                    })();
                    match verdict {
                        Ok(n) => {
                            counts[n].fetch_add(1, Ordering::Relaxed);
                            let _ = fs::remove_file(path);
                        }
                        Err(e) => bad.lock().unwrap().push(format!(
                            "{name}: {e}  [{}]",
                            describe(&fs::read(path).unwrap_or_default())
                        )),
                    }
                }
            });
        }
    });
    let mut bad = bad.into_inner().unwrap();
    bad.sort();
    let [none, one, two] = counts.map(AtomicUsize::into_inner);
    eprintln!(
        "blackbox matrix [{tag}]: {} files written by this crate, {} failing; {} confirmed by two \
         black-box readers, {} by one, {} in layouts no black-box reader here reads",
        files.len(),
        bad.len(),
        two,
        one,
        none
    );
    for b in &bad {
        eprintln!("  FAIL {b}");
    }
    assert!(bad.is_empty(), "{} writer cases fail in [{tag}]", bad.len());
    WriterTally {
        files: files.len(),
        two_readers: two,
        one_reader: one,
        unreadable_outside: none,
    }
}

/// Layouts this crate writes that the black-box readers on hand do not
/// read at all — their stated limitation, not a property of the file.
/// Matched against the [`describe`] line; these pages are only checked
/// against this crate's own reader.
fn reader_limited(desc: &str) -> Option<&'static str> {
    if desc.contains("comp=7 ") && !desc.contains("bps=8") {
        return Some(
            "JPEG at 12 / 16 bits: the readers are 8-bit JPEG builds (\"Improper JPEG data precision\")",
        );
    }
    if desc.contains("photo=6") && desc.contains("planar=2") && !desc.contains("sub=1,1") {
        return Some(
            "chroma-subsampled YCbCr in separate planes (TIFF 6.0 §21): the black-box tools \
             neither write nor read the reduced chroma planes",
        );
    }
    if desc.contains("bps=4 ") && (desc.contains("pred=2") || desc.contains("pred=3")) {
        return Some(
            "Predictor=2 on 4-bit samples: TIFF 6.0 §14 describes it; the reader reports it \
             \"not supported with 4-bit samples\"",
        );
    }
    None
}

#[test]
fn ours_to_foreign_matrix() {
    if !ready(&["magick", "tiffcp", "tiffinfo", "tiff2rgba"]) {
        return;
    }
    let t = writer_matrix("ours", 68, 42, true);
    assert!(t.files > 1000, "writer sweep shrank: {} files", t.files);
    assert_eq!(t.two_readers + t.one_reader + t.unreadable_outside, t.files);
}

#[test]
fn ours_to_foreign_small_and_odd_sizes() {
    if !ready(&["magick", "tiffcp", "tiffinfo", "tiff2rgba"]) {
        return;
    }
    for (w, h) in [(1, 1), (3, 70), (71, 2), (17, 17), (300, 200)] {
        writer_matrix(&format!("ours-{w}x{h}"), w, h, false);
    }
}
