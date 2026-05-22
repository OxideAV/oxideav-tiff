//! Integration tests for `PlanarConfiguration = 2` (separate
//! component planes), per TIFF 6.0 §"PlanarConfiguration" (page 38)
//! and the §"TileOffsets" / §"StripOffsets" length formulas.
//!
//! Two fixture sources:
//!
//! 1. **Hand-built byte-by-byte TIFFs.** These run with no external
//!    dependency — the test code lays out the file header, IFD,
//!    StripOffsets / StripByteCounts arrays, and per-plane sample
//!    bytes itself. Letting us pin the exact spec layout (3
//!    `StripOffsets` for an SPP=3 single-strip-per-plane file with
//!    StripsPerImage=1; planes ordered R, then G, then B) without
//!    depending on which way an encoder happens to lay it out.
//!
//! 2. **`tiffcp -p separate`** as a black-box validator. Converts an
//!    uncompressed chunky TIFF into a planar one; we then decode it
//!    and check the pixels match the original. Skipped when neither
//!    `tiffcp` nor `convert` is installed.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use oxideav_tiff::{decode_tiff, DecodedTiff};

// ---------------------------------------------------------------------------
// Hand-built planar TIFF
// ---------------------------------------------------------------------------

/// Build a little-endian classic TIFF carrying a planar
/// `PhotometricInterpretation = 2` (RGB), uncompressed, 8-bit per
/// channel image with `StripsPerImage = 1` (the entire image is one
/// strip per plane).
///
/// Layout the function emits, all little-endian:
///
/// ```text
///   offset 0:     "II", magic 42, first IFD offset = 8
///   offset 8:     IFD: 11 entries + next-IFD ptr (0)
///                     ImageWidth, ImageLength, BitsPerSample (=8,8,8,
///                     external blob), Compression=1,
///                     PhotometricInterpretation=2, StripOffsets
///                     (3 LONGs, external blob), SamplesPerPixel=3,
///                     RowsPerStrip=h, StripByteCounts (3 LONGs,
///                     external blob), PlanarConfiguration=2
///   blob area:    BitsPerSample SHORT[3] | StripOffsets LONG[3] |
///                 StripByteCounts LONG[3] | plane-R bytes |
///                 plane-G bytes | plane-B bytes
/// ```
fn build_planar_rgb_tiff(width: u32, height: u32, rgb_chunky: &[u8]) -> Vec<u8> {
    assert_eq!(rgb_chunky.len(), (width * height * 3) as usize);
    let plane_bytes = (width * height) as usize;
    let mut plane_r = Vec::with_capacity(plane_bytes);
    let mut plane_g = Vec::with_capacity(plane_bytes);
    let mut plane_b = Vec::with_capacity(plane_bytes);
    for i in 0..plane_bytes {
        plane_r.push(rgb_chunky[i * 3]);
        plane_g.push(rgb_chunky[i * 3 + 1]);
        plane_b.push(rgb_chunky[i * 3 + 2]);
    }

    // We assemble the IFD's entries first to know the entry count.
    // Each entry is 12 bytes: tag(2) + type(2) + count(4) + value/offset(4).
    // External-blob fields' value/offset slots are patched in once
    // we know the blob's absolute offset.
    let num_entries: u16 = 11;
    let ifd_offset: u32 = 8;
    let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4; // count + entries + next
    let blobs_offset: u32 = ifd_offset + ifd_size;

    // Blob layout decisions:
    //   blob 0: BitsPerSample SHORT[3] = 6 bytes
    //   blob 1: StripOffsets LONG[3] = 12 bytes
    //   blob 2: StripByteCounts LONG[3] = 12 bytes
    //   blob 3..5: plane R, G, B = plane_bytes each
    let bps_off = blobs_offset;
    let so_off = bps_off + 6;
    let sbc_off = so_off + 12;
    let plane_r_off = sbc_off + 12;
    let plane_g_off = plane_r_off + plane_bytes as u32;
    let plane_b_off = plane_g_off + plane_bytes as u32;

    let mut buf: Vec<u8> = Vec::new();
    // Header: II, 42, 8
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&ifd_offset.to_le_bytes());
    // IFD count
    buf.extend_from_slice(&num_entries.to_le_bytes());

    // Helper: push an inline-LONG entry.
    let push_entry =
        |buf: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value_bytes: [u8; 4]| {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&field_type.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
            buf.extend_from_slice(&value_bytes);
        };

    // tag 256 ImageWidth (LONG, 1)
    push_entry(&mut buf, 256, 4, 1, width.to_le_bytes());
    // tag 257 ImageLength (LONG, 1)
    push_entry(&mut buf, 257, 4, 1, height.to_le_bytes());
    // tag 258 BitsPerSample (SHORT, 3) -> 6 bytes, external blob
    push_entry(&mut buf, 258, 3, 3, bps_off.to_le_bytes());
    // tag 259 Compression (SHORT, 1) = 1 (none). SHORT inline -> low
    // 2 bytes hold the value, high 2 bytes are padding (per spec
    // "value/offset" is little-endian sized to the type).
    let mut v = [0u8; 4];
    v[..2].copy_from_slice(&1u16.to_le_bytes());
    push_entry(&mut buf, 259, 3, 1, v);
    // tag 262 PhotometricInterpretation (SHORT, 1) = 2 (RGB)
    let mut v = [0u8; 4];
    v[..2].copy_from_slice(&2u16.to_le_bytes());
    push_entry(&mut buf, 262, 3, 1, v);
    // tag 273 StripOffsets (LONG, 3) -> external blob.
    // PlanarConfiguration=2, StripsPerImage=1 -> SPP*StripsPerImage
    // = 3 entries.
    push_entry(&mut buf, 273, 4, 3, so_off.to_le_bytes());
    // tag 277 SamplesPerPixel (SHORT, 1) = 3
    let mut v = [0u8; 4];
    v[..2].copy_from_slice(&3u16.to_le_bytes());
    push_entry(&mut buf, 277, 3, 1, v);
    // tag 278 RowsPerStrip (LONG, 1) = height
    push_entry(&mut buf, 278, 4, 1, height.to_le_bytes());
    // tag 279 StripByteCounts (LONG, 3) -> external blob
    push_entry(&mut buf, 279, 4, 3, sbc_off.to_le_bytes());
    // tag 284 PlanarConfiguration (SHORT, 1) = 2
    let mut v = [0u8; 4];
    v[..2].copy_from_slice(&2u16.to_le_bytes());
    push_entry(&mut buf, 284, 3, 1, v);
    // tag 339 SampleFormat (SHORT, 3) = 1 (uint) — purely
    // informational here; we squeeze it inline in 6 bytes via an
    // external blob would also work. Inline as 3 SHORTs requires 6
    // bytes which doesn't fit in 4 — so we make this an external blob
    // too. Skipping it instead keeps the IFD smaller; spec default is
    // 1 (uint) so we can omit. Replace this with a different baseline
    // tag to keep entry count at 11.
    // Reuse this slot for tag 296 ResolutionUnit (SHORT, 1) = 1 (none),
    // because some readers complain when neither Resolution* nor
    // ResolutionUnit is present.
    let mut v = [0u8; 4];
    v[..2].copy_from_slice(&1u16.to_le_bytes());
    push_entry(&mut buf, 296, 3, 1, v);

    // Next-IFD pointer (0 = no more)
    buf.extend_from_slice(&0u32.to_le_bytes());

    // Blob area starts here.
    debug_assert_eq!(buf.len() as u32, blobs_offset);
    // BitsPerSample SHORT[3] = 8,8,8
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    // StripOffsets LONG[3]: plane_r_off, plane_g_off, plane_b_off
    buf.extend_from_slice(&plane_r_off.to_le_bytes());
    buf.extend_from_slice(&plane_g_off.to_le_bytes());
    buf.extend_from_slice(&plane_b_off.to_le_bytes());
    // StripByteCounts LONG[3]: plane_bytes, plane_bytes, plane_bytes
    let pb = plane_bytes as u32;
    buf.extend_from_slice(&pb.to_le_bytes());
    buf.extend_from_slice(&pb.to_le_bytes());
    buf.extend_from_slice(&pb.to_le_bytes());

    // Plane bytes.
    buf.extend_from_slice(&plane_r);
    buf.extend_from_slice(&plane_g);
    buf.extend_from_slice(&plane_b);

    buf
}

