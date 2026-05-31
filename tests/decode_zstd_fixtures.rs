//! Integration tests for `Compression = 50000` (Zstandard) decode.
//!
//! Per `docs/image/tiff/tiff-zstd-compression-50000.md` §3, every
//! strip / tile of a `Compression = 50000` TIFF is **one self-contained
//! zstd frame** (RFC 8878, magic `0x28B52FFD`) carrying the
//! post-`Predictor` byte stream. The decode test plan is therefore the
//! same shape used for the CCITT / ImageMagick fixture tests:
//!
//! 1. Synthesise a tiny uncompressed TIFF with `convert` (ImageMagick).
//! 2. Re-encode it with `tiffcp -c zstd[:opts]` (libtiff binary). This
//!    is the only externally-available encoder for `Compression =
//!    50000`; ImageMagick has no Q-coder for it.
//! 3. Decode both the uncompressed reference and the zstd-compressed
//!    copy with our `decode_tiff` and assert byte-identical pixels.
//!
//! Per workspace policy, `convert` / `tiffcp` are used as **black-box
//! validator binaries only** — we never read their source. The test
//! skips gracefully (printing a notice and passing) when either binary
//! is missing, so CI without libtiff installed stays green.
//!
//! Strip and tile, with and without `Predictor = 2`, across single-
//! and multi-component photometrics, are all exercised below.

#[cfg(feature = "zstd")]
use std::path::PathBuf;
#[cfg(feature = "zstd")]
use std::process::{Command, Stdio};

use oxideav_tiff::decode_tiff;

#[cfg(feature = "zstd")]
fn bin_available(name: &str) -> bool {
    Command::new(name)
        .arg("-h")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| true)
        .unwrap_or(false)
}

/// `tiffcp` only emits Compression=50000 when libtiff was built with
/// `--enable-zstd` (Homebrew's libtiff is; some distro packages are
/// not). Run `tiffcp -h` once and grep its capability listing.
#[cfg(feature = "zstd")]
fn tiffcp_has_zstd() -> bool {
    let out = match Command::new("tiffcp").arg("-h").output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let text2 = String::from_utf8_lossy(&out.stderr);
    text.contains("-c zstd") || text2.contains("-c zstd")
}

#[cfg(feature = "zstd")]
fn must_have_tooling() -> bool {
    if !bin_available("convert") {
        eprintln!("[oxideav-tiff/zstd-fixtures] skipping: `convert` (ImageMagick) not on PATH");
        return false;
    }
    if !bin_available("tiffcp") {
        eprintln!("[oxideav-tiff/zstd-fixtures] skipping: `tiffcp` (libtiff) not on PATH");
        return false;
    }
    if !tiffcp_has_zstd() {
        eprintln!(
            "[oxideav-tiff/zstd-fixtures] skipping: `tiffcp` build lacks `-c zstd` \
             (rebuild libtiff with --enable-zstd)"
        );
        return false;
    }
    true
}

#[cfg(feature = "zstd")]
fn unique_tmp(prefix: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("{prefix}-{pid}-{nanos}-{seq}"));
    p
}

