//! TIFF 6.0 §22 old-style JPEG **tables-form** layout over **tiles**.
//!
//! §22 "Strips and Tiles" applies the same rule to both segment
//! kinds: each strip / tile "points directly to the start of the
//! entropy coded data (not to a JPEG marker)", with the raw table
//! payloads behind `JPEGQTables` / `JPEGDCTables` / `JPEGACTables`.
//! The decoder rebuilds one T.81 datastream per tile of
//! `TileWidth × TileLength` and clips the §15 edge padding when
//! compositing (chunky) or blitting into the component planes
//! (`PlanarConfiguration = 2`).
//!
//! Fixture strategy: the crate's own `Compression = 7` writer (tiled,
//! per-segment tables) produces the reference file; each tile's
//! datastream is then *decomposed* into its raw §22 payloads by
//! walking the T.81 marker structure and re-wrapped as a §22
//! tables-form tiled TIFF. Both files carry byte-identical entropy
//! data and tables, so the two decodes must agree byte-for-byte —
//! any synthesis defect shows up as a decode error or pixel
//! divergence.

#![cfg(feature = "registry")]

use oxideav_tiff::ifd::{find, parse_header, parse_ifd, ByteOrder};
use oxideav_tiff::types::*;
use oxideav_tiff::{
    decode_tiff, encode_tiff, rgb24_to_ycbcr24, EncodePage, EncodePixelFormat, JpegOptions,
    JpegTablesLayout, PageExtras, TiffCompression, TiffError,
};

// ---------------------------------------------------------------------------
// T.81 marker walk: raw §22 payloads out of one interchange stream.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Decomposed {
    /// Quantisation table payloads by destination `Tq` (64 zig-zag bytes).
    q: Vec<Option<Vec<u8>>>,
    /// DC Huffman payloads by destination `Th` (16 BITS + values).
    dc: Vec<Option<Vec<u8>>>,
    /// AC Huffman payloads by destination.
    ac: Vec<Option<Vec<u8>>>,
    /// Per-component `(Tq, Td, Ta)` selections in frame order.
    comp_tables: Vec<(u8, u8, u8)>,
    entropy: Vec<u8>,
}

fn set_slot(v: &mut Vec<Option<Vec<u8>>>, idx: usize, data: Vec<u8>) {
    if v.len() <= idx {
        v.resize(idx + 1, None);
    }
    v[idx] = Some(data);
}

fn decompose(jpeg: &[u8]) -> Decomposed {
    let mut d = Decomposed::default();
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    let mut i = 2;
    let mut td_ta: Vec<(u8, u8)> = Vec::new();
    let mut tq: Vec<u8> = Vec::new();
    loop {
        assert_eq!(jpeg[i], 0xFF);
        let marker = jpeg[i + 1];
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        let body = &jpeg[i + 4..i + 2 + len];
        match marker {
            0xDB => {
                let mut k = 0;
                while k < body.len() {
                    let pq = body[k] >> 4;
                    let t = (body[k] & 0x0F) as usize;
                    assert_eq!(pq, 0, "8-bit tables only in the §22 layout");
                    set_slot(&mut d.q, t, body[k + 1..k + 65].to_vec());
                    k += 65;
                }
            }
            0xC4 => {
                let mut k = 0;
                while k < body.len() {
                    let class = body[k] >> 4;
                    let t = (body[k] & 0x0F) as usize;
                    let n: usize = body[k + 1..k + 17].iter().map(|&b| b as usize).sum();
                    let payload = body[k + 1..k + 17 + n].to_vec();
                    if class == 0 {
                        set_slot(&mut d.dc, t, payload);
                    } else {
                        set_slot(&mut d.ac, t, payload);
                    }
                    k += 17 + n;
                }
            }
            0xC0 => {
                let nf = body[5] as usize;
                for c in 0..nf {
                    tq.push(body[6 + c * 3 + 2]);
                }
            }
            0xDA => {
                let ns = body[0] as usize;
                for c in 0..ns {
                    let b = body[2 + c * 2];
                    td_ta.push((b >> 4, b & 0x0F));
                }
                let rest = &jpeg[i + 2 + len..];
                let eoi = rest.len() - 2;
                assert_eq!(&rest[eoi..], &[0xFF, 0xD9]);
                d.entropy = rest[..eoi].to_vec();
                break;
            }
            _ => {}
        }
        i += 2 + len;
    }
    for (c, &q) in tq.iter().enumerate() {
        d.comp_tables.push((q, td_ta[c].0, td_ta[c].1));
    }
    d
}

// ---------------------------------------------------------------------------
// §22 tiled tables-form TIFF builder (classic II).
// ---------------------------------------------------------------------------

struct TileCfg {
    width: u32,
    height: u32,
    tile_w: u32,
    tile_h: u32,
    photometric: u16,
    spp: u16,
    planar: u16,
    subsampling: Option<(u16, u16)>,
}

