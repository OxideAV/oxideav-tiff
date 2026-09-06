//! Round-trip and black-box checks for the in-crate T.81 JPEG encoder
//! engine (`jpeg_enc`) — independent of the TIFF wrapping.
//!
//! Every datastream the engine emits is decoded two ways:
//!
//! 1. through the crate's own `Compression = 7` segment decoder
//!    (`jpeg::decode_segment`, i.e. the `oxideav-mjpeg` codec behind
//!    the registry feature), and
//! 2. by `djpeg` (libjpeg-turbo's command-line decoder, used purely as
//!    an opaque black-box validator — skipped with a note when the
//!    binary is missing or lacks the needed precision support).
//!
//! DCT streams are compared by PSNR against the source; lossless
//! streams must reproduce the source sample-exact.

#![cfg(feature = "registry")]

use std::io::Write;
use std::process::{Command, Stdio};

use oxideav_tiff::jpeg::{decode_segment, JpegPixelFormat};
use oxideav_tiff::jpeg_enc::{
    encode_frame, gather_stats, scaled_quant_table, HuffSpec, HuffStats, JpegComponent, JpegFrame,
    JpegProcess, JpegTableSet, QUANT_CHROMINANCE_K2, QUANT_LUMINANCE_K1,
};
use oxideav_tiff::types::*;

fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// `djpeg [args] -outfile out.pnm in.jpg` → PNM bytes, or `None` when
/// the binary is missing / refuses the stream.
fn djpeg(jpeg: &[u8], args: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-jpegenc-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let in_path = dir.join("in.jpg");
    let out_path = dir.join("out.pnm");
    std::fs::File::create(&in_path).ok()?.write_all(jpeg).ok()?;
    let mut cmd = Command::new("djpeg");
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

/// Parse a binary PGM/PPM into (channels, width, height, maxval,
/// samples). 16-bit samples are big-endian per the PNM convention.
fn parse_pnm(raw: &[u8]) -> (usize, usize, usize, u32, Vec<u16>) {
    let mut fields: Vec<String> = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while raw[i].is_ascii_whitespace() {
            i += 1;
        }
        if raw[i] == b'#' {
            while raw[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while !raw[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&raw[start..i]).into_owned());
    }
    i += 1; // single whitespace after maxval
    let channels = match fields[0].as_str() {
        "P5" => 1,
        "P6" => 3,
        other => panic!("unexpected PNM magic {other}"),
    };
    let w: usize = fields[1].parse().unwrap();
    let h: usize = fields[2].parse().unwrap();
    let maxval: u32 = fields[3].parse().unwrap();
    let body = &raw[i..];
    let samples: Vec<u16> = if maxval > 255 {
        body.chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect()
    } else {
        body.iter().map(|&b| b as u16).collect()
    };
    assert_eq!(samples.len(), channels * w * h);
    (channels, w, h, maxval, samples)
}

fn psnr(a: &[u16], b: &[u16], peak: f64) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (peak * peak / mse).log10()
    }
}

fn baseline_tables(quality: u8, precision: u8) -> JpegTableSet {
    let mut t = JpegTableSet::default();
    t.quant[0] = Some(scaled_quant_table(&QUANT_LUMINANCE_K1, quality, precision));
    t.quant[1] = Some(scaled_quant_table(
        &QUANT_CHROMINANCE_K2,
        quality,
        precision,
    ));
    t.dc[0] = Some(HuffSpec::k3_dc_luminance());
    t.dc[1] = Some(HuffSpec::k4_dc_chrominance());
    t.ac[0] = Some(HuffSpec::k5_ac_luminance());
    t.ac[1] = Some(HuffSpec::k6_ac_chrominance());
    t
}

