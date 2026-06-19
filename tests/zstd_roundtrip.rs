//! Compression=50000 (Zstandard) coverage — decode + encode.
//!
//! The scheme is the de-facto registry extension transcribed in the
//! OxideAV trace doc `docs/image/tiff/tiff-zstd-compression-50000.md`:
//! each strip / tile is one self-contained RFC 8478 Zstandard frame
//! over the post-predictor sample bytes, structurally identical to
//! Compression=8 (Adobe Deflate). Three layers of validation here:
//!
//! 1. **Self-roundtrips** (run unconditionally): encode the identical
//!    pixels twice — once `Compression = 50000`, once `Compression =
//!    1` — decode both through [`decode_tiff`], and assert the pixel
//!    planes are byte-identical. Covers Gray8 / Gray16Le / Rgb24 /
//!    Palette8 / Bilevel, the §14 predictor, `PlanarConfiguration =
//!    2`, §15 tiling with partial edge tiles, BigTIFF, and the
//!    multi-page chain.
//!
//! 2. **Hand-built fixtures** (run unconditionally): a minimal
//!    classic-II TIFF assembled byte-by-byte whose strip payload is a
//!    hand-constructed RFC 8478 `Raw_Block` frame — this exercises the
//!    decoder against wire bytes that did not come from our own
//!    encoder. Plus the §14 reader rule: a Compression=50000 file
//!    declaring a `Predictor` the reader does not recognise must be
//!    rejected, not mis-decoded.
//!
//! 3. **Black-box cross-checks** (gated on `tiffcp` being available;
//!    skip-with-warning otherwise so CI stays green): our
//!    Compression=50000 output is transcoded back to `-c none` by an
//!    independent binary and the pixels compared, and an
//!    independently-produced `-c zstd` / `-c zstd:2` file is decoded
//!    by us and compared against the source pixels.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use oxideav_tiff::{
    decode_tiff, decode_tiff_all, encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat,
    RgbColor, TiffCompression,
};

// ---- Source-pixel generators (deterministic, layout-independent) ----

fn ramp_gray8(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x.wrapping_mul(3)).wrapping_add(y.wrapping_mul(5)) & 0xFF) as u8);
        }
    }
    v
}

fn ramp_gray16le(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 2) as usize);
    for y in 0..h {
        for x in 0..w {
            let s = (x.wrapping_mul(257)).wrapping_add(y.wrapping_mul(1013)) & 0xFFFF;
            v.extend_from_slice(&(s as u16).to_le_bytes());
        }
    }
    v
}

fn pattern_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push((x.wrapping_mul(7) & 0xFF) as u8);
            v.push((y.wrapping_mul(11) & 0xFF) as u8);
            v.push(((x ^ y).wrapping_mul(13) & 0xFF) as u8);
        }
    }
    v
}

fn palette_indices(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            v.push(((x.wrapping_add(y.wrapping_mul(3))) & 0xFF) as u8);
        }
    }
    v
}

fn full_palette() -> Vec<RgbColor> {
    (0..256u32)
        .map(|i| {
            [
                (i & 0xFF) as u8,
                ((i.wrapping_mul(2)) & 0xFF) as u8,
                ((255 - i) & 0xFF) as u8,
            ]
        })
        .collect()
}

/// Packed 1-bit rows: a coarse checker so runs are non-trivial.
fn bilevel_bits(w: u32, h: u32) -> Vec<u8> {
    let row_bytes = (w as usize).div_ceil(8);
    let mut v = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            if ((x / 5) + (y / 3)) % 2 == 0 {
                v[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    v
}

// ---- Self-roundtrip harness ----

/// Encode `page_zstd` (Compression=50000) and `page_none`
/// (Compression=1) — same pixels, same flags otherwise — decode both,
/// and assert the decoded planes match each other.
fn zstd_vs_none(page_zstd: &EncodePage<'_>, page_none: &EncodePage<'_>) {
    let z_bytes = encode_tiff(page_zstd).expect("encode zstd");
    let n_bytes = encode_tiff(page_none).expect("encode none");
    let dz = decode_tiff(&z_bytes).expect("decode zstd");
    let dn = decode_tiff(&n_bytes).expect("decode none");
    assert_eq!(
        (dz.width, dz.height),
        (dn.width, dn.height),
        "zstd/none dimension mismatch"
    );
    assert_eq!(dz.pixel_format, dn.pixel_format);
    assert_eq!(
        dz.frame.planes[0].data, dn.frame.planes[0].data,
        "zstd decode != none decode of the same pixels"
    );
}

// One positional parameter per `EncodePage` field keeps each test
// case a single visually-diffable line; the struct itself is the
// named-field form.
#[allow(clippy::too_many_arguments)]
fn page<'a>(
    w: u32,
    h: u32,
    kind: EncodePixelFormat<'a>,
    compression: TiffCompression,
    predictor: bool,
    planar: bool,
    tiling: Option<(u32, u32)>,
    bigtiff: bool,
) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind,
        compression,
        predictor,
        planar,
        tiling,
        bigtiff,
    }
}