/// `tiles` are the per-tile entropy payloads in storage order;
/// `per_comp` gives, per TIFF component, the `(q, dc, ac)` raw table
/// payloads written behind the §22 per-component offset arrays.
fn build_tiled_tables_form(
    cfg: &TileCfg,
    per_comp: &[(Vec<u8>, Vec<u8>, Vec<u8>)],
    tiles: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x4949u16.to_le_bytes());
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    let mut cursor = 8u32;
    let mut data = Vec::new();
    let mut place = |payload: &[u8]| -> u32 {
        if cursor % 2 == 1 {
            data.push(0);
            cursor += 1;
        }
        let off = cursor;
        data.extend_from_slice(payload);
        cursor += payload.len() as u32;
        off
    };
    let q_offs: Vec<u32> = per_comp.iter().map(|c| place(&c.0)).collect();
    let dc_offs: Vec<u32> = per_comp.iter().map(|c| place(&c.1)).collect();
    let ac_offs: Vec<u32> = per_comp.iter().map(|c| place(&c.2)).collect();
    let tile_offs: Vec<u32> = tiles.iter().map(|t| place(t)).collect();
    let _ = place(&[]);
    if cursor % 2 == 1 {
        data.push(0);
        cursor += 1;
    }
    let ifd_offset = cursor;
    out[4..8].copy_from_slice(&ifd_offset.to_le_bytes());
    out.extend_from_slice(&data);

    let short_val = |v: u16| -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&v.to_le_bytes());
        b
    };
    let long_val = |v: u32| -> [u8; 4] { v.to_le_bytes() };

    let n_entries = 16u32;
    let tail_base = ifd_offset + 2 + n_entries * 12 + 4;
    let mut tail: Vec<u8> = Vec::new();
    let mut entries: Vec<(u16, u16, u32, [u8; 4])> = vec![
        (TAG_IMAGE_WIDTH, TYPE_LONG, 1, long_val(cfg.width)),
        (TAG_IMAGE_LENGTH, TYPE_LONG, 1, long_val(cfg.height)),
        (
            TAG_COMPRESSION,
            TYPE_SHORT,
            1,
            short_val(COMPRESSION_JPEG_OLD),
        ),
        (
            TAG_PHOTOMETRIC_INTERPRETATION,
            TYPE_SHORT,
            1,
            short_val(cfg.photometric),
        ),
        (TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, short_val(cfg.spp)),
        (
            TAG_PLANAR_CONFIGURATION,
            TYPE_SHORT,
            1,
            short_val(cfg.planar),
        ),
        (TAG_TILE_WIDTH, TYPE_LONG, 1, long_val(cfg.tile_w)),
        (TAG_TILE_LENGTH, TYPE_LONG, 1, long_val(cfg.tile_h)),
        (TAG_JPEG_PROC, TYPE_SHORT, 1, short_val(JPEG_PROC_BASELINE)),
    ];
    if cfg.spp == 1 {
        entries.push((TAG_BITS_PER_SAMPLE, TYPE_SHORT, 1, short_val(8)));
    } else {
        let off = tail_base + tail.len() as u32;
        for _ in 0..cfg.spp {
            tail.extend_from_slice(&8u16.to_le_bytes());
        }
        entries.push((
            TAG_BITS_PER_SAMPLE,
            TYPE_SHORT,
            cfg.spp as u32,
            long_val(off),
        ));
    }
    let mut push_long_array = |tag: u16, vals: &[u32]| {
        if vals.len() == 1 {
            entries.push((tag, TYPE_LONG, 1, long_val(vals[0])));
        } else {
            let off = tail_base + tail.len() as u32;
            for v in vals {
                tail.extend_from_slice(&v.to_le_bytes());
            }
            entries.push((tag, TYPE_LONG, vals.len() as u32, long_val(off)));
        }
    };
    push_long_array(TAG_TILE_OFFSETS, &tile_offs);
    let lens: Vec<u32> = tiles.iter().map(|t| t.len() as u32).collect();
    push_long_array(TAG_TILE_BYTE_COUNTS, &lens);
    push_long_array(TAG_JPEG_Q_TABLES, &q_offs);
    push_long_array(TAG_JPEG_DC_TABLES, &dc_offs);
    push_long_array(TAG_JPEG_AC_TABLES, &ac_offs);
    if let Some((sh, sv)) = cfg.subsampling {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&sh.to_le_bytes());
        b[2..].copy_from_slice(&sv.to_le_bytes());
        entries.push((TAG_YCBCR_SUBSAMPLING, TYPE_SHORT, 2, b));
    }
    entries.sort_by_key(|e| e.0);
    assert!(entries.len() as u32 <= n_entries);
    while (entries.len() as u32) < n_entries {
        let tag = 60000 + entries.len() as u16;
        entries.push((tag, TYPE_SHORT, 1, short_val(0)));
    }
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, ty, count, val) in &entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(val);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(out.len() as u32, tail_base, "tail offset arithmetic");
    out.extend_from_slice(&tail);
    out
}

