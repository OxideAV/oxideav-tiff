//! TIFF 6.0 §22 old-style JPEG **tables-form** layout decode tests.
//!
//! The tables-form layout stores raw table payloads (§22 JPEGQTables:
//! "64 BYTES of compressed quantization values" in zigzag order;
//! JPEGDCTables / JPEGACTables: "16 BYTES of 'BITS'" + "VALUES")
//! behind per-component offset arrays, and each strip "points
//! directly to the start of the entropy coded data (not to a JPEG
//! marker)". The decoder rebuilds one ISO 10918-1 datastream per
//! strip from the staged T.81 Annex B marker syntax.
//!
//! Fixture strategy: `cjpeg` (black-box binary, availability-gated)
//! encodes deterministic rasters; the test then *decomposes* each
//! bitstream into its raw §22 payloads by walking the T.81 marker
//! structure (DQT → 64 zigzag bytes after the Pq/Tq byte; DHT → the
//! BITS + VALUES payload after the Tc/Th byte; SOS → the entropy data
//! up to EOI; SOF → per-component table destinations and, for SOF3,
//! the Ss predictor) and hand-builds the §22 tables-form TIFF. The
//! oracle is the crate's own decode of the *identical bitstream*
//! wrapped as `Compression = 7` — byte-for-byte output equality
//! proves the marker synthesis reconstructed an equivalent datastream
//! (both wraps route through the same JPEG codec, so any synthesis
//! defect shows up as a decode error or pixel divergence).

#![cfg(feature = "registry")]

use std::io::Write;
use std::process::{Command, Stdio};

use oxideav_tiff::types::*;
use oxideav_tiff::{decode_tiff, TiffError};

fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{n}-{c}")
}

