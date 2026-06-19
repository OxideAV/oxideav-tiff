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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    // A monotonic process-global counter guarantees uniqueness even
    // when two parallel test threads sample the same wall-clock
    // nanosecond — `SystemTime::now()` alone is not collision-proof
    // under `cargo test`'s thread pool, which let two fixtures share a
    // temp directory (and one read the other's `out.tiff`).
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{n}-{c}")
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
#[cfg(feature = "registry")]
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

#[cfg(feature = "registry")]
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

/// JPEG-in-TIFF (Compression=7, per TIFF Tech Note 2): each strip is
/// itself a freestanding JPEG datastream with the optional
/// `JPEGTables` (tag 347) holding shared DQT/DHT tables.
/// ImageMagick's default for `-compress jpeg` from a PPM is
/// `Photometric=RGB` (no chroma subsampling). JPEG is lossy so we
/// compare with a generous PSNR-like tolerance rather than asserting
/// bit-exactness.
///
/// JPEG-in-TIFF decode lives behind the `registry` feature (the JPEG
/// codec is `oxideav-mjpeg`); without it `decode_tiff` returns
/// `Error::Unsupported` for Compression=7, so this test only applies
/// to the default (registry-on) build.
#[cfg(feature = "registry")]
#[test]
fn decode_64x64_rgb_jpeg_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    // `-quality 95` to keep the lossy reconstruction close to the
    // input; we still allow per-channel slop below.
    let tiff = match convert_to_tiff(&ppm, "ppm", &["-compress", "jpeg", "-quality", "95"]) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce JPEG-TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff (JPEG-in-TIFF, RGB) failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 200.0,
        "RGB JPEG-in-TIFF reconstruction too far from input: MSE={mse}"
    );
}

/// Grayscale JPEG-in-TIFF (Photometric=BlackIsZero, SamplesPerPixel=1).
/// ImageMagick produces this for a `-compress jpeg` PGM input.
/// Registry-gated like the RGB-JPEG case above.
#[cfg(feature = "registry")]
#[test]
fn decode_64x64_gray_jpeg_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let mut pixels = Vec::with_capacity(64 * 64);
    for y in 0u8..64 {
        for x in 0u8..64 {
            pixels.push(x.wrapping_add(y).wrapping_mul(2));
        }
    }
    let pgm = make_pgm_gray(64, 64, &pixels);
    let tiff = match convert_to_tiff(&pgm, "pgm", &["-compress", "jpeg", "-quality", "95"]) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce JPEG-TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff (JPEG-in-TIFF, gray) failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_gray8_bytes(&d);
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 200.0,
        "Gray JPEG-in-TIFF reconstruction too far from input: MSE={mse}"
    );
}

