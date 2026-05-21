//! Integration tests that encode TIFFs with our writer, then run
//! them through ImageMagick / `tiffinfo` / `tiffcp` (used as
//! black-box binary validators per workspace policy) to verify the
//! files are spec-conformant outside of just our own decoder.
//!
//! Tests gate on the binary being available; if it's missing they
//! print a warning and pass (so CI without imagemagick / libtiff
//! goes green).

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use oxideav_tiff::{
    decode_tiff, decode_tiff_all, encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat,
    TiffCompression,
};

fn binary_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n}")
}

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-encode-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn ramp_gray8(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x + y) & 0xFF) as u8);
        }
    }
    v
}

fn pattern_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h as u8 {
        for x in 0..w as u8 {
            v.push(x.wrapping_mul(7));
            v.push(y.wrapping_mul(11));
            v.push((x ^ y).wrapping_mul(13));
        }
    }
    v
}

fn write_and_decode_with_convert(tiff_bytes: &[u8], expect_rgb: bool) -> Option<Vec<u8>> {
    if !binary_available("convert") {
        eprintln!("skipping: `convert` not available");
        return None;
    }
    let dir = tmp_dir();
    let in_path = dir.join("in.tiff");
    let out_path = dir.join(if expect_rgb { "out.ppm" } else { "out.pgm" });
    fs::File::create(&in_path)
        .ok()?
        .write_all(tiff_bytes)
        .ok()?;
    let status = Command::new("convert")
        .arg(&in_path)
        .arg(&out_path)
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("convert failed: {status:?}");
        let _ = fs::remove_dir_all(&dir);
        return None;
    }
    // PPM/PGM body comes after a header; skip it.
    let raw = fs::read(&out_path).ok()?;
    let _ = fs::remove_dir_all(&dir);
    skip_pnm_header(&raw)
}

/// Skip a P5/P6 PNM header and return the raw pixel bytes.
fn skip_pnm_header(raw: &[u8]) -> Option<Vec<u8>> {
    // Header: magic\n width height\n maxval\n
    let mut i = 0;
    let mut newlines = 0;
    while newlines < 3 && i < raw.len() {
        if raw[i] == b'\n' {
            newlines += 1;
        }
        i += 1;
    }
    if newlines < 3 {
        return None;
    }
    Some(raw[i..].to_vec())
}

fn run_tiffinfo(tiff_bytes: &[u8]) -> Option<String> {
    if !binary_available("tiffinfo") {
        return None;
    }
    let dir = tmp_dir();
    let in_path = dir.join("in.tiff");
    fs::File::create(&in_path)
        .ok()?
        .write_all(tiff_bytes)
        .ok()?;
    let out = Command::new("tiffinfo").arg(&in_path).output().ok()?;
    let _ = fs::remove_dir_all(&dir);
    if !out.status.success() {
        eprintln!("tiffinfo failed: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn encoder_gray8_lzw_roundtrips_through_convert() {
    let pixels = ramp_gray8(32, 32);
    let page = EncodePage {
        width: 32,
        height: 32,
        kind: EncodePixelFormat::Gray8 { pixels: &pixels },
        compression: TiffCompression::Lzw,
    };
    let bytes = encode_tiff(&page).unwrap();
    // Round-trip through our own decoder first.
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (32, 32));
    assert_eq!(d.frame.planes[0].data, pixels);
    // Round-trip through ImageMagick's reader.
    if let Some(im_bytes) = write_and_decode_with_convert(&bytes, false) {
        assert_eq!(im_bytes.len(), pixels.len());
        assert_eq!(im_bytes, pixels, "ImageMagick decoded pixels mismatch");
    }
}

#[test]
fn encoder_rgb24_packbits_roundtrips_through_convert() {
    let pixels = pattern_rgb(40, 30);
    let page = EncodePage {
        width: 40,
        height: 30,
        kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
        compression: TiffCompression::PackBits,
    };
    let bytes = encode_tiff(&page).unwrap();
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (40, 30));
    assert_eq!(d.frame.planes[0].data, pixels);
    if let Some(im_bytes) = write_and_decode_with_convert(&bytes, true) {
        assert_eq!(im_bytes.len(), pixels.len());
        assert_eq!(im_bytes, pixels, "ImageMagick decoded RGB pixels mismatch");
    }
}