fn cjpeg(input_pnm: &[u8], args: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-tablesform-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join("in.pnm");
    let out_path = dir.join("out.jpg");
    std::fs::File::create(&in_path)
        .ok()?
        .write_all(input_pnm)
        .ok()?;
    let mut cmd = Command::new("cjpeg");
    for a in args {
        cmd.arg(a);
    }
    cmd.arg("-outfile").arg(&out_path).arg(&in_path);
    let status = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
    let ok = matches!(status, Ok(s) if s.success());
    let bytes = if ok {
        std::fs::read(&out_path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

fn pgm(samples: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut v = format!("P5\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(samples);
    v
}

fn ppm(samples: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(samples);
    v
}

// ---------------------------------------------------------------------------
// T.81 marker-walk decomposition (test-side; staged spec syntax).
// ---------------------------------------------------------------------------

/// Raw §22 payloads harvested from one JPEG bitstream.
#[derive(Default)]
struct Decomposed {
    /// Q tables by destination (Tq → 64 zigzag bytes).
    q: Vec<Option<Vec<u8>>>,
    /// DC Huffman tables by destination (Th → BITS + VALUES payload).
    dc: Vec<Option<Vec<u8>>>,
    /// AC Huffman tables by destination.
    ac: Vec<Option<Vec<u8>>>,
    /// Per-component (Tq, Td, Ta) destinations, SOF/SOS order.
    comp_q: Vec<u8>,
    comp_dc: Vec<u8>,
    comp_ac: Vec<u8>,
    /// SOS Ss (the §22 predictor selection-value for SOF3 streams).
    ss: u8,
    /// SOS Al (the §22 point transform for SOF3 streams).
    al: u8,
    /// Entropy-coded data (after the SOS header, up to EOI).
    entropy: Vec<u8>,
    /// True for SOF3 (lossless) streams.
    lossless: bool,
}

fn set_slot(v: &mut Vec<Option<Vec<u8>>>, idx: usize, data: Vec<u8>) {
    if v.len() <= idx {
        v.resize(idx + 1, None);
    }
    v[idx] = Some(data);
}

/// Walk the marker structure of a complete JPEG interchange stream
/// (T.81 B.1.1.3 / B.2) and harvest the §22 raw payloads.
fn decompose(jpeg: &[u8]) -> Option<Decomposed> {
    let mut d = Decomposed::default();
    let mut i = 0usize;
    assert_eq!(&jpeg[0..2], &[0xFF, 0xD8], "SOI");
    i += 2;
    loop {
        assert_eq!(jpeg[i], 0xFF, "marker prefix at {i}");
        let marker = jpeg[i + 1];
        i += 2;
        match marker {
            0xD8 | 0x01 | 0xD0..=0xD7 => continue, // standalone
            0xD9 => break,                         // EOI
            _ => {}
        }
        let len = u16::from_be_bytes([jpeg[i], jpeg[i + 1]]) as usize;
        let seg = &jpeg[i + 2..i + len];
        match marker {
            0xDB => {
                // DQT: repeated (PqTq, 64 bytes) — Pq=0 for 8-bit.
                let mut j = 0;
                while j < seg.len() {
                    let pq = seg[j] >> 4;
                    let tq = (seg[j] & 0x0F) as usize;
                    assert_eq!(pq, 0, "8-bit Qk expected");
                    set_slot(&mut d.q, tq, seg[j + 1..j + 65].to_vec());
                    j += 65;
                }
            }
            0xC4 => {
                // DHT: repeated (TcTh, BITS16, VALUES).
                let mut j = 0;
                while j < seg.len() {
                    let tc = seg[j] >> 4;
                    let th = (seg[j] & 0x0F) as usize;
                    let n: usize = seg[j + 1..j + 17].iter().map(|&b| b as usize).sum();
                    let payload = seg[j + 1..j + 17 + n].to_vec();
                    if tc == 0 {
                        set_slot(&mut d.dc, th, payload);
                    } else {
                        set_slot(&mut d.ac, th, payload);
                    }
                    j += 17 + n;
                }
            }
            0xC0 | 0xC1 => {
                // SOF0/1: components carry Tq.
                let nf = seg[5] as usize;
                for c in 0..nf {
                    d.comp_q.push(seg[6 + c * 3 + 2]);
                }
            }
            0xC3 => {
                d.lossless = true;
                let nf = seg[5] as usize;
                for c in 0..nf {
                    d.comp_q.push(seg[6 + c * 3 + 2]); // unused for SOF3
                }
            }
            0xC2 | 0xC5..=0xCB | 0xCD..=0xCF => {
                // Progressive / arithmetic / differential — not a
                // §22 process; caller should not feed these.
                return None;
            }
            0xDA => {
                // SOS: harvest Td/Ta + Ss/Al, then the entropy data
                // runs to EOI (single-scan streams only here).
                let ns = seg[0] as usize;
                for c in 0..ns {
                    d.comp_dc.push(seg[1 + c * 2 + 1] >> 4);
                    d.comp_ac.push(seg[1 + c * 2 + 1] & 0x0F);
                }
                d.ss = seg[1 + ns * 2];
                d.al = seg[1 + ns * 2 + 2] & 0x0F;
                let entropy_start = i + len;
                let rest = &jpeg[entropy_start..];
                let eoi = rest
                    .windows(2)
                    .rposition(|w| w == [0xFF, 0xD9])
                    .expect("EOI");
                d.entropy = rest[..eoi].to_vec();
                return Some(d);
            }
            _ => {} // APPn / COM / DRI — skip
        }
        i += len;
    }
    None
}

// ---------------------------------------------------------------------------
// TIFF builders.
// ---------------------------------------------------------------------------

struct TfCfg {
    width: u32,
    height: u32,
    photometric: u16,
    spp: u16,
    planar: u16,
    rows_per_strip: u32,
    subsampling: Option<(u16, u16)>,
    proc: u16,
    /// §22 JPEGLosslessPredictors / JPEGPointTransforms (lossless).
    predictors: Option<u16>,
    point_transforms: Option<u16>,
}

/// Hand-build a §22 tables-form TIFF: raw table payloads + entropy
/// strips + the tags 512/515/517-521.
struct TfTables {
    /// Per-component raw payloads (q may be empty for lossless).
    q: Vec<Vec<u8>>,
    dc: Vec<Vec<u8>>,
    ac: Vec<Vec<u8>>,
}

fn build_tables_form_tiff(cfg: &TfCfg, tables: &TfTables, strips: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x4949u16.to_le_bytes());
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());

    let mut cursor = 8u32;
    let mut data = Vec::new();
    let place = |payload: &[u8], data: &mut Vec<u8>, cursor: &mut u32| -> u32 {
        if *cursor % 2 == 1 {
            data.push(0);
            *cursor += 1;
        }
        let off = *cursor;
        data.extend_from_slice(payload);
        *cursor += payload.len() as u32;
        off
    };
    let q_offs: Vec<u32> = tables
        .q
        .iter()
        .map(|t| place(t, &mut data, &mut cursor))
        .collect();
    let dc_offs: Vec<u32> = tables
        .dc
        .iter()
        .map(|t| place(t, &mut data, &mut cursor))
        .collect();
    let ac_offs: Vec<u32> = tables
        .ac
        .iter()
        .map(|t| place(t, &mut data, &mut cursor))
        .collect();
    let strip_offs: Vec<u32> = strips
        .iter()
        .map(|s| place(s, &mut data, &mut cursor))
        .collect();
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
    let two_shorts = |a: u16, b_: u16| -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&a.to_le_bytes());
        b[2..].copy_from_slice(&b_.to_le_bytes());
        b
    };

    let mut tail: Vec<u8> = Vec::new();
    let n_entries_guess = 20u32;
    let tail_base = ifd_offset + 2 + n_entries_guess * 12 + 4;

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
        (
            TAG_ROWS_PER_STRIP,
            TYPE_LONG,
            1,
            long_val(cfg.rows_per_strip),
        ),
        (TAG_JPEG_PROC, TYPE_SHORT, 1, short_val(cfg.proc)),
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

    let push_long_array = |entries: &mut Vec<(u16, u16, u32, [u8; 4])>,
                           tail: &mut Vec<u8>,
                           tag: u16,
                           vals: &[u32]| {
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

    push_long_array(&mut entries, &mut tail, TAG_STRIP_OFFSETS, &strip_offs);
    let strip_lens: Vec<u32> = strips.iter().map(|s| s.len() as u32).collect();
    push_long_array(&mut entries, &mut tail, TAG_STRIP_BYTE_COUNTS, &strip_lens);
    if !q_offs.is_empty() {
        push_long_array(&mut entries, &mut tail, TAG_JPEG_Q_TABLES, &q_offs);
    }
    push_long_array(&mut entries, &mut tail, TAG_JPEG_DC_TABLES, &dc_offs);
    if !ac_offs.is_empty() {
        push_long_array(&mut entries, &mut tail, TAG_JPEG_AC_TABLES, &ac_offs);
    }
    if let Some(p) = cfg.predictors {
        if cfg.spp == 1 {
            entries.push((TAG_JPEG_LOSSLESS_PREDICTORS, TYPE_SHORT, 1, short_val(p)));
        } else {
            let off = tail_base + tail.len() as u32;
            for _ in 0..cfg.spp {
                tail.extend_from_slice(&p.to_le_bytes());
            }
            entries.push((
                TAG_JPEG_LOSSLESS_PREDICTORS,
                TYPE_SHORT,
                cfg.spp as u32,
                long_val(off),
            ));
        }
    }
    if let Some(pt) = cfg.point_transforms {
        if cfg.spp == 1 {
            entries.push((TAG_JPEG_POINT_TRANSFORMS, TYPE_SHORT, 1, short_val(pt)));
        } else {
            let off = tail_base + tail.len() as u32;
            for _ in 0..cfg.spp {
                tail.extend_from_slice(&pt.to_le_bytes());
            }
            entries.push((
                TAG_JPEG_POINT_TRANSFORMS,
                TYPE_SHORT,
                cfg.spp as u32,
                long_val(off),
            ));
        }
    }
    if let Some((sh, sv)) = cfg.subsampling {
        entries.push((TAG_YCBCR_SUBSAMPLING, TYPE_SHORT, 2, two_shorts(sh, sv)));
    }

    entries.sort_by_key(|e| e.0);
    assert!(entries.len() as u32 <= n_entries_guess);
    while (entries.len() as u32) < n_entries_guess {
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

/// Wrap a complete JPEG bitstream as a minimal Compression=7 TIFF —
/// the equivalence oracle (identical bitstream, identical codec).
fn build_c7_tiff(
    jpeg: &[u8],
    w: u32,
    h: u32,
    photometric: u16,
    spp: u16,
    subsampling: Option<(u16, u16)>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0x4949u16.to_le_bytes());
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(jpeg);
    if out.len() % 2 == 1 {
        out.push(0);
    }
    // Out-of-line BitsPerSample array for multi-sample photometrics
    // (§8: N = SamplesPerPixel entries).
    let bps_off = out.len() as u32;
    if spp > 1 {
        for _ in 0..spp {
            out.extend_from_slice(&8u16.to_le_bytes());
        }
    }
    let ifd_offset = out.len() as u32;
    out[4..8].copy_from_slice(&ifd_offset.to_le_bytes());
    let short_val = |v: u16| -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&v.to_le_bytes());
        b
    };
    let two_shorts = |a: u16, b_: u16| -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&a.to_le_bytes());
        b[2..].copy_from_slice(&b_.to_le_bytes());
        b
    };
    let mut entries: Vec<(u16, u16, u32, [u8; 4])> = vec![
        (TAG_IMAGE_WIDTH, TYPE_LONG, 1, w.to_le_bytes()),
        (TAG_IMAGE_LENGTH, TYPE_LONG, 1, h.to_le_bytes()),
        (
            TAG_COMPRESSION,
            TYPE_SHORT,
            1,
            short_val(COMPRESSION_JPEG_NEW),
        ),
        (
            TAG_PHOTOMETRIC_INTERPRETATION,
            TYPE_SHORT,
            1,
            short_val(photometric),
        ),
        (TAG_STRIP_OFFSETS, TYPE_LONG, 1, 8u32.to_le_bytes()),
        (TAG_SAMPLES_PER_PIXEL, TYPE_SHORT, 1, short_val(spp)),
        (TAG_ROWS_PER_STRIP, TYPE_LONG, 1, h.to_le_bytes()),
        (
            TAG_STRIP_BYTE_COUNTS,
            TYPE_LONG,
            1,
            (jpeg.len() as u32).to_le_bytes(),
        ),
    ];
    if spp > 1 {
        entries.push((
            TAG_BITS_PER_SAMPLE,
            TYPE_SHORT,
            spp as u32,
            bps_off.to_le_bytes(),
        ));
    } else {
        entries.push((TAG_BITS_PER_SAMPLE, TYPE_SHORT, 1, short_val(8)));
    }
    if let Some((sh, sv)) = subsampling {
        entries.push((TAG_YCBCR_SUBSAMPLING, TYPE_SHORT, 2, two_shorts(sh, sv)));
    }
    entries.sort_by_key(|e| e.0);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, ty, count, val) in &entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(val);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn image_bytes(img: &oxideav_tiff::TiffImage, bytes_per_pixel: usize) -> Vec<u8> {
    assert_eq!(img.planes.len(), 1);
    let p = &img.planes[0];
    let row_bytes = img.width as usize * bytes_per_pixel;
    let mut out = Vec::with_capacity(row_bytes * img.height as usize);
    for y in 0..img.height as usize {
        out.extend_from_slice(&p.data[y * p.stride..y * p.stride + row_bytes]);
    }
    out
}