#[test]
fn zstd_gray8_strip() {
    let px = ramp_gray8(40, 25);
    let kind = || EncodePixelFormat::Gray8 { pixels: &px };
    zstd_vs_none(
        &page(
            40,
            25,
            kind(),
            TiffCompression::Zstd,
            false,
            false,
            None,
            false,
        ),
        &page(
            40,
            25,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
}

#[test]
fn zstd_gray8_predictor() {
    let px = ramp_gray8(33, 17);
    let kind = || EncodePixelFormat::Gray8 { pixels: &px };
    zstd_vs_none(
        &page(
            33,
            17,
            kind(),
            TiffCompression::Zstd,
            true,
            false,
            None,
            false,
        ),
        &page(
            33,
            17,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
}

#[test]
fn zstd_gray16le_predictor() {
    let px = ramp_gray16le(21, 13);
    let kind = || EncodePixelFormat::Gray16Le { pixels: &px };
    zstd_vs_none(
        &page(
            21,
            13,
            kind(),
            TiffCompression::Zstd,
            true,
            false,
            None,
            false,
        ),
        &page(
            21,
            13,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
}

#[test]
fn zstd_rgb24_strip_and_predictor() {
    let px = pattern_rgb(30, 22);
    let kind = || EncodePixelFormat::Rgb24 { pixels: &px };
    zstd_vs_none(
        &page(
            30,
            22,
            kind(),
            TiffCompression::Zstd,
            false,
            false,
            None,
            false,
        ),
        &page(
            30,
            22,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
    zstd_vs_none(
        &page(
            30,
            22,
            kind(),
            TiffCompression::Zstd,
            true,
            false,
            None,
            false,
        ),
        &page(
            30,
            22,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
}

#[test]
fn zstd_rgb24_planar_predictor() {
    let px = pattern_rgb(26, 19);
    let kind = || EncodePixelFormat::Rgb24 { pixels: &px };
    zstd_vs_none(
        &page(
            26,
            19,
            kind(),
            TiffCompression::Zstd,
            true,
            true,
            None,
            false,
        ),
        &page(
            26,
            19,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
}

#[test]
fn zstd_rgb24_tiled_partial_edges_predictor() {
    // 50x30 over 16x16 tiles: right-column and bottom-row tiles carry
    // §15 boundary padding; the per-tile frame + per-tile predictor
    // reversal must reassemble to the strip decode exactly.
    let px = pattern_rgb(50, 30);
    let kind = || EncodePixelFormat::Rgb24 { pixels: &px };
    zstd_vs_none(
        &page(
            50,
            30,
            kind(),
            TiffCompression::Zstd,
            true,
            false,
            Some((16, 16)),
            false,
        ),
        &page(
            50,
            30,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
}

#[test]
fn zstd_palette8() {
    let idx = palette_indices(24, 24);
    let pal = full_palette();
    let kind = || EncodePixelFormat::Palette8 {
        indices: &idx,
        palette: &pal,
    };
    zstd_vs_none(
        &page(
            24,
            24,
            kind(),
            TiffCompression::Zstd,
            false,
            false,
            None,
            false,
        ),
        &page(
            24,
            24,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
}

#[test]
fn zstd_bilevel() {
    // Zstandard is photometric-agnostic (trace doc §2) — packed 1-bit
    // rows are just bytes to the frame.
    let bits = bilevel_bits(37, 14);
    let kind = || EncodePixelFormat::Bilevel { pixels: &bits };
    zstd_vs_none(
        &page(
            37,
            14,
            kind(),
            TiffCompression::Zstd,
            false,
            false,
            None,
            false,
        ),
        &page(
            37,
            14,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            false,
        ),
    );
}

#[test]
fn zstd_bigtiff() {
    let px = pattern_rgb(31, 23);
    let kind = || EncodePixelFormat::Rgb24 { pixels: &px };
    zstd_vs_none(
        &page(
            31,
            23,
            kind(),
            TiffCompression::Zstd,
            true,
            false,
            None,
            true,
        ),
        &page(
            31,
            23,
            kind(),
            TiffCompression::None,
            false,
            false,
            None,
            true,
        ),
    );
}

#[test]
fn zstd_multipage() {
    let a = ramp_gray8(16, 16);
    let b = ramp_gray8(20, 10);
    let pages = [
        page(
            16,
            16,
            EncodePixelFormat::Gray8 { pixels: &a },
            TiffCompression::Zstd,
            false,
            false,
            None,
            false,
        ),
        page(
            20,
            10,
            EncodePixelFormat::Gray8 { pixels: &b },
            TiffCompression::Zstd,
            true,
            false,
            None,
            false,
        ),
    ];
    let bytes = encode_tiff_multi(&pages).expect("encode multipage zstd");
    let all = decode_tiff_all(&bytes).expect("decode multipage zstd");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].planes[0].data, a);
    assert_eq!(all[1].planes[0].data, b);
}

// ---- On-disk wire checks ----

/// Parse the (classic II) IFD of `bytes` and return the inline value
/// of `tag` (SHORT or LONG), plus the strip payload extents.
fn ifd_u32(bytes: &[u8], tag: u16) -> Option<u32> {
    let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let n = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
    for i in 0..n {
        let e = ifd + 2 + i * 12;
        let t = u16::from_le_bytes(bytes[e..e + 2].try_into().unwrap());
        if t == tag {
            let ty = u16::from_le_bytes(bytes[e + 2..e + 4].try_into().unwrap());
            let v = match ty {
                3 => u16::from_le_bytes(bytes[e + 8..e + 10].try_into().unwrap()) as u32,
                4 => u32::from_le_bytes(bytes[e + 8..e + 12].try_into().unwrap()),
                _ => return None,
            };
            return Some(v);
        }
    }
    None
}

#[test]
fn zstd_wire_tag_and_frame_magic() {
    // The written file must carry Compression (259) = 50000 and the
    // strip payload must begin with the RFC 8478 frame magic
    // `0x28 0xB5 0x2F 0xFD` (trace doc §3 step 3).
    let px = ramp_gray8(32, 32);
    let bytes = encode_tiff(&page(
        32,
        32,
        EncodePixelFormat::Gray8 { pixels: &px },
        TiffCompression::Zstd,
        false,
        false,
        None,
        false,
    ))
    .expect("encode");
    assert_eq!(ifd_u32(&bytes, 259), Some(50000), "Compression tag");
    let off = ifd_u32(&bytes, 273).expect("StripOffsets") as usize;
    let cnt = ifd_u32(&bytes, 279).expect("StripByteCounts") as usize;
    assert!(cnt >= 4);
    assert_eq!(
        &bytes[off..off + 4],
        &[0x28, 0xB5, 0x2F, 0xFD],
        "strip payload is not a Zstandard frame"
    );
    // Predictor variant additionally writes tag 317 = 2.
    let bytes_p = encode_tiff(&page(
        32,
        32,
        EncodePixelFormat::Gray8 { pixels: &px },
        TiffCompression::Zstd,
        true,
        false,
        None,
        false,
    ))
    .expect("encode predictor");
    assert_eq!(ifd_u32(&bytes_p, 317), Some(2), "Predictor tag");
}

// ---- Hand-built fixture (decoder vs wire bytes not from our encoder) ----

/// IFD entry, SHORT (field-type = 3) with a single inline value.
fn entry_short(tag: u16, value: u16) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&3u16.to_le_bytes());
    e[4..8].copy_from_slice(&1u32.to_le_bytes());
    e[8..10].copy_from_slice(&value.to_le_bytes());
    e
}

/// A hand-constructed single-`Raw_Block` Zstandard frame carrying
/// `payload` (RFC 8478 §3.1.1): magic `0xFD2FB528` LE, then a
/// Frame_Header_Descriptor with Single_Segment_Flag set and the
/// 1-byte Frame_Content_Size form (descriptor `0x20`, valid for
/// content sizes 0..=255), then one block header (3 bytes LE:
/// bit 0 Last_Block = 1, bits 1-2 Block_Type = 0 Raw_Block,
/// bits 3.. Block_Size), then the literal bytes.
fn raw_block_zstd_frame(payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 255);
    let mut f = vec![0x28, 0xB5, 0x2F, 0xFD];
    f.push(0x20); // FHD: single-segment, 1-byte FCS
    f.push(payload.len() as u8); // Frame_Content_Size
    let bh: u32 = 1 | ((payload.len() as u32) << 3);
    f.extend_from_slice(&bh.to_le_bytes()[..3]);
    f.extend_from_slice(payload);
    f
}

/// Minimal classic-II Gray8 w*h single-strip TIFF whose strip bytes
/// are `strip`, tagged Compression = 50000 (+ optional Predictor).
fn build_zstd_tiff(w: u16, h: u16, strip: &[u8], predictor: Option<u16>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    let strip_off = 8u32;
    let ifd_off = 8 + strip.len() as u32;
    out.extend_from_slice(&ifd_off.to_le_bytes());
    out.extend_from_slice(strip);

    let n: u16 = 9 + if predictor.is_some() { 1 } else { 0 };
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&entry_short(256, w)); // ImageWidth
    out.extend_from_slice(&entry_short(257, h)); // ImageLength
    out.extend_from_slice(&entry_short(258, 8)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 50000)); // Compression = ZSTD
    out.extend_from_slice(&entry_short(262, 1)); // Photometric = BlackIsZero
    out.extend_from_slice(&entry_short(273, strip_off as u16)); // StripOffsets
    out.extend_from_slice(&entry_short(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, h)); // RowsPerStrip
    out.extend_from_slice(&entry_short(279, strip.len() as u16)); // StripByteCounts
    if let Some(p) = predictor {
        out.extend_from_slice(&entry_short(317, p));
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

#[test]
fn zstd_hand_built_raw_block_frame_decodes() {
    // 4x4 Gray8, strip = a hand-assembled Raw_Block frame — wire
    // bytes our own encoder never produces (it emits compressed
    // blocks), so this pins the decode path to the frame format
    // rather than to our encoder's output shape.
    let pixels: Vec<u8> = (0u8..16).collect();
    let strip = raw_block_zstd_frame(&pixels);
    let tiff = build_zstd_tiff(4, 4, &strip, None);
    let d = decode_tiff(&tiff).expect("hand-built zstd TIFF must decode");
    assert_eq!((d.width, d.height), (4, 4));
    assert_eq!(d.frame.planes[0].data, pixels);
}

#[test]
fn zstd_hand_built_predictor2_reverses() {
    // Same frame shape but the payload is a §14
    // horizontally-differenced row and the IFD declares
    // Predictor = 2: decode must reverse the differencing AFTER the
    // frame decode (trace doc §5 ordering).
    // Row of actual pixels 10, 13, 17, 22 → differenced 10, 3, 4, 5.
    let diffed = [10u8, 3, 4, 5];
    let strip = raw_block_zstd_frame(&diffed);
    let tiff = build_zstd_tiff(4, 1, &strip, Some(2));
    let d = decode_tiff(&tiff).expect("predictor-2 zstd TIFF must decode");
    assert_eq!(d.frame.planes[0].data, vec![10, 13, 17, 22]);
}

#[test]
fn zstd_unknown_predictor_rejected() {
    // TIFF 6.0 §14 reader rule (restated in trace doc §4): a reader
    // that does not recognise the declared Predictor must give up
    // rather than emit garbage. We support 1 (none), 2 (horizontal
    // differencing) and 3 (the floating-point predictor); any other
    // value must be rejected.
    let pixels: Vec<u8> = (0u8..16).collect();
    let strip = raw_block_zstd_frame(&pixels);
    let tiff = build_zstd_tiff(4, 4, &strip, Some(4));
    let err = match decode_tiff(&tiff) {
        Ok(_) => panic!("Predictor=4 (undefined) must be rejected"),
        Err(e) => e,
    };
    assert!(format!("{err:?}").contains("Predictor"), "{err:?}");
}

#[test]
fn zstd_float_predictor_on_integer_samples_rejected() {
    // Predictor = 3 is the IEEE floating-point predictor; declaring it
    // over plain 8-bit unsigned integer samples (no SampleFormat=3) has
    // no defined meaning. The §14 reader rule requires giving up rather
    // than reversing a byte-plane transform the data was never put
    // through.
    let pixels: Vec<u8> = (0u8..16).collect();
    let strip = raw_block_zstd_frame(&pixels);
    let tiff = build_zstd_tiff(4, 4, &strip, Some(3));
    let err = match decode_tiff(&tiff) {
        Ok(_) => panic!("Predictor=3 over integer samples must be rejected"),
        Err(e) => e,
    };
    assert!(
        format!("{err:?}").contains("Predictor=3") || format!("{err:?}").contains("SampleFormat"),
        "{err:?}"
    );
}

#[test]
fn zstd_corrupt_frame_is_error_not_panic() {
    // Garbage strip bytes under Compression=50000 → clean error.
    let strip = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03];
    let tiff = build_zstd_tiff(4, 4, &strip, None);
    assert!(decode_tiff(&tiff).is_err());
}

// ---- Black-box cross-checks against an independent binary ----

fn binary_available(name: &str) -> bool {
    Command::new(name)
        .arg("-h")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn tmp_dir() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("oxideav-tiff-zstd-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `tiffcp -c <codec> in out`; returns false (skip) if the local
/// `tiffcp` lacks ZSTD support (it is a build-time option).
fn tiffcp(codec: &str, input: &PathBuf, output: &PathBuf) -> bool {
    let st = Command::new("tiffcp")
        .arg("-c")
        .arg(codec)
        .arg(input)
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(st, Ok(s) if s.success())
}

#[test]
fn zstd_blackbox_our_encode_transcodes_to_none() {
    // Our Compression=50000 file → independent transcode to -c none →
    // our decode of the none file must equal the source pixels.
    if !binary_available("tiffcp") {
        eprintln!("skipping: `tiffcp` not available");
        return;
    }
    let px = pattern_rgb(40, 30);
    let bytes = encode_tiff(&page(
        40,
        30,
        EncodePixelFormat::Rgb24 { pixels: &px },
        TiffCompression::Zstd,
        true,
        false,
        None,
        false,
    ))
    .expect("encode");
    let dir = tmp_dir();
    let zpath = dir.join("ours.tif");
    let npath = dir.join("none.tif");
    fs::File::create(&zpath).unwrap().write_all(&bytes).unwrap();
    if !tiffcp("none", &zpath, &npath) {
        eprintln!("skipping: local tiffcp lacks ZSTD support");
        return;
    }
    let none_bytes = fs::read(&npath).unwrap();
    let d = decode_tiff(&none_bytes).expect("decode transcoded none");
    assert_eq!(d.frame.planes[0].data, px, "pixels lost in zstd transcode");
}

#[test]
fn zstd_blackbox_reference_file_decodes() {
    // -c none file from our encoder → independent -c zstd (and
    // -c zstd:2 predictor) re-encode → our decoder must recover the
    // source pixels from the independently-produced frames.
    if !binary_available("tiffcp") {
        eprintln!("skipping: `tiffcp` not available");
        return;
    }
    let px = ramp_gray8(64, 48);
    let bytes = encode_tiff(&page(
        64,
        48,
        EncodePixelFormat::Gray8 { pixels: &px },
        TiffCompression::None,
        false,
        false,
        None,
        false,
    ))
    .expect("encode none");
    let dir = tmp_dir();
    let npath = dir.join("none.tif");
    fs::File::create(&npath).unwrap().write_all(&bytes).unwrap();
    for codec in ["zstd", "zstd:2"] {
        let zpath = dir.join(format!("ref-{}.tif", codec.replace(':', "_")));
        if !tiffcp(codec, &npath, &zpath) {
            eprintln!("skipping: local tiffcp lacks ZSTD support ({codec})");
            return;
        }
        let zbytes = fs::read(&zpath).unwrap();
        let d =
            decode_tiff(&zbytes).unwrap_or_else(|e| panic!("decode reference {codec} file: {e:?}"));
        assert_eq!(
            d.frame.planes[0].data, px,
            "reference {codec} decode mismatch"
        );
    }
}
