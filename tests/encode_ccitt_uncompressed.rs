//! Opt-in CCITT *uncompressed mode* emission (Table 5/T.4 = Table
//! 4/T.6; TIFF 6.0 §11 `T4Options` bit 1 / `T6Options` bit 1).
//!
//! With `uncompressed: true` on [`TiffCompression::CcittT4TwoD`] /
//! [`TiffCompression::CcittT6`], every 2-D coded row is encoded both
//! as an ordinary READ row and as an uncompressed segment (entrance
//! code + literal image patterns + exit code) and the cheaper form is
//! emitted — the bit-rate-control role T.4 §4.2.1.6 / T.6 §2.3 define
//! for the optional extension. The file advertises the extension via
//! the §11 options bit so a conforming reader knows the codes may
//! appear.
//!
//! Validation: in-crate round trips must be byte-exact for every
//! content class (the decoder's uncompressed-mode support predates
//! this emission path); dithered content must actually get *smaller*
//! with the flag on (proving the selector engages); the §11 options
//! bits are checked on the wire via the crate's own IFD parser and,
//! when available, `tiffdump`'s structural listing (black-box).

use std::io::Write;
use std::process::{Command, Stdio};

use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
use oxideav_tiff::types::*;
use oxideav_tiff::{decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, TiffCompression};

fn bilevel_page(w: u32, h: u32, pixels: &[u8], compression: TiffCompression) -> Vec<u8> {
    let page = EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Bilevel { pixels },
        compression,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: Default::default(),
    };
    encode_tiff(&page).expect("encode")
}

/// Pack a per-pixel 0/1 raster MSB-first.
fn pack_bits(pixels: &[u8], w: usize, h: usize) -> Vec<u8> {
    let row_bytes = w.div_ceil(8);
    let mut out = vec![0u8; row_bytes * h];
    for y in 0..h {
        for x in 0..w {
            if pixels[y * w + x] != 0 {
                out[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    out
}

/// Decode a bilevel TIFF back to a per-pixel 0/1 raster
/// (WhiteIsZero: white renders 0xFF... the encoder writes
/// WhiteIsZero, decoder maps bit 1 (black) → 0x00 after inversion, so
/// recover "1 = black" as `byte == 0x00`).
fn decode_bits(tiff: &[u8], w: usize, h: usize) -> Vec<u8> {
    let d = decode_tiff(tiff).expect("decode");
    assert_eq!((d.width as usize, d.height as usize), (w, h));
    let p = &d.frame.planes[0];
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            out.push(if p.data[y * p.stride + x] == 0 { 1 } else { 0 });
        }
    }
    out
}

/// Deterministic content classes: solid, text-like runs, and a fine
/// checkerboard dither (the pathological case for READ coding).
fn content(kind: &str, w: usize, h: usize) -> Vec<u8> {
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            match kind {
                "solid_white" => 0,
                "solid_black" => 1,
                "runs" => u8::from(x / 7 % 2 == 0 && y % 3 != 0),
                "checker" => ((x + y) % 2) as u8,
                "noise" => (((x * 31 + y * 17) ^ (x * y)) % 2) as u8,
                _ => unreachable!(),
            }
        })
        .collect()
}

/// The §11 options-tag value of the first IFD.
fn options_tag(tiff: &[u8], tag: u16) -> Option<u32> {
    let hdr = parse_header(tiff).expect("header");
    let (entries, _) =
        parse_ifd(tiff, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).expect("ifd");
    find(&entries, tag).map(|e| e.as_u32(hdr.byte_order).expect("options value"))
}

#[test]
fn t6_uncompressed_roundtrips_all_content_classes() {
    let (w, h) = (61usize, 24usize); // non-byte-aligned width
    for kind in ["solid_white", "solid_black", "runs", "checker", "noise"] {
        let pixels = content(kind, w, h);
        let packed = pack_bits(&pixels, w, h);
        let tiff = bilevel_page(
            w as u32,
            h as u32,
            &packed,
            TiffCompression::CcittT6 { uncompressed: true },
        );
        assert_eq!(
            decode_bits(&tiff, w, h),
            pixels,
            "{kind}: T.6 uncompressed round trip"
        );
        assert_eq!(
            options_tag(&tiff, TAG_T6_OPTIONS),
            Some(T6OPT_UNCOMPRESSED),
            "{kind}: T6Options must advertise the extension"
        );
    }
}