fn gray_pattern(w: usize, h: usize, seed: u8) -> Vec<u8> {
    (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            (x as u32 * 5 + y as u32 * 11 + seed as u32) as u8
        })
        .collect()
}

/// Decompose one JPEG into §22 payloads, mapping table destinations
/// to per-component §22 arrays.
fn to_tf_tables(d: &Decomposed) -> TfTables {
    let comp_count = d.comp_dc.len();
    let mut t = TfTables {
        q: Vec::new(),
        dc: Vec::new(),
        ac: Vec::new(),
    };
    for c in 0..comp_count {
        if !d.lossless {
            t.q.push(
                d.q[d.comp_q[c] as usize]
                    .clone()
                    .expect("Q table for component"),
            );
            t.ac.push(
                d.ac[d.comp_ac[c] as usize]
                    .clone()
                    .expect("AC table for component"),
            );
        }
        t.dc.push(
            d.dc[d.comp_dc[c] as usize]
                .clone()
                .expect("DC table for component"),
        );
    }
    t
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Baseline grayscale, single strip: the tables-form wrap decodes
/// byte-identically to the Compression=7 wrap of the same bitstream.
#[test]
fn tables_form_gray_baseline_matches_c7() {
    let (w, h) = (32usize, 32usize);
    let src = gray_pattern(w, h, 0);
    let Some(jpeg) = cjpeg(&pgm(&src, w, h), &["-grayscale"]) else {
        eprintln!("skipping: cjpeg unavailable");
        return;
    };
    let Some(d) = decompose(&jpeg) else {
        panic!("unexpected process in cjpeg output");
    };
    assert!(!d.lossless);
    let tf = build_tables_form_tiff(
        &TfCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_BLACK_IS_ZERO,
            spp: 1,
            planar: PLANAR_CHUNKY,
            rows_per_strip: h as u32,
            subsampling: None,
            proc: JPEG_PROC_BASELINE,
            predictors: None,
            point_transforms: None,
        },
        &to_tf_tables(&d),
        std::slice::from_ref(&d.entropy),
    );
    let got = decode_tiff(&tf).expect("tables-form decode");
    let oracle = decode_tiff(&build_c7_tiff(
        &jpeg,
        w as u32,
        h as u32,
        PHOTO_BLACK_IS_ZERO,
        1,
        None,
    ))
    .expect("C7 oracle decode");
    assert_eq!(
        image_bytes(&got.frame, 1),
        image_bytes(&oracle.frame, 1),
        "tables-form synthesis must reproduce the interchange decode"
    );
}