/// YCbCr JPEG-in-TIFF (Photometric=YCbCr, SamplesPerPixel=3,
/// JPEGTables present). Produced by `tiffcp -c jpeg` on an
/// uncompressed RGB TIFF — that path picks the YCbCr photometric +
/// the default 2:2 chroma subsampling, which exercises our 4:2:0
/// composite path. Registry-gated like the other JPEG-in-TIFF cases.
#[cfg(feature = "registry")]
#[test]
fn decode_64x64_ycbcr_jpeg_tiffcp() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    if Command::new("tiffcp")
        .arg("-h")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        // tiffcp returns non-zero for -h; fall back to checking
        // version probe which exits 0.
        if Command::new("tiffcp")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipping: `tiffcp` not available");
            return;
        }
    }
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let uncompressed = match convert_to_tiff(&ppm, "ppm", &["-compress", "none"]) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce uncompressed TIFF");
            return;
        }
    };

    // Stage the uncompressed TIFF in a tmp dir, run `tiffcp -c jpeg`,
    // read back the result.
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-jpeg-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let in_path = dir.join("in.tiff");
    let out_path = dir.join("out.tiff");
    std::fs::write(&in_path, &uncompressed).unwrap();
    let status = Command::new("tiffcp")
        .args(["-c", "jpeg"])
        .arg(&in_path)
        .arg(&out_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();
    let Some(s) = status else {
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("skipping: tiffcp invocation failed");
        return;
    };
    if !s.success() {
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("skipping: tiffcp -c jpeg failed");
        return;
    }
    let tiff = std::fs::read(&out_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let d = decode_tiff(&tiff).expect("decode_tiff (YCbCr JPEG-in-TIFF) failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    // Default tiffcp quality + 2:2 chroma subsampling: looser
    // tolerance than the RGB-JPEG path because chroma is downsampled.
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 1500.0,
        "YCbCr JPEG-in-TIFF reconstruction too far from input: MSE={mse}"
    );
}

/// CMYK JPEG-in-TIFF (Photometric=CMYK (5), SamplesPerPixel=4).
/// `convert -colorspace CMYK -compress jpeg` produces a 4-component
/// JPEG datastream wrapped in TIFF (per TN2: "A JPEG-compressed TIFF
/// file will typically have PhotometricInterpretation = YCbCr ...
/// unless the source data was grayscale or CMYK"). `oxideav-mjpeg`
/// produces a single packed CMYK plane; the TIFF compositor converts
/// to Rgb24 using the same additive-RGB formula the uncompressed
/// CMYK path uses (TIFF 6.0 §16, InkSet=1). The round-trip is lossy
/// (JPEG quantisation + CMYK→RGB colour conversion in ImageMagick)
/// so we only assert (a) decode succeeds, (b) output dimensions
/// match, (c) output is Rgb24-sized, and (d) sanity-check that the
/// result isn't all-zero / all-saturated.
#[cfg(feature = "registry")]
#[test]
fn decode_64x64_cmyk_jpeg_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    // -colorspace CMYK + -compress jpeg: ImageMagick converts to a
    // 4-component CMYK image then JPEG-compresses (typically without
    // chroma subsampling for CMYK).
    let tiff = match convert_to_tiff(
        &ppm,
        "ppm",
        &["-colorspace", "CMYK", "-compress", "jpeg", "-quality", "90"],
    ) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce CMYK JPEG-TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff (CMYK JPEG-in-TIFF) failed");
    assert_eq!((d.width, d.height), (64, 64));
    // Output is Rgb24 (CMYK -> additive RGB).
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got.len(), 64 * 64 * 3);
    // Sanity: not all-zero (would indicate decode failure or
    // all-ink composite collapse to black).
    let nonzero = got.iter().filter(|&&b| b != 0).count();
    assert!(
        nonzero > got.len() / 10,
        "CMYK JPEG decode produced suspiciously many zeros: {nonzero}/{}",
        got.len()
    );
    // Sanity: not all-saturated.
    let unsat = got.iter().filter(|&&b| b != 255).count();
    assert!(
        unsat > got.len() / 10,
        "CMYK JPEG decode produced suspiciously many 255s: {unsat}/{}",
        got.len()
    );
    // Compare to the round-tripped pixels with a very loose
    // tolerance — CMYK colour conversion + JPEG quant adds
    // ImageMagick-side variation we cannot match exactly.
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 6000.0,
        "CMYK JPEG-in-TIFF reconstruction too far from input: MSE={mse}"
    );
}

/// Verify we reject Compression=6 (deprecated old-style JPEG per
/// TN2) with a precise Unsupported error rather than mis-decoding.
#[test]
fn reject_old_style_jpeg_compression_6() {
    use oxideav_tiff::types::COMPRESSION_JPEG_OLD;
    // Hand-build the minimum IFD that just sets Compression=6 plus
    // the other mandatory tags. We don't even need real pixel data —
    // the IFD walker should bail out before reaching strip decode.
    let tiff = build_compression_only_tiff(COMPRESSION_JPEG_OLD);
    match decode_tiff(&tiff) {
        Ok(_) => panic!("Compression=6 should be rejected, decode succeeded"),
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                msg.contains("Compression=6") || msg.contains("old-style") || msg.contains("JPEG"),
                "expected Compression=6 error, got {msg}"
            );
        }
    }
}

/// Sum of squared per-byte differences divided by the byte count —
/// suitable for "is the reconstruction close enough?" assertions
/// without dragging in a real PSNR library. Only the (registry-gated)
/// lossy JPEG-in-TIFF tests need it.
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

/// **Tiled** JPEG-in-TIFF (Compression=7, TileWidth/TileLength present).
/// Exercises `decode_ifd_jpeg_tiles`: every tile is a self-contained
/// JPEG datastream and the decoder composites the tile grid (with edge
/// tiles clipped to the image bounds) into the output plane. TIFF Tech
/// Note 2 §"Tiles" carries JPEG exactly as the strip case, one
/// datastream per tile. RGB photometric.
#[cfg(feature = "registry")]
#[test]
fn decode_64x64_rgb_jpeg_tiled_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let pixels = rgb_pattern_64();
    let ppm = make_ppm_rgb(64, 64, &pixels);
    let tiff = match convert_to_tiff(
        &ppm,
        "ppm",
        &[
            "-define",
            "tiff:tile-geometry=32x32",
            "-compress",
            "jpeg",
            "-quality",
            "95",
        ],
    ) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce tiled JPEG-TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff (tiled JPEG-in-TIFF, RGB) failed");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 200.0,
        "tiled RGB JPEG-in-TIFF reconstruction too far from input: MSE={mse}"
    );
}

