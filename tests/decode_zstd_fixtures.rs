//! Integration tests that synthesise tiny TIFFs encoded with
//! Compression=50000 (Zstandard, extension codec ID) via the
//! `tiffcp -c zstd` and `tiffcp -c zstd:p2` black-box validator
//! invocations, then decode them with our decoder and check we get
//! back the same pixels.
//!
//! Per workspace policy: `tiffcp` is used as an opaque binary only
//! (raw TIFF in, ZSTD TIFF out). Its source is never read.
//!
//! Test matrix (7 round-trip positives):
//!
//!   * Gray8 strip   Predictor=1
//!   * Gray8 strip   Predictor=2
//!   * Gray8 tile    Predictor=1
//!   * RGB24 strip   Predictor=1
//!   * RGB24 strip   Predictor=2
//!   * RGB24 tile    Predictor=1
//!   * RGB24 tile    Predictor=2
//!
//! Each test is gated on `tiffcp` being available and on its build
//! actually understanding `-c zstd` (some packaged validator builds
//! omit the codec). If either check fails the test prints a
//! `skipping:` line and passes, so CI without these prerequisites
//! stays green.
//!
//! The 2 negative-path unit tests for missing-magic and truncated
//! frames live alongside `unpack_zstd` in `src/compress.rs`.

#![cfg(feature = "zstd")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use oxideav_tiff::{decode_tiff, DecodedTiff};

// ---------------------------------------------------------------------------
// Validator gating
// ---------------------------------------------------------------------------

fn binary_available(name: &str) -> bool {
    Command::new(name)
        .arg("-h")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| true)
        .unwrap_or(false)
}

/// `tiffcp -h` lists supported `-c <codec>` flags in its usage text.
/// We probe the string to gate the ZSTD tests on the local build
/// actually shipping the codec (some packages omit it).
fn tiffcp_supports_zstd() -> bool {
    let out = Command::new("tiffcp")
        .arg("-h")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).to_string()
                + &String::from_utf8_lossy(&o.stderr);
            s.contains("zstd")
        }
        Err(_) => false,
    }
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n}")
}

fn tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-zstd-{}-{}-{}",
        label,
        std::process::id(),
        rand_suffix()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Synthetic pixel patterns (deterministic, no fixture files on disk)
// ---------------------------------------------------------------------------

/// 64x64 RGB pattern: per-component gradients that the horizontal
/// predictor can squash flat — exercises both the Predictor=1 path
/// (raw bytes) and the Predictor=2 reverse (differenced bytes plus
/// per-component differencing for SPP=3).
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

fn gray_pattern_64() -> Vec<u8> {
    let mut p = Vec::with_capacity(64 * 64);
    for y in 0u8..64 {
        for x in 0u8..64 {
            p.push(x.wrapping_add(y).wrapping_mul(2));
        }
    }
    p
}

// ---------------------------------------------------------------------------
// Container helpers (PPM / PGM → raw uncompressed TIFF via our encoder)
// ---------------------------------------------------------------------------

fn make_ppm_rgb(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), (w * h * 3) as usize);
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(pixels);
    v
}

fn make_pgm_gray(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), (w * h) as usize);
    let mut v = format!("P5\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(pixels);
    v
}

/// Write `bytes` to a `<dir>/in.<src_ext>` file. Returns the path.
fn write_input(dir: &Path, src_ext: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(format!("in.{src_ext}"));
    fs::File::create(&p).unwrap().write_all(bytes).unwrap();
    p
}

/// Convert a PPM/PGM input to an uncompressed TIFF with `convert`,
/// then re-encode through `tiffcp` with the requested zstd options.
/// Returns the final ZSTD-compressed TIFF bytes, or `None` if any
/// stage failed (caller skips the test).
fn make_zstd_tiff(
    dir: &Path,
    src_ext: &str,
    src_bytes: &[u8],
    extra_tiffcp_args: &[&str],
) -> Option<Vec<u8>> {
    if !binary_available("convert") {
        eprintln!("skipping: `convert` not available");
        return None;
    }
    let in_path = write_input(dir, src_ext, src_bytes);
    let raw_tiff = dir.join("raw.tiff");
    let zstd_tiff = dir.join("out.tiff");

    let status = Command::new("convert")
        .arg(&in_path)
        .arg("-compress")
        .arg("none")
        .arg(&raw_tiff)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("skipping: convert → uncompressed TIFF failed");
        return None;
    }

    let mut cmd = Command::new("tiffcp");
    for a in extra_tiffcp_args {
        cmd.arg(a);
    }
    let status = cmd
        .arg(&raw_tiff)
        .arg(&zstd_tiff)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("skipping: tiffcp re-encode to zstd failed");
        return None;
    }
    fs::read(&zstd_tiff).ok()
}

// ---------------------------------------------------------------------------
// DecodedTiff → flat RGB24 / Gray8 helpers (mirror the imagemagick suite)
// ---------------------------------------------------------------------------

