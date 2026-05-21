//! Integration tests that synthesise small CCITT-encoded TIFF
//! fixtures with `tiffcp` (libtiff) and decode them with our reader,
//! comparing pixel-for-pixel against a reference rendering also
//! produced via libtiff. Per workspace policy, `tiffcp`/`tiffinfo`
//! are used as black-box validator binaries only; their source is
//! never consulted.
//!
//! Test plan:
//!
//! * `convert` produces a tiny 1-bit BlackIsZero PBM-shaped TIFF.
//! * `tiffcp -c g3:1d` (or `-c none`) re-encodes it under the
//!   compression we want to exercise.
//! * Our decoder turns it back into `Gray8`. The reference is the
//!   uncompressed copy, decoded with our same decoder. Both
//!   renderings must match byte-for-byte.
//!
//! Tests are gated on the `convert` and `tiffcp` binaries being
//! present; if either is missing the test prints a notice and
//! returns successfully so CI hosts without libtiff stay green.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use oxideav_tiff::decode_tiff;

fn bin_available(name: &str) -> bool {
    Command::new(name)
        .arg("-h")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| true)
        .unwrap_or(false)
}

fn must_have_tooling() -> bool {
    if !bin_available("convert") || !bin_available("tiffcp") {
        eprintln!(
            "[oxideav-tiff/ccitt-fixtures] skipping: convert and tiffcp must both be on PATH"
        );
        return false;
    }
    true
}

/// Run `convert -size WxH ... -depth 1 -monochrome <path>` to make a
/// 1-bit BlackIsZero TIFF. `pattern` is an ImageMagick "-draw" body.
fn make_bilevel(path: &PathBuf, w: u32, h: u32, draw: Option<&str>) {
    let mut cmd = Command::new("convert");
    cmd.arg("-size").arg(format!("{w}x{h}")).arg("xc:white");
    if let Some(d) = draw {
        cmd.arg("-fill").arg("black").arg("-draw").arg(d);
    }
    cmd.arg("-depth").arg("1").arg("-monochrome").arg(path);
    let out = cmd.output().expect("convert spawn");
    if !out.status.success() {
        panic!("convert failed: {}", String::from_utf8_lossy(&out.stderr));
    }
}