/// Baseline YCbCr 4:2:0 (cjpeg default) — three components, distinct
/// luma/chroma table destinations, subsampled sampling factors in the
/// synthesized SOF.
#[test]
fn tables_form_ycbcr420_matches_c7() {
    let (w, h) = (32usize, 32usize);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            rgb.push((x * 255 / (w - 1)) as u8);
            rgb.push((y * 255 / (h - 1)) as u8);
            rgb.push(((x + y) * 255 / (w + h - 2)) as u8);
        }
    }
    let Some(jpeg) = cjpeg(&ppm(&rgb, w, h), &["-sample", "2x2"]) else {
        eprintln!("skipping: cjpeg unavailable");
        return;
    };
    let Some(d) = decompose(&jpeg) else {
        panic!("unexpected process in cjpeg output");
    };
    let tf = build_tables_form_tiff(
        &TfCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_YCBCR,
            spp: 3,
            planar: PLANAR_CHUNKY,
            rows_per_strip: h as u32,
            subsampling: Some((2, 2)),
            proc: JPEG_PROC_BASELINE,
            predictors: None,
            point_transforms: None,
        },
        &to_tf_tables(&d),
        std::slice::from_ref(&d.entropy),
    );
    let got = decode_tiff(&tf).expect("tables-form YCbCr decode");
    let oracle = decode_tiff(&build_c7_tiff(
        &jpeg,
        w as u32,
        h as u32,
        PHOTO_YCBCR,
        3,
        Some((2, 2)),
    ))
    .expect("C7 oracle decode");
    assert_eq!(
        image_bytes(&got.frame, 3),
        image_bytes(&oracle.frame, 3),
        "tables-form YCbCr must reproduce the interchange decode"
    );
}