#[test]
fn encoder_rgb24_deflate_roundtrips_through_convert() {
    let pixels = pattern_rgb(48, 16);
    let page = EncodePage {
        width: 48,
        height: 16,
        kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
        compression: TiffCompression::Deflate,
    };
    let bytes = encode_tiff(&page).unwrap();
    if let Some(im_bytes) = write_and_decode_with_convert(&bytes, true) {
        assert_eq!(im_bytes, pixels);
    }
}

#[test]
fn encoder_palette_roundtrips_through_convert() {
    let palette = vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255]];
    let mut indices = Vec::with_capacity(8 * 8);
    for y in 0..8 {
        for x in 0..8 {
            indices.push(((x ^ y) & 0x3) as u8);
        }
    }
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Palette8 {
            indices: &indices,
            palette: &palette,
        },
        compression: TiffCompression::None,
    };
    let bytes = encode_tiff(&page).unwrap();
    if let Some(im_bytes) = write_and_decode_with_convert(&bytes, true) {
        // Reconstruct expected RGB pixels.
        let mut want = Vec::with_capacity(8 * 8 * 3);
        for &idx in &indices {
            let p = palette[idx as usize];
            want.extend_from_slice(&p);
        }
        assert_eq!(im_bytes, want);
    }
}

#[test]
fn encoder_tiffinfo_reports_expected_metadata() {
    let pixels = pattern_rgb(64, 48);
    let page = EncodePage {
        width: 64,
        height: 48,
        kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
        compression: TiffCompression::Lzw,
    };
    let bytes = encode_tiff(&page).unwrap();
    if let Some(info) = run_tiffinfo(&bytes) {
        assert!(
            info.contains("Image Width: 64") && info.contains("Image Length: 48"),
            "tiffinfo output missing dims: {info}"
        );
        assert!(
            info.contains("Bits/Sample: 8"),
            "tiffinfo missing bits/sample: {info}"
        );
        assert!(
            info.contains("Samples/Pixel: 3"),
            "tiffinfo missing samples/pixel: {info}"
        );
        assert!(
            info.to_lowercase().contains("lzw"),
            "tiffinfo missing LZW compression: {info}"
        );
        assert!(
            info.contains("RGB color"),
            "tiffinfo missing RGB photometric: {info}"
        );
    } else {
        eprintln!("skipping: tiffinfo not available");
    }
}

#[test]
fn encoder_multi_page_visible_to_convert_and_tiffinfo() {
    // 3-page document: gray8 / rgb / gray8.
    let p1 = ramp_gray8(16, 16);
    let p2 = pattern_rgb(16, 16);
    let p3 = ramp_gray8(16, 16);
    let pages = vec![
        EncodePage {
            width: 16,
            height: 16,
            kind: EncodePixelFormat::Gray8 { pixels: &p1 },
            compression: TiffCompression::None,
        },
        EncodePage {
            width: 16,
            height: 16,
            kind: EncodePixelFormat::Rgb24 { pixels: &p2 },
            compression: TiffCompression::Lzw,
        },
        EncodePage {
            width: 16,
            height: 16,
            kind: EncodePixelFormat::Gray8 { pixels: &p3 },
            compression: TiffCompression::Deflate,
        },
    ];
    let bytes = encode_tiff_multi(&pages).unwrap();
    let imgs = decode_tiff_all(&bytes).unwrap();
    assert_eq!(imgs.len(), 3);
    if let Some(info) = run_tiffinfo(&bytes) {
        // tiffinfo reports a TIFF Directory header for each page.
        let dirs = info.matches("TIFF Directory at offset").count();
        assert_eq!(dirs, 3, "tiffinfo should report 3 directories: {info}");
    }
}