// ---------------------------------------------------------------------------
// Reference: a Compression = 7 tiled file from our own writer.
// ---------------------------------------------------------------------------

fn tile_streams(tiff: &[u8]) -> Vec<Vec<u8>> {
    let hdr = parse_header(tiff).unwrap();
    let (entries, _) = parse_ifd(tiff, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let bo = hdr.byte_order;
    assert!(
        find(&entries, TAG_JPEG_TABLES).is_none(),
        "per-segment tables expected"
    );
    let offs = find(&entries, TAG_TILE_OFFSETS)
        .unwrap()
        .as_u64_vec(bo)
        .unwrap();
    let counts = find(&entries, TAG_TILE_BYTE_COUNTS)
        .unwrap()
        .as_u64_vec(bo)
        .unwrap();
    offs.iter()
        .zip(counts.iter())
        .map(|(&o, &c)| tiff[o as usize..(o + c) as usize].to_vec())
        .collect()
}

fn frame_bytes(img: &oxideav_tiff::TiffImage, bpp: usize) -> Vec<u8> {
    let p = &img.planes[0];
    let w = img.width as usize * bpp;
    (0..img.height as usize)
        .flat_map(|y| p.data[y * p.stride..y * p.stride + w].to_vec())
        .collect()
}

fn smooth_gray(w: usize, h: usize, seed: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f64 / w.max(2) as f64;
            let fy = y as f64 / h.max(2) as f64;
            let s = 0.5
                + 0.25 * ((fx * 3.0 + seed as f64) * std::f64::consts::PI).sin()
                + 0.25 * ((fy * 2.0) * std::f64::consts::PI).cos();
            v.push((s.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    v
}

fn smooth_rgb(w: usize, h: usize) -> Vec<u8> {
    let (r, g, b) = (
        smooth_gray(w, h, 1),
        smooth_gray(w, h, 2),
        smooth_gray(w, h, 3),
    );
    (0..w * h).flat_map(|i| [r[i], g[i], b[i]]).collect()
}

/// Per-segment tables (each tile a self-contained stream), tiled.
fn c7_page<'a>(
    w: u32,
    h: u32,
    kind: EncodePixelFormat<'a>,
    tile: (u32, u32),
    planar: bool,
) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind,
        compression: TiffCompression::Jpeg(JpegOptions {
            quality: 88,
            tables: JpegTablesLayout::PerSegment,
            ..JpegOptions::default()
        }),
        predictor: false,
        planar,
        tiling: Some(tile),
        bigtiff: false,
        extras: PageExtras::default(),
    }
}

/// Convert the reference file into the §22 tiled tables-form layout
/// and check both decodes agree byte-for-byte.
fn roundtrip_as_tables_form(c7: &[u8], cfg: TileCfg, bpp: usize) -> Vec<u8> {
    let oracle = decode_tiff(c7).expect("Compression=7 reference decode");
    let streams = tile_streams(c7);
    let decomposed: Vec<Decomposed> = streams.iter().map(|s| decompose(s)).collect();
    // Every tile carries the same table set (same quality, same
    // typical Huffman tables), so any tile's tables serve as the §22
    // shared per-component payloads.
    let first = &decomposed[0];
    let spp = cfg.spp as usize;
    let per_comp: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = (0..spp)
        .map(|c| {
            // Planar files: every tile is a one-component stream whose
            // component 0 selects the tables of TIFF component c —
            // fetch them from the first tile of plane c.
            let src = if cfg.planar == PLANAR_SEPARATE {
                &decomposed[c * (streams.len() / spp)]
            } else {
                first
            };
            let (tq, td, ta) = if cfg.planar == PLANAR_SEPARATE {
                src.comp_tables[0]
            } else {
                src.comp_tables[c]
            };
            (
                src.q[tq as usize].clone().unwrap(),
                src.dc[td as usize].clone().unwrap(),
                src.ac[ta as usize].clone().unwrap(),
            )
        })
        .collect();
    let tiles: Vec<Vec<u8>> = decomposed.iter().map(|d| d.entropy.clone()).collect();
    let tf = build_tiled_tables_form(&cfg, &per_comp, &tiles);
    let got = decode_tiff(&tf).expect("§22 tiled tables-form decode");
    assert_eq!(got.format.compression, Some(COMPRESSION_JPEG_OLD));
    assert!(got.format.tiled);
    assert_eq!((got.width, got.height), (oracle.width, oracle.height));
    assert_eq!(
        frame_bytes(&got.frame, bpp),
        frame_bytes(&oracle.frame, bpp)
    );
    tf
}