/// Lossless (JPEGProc = 14) grayscale: SOF3 synthesis with the §22
/// predictor selection-value; byte-exact against the source raster.
#[test]
fn tables_form_lossless_gray_exact() {
    let (w, h) = (32usize, 32usize);
    let src = gray_pattern(w, h, 30);
    for psv in [1u16, 2, 4] {
        let Some(jpeg) = cjpeg(
            &pgm(&src, w, h),
            &["-lossless", &psv.to_string(), "-grayscale"],
        ) else {
            eprintln!("skipping: cjpeg -lossless unavailable");
            return;
        };
        let Some(d) = decompose(&jpeg) else {
            panic!("unexpected process in cjpeg output");
        };
        assert!(d.lossless);
        assert_eq!(d.ss as u16, psv, "SOS Ss is the predictor");
        let tf = build_tables_form_tiff(
            &TfCfg {
                width: w as u32,
                height: h as u32,
                photometric: PHOTO_BLACK_IS_ZERO,
                spp: 1,
                planar: PLANAR_CHUNKY,
                rows_per_strip: h as u32,
                subsampling: None,
                proc: JPEG_PROC_LOSSLESS,
                predictors: Some(psv),
                point_transforms: Some(d.al as u16),
            },
            &to_tf_tables(&d),
            std::slice::from_ref(&d.entropy),
        );
        let got = decode_tiff(&tf).expect("tables-form lossless decode");
        assert_eq!(
            image_bytes(&got.frame, 1),
            src,
            "psv={psv} lossless tables-form must be byte-exact"
        );
    }
}