fn rgb_pattern(w: u32, h: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            p.push((x.wrapping_mul(4) & 0xFF) as u8);
            p.push((y.wrapping_mul(4) & 0xFF) as u8);
            p.push(((x ^ y).wrapping_mul(4) & 0xFF) as u8);
        }
    }
    p
}

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

#[test]
fn hand_built_planar_rgb_uncompressed_8bit() {
    let pixels = rgb_pattern(32, 16);
    let tiff = build_planar_rgb_tiff(32, 16, &pixels);
    let d = decode_tiff(&tiff).expect("hand-built planar RGB decode");
    assert_eq!((d.width, d.height), (32, 16));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(
        got, pixels,
        "PlanarConfiguration=2 hand-built RGB pixels mismatch"
    );
}

#[test]
fn hand_built_planar_rgb_solid_color() {
    // A solid pure-blue image: 0x00, 0x00, 0xFF everywhere. If the
    // planes were mis-ordered (e.g. read as B,G,R instead of R,G,B)
    // we'd see a solid-red image — distinguishing planar mis-ordering
    // from chunky mis-ordering. This is the canonical regression
    // catcher for "are the planes interleaved in the right order".
    let mut pixels = Vec::with_capacity(8 * 8 * 3);
    for _ in 0..(8 * 8) {
        pixels.push(0x00); // R
        pixels.push(0x00); // G
        pixels.push(0xFF); // B
    }
    let tiff = build_planar_rgb_tiff(8, 8, &pixels);
    let d = decode_tiff(&tiff).expect("solid-blue planar decode");
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "solid-blue planar pixels mismatch");
}