#[test]
fn tiled_tables_form_gray_with_edge_tiles_matches_c7() {
    let (w, h) = (40u32, 24u32);
    let src = smooth_gray(w as usize, h as usize, 5);
    let c7 = encode_tiff(&c7_page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &src },
        (16, 16),
        false,
    ))
    .unwrap();
    assert_eq!(tile_streams(&c7).len(), 3 * 2);
    roundtrip_as_tables_form(
        &c7,
        TileCfg {
            width: w,
            height: h,
            tile_w: 16,
            tile_h: 16,
            photometric: PHOTO_BLACK_IS_ZERO,
            spp: 1,
            planar: PLANAR_CHUNKY,
            subsampling: None,
        },
        1,
    );
}

#[test]
fn tiled_tables_form_ycbcr420_chunky_matches_c7() {
    let (w, h) = (48u32, 32u32);
    let ycc = rgb24_to_ycbcr24(&smooth_rgb(w as usize, h as usize));
    let c7 = encode_tiff(&c7_page(
        w,
        h,
        EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &ycc,
            subsampling: (2, 2),
        },
        (16, 16),
        false,
    ))
    .unwrap();
    roundtrip_as_tables_form(
        &c7,
        TileCfg {
            width: w,
            height: h,
            tile_w: 16,
            tile_h: 16,
            photometric: PHOTO_YCBCR,
            spp: 3,
            planar: PLANAR_CHUNKY,
            subsampling: Some((2, 2)),
        },
        3,
    );
}

#[test]
fn tiled_tables_form_planar_rgb_and_subsampled_ycbcr_match_c7() {
    let (w, h) = (40u32, 24u32);
    let rgb = smooth_rgb(w as usize, h as usize);
    let c7 = encode_tiff(&c7_page(
        w,
        h,
        EncodePixelFormat::Rgb24 { pixels: &rgb },
        (16, 16),
        true,
    ))
    .unwrap();
    assert_eq!(tile_streams(&c7).len(), 3 * 3 * 2);
    roundtrip_as_tables_form(
        &c7,
        TileCfg {
            width: w,
            height: h,
            tile_w: 16,
            tile_h: 16,
            photometric: PHOTO_RGB,
            spp: 3,
            planar: PLANAR_SEPARATE,
            subsampling: None,
        },
        3,
    );
    // 4:2:2 planar: 32x16 luma tiles, 16x16 chroma tiles.
    let ycc = rgb24_to_ycbcr24(&rgb);
    let c7 = encode_tiff(&c7_page(
        w,
        h,
        EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &ycc,
            subsampling: (2, 1),
        },
        (32, 16),
        true,
    ))
    .unwrap();
    let tf = roundtrip_as_tables_form(
        &c7,
        TileCfg {
            width: w,
            height: h,
            tile_w: 32,
            tile_h: 16,
            photometric: PHOTO_YCBCR,
            spp: 3,
            planar: PLANAR_SEPARATE,
            subsampling: Some((2, 1)),
        },
        3,
    );
    // Structural sanity of the hand-built file.
    let hdr = parse_header(&tf).unwrap();
    let (entries, _) = parse_ifd(&tf, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    assert_eq!(
        find(&entries, TAG_TILE_OFFSETS)
            .unwrap()
            .as_u64_vec(ByteOrder::Little)
            .unwrap()
            .len(),
        3 * 2 * 2
    );
}

#[test]
fn tiled_tables_form_rejects_bad_tile_counts_precisely() {
    let (w, h) = (16u32, 16u32);
    let src = smooth_gray(16, 16, 1);
    let c7 = encode_tiff(&c7_page(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels: &src },
        (16, 16),
        false,
    ))
    .unwrap();
    let d = decompose(&tile_streams(&c7)[0]);
    let (tq, td, ta) = d.comp_tables[0];
    let per_comp = vec![(
        d.q[tq as usize].clone().unwrap(),
        d.dc[td as usize].clone().unwrap(),
        d.ac[ta as usize].clone().unwrap(),
    )];
    let cfg = TileCfg {
        width: w,
        height: h,
        tile_w: 16,
        tile_h: 16,
        photometric: PHOTO_BLACK_IS_ZERO,
        spp: 1,
        planar: PLANAR_CHUNKY,
        subsampling: None,
    };
    // Two tile entries for a one-tile image.
    let tf = build_tiled_tables_form(&cfg, &per_comp, &[d.entropy.clone(), d.entropy.clone()]);
    let Err(e) = decode_tiff(&tf) else {
        panic!("tile-count mismatch must not decode");
    };
    assert!(matches!(e, TiffError::InvalidData(_)), "{e:?}");
    assert!(format!("{e:?}").contains("tile entries"), "{e:?}");
}