/// Multi-strip baseline grayscale: two independently encoded halves
/// sharing one table set (both cjpeg invocations use the default
/// quality, so their DQT/DHT payloads are identical — asserted).
#[test]
fn tables_form_multistrip_gray_matches_c7_halves() {
    let (w, h) = (32usize, 32usize);
    let src = gray_pattern(w, h, 60);
    let top_pgm = pgm(&src[..w * h / 2], w, h / 2);
    let bottom_pgm = pgm(&src[w * h / 2..], w, h / 2);
    let (Some(j_top), Some(j_bottom)) = (
        cjpeg(&top_pgm, &["-grayscale"]),
        cjpeg(&bottom_pgm, &["-grayscale"]),
    ) else {
        eprintln!("skipping: cjpeg unavailable");
        return;
    };
    let (Some(d_top), Some(d_bottom)) = (decompose(&j_top), decompose(&j_bottom)) else {
        panic!("unexpected process in cjpeg output");
    };
    // Table payloads must agree for a shared §22 table set.
    assert_eq!(d_top.q, d_bottom.q, "same-quality DQT payloads");
    assert_eq!(d_top.dc, d_bottom.dc, "same DHT DC payloads");
    assert_eq!(d_top.ac, d_bottom.ac, "same DHT AC payloads");

    let tf = build_tables_form_tiff(
        &TfCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_BLACK_IS_ZERO,
            spp: 1,
            planar: PLANAR_CHUNKY,
            rows_per_strip: h as u32 / 2,
            subsampling: None,
            proc: JPEG_PROC_BASELINE,
            predictors: None,
            point_transforms: None,
        },
        &to_tf_tables(&d_top),
        &[d_top.entropy.clone(), d_bottom.entropy.clone()],
    );
    let got = decode_tiff(&tf).expect("multi-strip tables-form decode");

    // Oracle: decode each half's complete bitstream via C7 wraps.
    let top_or = decode_tiff(&build_c7_tiff(
        &j_top,
        w as u32,
        h as u32 / 2,
        PHOTO_BLACK_IS_ZERO,
        1,
        None,
    ))
    .expect("top oracle");
    let bottom_or = decode_tiff(&build_c7_tiff(
        &j_bottom,
        w as u32,
        h as u32 / 2,
        PHOTO_BLACK_IS_ZERO,
        1,
        None,
    ))
    .expect("bottom oracle");
    let mut want = image_bytes(&top_or.frame, 1);
    want.extend_from_slice(&image_bytes(&bottom_or.frame, 1));
    assert_eq!(image_bytes(&got.frame, 1), want);
}