/// `magick -size WxH gradient: -depth 8 -compress none <path>` — a
/// deterministic grayscale ramp. We use the ImageMagick `gradient:`
/// generator (top→bottom black→white) because it produces non-flat
/// pixels, which guarantees the zstd round-trip exercises the entropy
/// decoder rather than a trivially-compressible all-one-byte stream.
#[cfg(feature = "zstd")]
fn make_gray8_tiff(path: &PathBuf, w: u32, h: u32) {
    let out = Command::new("magick")
        .arg("-size")
        .arg(format!("{w}x{h}"))
        .arg("gradient:")
        .arg("-colorspace")
        .arg("Gray")
        .arg("-depth")
        .arg("8")
        .arg("-compress")
        .arg("none")
        .arg(path)
        .output()
        .expect("magick spawn");
    if !out.status.success() {
        panic!(
            "magick gradient -> tiff failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `magick -size WxH plasma:fractal -depth 8 -compress none <path>`
/// — RGB pseudo-fractal noise. Plasma noise is 3-channel and varies
/// enough that the horizontal predictor saves a non-trivial number of
/// bytes, so the `Predictor = 2 + zstd` path actually has work to do.
#[cfg(feature = "zstd")]
fn make_rgb_tiff(path: &PathBuf, w: u32, h: u32) {
    let out = Command::new("magick")
        .arg("-size")
        .arg(format!("{w}x{h}"))
        .arg("plasma:fractal")
        .arg("-depth")
        .arg("8")
        .arg("-compress")
        .arg("none")
        .arg(path)
        .output()
        .expect("magick spawn");
    if !out.status.success() {
        panic!(
            "magick plasma -> tiff failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `tiffcp [extra_args...] -c <opts> src dst`. The extra args slot in
/// before `-c` so callers can request tiling (`-t -w 16 -l 16`) or
/// the horizontal-differencing predictor (`-p`).
#[cfg(feature = "zstd")]
fn tiffcp(src: &PathBuf, dst: &PathBuf, copts: &str, extra: &[&str]) {
    let mut cmd = Command::new("tiffcp");
    for a in extra {
        cmd.arg(a);
    }
    cmd.arg("-c").arg(copts).arg(src).arg(dst);
    let out = cmd.output().expect("tiffcp spawn");
    if !out.status.success() {
        panic!(
            "tiffcp {extra:?} -c {copts} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(feature = "zstd")]
fn decode_pixels(path: &PathBuf) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("read tiff");
    let img = decode_tiff(&bytes).expect("decode_tiff");
    img.frame.planes[0].data.clone()
}

/// Pipe a uncompressed reference TIFF through `tiffcp -c zstd …` and
/// assert our decoder produces byte-identical pixels for both the
/// uncompressed reference and the zstd-compressed copy. Returns the
/// compressed-file byte size so the caller can sanity-check that
/// compression actually happened.
#[cfg(feature = "zstd")]
fn run_roundtrip(
    label: &str,
    mk: fn(&PathBuf, u32, u32),
    w: u32,
    h: u32,
    copts: &str,
    extra: &[&str],
) -> u64 {
    if !must_have_tooling() {
        return 0;
    }
    let base = unique_tmp(&format!("oxideav-tiff-zstd-{label}"));
    let raw = base.with_extension("none.tif");
    let comp = base.with_extension("zstd.tif");
    mk(&raw, w, h);
    tiffcp(&raw, &comp, copts, extra);

    let comp_size = std::fs::metadata(&comp).expect("stat compressed").len();
    let pix_ref = decode_pixels(&raw);
    let pix_got = decode_pixels(&comp);

    let _ = std::fs::remove_file(&raw);
    let _ = std::fs::remove_file(&comp);

    assert_eq!(
        pix_got.len(),
        pix_ref.len(),
        "{label}: length mismatch (zstd-decoded {} vs uncompressed reference {})",
        pix_got.len(),
        pix_ref.len()
    );
    if pix_got != pix_ref {
        let first = pix_got.iter().zip(pix_ref.iter()).position(|(a, b)| a != b);
        panic!(
            "{label}: pixel mismatch at byte {first:?} (got len={}, ref len={})",
            pix_got.len(),
            pix_ref.len()
        );
    }
    comp_size
}

// -------------------------------------------------------------------------
// Compression=50000 strip-organised. Predictor=1 first, then the
// horizontal-differencing variant (`-p` on the tiffcp side requests
// Predictor=2). All gated on the default-on `zstd` Cargo feature —
// the `--no-default-features` build path returns a precise
// feature-gate error rather than decoding, so these round-trips don't
// apply there.
// -------------------------------------------------------------------------

#[cfg(feature = "zstd")]
#[test]
fn zstd_gray8_strips_predictor1_32x32() {
    run_roundtrip("g8_s_p1_32", make_gray8_tiff, 32, 32, "zstd", &[]);
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_gray8_strips_predictor1_128x64() {
    run_roundtrip("g8_s_p1_128", make_gray8_tiff, 128, 64, "zstd", &[]);
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_gray8_strips_predictor2_128x64() {
    // `tiffcp -c zstd:p2` carries Predictor=2 into the post-compress
    // stream the libtiff way. The decoder reverses the predictor after
    // ZSTD-decoding, so the recovered pixels must match the
    // uncompressed reference even though the zstd-compressed byte
    // stream is the *differenced* (not the literal-pixel) bytes.
    run_roundtrip("g8_s_p2_128", make_gray8_tiff, 128, 64, "zstd:p2", &[]);
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_rgb24_strips_predictor1_64x64() {
    run_roundtrip("rgb_s_p1_64", make_rgb_tiff, 64, 64, "zstd", &[]);
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_rgb24_strips_predictor2_64x64() {
    // RGB chunky + horizontal differencing: the predictor is applied
    // per-component within each scan row.
    run_roundtrip("rgb_s_p2_64", make_rgb_tiff, 64, 64, "zstd:p2", &[]);
}

// -------------------------------------------------------------------------
// Compression=50000 tile-organised. `tiffcp -t -w W -l L` carries
// the tiled layout. Each tile is its own standalone zstd frame.
// -------------------------------------------------------------------------

#[cfg(feature = "zstd")]
#[test]
fn zstd_gray8_tiled_predictor1_128x128_tile32() {
    run_roundtrip(
        "g8_t_p1_128",
        make_gray8_tiff,
        128,
        128,
        "zstd",
        &["-t", "-w", "32", "-l", "32"],
    );
}

#[cfg(feature = "zstd")]
#[test]
fn zstd_rgb24_tiled_predictor2_128x128_tile32() {
    run_roundtrip(
        "rgb_t_p2_128",
        make_rgb_tiff,
        128,
        128,
        "zstd:p2",
        &["-t", "-w", "32", "-l", "32"],
    );
}

// -------------------------------------------------------------------------
// Negative paths. These run unconditionally (no validator binary
// required) — they exercise the decoder's failure-mode handling for
// malformed Compression=50000 strips, which is in scope regardless of
// whether `tiffcp` is on PATH.
// -------------------------------------------------------------------------

/// Build the smallest legal TIFF carrying one synthetic strip whose
/// payload is `strip_bytes`. The IFD declares `Compression =
/// compression`, 1×1 BlackIsZero Gray8, one strip of one row.
/// `RowsPerStrip` and `ImageLength` both = 1, so the un-compressed
/// strip size is one byte. The function is small enough that we
/// hand-build the bytes from the layout rather than dragging in the
/// encoder — the encoder doesn't write Compression=50000 in this
/// round.
fn synthesise_min_strip_tiff(compression: u16, strip_bytes: &[u8]) -> Vec<u8> {
    // Little-endian classic TIFF (magic 42). IFD at offset 8 has six
    // entries: ImageWidth (256), ImageLength (257), BitsPerSample (258),
    // Compression (259), PhotometricInterpretation (262),
    // StripOffsets (273), StripByteCounts (279), SamplesPerPixel (277),
    // RowsPerStrip (278).
    //
    // Layout:
    //   header  (8 bytes)         off 0..8
    //   IFD     (2 + 9*12 + 4)    off 8..(8 + 2 + 108 + 4) = 8..122
    //   strip                      off 122..
    let entries: [(u16, u16, u32, u32); 9] = [
        (256, 3, 1, 1), // ImageWidth = 1
        (257, 3, 1, 1), // ImageLength = 1
        (258, 3, 1, 8), // BitsPerSample = 8
        (259, 3, 1, compression as u32),
        (262, 3, 1, 1),                        // BlackIsZero
        (273, 4, 1, 122),                      // StripOffsets
        (277, 3, 1, 1),                        // SamplesPerPixel
        (278, 3, 1, 1),                        // RowsPerStrip
        (279, 4, 1, strip_bytes.len() as u32), // StripByteCounts
    ];
    let mut out = Vec::with_capacity(122 + strip_bytes.len());
    out.extend_from_slice(b"II"); // little-endian
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes()); // IFD at offset 8
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, ty, cnt, val) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&cnt.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD = none
    debug_assert_eq!(out.len(), 122);
    out.extend_from_slice(strip_bytes);
    out
}

/// `decode_tiff` returns `DecodedTiff` (which does not implement
/// `Debug`), so we can't use `expect_err`. Map to a tiny helper that
/// asserts on the error path explicitly.
fn must_fail(tiff: &[u8], label: &str) -> String {
    match decode_tiff(tiff) {
        Ok(_) => panic!("{label}: expected an error, got Ok"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn zstd_strip_missing_magic_is_rejected() {
    // Compression=50000 with a strip that doesn't start with the
    // 0x28B52FFD zstd frame magic. Should surface a precise decode
    // error instead of panicking inside the zstd decoder.
    let tiff = synthesise_min_strip_tiff(50000, b"\x00\x00\x00\x00ZZZZ");
    let msg = must_fail(&tiff, "missing-magic");
    assert!(
        msg.contains("ZSTD") || msg.contains("zstd"),
        "expected ZSTD error, got {msg:?}"
    );
}

#[test]
fn zstd_strip_truncated_is_rejected() {
    // Compression=50000 with a truncated zstd frame (just the magic,
    // no frame header). ruzstd's frame init must fail; our wrapper
    // turns that into a TIFF decode error.
    let tiff = synthesise_min_strip_tiff(50000, b"\x28\xb5\x2f\xfd");
    let msg = must_fail(&tiff, "truncated");
    assert!(
        msg.contains("ZSTD") || msg.contains("zstd"),
        "expected ZSTD error, got {msg:?}"
    );
}