/// Tiled JPEG-in-TIFF with a non-square image whose right/bottom tile
/// columns/rows are partial — the decoder must clip each edge tile to the
/// visible region rather than overrun the destination plane. 48×40 image
/// at 16×16 tiles gives 3×3 tiles, the last column 16px wide but the last
/// row only 8px tall (40 = 16+16+8).
#[cfg(feature = "registry")]
#[test]
fn decode_partial_edge_jpeg_tiled_imagemagick() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    let (w, h) = (48u32, 40u32);
    let mut pixels = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            pixels.push(((x * 5 + y * 3) & 0xFF) as u8);
        }
    }
    let pgm = make_pgm_gray(w, h, &pixels);
    let tiff = match convert_to_tiff(
        &pgm,
        "pgm",
        &[
            "-define",
            "tiff:tile-geometry=16x16",
            "-compress",
            "jpeg",
            "-quality",
            "95",
        ],
    ) {
        Some(b) => b,
        None => {
            eprintln!("skipping: convert failed to produce partial-edge tiled JPEG-TIFF");
            return;
        }
    };
    let d = decode_tiff(&tiff).expect("decode_tiff (partial-edge tiled JPEG-in-TIFF) failed");
    assert_eq!((d.width, d.height), (w, h));
    let got = frame_to_gray8_bytes(&d);
    let mse = mean_squared_error(&got, &pixels);
    assert!(
        mse < 200.0,
        "partial-edge tiled gray JPEG-in-TIFF reconstruction too far from input: MSE={mse}"
    );
}

/// Build a minimal valid TIFF file (1x1 image, RGB, 8 bps) with the
/// given Compression value. Used only to drive the compression-tag
/// validation in the decoder; the strip data is intentionally bogus
/// because we expect to bail before touching it.
fn build_compression_only_tiff(compression: u16) -> Vec<u8> {
    use oxideav_tiff::types::*;
    // Build a tiny IFD with: ImageWidth=1, ImageLength=1,
    // BitsPerSample=8, Compression=<value>, Photometric=RGB(2),
    // SamplesPerPixel=3, StripOffsets, StripByteCounts.
    // TIFF entries must be sorted by tag.
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    // Single-channel BlackIsZero(1) so we can use a single BPS entry.
    let entries: &[(u16, u16, u32, u32)] = &[
        (TAG_IMAGE_WIDTH, TYPE_SHORT, 1, 1),
        (TAG_IMAGE_LENGTH, TYPE_SHORT, 1, 1),
        (TAG_BITS_PER_SAMPLE, TYPE_SHORT, 1, 8),
        (TAG_COMPRESSION, TYPE_SHORT, 1, compression as u32),
        (TAG_PHOTOMETRIC_INTERPRETATION, TYPE_SHORT, 1, 1),
        (TAG_STRIP_OFFSETS, TYPE_LONG, 1, 0),
        (TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, 1),
        (TAG_ROWS_PER_STRIP, TYPE_SHORT, 1, 1),
        (TAG_STRIP_BYTE_COUNTS, TYPE_LONG, 1, 0),
    ];
    v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for &(tag, ty, cnt, val) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&cnt.to_le_bytes());
        // SHORT values use the low 2 bytes; LONG uses all 4.
        if ty == TYPE_SHORT {
            v.extend_from_slice(&(val as u16).to_le_bytes());
            v.extend_from_slice(&[0u8, 0u8]);
        } else {
            v.extend_from_slice(&val.to_le_bytes());
        }
    }
    v.extend_from_slice(&0u32.to_le_bytes()); // next-IFD
    v
}

/// Silence the unused PathBuf import warning when the file is read
/// without all tests being enabled (PathBuf is genuinely useful for
/// debugging when one of the convert calls fails in CI logs).
#[allow(dead_code)]
fn _silence_unused_import_warnings() {
    let _: PathBuf = PathBuf::new();
}