/// Planar (PlanarConfiguration = 2) lossless RGB tables-form: three
/// single-component planes with per-component table offsets;
/// byte-exact reassembly.
#[test]
fn tables_form_planar_rgb_lossless_exact() {
    let (w, h) = (16usize, 16usize);
    let planes: Vec<Vec<u8>> = (0..3).map(|c| gray_pattern(w, h, c as u8 * 80)).collect();
    let mut decomposed = Vec::new();
    for p in &planes {
        let Some(j) = cjpeg(&pgm(p, w, h), &["-lossless", "1", "-grayscale"]) else {
            eprintln!("skipping: cjpeg -lossless unavailable");
            return;
        };
        let Some(d) = decompose(&j) else {
            panic!("unexpected process");
        };
        decomposed.push(d);
    }
    // Per-component DC tables (one per plane; the §22 arrays carry
    // N = SamplesPerPixel offsets).
    let tables = TfTables {
        q: Vec::new(),
        dc: decomposed
            .iter()
            .map(|d| d.dc[d.comp_dc[0] as usize].clone().unwrap())
            .collect(),
        ac: Vec::new(),
    };
    let strips: Vec<Vec<u8>> = decomposed.iter().map(|d| d.entropy.clone()).collect();
    let tf = build_tables_form_tiff(
        &TfCfg {
            width: w as u32,
            height: h as u32,
            photometric: PHOTO_RGB,
            spp: 3,
            planar: PLANAR_SEPARATE,
            rows_per_strip: h as u32,
            subsampling: None,
            proc: JPEG_PROC_LOSSLESS,
            predictors: Some(1),
            point_transforms: None,
        },
        &tables,
        &strips,
    );
    let got = decode_tiff(&tf).expect("planar tables-form decode");
    let mut want = Vec::with_capacity(w * h * 3);
    for (i, &r) in planes[0].iter().enumerate() {
        want.push(r);
        want.push(planes[1][i]);
        want.push(planes[2][i]);
    }
    assert_eq!(image_bytes(&got.frame, 3), want, "planar lossless exact");
}

/// Rejection semantics that must survive: a well-formed *tiled*
/// tables-form IFD stays a precise error, and malformed field sets
/// keep their established messages.
#[test]
fn tables_form_remaining_gates() {
    // Missing AC tables (baseline) → invalid naming the gap. This is
    // the established `tables_form_error` path; the synthesis must
    // not bypass it.
    let tf = build_tables_form_tiff(
        &TfCfg {
            width: 8,
            height: 8,
            photometric: PHOTO_BLACK_IS_ZERO,
            spp: 1,
            planar: PLANAR_CHUNKY,
            rows_per_strip: 8,
            subsampling: None,
            proc: JPEG_PROC_BASELINE,
            predictors: None,
            point_transforms: None,
        },
        &TfTables {
            q: vec![vec![1u8; 64]],
            dc: vec![vec![0u8; 17]],
            ac: Vec::new(),
        },
        &[vec![0u8; 4]],
    );
    let Err(e) = decode_tiff(&tf) else {
        panic!("baseline without AC tables must not decode");
    };
    let msg = format!("{e:?}");
    assert!(msg.contains("JPEGACTables"), "{msg}");
    assert!(matches!(e, TiffError::InvalidData(_)), "{e:?}");
}