/// Optimal (K.2) tables for one frame.
fn optimal_tables(frame: &JpegFrame, comps: &[JpegComponent<'_>], quality: u8) -> JpegTableSet {
    let mut t = JpegTableSet::default();
    t.quant[0] = Some(scaled_quant_table(
        &QUANT_LUMINANCE_K1,
        quality,
        frame.precision,
    ));
    t.quant[1] = Some(scaled_quant_table(
        &QUANT_CHROMINANCE_K2,
        quality,
        frame.precision,
    ));
    let mut dc: [HuffStats; 4] = Default::default();
    let mut ac: [HuffStats; 4] = Default::default();
    gather_stats(frame, comps, &t, &mut dc, &mut ac).unwrap();
    for i in 0..4 {
        if !dc[i].is_empty() {
            t.dc[i] = Some(dc[i].to_spec());
        }
        if !ac[i].is_empty() {
            t.ac[i] = Some(ac[i].to_spec());
        }
    }
    t
}

/// Smooth synthetic content (a sum of gradients) at the given
/// precision — the kind of signal DCT coding reproduces closely.
fn smooth(w: usize, h: usize, bits: u32, seed: u32) -> Vec<u16> {
    let peak = (1u32 << bits) - 1;
    let mut v = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f64 / w.max(2) as f64;
            let fy = y as f64 / h.max(2) as f64;
            let s = 0.5
                + 0.25 * ((fx * 3.0 + seed as f64) * std::f64::consts::PI).sin()
                + 0.25 * ((fy * 2.0) * std::f64::consts::PI).cos();
            v.push((s.clamp(0.0, 1.0) * peak as f64).round() as u16);
        }
    }
    v
}

/// Box-decimate a full-resolution plane by (sh, sv), rounding the
/// mean — the same decimation the TIFF §21 writer uses.
fn decimate(src: &[u16], w: usize, h: usize, sh: usize, sv: usize) -> (Vec<u16>, usize, usize) {
    let cw = w.div_ceil(sh);
    let ch = h.div_ceil(sv);
    let mut out = vec![0u16; cw * ch];
    for by in 0..ch {
        for bx in 0..cw {
            let mut sum = 0u32;
            let mut n = 0u32;
            for sy in 0..sv {
                for sx in 0..sh {
                    let x = bx * sh + sx;
                    let y = by * sv + sy;
                    if x < w && y < h {
                        sum += src[y * w + x] as u32;
                        n += 1;
                    }
                }
            }
            out[by * cw + bx] = ((sum + n / 2) / n) as u16;
        }
    }
    (out, cw, ch)
}

fn gray_comp(samples: &[u16], w: usize, h: usize) -> JpegComponent<'_> {
    JpegComponent {
        samples,
        width: w,
        height: h,
        h: 1,
        v: 1,
        quant_id: 0,
        huff_id: 0,
    }
}

// ---------------------------------------------------------------------------
// 8-bit baseline grayscale.
// ---------------------------------------------------------------------------

