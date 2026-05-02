//! Integration tests that synthesise tiny TIFF fixtures via the
//! `convert` (ImageMagick) binary as a black-box validator, then
//! decode them with our decoder and check we get back the same
//! pixels.
//!
//! Per workspace policy: ImageMagick is used as a *binary* only
//! (input goes in, bytes come out). We never look at its source.
//!
//! Tests are gated on the binary being available; if it's missing
//! they print a warning and pass (so CI without imagemagick still
//! goes green).

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use oxideav_tiff::{decode_tiff, DecodedTiff};

fn convert_available() -> bool {
    Command::new("convert")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write `bytes` to a tmp file with extension `ext`, run `convert
/// in.<src_ext> [convert_args...] out.tiff`, return the resulting
/// .tiff bytes.
fn convert_to_tiff(input_bytes: &[u8], src_ext: &str, convert_args: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-test-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join(format!("in.{src_ext}"));
    let out_path = dir.join("out.tiff");
    fs::File::create(&in_path)
        .ok()?
        .write_all(input_bytes)
        .ok()?;

    let mut cmd = Command::new("convert");
    cmd.arg(&in_path);
    for a in convert_args {
        cmd.arg(a);
    }
    cmd.arg(&out_path);
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("convert failed: {status:?}");
        return None;
    }
    let bytes = fs::read(&out_path).ok();
    let _ = fs::remove_dir_all(&dir);
    bytes
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n}")
}

/// Build a tiny PPM (RGB) source for `convert` to ingest.
fn make_ppm_rgb(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), (w * h * 3) as usize);
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(pixels);
    v
}

/// Build a tiny PGM (gray8) source for `convert` to ingest.
fn make_pgm_gray(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), (w * h) as usize);
    let mut v = format!("P5\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(pixels);
    v
}

/// 64x64 RGB pattern: gradient X/Y/diagonal.
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

fn frame_to_rgb24_bytes(d: &DecodedTiff) -> Vec<u8> {
    // The frame is single-plane; row stride may be tighter than
    // width*3 only if we somehow added padding (we don't, so they
    // should be equal).
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

#[test]
fn decode_64x64_rgb_uncompressed_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    // -compress none → Compression=1
    let tiff = match convert_to_tiff(&ppm, "ppm", &["-compress", "none"]) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "RGB uncompressed pixels mismatch");
}

#[test]
fn decode_64x64_rgb_packbits_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let tiff = match convert_to_tiff(&ppm, "ppm", &["-compress", "rle"]) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "RGB PackBits pixels mismatch");
}

#[test]
fn decode_64x64_rgb_lzw_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let tiff = match convert_to_tiff(&ppm, "ppm", &["-compress", "lzw"]) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "RGB LZW pixels mismatch");
}

#[test]
fn decode_64x64_gray8_packbits_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    // Gray8 ramp.
    let mut pixels = Vec::with_capacity(64 * 64);
    for y in 0u8..64 {
        for x in 0u8..64 {
            pixels.push(x.wrapping_add(y).wrapping_mul(2));
        }
    }
    let pgm = make_pgm_gray(64, 64, &pixels);
    let tiff = match convert_to_tiff(&pgm, "pgm", &["-compress", "rle"]) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_gray8_bytes(&d);
    assert_eq!(got, pixels, "Gray8 PackBits pixels mismatch");
}

#[test]
fn decode_64x64_rgb_deflate_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let tiff = match convert_to_tiff(&ppm, "ppm", &["-compress", "zip"]) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce TIFF (zip support may be missing)");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "RGB Deflate pixels mismatch");
}

/// Quick sanity check: end-to-end through the registry's probe_input
/// using a minimal hand-built TIFF header — confirms the
/// container is registered and the probe wins.
#[test]
fn probe_recognises_minimal_le_tiff() {
    use std::io::Cursor;
    let buf = [b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00];
    let mut cursor = Cursor::new(&buf[..]);
    let mut reg = oxideav_core::ContainerRegistry::new();
    oxideav_tiff::register_containers(&mut reg);
    let name = reg.probe_input(&mut cursor, Some("tiff")).expect("probe");
    assert_eq!(name, "tiff");
}

#[test]
fn probe_recognises_minimal_be_tiff() {
    use std::io::Cursor;
    let buf = [b'M', b'M', 0x00, 0x2A, 0x00, 0x00, 0x00, 0x08];
    let mut cursor = Cursor::new(&buf[..]);
    let mut reg = oxideav_core::ContainerRegistry::new();
    oxideav_tiff::register_containers(&mut reg);
    let name = reg.probe_input(&mut cursor, Some("tiff")).expect("probe");
    assert_eq!(name, "tiff");
}

/// Silence the unused PathBuf import warning when the file is read
/// without all tests being enabled (PathBuf is genuinely useful for
/// debugging when one of the convert calls fails in CI logs).
#[allow(dead_code)]
fn _silence_unused_import_warnings() {
    let _: PathBuf = PathBuf::new();
}