fn frame_to_rgb24_bytes(d: &DecodedTiff) -> Vec<u8> {
    assert_eq!(d.frame.planes.len(), 1);
    let stride = d.frame.planes[0].stride;
    let row_bytes = d.width as usize * 3;
    if stride == row_bytes {
        d.frame.planes[0].data.clone()
    } else {
        let mut out = Vec::with_capacity(row_bytes * d.height as usize);
        for y in 0..d.height as usize {
            out.extend_from_slice(&d.frame.planes[0].data[y * stride..y * stride + row_bytes]);
        }
        out
    }
}

fn frame_to_gray8_bytes(d: &DecodedTiff) -> Vec<u8> {
    assert_eq!(d.frame.planes.len(), 1);
    let stride = d.frame.planes[0].stride;
    let row_bytes = d.width as usize;
    if stride == row_bytes {
        d.frame.planes[0].data.clone()
    } else {
        let mut out = Vec::with_capacity(row_bytes * d.height as usize);
        for y in 0..d.height as usize {
            out.extend_from_slice(&d.frame.planes[0].data[y * stride..y * stride + row_bytes]);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests — 7 round-trip positives
// ---------------------------------------------------------------------------

#[test]
fn decode_64x64_gray8_zstd_strip_predictor1() {
    if !tiffcp_supports_zstd() {
        eprintln!("skipping: tiffcp lacks zstd support");
        return;
    }
    let dir = tmp_dir("gray-strip-p1");
    let pixels = gray_pattern_64();
    let pgm = make_pgm_gray(64, 64, &pixels);
    let tiff = match make_zstd_tiff(&dir, "pgm", &pgm, &["-s", "-c", "zstd"]) {
        Some(b) => b,
        None => return,
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_gray8_bytes(&d);
    assert_eq!(got, pixels, "Gray8 strip Zstd Predictor=1 pixel mismatch");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decode_64x64_gray8_zstd_strip_predictor2() {
    if !tiffcp_supports_zstd() {
        eprintln!("skipping: tiffcp lacks zstd support");
        return;
    }
    let dir = tmp_dir("gray-strip-p2");
    let pixels = gray_pattern_64();
    let pgm = make_pgm_gray(64, 64, &pixels);
    let tiff = match make_zstd_tiff(&dir, "pgm", &pgm, &["-s", "-c", "zstd:2"]) {
        Some(b) => b,
        None => return,
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_gray8_bytes(&d);
    assert_eq!(got, pixels, "Gray8 strip Zstd Predictor=2 pixel mismatch");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decode_64x64_gray8_zstd_tile_predictor1() {
    if !tiffcp_supports_zstd() {
        eprintln!("skipping: tiffcp lacks zstd support");
        return;
    }
    let dir = tmp_dir("gray-tile-p1");
    let pixels = gray_pattern_64();
    let pgm = make_pgm_gray(64, 64, &pixels);
    let tiff = match make_zstd_tiff(
        &dir,
        "pgm",
        &pgm,
        &["-t", "-w", "32", "-l", "32", "-c", "zstd"],
    ) {
        Some(b) => b,
        None => return,
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_gray8_bytes(&d);
    assert_eq!(got, pixels, "Gray8 tile Zstd Predictor=1 pixel mismatch");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decode_64x64_rgb24_zstd_strip_predictor1() {
    if !tiffcp_supports_zstd() {
        eprintln!("skipping: tiffcp lacks zstd support");
        return;
    }
    let dir = tmp_dir("rgb-strip-p1");
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let tiff = match make_zstd_tiff(&dir, "ppm", &ppm, &["-s", "-c", "zstd"]) {
        Some(b) => b,
        None => return,
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "RGB strip Zstd Predictor=1 pixel mismatch");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decode_64x64_rgb24_zstd_strip_predictor2() {
    if !tiffcp_supports_zstd() {
        eprintln!("skipping: tiffcp lacks zstd support");
        return;
    }
    let dir = tmp_dir("rgb-strip-p2");
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let tiff = match make_zstd_tiff(&dir, "ppm", &ppm, &["-s", "-c", "zstd:2"]) {
        Some(b) => b,
        None => return,
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "RGB strip Zstd Predictor=2 pixel mismatch");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decode_64x64_rgb24_zstd_tile_predictor1() {
    if !tiffcp_supports_zstd() {
        eprintln!("skipping: tiffcp lacks zstd support");
        return;
    }
    let dir = tmp_dir("rgb-tile-p1");
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let tiff = match make_zstd_tiff(
        &dir,
        "ppm",
        &ppm,
        &["-t", "-w", "32", "-l", "32", "-c", "zstd"],
    ) {
        Some(b) => b,
        None => return,
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "RGB tile Zstd Predictor=1 pixel mismatch");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decode_64x64_rgb24_zstd_tile_predictor2() {
    if !tiffcp_supports_zstd() {
        eprintln!("skipping: tiffcp lacks zstd support");
        return;
    }
    let dir = tmp_dir("rgb-tile-p2");
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let tiff = match make_zstd_tiff(
        &dir,
        "ppm",
        &ppm,
        &["-t", "-w", "32", "-l", "32", "-c", "zstd:2"],
    ) {
        Some(b) => b,
        None => return,
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "RGB tile Zstd Predictor=2 pixel mismatch");
    let _ = fs::remove_dir_all(&dir);
}