#[test]
fn t4_2d_uncompressed_roundtrips_all_content_classes() {
    let (w, h) = (61usize, 24usize);
    for eol_byte_aligned in [false, true] {
        for kind in ["solid_white", "runs", "checker", "noise"] {
            let pixels = content(kind, w, h);
            let packed = pack_bits(&pixels, w, h);
            let tiff = bilevel_page(
                w as u32,
                h as u32,
                &packed,
                TiffCompression::CcittT4TwoD {
                    eol_byte_aligned,
                    uncompressed: true,
                },
            );
            assert_eq!(
                decode_bits(&tiff, w, h),
                pixels,
                "{kind}: T.4 2-D uncompressed round trip (aligned={eol_byte_aligned})"
            );
            let opts = options_tag(&tiff, TAG_T4_OPTIONS).expect("T4Options present");
            assert_ne!(opts & T4OPT_UNCOMPRESSED, 0, "{kind}: T4Options bit 1 set");
            assert_ne!(opts & T4OPT_2D_CODING, 0, "{kind}: T4Options bit 0 set");
        }
    }
}

/// The selector must actually engage: a fine checkerboard costs READ
/// coding a Horizontal-mode code (≥ 3 + 2 + 2 bits) per pixel pair,
/// while an uncompressed row is ~2 bits per pixel — the flagged
/// encode must be strictly smaller. Content with long runs must not
/// regress (the coded form stays the cheaper choice per row).
#[test]
fn uncompressed_selector_shrinks_dither_and_never_hurts_runs() {
    let (w, h) = (128usize, 64usize);
    for kind in ["checker", "noise"] {
        let pixels = content(kind, w, h);
        let packed = pack_bits(&pixels, w, h);
        let off = bilevel_page(
            w as u32,
            h as u32,
            &packed,
            TiffCompression::CcittT6 {
                uncompressed: false,
            },
        );
        let on = bilevel_page(
            w as u32,
            h as u32,
            &packed,
            TiffCompression::CcittT6 { uncompressed: true },
        );
        assert!(
            on.len() < off.len(),
            "{kind}: flagged encode {} must beat coded-only {}",
            on.len(),
            off.len()
        );
    }
    // Run-structured content: per-row cheapest-form selection can
    // only ever match or beat the coded-only stream size (the file
    // adds no headers for the flag; only the rows change).
    let pixels = content("runs", w, h);
    let packed = pack_bits(&pixels, w, h);
    let off = bilevel_page(
        w as u32,
        h as u32,
        &packed,
        TiffCompression::CcittT6 {
            uncompressed: false,
        },
    );
    let on = bilevel_page(
        w as u32,
        h as u32,
        &packed,
        TiffCompression::CcittT6 { uncompressed: true },
    );
    assert!(on.len() <= off.len(), "runs: flag must never enlarge");
}

/// Multi-strip layout composes with the flag (each strip restarts the
/// reference line; rows inside every strip keep the per-row choice).
#[test]
fn uncompressed_multistrip_roundtrip() {
    let (w, h) = (40usize, 32usize);
    let pixels = content("noise", w, h);
    let packed = pack_bits(&pixels, w, h);
    let page = EncodePage {
        width: w as u32,
        height: h as u32,
        kind: EncodePixelFormat::Bilevel { pixels: &packed },
        compression: TiffCompression::CcittT6 { uncompressed: true },
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: oxideav_tiff::PageExtras {
            rows_per_strip: Some(8),
            ..Default::default()
        },
    };
    let tiff = encode_tiff(&page).expect("encode");
    assert_eq!(decode_bits(&tiff, w, h), pixels);
}

/// Black-box structural check: `tiffdump` (when installed) must list
/// the advertised options value on the wire. libtiff's fax decoder
/// does not implement the uncompressed-mode extension codes, so a
/// pixel-level `tiffcp` comparison is not attempted — the structural
/// listing plus the in-crate byte-exact round trips carry the
/// validation.
#[test]
fn tiffdump_lists_options_bits() {
    let (w, h) = (32usize, 16usize);
    let pixels = content("checker", w, h);
    let packed = pack_bits(&pixels, w, h);
    let tiff = bilevel_page(
        w as u32,
        h as u32,
        &packed,
        TiffCompression::CcittT6 { uncompressed: true },
    );

    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-uncmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let path = dir.join("t6u.tif");
    std::fs::File::create(&path)
        .expect("create")
        .write_all(&tiff)
        .expect("write");
    let out = Command::new("tiffdump")
        .arg(&path)
        .stderr(Stdio::null())
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    let Ok(out) = out else {
        eprintln!("skipping: tiffdump unavailable");
        return;
    };
    if !out.status.success() {
        eprintln!("skipping: tiffdump failed");
        return;
    }
    let listing = String::from_utf8_lossy(&out.stdout).to_lowercase();
    // tiffdump prints the raw tag: "t6options (293) long (4) 1<2>".
    assert!(
        listing.contains("293") && listing.contains("<2>"),
        "tiffdump must list T6Options=2, got:\n{listing}"
    );
}