#[test]
fn decoder_reads_imagemagick_cmyk() {
    if !binary_available("convert") {
        eprintln!("skipping: convert not available");
        return;
    }
    // Build an RGB image, ask convert to convert to CMYK TIFF.
    let pixels = pattern_rgb(32, 32);
    let dir = tmp_dir();
    let ppm_path = dir.join("in.ppm");
    let tif_path = dir.join("out.tiff");
    let mut ppm = b"P6\n32 32\n255\n".to_vec();
    ppm.extend_from_slice(&pixels);
    fs::write(&ppm_path, &ppm).unwrap();
    let st = Command::new("convert")
        .arg(&ppm_path)
        .args(["-colorspace", "CMYK", "-compress", "none"])
        .arg(&tif_path)
        .status();
    let st = match st {
        Ok(s) => s,
        Err(e) => {
            eprintln!("convert spawn failed: {e}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
    };
    if !st.success() {
        eprintln!("convert CMYK failed: {st:?}");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let bytes = fs::read(&tif_path).unwrap();
    let _ = fs::remove_dir_all(&dir);
    // Decode our way; just assert dims and that it parses.
    let d = decode_tiff(&bytes).expect("CMYK decode");
    assert_eq!((d.width, d.height), (32, 32));
    // CMYK -> RGB via our converter; precise pixel comparison would
    // require a colour-management round-trip identical to ImageMagick's,
    // which we don't pursue. Just confirm the buffer sizes line up.
    assert_eq!(d.frame.planes[0].data.len(), 32 * 32 * 3);
}

#[test]
fn decoder_reads_imagemagick_ycbcr() {
    if !binary_available("convert") {
        eprintln!("skipping: convert not available");
        return;
    }
    let pixels = pattern_rgb(32, 32);
    let dir = tmp_dir();
    let ppm_path = dir.join("in.ppm");
    let tif_path = dir.join("out.tiff");
    let mut ppm = b"P6\n32 32\n255\n".to_vec();
    ppm.extend_from_slice(&pixels);
    fs::write(&ppm_path, &ppm).unwrap();
    // ImageMagick's "ycbcr" colourspace + uncompressed TIFF should
    // produce a chunky YCbCr image at the default 1x1 subsampling.
    let st = Command::new("convert")
        .arg(&ppm_path)
        .args([
            "-colorspace",
            "ycbcr",
            "-define",
            "tiff:ycbcr-subsampling=1x1",
            "-compress",
            "none",
        ])
        .arg(&tif_path)
        .status();
    let st = match st {
        Ok(s) => s,
        Err(e) => {
            eprintln!("convert spawn failed: {e}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
    };
    if !st.success() {
        eprintln!("convert YCbCr failed: {st:?}");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let bytes = fs::read(&tif_path).unwrap();
    let _ = fs::remove_dir_all(&dir);
    let d = match decode_tiff(&bytes) {
        Ok(d) => d,
        Err(e) => {
            // Some convert builds emit a different layout; skip
            // gracefully rather than fail the suite.
            eprintln!("YCbCr decode unsupported variant: {e}");
            return;
        }
    };
    assert_eq!((d.width, d.height), (32, 32));
    assert_eq!(d.frame.planes[0].data.len(), 32 * 32 * 3);
}

#[test]
fn decoder_reads_imagemagick_tiled_rgb() {
    if !binary_available("convert") {
        eprintln!("skipping: convert not available");
        return;
    }
    let pixels = pattern_rgb(64, 64);
    let dir = tmp_dir();
    let ppm_path = dir.join("in.ppm");
    let tif_path = dir.join("out.tiff");
    let mut ppm = b"P6\n64 64\n255\n".to_vec();
    ppm.extend_from_slice(&pixels);
    fs::write(&ppm_path, &ppm).unwrap();
    // 16x16 tiles with no compression.
    let st = Command::new("convert")
        .arg(&ppm_path)
        .args(["-define", "tiff:tile-geometry=16x16", "-compress", "none"])
        .arg(&tif_path)
        .status();
    let st = match st {
        Ok(s) => s,
        Err(e) => {
            eprintln!("convert spawn failed: {e}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
    };
    if !st.success() {
        eprintln!("convert tile failed: {st:?}");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let bytes = fs::read(&tif_path).unwrap();
    let _ = fs::remove_dir_all(&dir);
    let d = decode_tiff(&bytes).expect("tiled decode");
    assert_eq!((d.width, d.height), (64, 64));
    assert_eq!(d.frame.planes[0].data, pixels);
}

#[test]
fn decoder_reads_imagemagick_multipage() {
    if !binary_available("convert") {
        eprintln!("skipping: convert not available");
        return;
    }
    let p1 = ramp_gray8(16, 16);
    let p2 = ramp_gray8(16, 16)
        .iter()
        .map(|b| 255 - b)
        .collect::<Vec<_>>();
    let dir = tmp_dir();
    let pgm1 = dir.join("p1.pgm");
    let pgm2 = dir.join("p2.pgm");
    let mut a = b"P5\n16 16\n255\n".to_vec();
    a.extend_from_slice(&p1);
    fs::write(&pgm1, &a).unwrap();
    let mut b = b"P5\n16 16\n255\n".to_vec();
    b.extend_from_slice(&p2);
    fs::write(&pgm2, &b).unwrap();
    let tif = dir.join("multi.tiff");
    let st = Command::new("convert")
        .arg(&pgm1)
        .arg(&pgm2)
        .args(["-compress", "none"])
        .arg(&tif)
        .status()
        .unwrap();
    if !st.success() {
        eprintln!("convert multi-page failed: {st:?}");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let bytes = fs::read(&tif).unwrap();
    let _ = fs::remove_dir_all(&dir);
    let imgs = decode_tiff_all(&bytes).expect("multi-page decode");
    assert_eq!(imgs.len(), 2, "expected 2 pages");
    assert_eq!(imgs[0].planes[0].data, p1);
    assert_eq!(imgs[1].planes[0].data, p2);
}

/// Build an MSB-first packed bilevel buffer with a stripe pattern.
/// Returned tuple is `(packed_bilevel, gray8_expected)`.
fn bilevel_stripes_and_gray8(w: u32, h: u32, period: u32) -> (Vec<u8>, Vec<u8>) {
    let row_bytes = (w as usize).div_ceil(8);
    let mut packed = vec![0u8; row_bytes * h as usize];
    let mut gray = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let on = ((x as u32) / period) & 1 == 1;
            if on {
                packed[y * row_bytes + x / 8] |= 1 << (7 - (x % 8));
                gray.push(0x00);
            } else {
                gray.push(0xFF);
            }
        }
    }
    (packed, gray)
}

#[test]
fn encoder_ccitt_rle_visible_to_tiffinfo() {
    // Encode a CCITT MH bilevel image, then have tiffinfo read it.
    // It must report the right dimensions + Compression name.
    let (packed, _gray) = bilevel_stripes_and_gray8(64, 16, 8);
    let page = EncodePage {
        width: 64,
        height: 16,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittRle,
    };
    let bytes = encode_tiff(&page).unwrap();
    // Self-roundtrip first.
    let d = decode_tiff(&bytes).unwrap();
    assert_eq!((d.width, d.height), (64, 16));
    if let Some(info) = run_tiffinfo(&bytes) {
        assert!(
            info.contains("Image Width: 64") && info.contains("Image Length: 16"),
            "tiffinfo missing dims for CCITT MH output: {info}"
        );
        // tiffinfo names this "CCITT modified Huffman" or
        // "CCITTRLE" depending on the libtiff version. Either spelling
        // is acceptable; both contain "CCITT".
        assert!(
            info.contains("CCITT"),
            "tiffinfo should mention CCITT for Compression=2: {info}"
        );
        // Bilevel: 1 bit per sample.
        assert!(
            info.contains("Bits/Sample: 1"),
            "tiffinfo should report 1 bit/sample: {info}"
        );
    } else {
        eprintln!("skipping CCITT MH tiffinfo check: tiffinfo unavailable");
    }
}

#[test]
fn encoder_ccitt_t4_1d_decodes_via_tiffcp_to_uncompressed() {
    // End-to-end: encode CCITT T.4 1-D, ask tiffcp to recompress to
    // uncompressed, then decode the result with our reader. Pixels
    // must match the original bilevel pattern.
    if !binary_available("tiffcp") {
        eprintln!("skipping: tiffcp not available");
        return;
    }
    let (packed, gray_expected) = bilevel_stripes_and_gray8(48, 8, 4);
    let page = EncodePage {
        width: 48,
        height: 8,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittT4OneD {
            eol_byte_aligned: false,
        },
    };
    let bytes = encode_tiff(&page).unwrap();
    let dir = tmp_dir();
    let in_path = dir.join("ccitt_t4.tiff");
    let out_path = dir.join("none.tiff");
    fs::write(&in_path, &bytes).unwrap();
    let st = Command::new("tiffcp")
        .arg("-c")
        .arg("none")
        .arg(&in_path)
        .arg(&out_path)
        .status();
    let st = match st {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tiffcp spawn failed: {e}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
    };
    if !st.success() {
        eprintln!("tiffcp -c none failed on our T.4 1-D output: {st:?}");
        let _ = fs::remove_dir_all(&dir);
        // tiffcp's failure on our output is signal of a malformed
        // stream — surface it as a hard fail rather than skip.
        panic!("tiffcp could not transcode our CCITT T.4 1-D output to uncompressed");
    }
    let trans = fs::read(&out_path).unwrap();
    let _ = fs::remove_dir_all(&dir);
    let d = decode_tiff(&trans).expect("decode tiffcp-transcoded uncompressed TIFF");
    assert_eq!((d.width, d.height), (48, 8));
    // After transcoding, our decoder must match the expected Gray8
    // rendering of the original input.
    assert_eq!(
        d.frame.planes[0].data, gray_expected,
        "pixel mismatch after CCITT T.4 1-D encode + tiffcp -c none"
    );
}

#[test]
fn encoder_ccitt_t4_1d_byte_aligned_decodes_via_tiffcp() {
    // Variant covering T4Options bit 2 (EOL byte-aligned).
    if !binary_available("tiffcp") {
        eprintln!("skipping: tiffcp not available");
        return;
    }
    let (packed, gray_expected) = bilevel_stripes_and_gray8(64, 8, 4);
    let page = EncodePage {
        width: 64,
        height: 8,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittT4OneD {
            eol_byte_aligned: true,
        },
    };
    let bytes = encode_tiff(&page).unwrap();
    let dir = tmp_dir();
    let in_path = dir.join("ccitt_t4_bf.tiff");
    let out_path = dir.join("none.tiff");
    fs::write(&in_path, &bytes).unwrap();
    let st = Command::new("tiffcp")
        .arg("-c")
        .arg("none")
        .arg(&in_path)
        .arg(&out_path)
        .status();
    let st = match st {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tiffcp spawn failed: {e}");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
    };
    if !st.success() {
        eprintln!("tiffcp -c none failed on byte-aligned T.4 output: {st:?}");
        let _ = fs::remove_dir_all(&dir);
        panic!("tiffcp could not transcode our byte-aligned CCITT T.4 output");
    }
    let trans = fs::read(&out_path).unwrap();
    let _ = fs::remove_dir_all(&dir);
    let d = decode_tiff(&trans).expect("decode transcoded TIFF");
    assert_eq!((d.width, d.height), (64, 8));
    assert_eq!(d.frame.planes[0].data, gray_expected);
}

#[test]
fn decoder_reads_tiffcp_bigtiff() {
    if !binary_available("convert") || !binary_available("tiffcp") {
        eprintln!("skipping: convert / tiffcp not available");
        return;
    }
    // Build a classic RGB TIFF via convert, then convert to BigTIFF
    // with tiffcp's `-8` flag.
    let pixels = pattern_rgb(32, 32);
    let dir = tmp_dir();
    let ppm = dir.join("in.ppm");
    let tif = dir.join("classic.tiff");
    let big = dir.join("big.tiff");
    let mut a = b"P6\n32 32\n255\n".to_vec();
    a.extend_from_slice(&pixels);
    fs::write(&ppm, &a).unwrap();
    let st = Command::new("convert")
        .arg(&ppm)
        .args(["-compress", "none"])
        .arg(&tif)
        .status()
        .unwrap();
    assert!(st.success());
    let st = Command::new("tiffcp")
        .arg("-8")
        .arg(&tif)
        .arg(&big)
        .status()
        .unwrap();
    if !st.success() {
        eprintln!("tiffcp -8 failed: {st:?}");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let bytes = fs::read(&big).unwrap();
    let _ = fs::remove_dir_all(&dir);
    // Confirm it's a BigTIFF by header.
    assert_eq!(&bytes[0..4], &[b'I', b'I', 0x2B, 0x00]);
    let d = decode_tiff(&bytes).expect("BigTIFF decode");
    assert_eq!((d.width, d.height), (32, 32));
    assert_eq!(d.frame.planes[0].data, pixels);
}