#[test]
fn baseline_gray_roundtrips_through_own_decoder_and_djpeg() {
    for &(w, h) in &[(8usize, 8usize), (13, 9), (64, 40), (1, 1), (17, 33)] {
        let src = smooth(w, h, 8, 1);
        let frame = JpegFrame {
            width: w as u16,
            height: h as u16,
            precision: 8,
            process: JpegProcess::Dct,
        };
        let comps = [gray_comp(&src, w, h)];
        for quality in [50u8, 90, 100] {
            let bytes = encode_frame(&frame, &comps, &baseline_tables(quality, 8), true).unwrap();
            let seg =
                decode_segment(None, &bytes, w as u32, h as u32, PHOTO_BLACK_IS_ZERO, 8).unwrap();
            assert_eq!(seg.pixel_format, JpegPixelFormat::Gray8);
            let plane = &seg.planes[0];
            let got: Vec<u16> = (0..h)
                .flat_map(|y| {
                    plane.data[y * plane.stride..y * plane.stride + w]
                        .iter()
                        .map(|&b| b as u16)
                        .collect::<Vec<_>>()
                })
                .collect();
            let p = psnr(&src, &got, 255.0);
            let floor = if quality >= 90 { 38.0 } else { 30.0 };
            assert!(
                p >= floor,
                "{w}x{h} q{quality}: own-decoder PSNR {p:.2} dB < {floor}"
            );
            if let Some(pnm) = djpeg(&bytes, &["-pnm"]) {
                let (c, pw, ph, maxval, samples) = parse_pnm(&pnm);
                assert_eq!((c, pw, ph, maxval), (1, w, h, 255));
                let p2 = psnr(&src, &samples, 255.0);
                assert!(p2 >= floor, "{w}x{h} q{quality}: djpeg PSNR {p2:.2} dB");
                // Two independent IDCTs agree to within ±1 code value
                // almost everywhere; require near-identity between
                // the decoders.
                assert!(psnr(&got, &samples, 255.0) >= 45.0);
            } else {
                eprintln!("note: djpeg unavailable; own-decoder check only");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 8-bit baseline YCbCr 4:4:4 / 4:2:2 / 4:2:0 interleaved.
// ---------------------------------------------------------------------------

#[test]
fn baseline_ycbcr_subsampled_roundtrips() {
    let (w, h) = (40usize, 24usize);
    let y = smooth(w, h, 8, 1);
    let cb = smooth(w, h, 8, 7);
    let cr = smooth(w, h, 8, 13);
    for &(sh, sv, want) in &[
        (1usize, 1usize, JpegPixelFormat::Yuv444P),
        (2, 1, JpegPixelFormat::Yuv422P),
        (2, 2, JpegPixelFormat::Yuv420P),
    ] {
        let (cbd, cw, ch) = decimate(&cb, w, h, sh, sv);
        let (crd, _, _) = decimate(&cr, w, h, sh, sv);
        let frame = JpegFrame {
            width: w as u16,
            height: h as u16,
            precision: 8,
            process: JpegProcess::Dct,
        };
        let comps = [
            JpegComponent {
                samples: &y,
                width: w,
                height: h,
                h: sh as u8,
                v: sv as u8,
                quant_id: 0,
                huff_id: 0,
            },
            JpegComponent {
                samples: &cbd,
                width: cw,
                height: ch,
                h: 1,
                v: 1,
                quant_id: 1,
                huff_id: 1,
            },
            JpegComponent {
                samples: &crd,
                width: cw,
                height: ch,
                h: 1,
                v: 1,
                quant_id: 1,
                huff_id: 1,
            },
        ];
        let bytes = encode_frame(&frame, &comps, &baseline_tables(92, 8), true).unwrap();
        let seg = decode_segment(None, &bytes, w as u32, h as u32, PHOTO_YCBCR, 8).unwrap();
        assert_eq!(seg.pixel_format, want, "{sh}x{sv}");
        let yp = &seg.planes[0];
        let got: Vec<u16> = (0..h)
            .flat_map(|r| {
                yp.data[r * yp.stride..r * yp.stride + w]
                    .iter()
                    .map(|&b| b as u16)
                    .collect::<Vec<_>>()
            })
            .collect();
        let p = psnr(&y, &got, 255.0);
        assert!(p >= 38.0, "{sh}x{sv}: luma PSNR {p:.2}");
        let cbp = &seg.planes[1];
        let gcb: Vec<u16> = (0..ch)
            .flat_map(|r| {
                cbp.data[r * cbp.stride..r * cbp.stride + cw]
                    .iter()
                    .map(|&b| b as u16)
                    .collect::<Vec<_>>()
            })
            .collect();
        let p = psnr(&cbd, &gcb, 255.0);
        assert!(p >= 36.0, "{sh}x{sv}: Cb PSNR {p:.2}");
        // Black-box: djpeg decodes the stream to RGB; just require it
        // to accept the stream and produce the right geometry.
        if let Some(pnm) = djpeg(&bytes, &["-pnm"]) {
            let (c, pw, ph, _, _) = parse_pnm(&pnm);
            assert_eq!((c, pw, ph), (3, w, h));
        }
    }
}

// ---------------------------------------------------------------------------
// 12-bit extended sequential (SOF1) with K.2 optimal tables.
// ---------------------------------------------------------------------------

#[test]
fn extended_12bit_gray_roundtrips() {
    let (w, h) = (24usize, 17usize);
    let src = smooth(w, h, 12, 3);
    let frame = JpegFrame {
        width: w as u16,
        height: h as u16,
        precision: 12,
        process: JpegProcess::Dct,
    };
    let comps = [gray_comp(&src, w, h)];
    let tables = optimal_tables(&frame, &comps, 95);
    let bytes = encode_frame(&frame, &comps, &tables, true).unwrap();
    let seg = decode_segment(None, &bytes, w as u32, h as u32, PHOTO_BLACK_IS_ZERO, 12).unwrap();
    let plane = &seg.planes[0];
    let got: Vec<u16> = (0..h)
        .flat_map(|y| {
            plane.data[y * plane.stride..y * plane.stride + w * 2]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>()
        })
        .collect();
    let p = psnr(&src, &got, 4095.0);
    assert!(p >= 40.0, "12-bit own-decoder PSNR {p:.2}");
    if let Some(pnm) = djpeg(&bytes, &["-pnm", "-precision", "12"]) {
        let (c, pw, ph, maxval, samples) = parse_pnm(&pnm);
        assert_eq!((c, pw, ph, maxval), (1, w, h, 4095));
        let p2 = psnr(&src, &samples, 4095.0);
        assert!(p2 >= 40.0, "12-bit djpeg PSNR {p2:.2}");
    } else {
        eprintln!("note: djpeg -precision 12 unavailable");
    }
}

// ---------------------------------------------------------------------------
// Lossless (SOF3): 8-, 12- and 16-bit, every predictor, sample-exact.
// ---------------------------------------------------------------------------

#[test]
fn lossless_gray_is_sample_exact_for_every_predictor() {
    for &bits in &[8u32, 12, 16] {
        let (w, h) = (19usize, 11usize);
        let mut src = smooth(w, h, bits, 5);
        // Salt with extremes so the modulo-2^16 wrap and the SSSS = 16
        // category get exercised at 16 bits.
        let peak = ((1u32 << bits) - 1) as u16;
        src[0] = peak;
        src[1] = 0;
        src[w] = 0;
        src[w + 1] = peak;
        for predictor in 1..=7u8 {
            let frame = JpegFrame {
                width: w as u16,
                height: h as u16,
                precision: bits as u8,
                process: JpegProcess::Lossless { predictor },
            };
            let comps = [gray_comp(&src, w, h)];
            let tables = optimal_tables(&frame, &comps, 50);
            let bytes = encode_frame(&frame, &comps, &tables, true).unwrap();
            let seg = decode_segment(
                None,
                &bytes,
                w as u32,
                h as u32,
                PHOTO_BLACK_IS_ZERO,
                bits as u16,
            )
            .unwrap();
            let plane = &seg.planes[0];
            let got: Vec<u16> = if bits > 8 {
                (0..h)
                    .flat_map(|y| {
                        plane.data[y * plane.stride..y * plane.stride + w * 2]
                            .chunks_exact(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
                            .collect::<Vec<_>>()
                    })
                    .collect()
            } else {
                (0..h)
                    .flat_map(|y| {
                        plane.data[y * plane.stride..y * plane.stride + w]
                            .iter()
                            .map(|&b| b as u16)
                            .collect::<Vec<_>>()
                    })
                    .collect()
            };
            assert_eq!(got, src, "{bits}-bit predictor {predictor}");
            let args: Vec<&str> = match bits {
                8 => vec!["-pnm"],
                12 => vec!["-pnm", "-precision", "12"],
                _ => vec!["-pnm", "-precision", "16"],
            };
            if let Some(pnm) = djpeg(&bytes, &args) {
                let (c, pw, ph, _, samples) = parse_pnm(&pnm);
                assert_eq!((c, pw, ph), (1, w, h));
                assert_eq!(samples, src, "djpeg {bits}-bit predictor {predictor}");
            }
        }
    }
}

#[test]
fn lossless_rgb_interleaved_is_sample_exact() {
    let (w, h) = (9usize, 6usize);
    let r = smooth(w, h, 16, 1);
    let g = smooth(w, h, 16, 2);
    let b = smooth(w, h, 16, 3);
    let frame = JpegFrame {
        width: w as u16,
        height: h as u16,
        precision: 16,
        process: JpegProcess::Lossless { predictor: 1 },
    };
    let comps = [
        gray_comp(&r, w, h),
        gray_comp(&g, w, h),
        gray_comp(&b, w, h),
    ];
    let tables = optimal_tables(&frame, &comps, 50);
    let bytes = encode_frame(&frame, &comps, &tables, true).unwrap();
    let seg = decode_segment(None, &bytes, w as u32, h as u32, PHOTO_RGB, 16).unwrap();
    // The codec may hand a deep 3-component frame back either as three
    // planes or as one packed interleaved plane (both shapes are in
    // circulation — see `jpeg::JpegPixelFormat`); gather (R, G, B)
    // triplets either way.
    let mut got: Vec<(u16, u16, u16)> = Vec::with_capacity(w * h);
    if seg.planes.len() == 3 {
        for y in 0..h {
            for x in 0..w {
                let s = |pi: usize| {
                    let pl = &seg.planes[pi];
                    let o = y * pl.stride + x * 2;
                    u16::from_le_bytes([pl.data[o], pl.data[o + 1]])
                };
                got.push((s(0), s(1), s(2)));
            }
        }
    } else {
        assert_eq!(seg.planes.len(), 1);
        let pl = &seg.planes[0];
        for y in 0..h {
            for x in 0..w {
                let o = y * pl.stride + x * 6;
                let s = |k: usize| u16::from_le_bytes([pl.data[o + 2 * k], pl.data[o + 2 * k + 1]]);
                got.push((s(0), s(1), s(2)));
            }
        }
    }
    for i in 0..w * h {
        assert_eq!(got[i], (r[i], g[i], b[i]), "pixel {i}");
    }
    if let Some(pnm) = djpeg(&bytes, &["-pnm", "-precision", "16", "-rgb"]) {
        let (c, pw, ph, _, samples) = parse_pnm(&pnm);
        assert_eq!((c, pw, ph), (3, w, h));
        for i in 0..w * h {
            assert_eq!(
                (samples[i * 3], samples[i * 3 + 1], samples[i * 3 + 2]),
                (r[i], g[i], b[i])
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Abbreviated segments + tables stream merge (TN2 JPEGTables shape).
// ---------------------------------------------------------------------------

#[test]
fn abbreviated_segment_decodes_with_tables_stream() {
    let (w, h) = (20usize, 12usize);
    let src = smooth(w, h, 8, 9);
    let frame = JpegFrame {
        width: w as u16,
        height: h as u16,
        precision: 8,
        process: JpegProcess::Dct,
    };
    let comps = [gray_comp(&src, w, h)];
    let tables = baseline_tables(85, 8);
    let abbr = encode_frame(&frame, &comps, &tables, false).unwrap();
    let tstream = tables.tables_stream(true);
    assert_eq!(&tstream[..2], &[0xFF, 0xD8]);
    assert_eq!(&tstream[tstream.len() - 2..], &[0xFF, 0xD9]);
    // Without the tables the abbreviated stream is undecodable.
    assert!(decode_segment(None, &abbr, w as u32, h as u32, PHOTO_BLACK_IS_ZERO, 8).is_err());
    let seg = decode_segment(
        Some(&tstream),
        &abbr,
        w as u32,
        h as u32,
        PHOTO_BLACK_IS_ZERO,
        8,
    )
    .unwrap();
    let plane = &seg.planes[0];
    let got: Vec<u16> = (0..h)
        .flat_map(|y| {
            plane.data[y * plane.stride..y * plane.stride + w]
                .iter()
                .map(|&b| b as u16)
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(psnr(&src, &got, 255.0) >= 35.0);
}