#[test]
fn hand_built_planar_rgb_distinct_planes() {
    // Each plane carries a different gradient so the test fails if
    // any cross-plane swap happens:
    //   R plane = x*8
    //   G plane = y*8
    //   B plane = 128 (constant)
    let mut pixels = Vec::with_capacity(16 * 16 * 3);
    for y in 0..16u32 {
        for x in 0..16u32 {
            pixels.push((x.wrapping_mul(8) & 0xFF) as u8);
            pixels.push((y.wrapping_mul(8) & 0xFF) as u8);
            pixels.push(128);
        }
    }
    let tiff = build_planar_rgb_tiff(16, 16, &pixels);
    let d = decode_tiff(&tiff).expect("distinct-planes decode");
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "distinct-plane RGB pixels mismatch");
}

// ---------------------------------------------------------------------------
// Black-box validator: tiffcp -p separate
// ---------------------------------------------------------------------------

fn convert_available() -> bool {
    Command::new("convert")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tiffcp_available() -> bool {
    // tiffcp with no args exits 0 (prints usage). Use that as the
    // probe, since `--version` and `-h` flag behaviour varies
    // between tiffcp builds.
    Command::new("tiffcp")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.code().is_some())
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

fn make_ppm_rgb(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), (w * h * 3) as usize);
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(pixels);
    v
}

/// Build a chunky PPM, convert to chunky TIFF via `convert`, then
/// `tiffcp -p separate` to a planar TIFF, then decode + compare.
#[test]
fn decode_64x64_rgb_planar_tiffcp() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    if !tiffcp_available() {
        eprintln!("skipping: `tiffcp` binary not found");
        return;
    }

    let pixels = rgb_pattern(64, 64);
    let ppm = make_ppm_rgb(64, 64, &pixels);

    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-planar-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let ppm_path: PathBuf = dir.join("in.ppm");
    let chunky_path = dir.join("chunky.tiff");
    let planar_path = dir.join("planar.tiff");
    std::fs::File::create(&ppm_path)
        .unwrap()
        .write_all(&ppm)
        .unwrap();

    let chunky_ok = Command::new("convert")
        .arg(&ppm_path)
        .args(["-compress", "none"])
        .arg(&chunky_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !chunky_ok {
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("skipping: convert(ppm->tiff) failed");
        return;
    }
    let planar_ok = Command::new("tiffcp")
        .args(["-p", "separate", "-c", "none"])
        .arg(&chunky_path)
        .arg(&planar_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !planar_ok {
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("skipping: tiffcp(-p separate) failed");
        return;
    }
    let tiff = std::fs::read(&planar_path).expect("read planar.tiff");
    let _ = std::fs::remove_dir_all(&dir);

    let d = decode_tiff(&tiff).expect("decode planar RGB TIFF (tiffcp)");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    assert_eq!(got, pixels, "tiffcp planar RGB pixels mismatch");
}

/// Same as the uncompressed test, but also pushes the planar file
/// through `-c lzw` to verify the per-plane compressed strip path.
#[test]
fn decode_64x64_rgb_planar_lzw_tiffcp() {
    if !convert_available() {
        eprintln!("skipping: `convert` binary not found");
        return;
    }
    if !tiffcp_available() {
        eprintln!("skipping: `tiffcp` binary not found");
        return;
    }

    let pixels = rgb_pattern(64, 64);
    let ppm = make_ppm_rgb(64, 64, &pixels);

    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-planar-lzw-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let ppm_path = dir.join("in.ppm");
    let chunky_path = dir.join("chunky.tiff");
    let planar_path = dir.join("planar-lzw.tiff");
    std::fs::File::create(&ppm_path)
        .unwrap()
        .write_all(&ppm)
        .unwrap();

    if !Command::new("convert")
        .arg(&ppm_path)
        .args(["-compress", "none"])
        .arg(&chunky_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("skipping: convert(ppm->tiff) failed");
        return;
    }
    if !Command::new("tiffcp")
        .args(["-p", "separate", "-c", "lzw"])
        .arg(&chunky_path)
        .arg(&planar_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("skipping: tiffcp(-p separate -c lzw) failed");
        return;
    }
    let tiff = std::fs::read(&planar_path).expect("read planar-lzw.tiff");
    let _ = std::fs::remove_dir_all(&dir);

    let d = decode_tiff(&tiff).expect("decode planar LZW TIFF");
    assert_eq!((d.width, d.height), (64, 64));
    let got = frame_to_rgb24_bytes(&d);
    // LZW with predictor=1 (no prediction) is lossless; pixels must
    // match exactly. (tiffcp does not enable Predictor=2 by default.)
    assert_eq!(got, pixels, "tiffcp planar LZW RGB pixels mismatch");
}