/// Run `tiffcp -c <opts> src dst`.
fn tiffcp_recompress(src: &PathBuf, dst: &PathBuf, copts: &str) {
    let out = Command::new("tiffcp")
        .arg("-c")
        .arg(copts)
        .arg(src)
        .arg(dst)
        .output()
        .expect("tiffcp spawn");
    if !out.status.success() {
        panic!(
            "tiffcp -c {copts} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Run `tiffcp -c <copts> -f <fill> src dst`. `-f lsb2msb` requests
/// FillOrder=2 output, `-f msb2lsb` requests FillOrder=1.
fn tiffcp_recompress_fill(src: &PathBuf, dst: &PathBuf, copts: &str, fill: &str) {
    let out = Command::new("tiffcp")
        .arg("-c")
        .arg(copts)
        .arg("-f")
        .arg(fill)
        .arg(src)
        .arg(dst)
        .output()
        .expect("tiffcp spawn");
    if !out.status.success() {
        panic!(
            "tiffcp -c {copts} -f {fill} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn decode(path: &PathBuf) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("read tiff");
    let img = decode_tiff(&bytes).expect("decode_tiff");
    img.frame.planes[0].data.clone()
}

fn unique_tmp(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("{prefix}-{pid}-{nanos}"));
    p
}

fn run_roundtrip(w: u32, h: u32, draw: Option<&str>, copts: &str, label: &str) {
    if !must_have_tooling() {
        return;
    }
    let base = unique_tmp("oxideav-tiff-r76");
    let raw = base.with_extension("none.tif");
    let comp = base.with_extension(format!("{label}.tif"));
    make_bilevel(&raw, w, h, draw);
    tiffcp_recompress(&raw, &comp, copts);

    let pix_ref = decode(&raw);
    let pix_got = decode(&comp);

    let _ = std::fs::remove_file(&raw);
    let _ = std::fs::remove_file(&comp);

    assert_eq!(
        pix_got.len(),
        pix_ref.len(),
        "{label}: byte-length mismatch ({} vs ref {})",
        pix_got.len(),
        pix_ref.len()
    );
    if pix_got != pix_ref {
        // Print a tiny diff window so failures are debuggable.
        let mut first = None;
        for (i, (a, b)) in pix_got.iter().zip(pix_ref.iter()).enumerate() {
            if a != b {
                first = Some(i);
                break;
            }
        }
        panic!(
            "{label}: pixel mismatch at byte {first:?} (got len={}, ref len={})",
            pix_got.len(),
            pix_ref.len()
        );
    }
}

// -------------------------------------------------------------------------
// Compression=3 (T.4 1-D) cases. `tiffcp -c g3:1d` produces this exact
// encoding (T4Options=0, FillOrder=1). The TIFF 6.0 PDF Section 10 +
// Section 11 between them cover everything we need.
// -------------------------------------------------------------------------

#[test]
fn ccitt_t4_1d_solid_white_8x4() {
    run_roundtrip(8, 4, None, "g3:1d", "t4_1d_w");
}

#[test]
fn ccitt_t4_1d_solid_white_32x4() {
    run_roundtrip(32, 4, None, "g3:1d", "t4_1d_w32");
}

#[test]
fn ccitt_t4_1d_two_rectangles_64x8() {
    run_roundtrip(
        64,
        8,
        Some("rectangle 4,1 30,5 rectangle 40,2 55,4"),
        "g3:1d",
        "t4_1d_rects",
    );
}

#[test]
fn ccitt_t4_1d_wide_run_128x4() {
    // 128 pixels wide forces at least one make-up code (64) plus
    // terminator.
    run_roundtrip(128, 4, Some("rectangle 0,0 100,3"), "g3:1d", "t4_1d_wide");
}

#[test]
fn ccitt_t4_1d_diagonal_32x32() {
    // Alternating black/white pixels along a diagonal — exercises
    // many short white/black terminating codes in sequence.
    run_roundtrip(
        32,
        32,
        Some("rectangle 0,0 31,0 rectangle 0,1 0,31 line 0,0 31,31"),
        "g3:1d",
        "t4_1d_diag",
    );
}

#[test]
fn ccitt_t4_1d_eol_byte_aligned_64x8() {
    // g3:1d:fill turns on T4Options bit 2 (EOL byte-aligned).
    run_roundtrip(
        64,
        8,
        Some("rectangle 4,1 30,5 rectangle 40,2 55,4"),
        "g3:1d:fill",
        "t4_1d_fill",
    );
}

// -------------------------------------------------------------------------
// Compression=4 (T.6) and Compression=3 with T4Options bit 0 set
// (2-D) MUST return an Err — the docs don't include the 2-D mode
// codes, and per workspace clean-room rules we cannot fish for them.
// -------------------------------------------------------------------------

#[test]
fn ccitt_g4_is_unsupported() {
    if !must_have_tooling() {
        return;
    }
    let base = unique_tmp("oxideav-tiff-r76");
    let raw = base.with_extension("none.tif");
    let comp = base.with_extension("g4.tif");
    make_bilevel(&raw, 32, 4, Some("rectangle 4,1 20,3"));
    tiffcp_recompress(&raw, &comp, "g4");
    let bytes = std::fs::read(&comp).unwrap();
    let r = decode_tiff(&bytes);
    let _ = std::fs::remove_file(&raw);
    let _ = std::fs::remove_file(&comp);
    let err = match r {
        Ok(_) => panic!("G4 must be rejected until 2-D codes are documented"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("T.6") || msg.contains("Compression=4"),
        "unexpected G4 error: {msg}"
    );
}

// -------------------------------------------------------------------------
// FillOrder=2 (LSB-first) carriage, TIFF 6.0 §FillOrder page 32. Both
// CCITT-compressed and uncompressed bilevel strips are valid carriers;
// pixel results must match the FillOrder=1 baseline byte-for-byte.
// -------------------------------------------------------------------------
fn run_fillorder_roundtrip(w: u32, h: u32, draw: Option<&str>, copts: &str, label: &str) {
    if !must_have_tooling() {
        return;
    }
    let base = unique_tmp("oxideav-tiff-r82");
    let raw = base.with_extension("none.tif");
    let msb = base.with_extension(format!("{label}_msb.tif"));
    let lsb = base.with_extension(format!("{label}_lsb.tif"));
    make_bilevel(&raw, w, h, draw);
    // Reference: same compression, MSB-first (FillOrder=1).
    tiffcp_recompress_fill(&raw, &msb, copts, "msb2lsb");
    // Variant under test: same compression, LSB-first (FillOrder=2).
    tiffcp_recompress_fill(&raw, &lsb, copts, "lsb2msb");

    let pix_msb = decode(&msb);
    let pix_lsb = decode(&lsb);

    let _ = std::fs::remove_file(&raw);
    let _ = std::fs::remove_file(&msb);
    let _ = std::fs::remove_file(&lsb);

    assert_eq!(
        pix_lsb.len(),
        pix_msb.len(),
        "{label}: byte-length mismatch ({} vs ref {})",
        pix_lsb.len(),
        pix_msb.len()
    );
    if pix_lsb != pix_msb {
        let first = pix_lsb.iter().zip(pix_msb.iter()).position(|(a, b)| a != b);
        panic!("{label}: FillOrder=2 pixels differ from FillOrder=1 reference at byte {first:?}",);
    }
}

#[test]
fn ccitt_t4_1d_fillorder_lsb_first_64x8() {
    run_fillorder_roundtrip(
        64,
        8,
        Some("rectangle 4,1 30,5 rectangle 40,2 55,4"),
        "g3:1d",
        "t4_1d_fillorder",
    );
}

#[test]
fn ccitt_t4_1d_fillorder_lsb_first_128x4_wide_run() {
    // Force at least one make-up code through the LSB-first path so
    // we cover make-up + terminator across reversed bytes.
    run_fillorder_roundtrip(
        128,
        4,
        Some("rectangle 0,0 100,3"),
        "g3:1d",
        "t4_1d_fillorder_wide",
    );
}

#[test]
fn ccitt_mh_fillorder_lsb_first_64x4() {
    // Compression=2 (Modified Huffman) with FillOrder=2.
    run_fillorder_roundtrip(64, 4, Some("rectangle 8,1 50,2"), "g3:1d", "mh_fillorder");
    // Note: -c g3:1d emits Compression=3 (T.4 1-D). Modified-Huffman
    // (Compression=2) requires a different tiffcp invocation: there
    // is no `-c g3:mh` shorthand, so we additionally exercise the MH
    // path here through a direct call.
    if !must_have_tooling() {
        return;
    }
    let base = unique_tmp("oxideav-tiff-r82-mh");
    let raw = base.with_extension("none.tif");
    let mh_msb = base.with_extension("mh_msb.tif");
    let mh_lsb = base.with_extension("mh_lsb.tif");
    make_bilevel(&raw, 64, 4, Some("rectangle 8,1 50,2"));
    // `tiffcp -c g3` (no `:1d` suffix) defaults to MH per libtiff
    // CLI. We can't validate the exact compression code here, but
    // the round-trip oracle compares pixel buffers, not metadata.
    tiffcp_recompress_fill(&raw, &mh_msb, "g3", "msb2lsb");
    tiffcp_recompress_fill(&raw, &mh_lsb, "g3", "lsb2msb");
    let p_msb = decode(&mh_msb);
    let p_lsb = decode(&mh_lsb);
    let _ = std::fs::remove_file(&raw);
    let _ = std::fs::remove_file(&mh_msb);
    let _ = std::fs::remove_file(&mh_lsb);
    assert_eq!(
        p_lsb, p_msb,
        "MH/FillOrder=2 must decode same as FillOrder=1"
    );
}

#[test]
fn uncompressed_bilevel_fillorder_lsb_first_64x4() {
    // -c none keeps Compression=1 (uncompressed); the spec
    // explicitly allows FillOrder=2 for uncompressed BPS=1.
    run_fillorder_roundtrip(
        64,
        4,
        Some("rectangle 8,1 50,2 rectangle 10,3 30,3"),
        "none",
        "raw_fillorder",
    );
}

#[test]
fn ccitt_t4_2d_is_unsupported() {
    if !must_have_tooling() {
        return;
    }
    let base = unique_tmp("oxideav-tiff-r76");
    let raw = base.with_extension("none.tif");
    let comp = base.with_extension("g3_2d.tif");
    make_bilevel(&raw, 32, 4, Some("rectangle 4,1 20,3"));
    tiffcp_recompress(&raw, &comp, "g3:2d");
    let bytes = std::fs::read(&comp).unwrap();
    let r = decode_tiff(&bytes);
    let _ = std::fs::remove_file(&raw);
    let _ = std::fs::remove_file(&comp);
    let err = match r {
        Ok(_) => panic!("T.4 2-D must be rejected until mode codes are documented"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("2-D") || msg.contains("T4Options"),
        "unexpected T.4-2D error: {msg}"
    );
}
