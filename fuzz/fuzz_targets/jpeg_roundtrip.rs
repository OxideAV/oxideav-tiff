#![no_main]

//! JPEG-in-TIFF writer → reader pair.
//!
//! The input bytes steer the encoder configuration (pixel format,
//! geometry, quality, table layout, process, planar / tiled flags)
//! and supply the sample raster. Contract:
//!
//! 1. `encode_tiff` never panics; it either produces a file or a
//!    typed error.
//! 2. Every file it produces decodes through `decode_tiff` (the
//!    `Compression = 7` reader routes each segment through the
//!    registered JPEG codec) — the one documented exception being
//!    `YCbCrSubSampling = [4, 2]`, which the codec does not accept.
//! 3. The lossless process (`SOF3`) reproduces the input samples
//!    exactly for the formats whose decode output is the raw sample
//!    layout (grayscale and RGB at 8 / 12 / 16 bits).
//! 4. A one-byte mutation of the produced file must not panic the
//!    reader (TN2 segment walker, JPEGTables merge, codec hand-off).
//! 5. The bare T.81 engine (`jpeg_enc::encode_frame`) never panics
//!    on arbitrary component geometry — it validates per A.1.1 /
//!    B.2.3 and returns typed errors.

use libfuzzer_sys::fuzz_target;
use oxideav_tiff::jpeg_enc::{
    encode_frame, scaled_quant_table, HuffSpec, JpegComponent, JpegFrame, JpegTableSet,
    QUANT_LUMINANCE_K1,
};
use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, JpegOptions, JpegProcess,
    JpegTablesLayout, PageExtras, TiffCompression, TiffPixelFormat,
};

fn byte(data: &[u8], i: usize) -> u8 {
    data.get(i).copied().unwrap_or(0)
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 9 {
        return;
    }
    let kind_sel = byte(data, 0) % 10;
    let width = (byte(data, 1) % 40) as u32 + 1;
    let height = (byte(data, 2) % 40) as u32 + 1;
    let quality = byte(data, 3) % 101;
    let flags = byte(data, 4);
    let planar = flags & 1 != 0;
    let tiled = flags & 2 != 0;
    let tables = if flags & 4 != 0 {
        JpegTablesLayout::PerSegment
    } else {
        JpegTablesLayout::Shared
    };
    let process = if flags & 8 != 0 {
        JpegProcess::Lossless {
            predictor: (flags >> 4) % 8,
        }
    } else {
        JpegProcess::Dct
    };
    let optimize_huffman = flags & 0x80 != 0;
    let rows_per_strip = byte(data, 5);
    let sub_sel = byte(data, 6) % 6;
    let mutate_at = byte(data, 7);
    let samples = &data[8..];
    let px = |i: usize| -> u8 { samples[i % samples.len().max(1)] };
    let w = width as usize;
    let h = height as usize;

    let n = w * h;
    let gray8: Vec<u8> = (0..n).map(px).collect();
    let rgb24: Vec<u8> = (0..n * 3).map(px).collect();
    let cmyk: Vec<u8> = (0..n * 4).map(px).collect();
    let u16s: Vec<u16> = (0..n * 3)
        .map(|i| u16::from_le_bytes([px(2 * i), px(2 * i + 1)]))
        .collect();
    let g12: Vec<u16> = u16s[..n].iter().map(|v| v & 0x0FFF).collect();
    let rgb36: Vec<u16> = u16s.iter().map(|v| v & 0x0FFF).collect();
    let g16le: Vec<u8> = u16s[..n].iter().flat_map(|v| v.to_le_bytes()).collect();
    let rgb48: Vec<u8> = u16s.iter().flat_map(|v| v.to_le_bytes()).collect();
    let subsampling: (u16, u16) = match sub_sel {
        0 => (1, 1),
        1 => (2, 1),
        2 => (2, 2),
        3 => (4, 1),
        4 => (4, 2),
        _ => (byte(data, 6) as u16, byte(data, 5) as u16),
    };

    let kind = match kind_sel {
        0 => EncodePixelFormat::Gray8 { pixels: &gray8 },
        1 => EncodePixelFormat::Rgb24 { pixels: &rgb24 },
        2 => EncodePixelFormat::YCbCr24 { pixels: &rgb24 },
        3 => EncodePixelFormat::YCbCrSubsampled24 {
            pixels: &rgb24,
            subsampling,
        },
        4 => EncodePixelFormat::Cmyk32 { pixels: &cmyk },
        5 => EncodePixelFormat::Gray12 { pixels: &g12 },
        6 => EncodePixelFormat::Rgb36 { pixels: &rgb36 },
        7 => EncodePixelFormat::Gray16Le { pixels: &g16le },
        8 => EncodePixelFormat::Rgb48 { pixels: &rgb48 },
        _ => EncodePixelFormat::Bilevel { pixels: &gray8 },
    };
    let mut extras = PageExtras::default();
    if rows_per_strip != 0 && !tiled {
        extras.rows_per_strip = Some(rows_per_strip as u32);
    }
    let page = EncodePage {
        width,
        height,
        kind,
        compression: TiffCompression::Jpeg(JpegOptions {
            quality,
            tables,
            process,
            optimize_huffman,
        }),
        predictor: false,
        planar,
        tiling: if tiled { Some((16, 16)) } else { None },
        bigtiff: flags & 0x40 != 0 && kind_sel == 0,
        extras,
    };

    // (1) + (2): encode, then the file must decode.
    if let Ok(tiff) = encode_tiff(&page) {
        let decoded = decode_tiff(&tiff);
        let four_two = kind_sel == 3 && subsampling == (4, 2);
        match decoded {
            Ok(dec) => {
                assert_eq!((dec.width, dec.height), (width, height));
                // (3) lossless exactness on raw-layout formats.
                if let JpegProcess::Lossless { .. } = process {
                    let p = &dec.frame.planes[0];
                    let row = |y: usize, bytes: usize| &p.data[y * p.stride..y * p.stride + bytes];
                    match (kind_sel, dec.pixel_format) {
                        (0, TiffPixelFormat::Gray8) => {
                            for y in 0..h {
                                assert_eq!(row(y, w), &gray8[y * w..(y + 1) * w]);
                            }
                        }
                        (1, TiffPixelFormat::Rgb24) => {
                            for y in 0..h {
                                assert_eq!(row(y, w * 3), &rgb24[y * w * 3..(y + 1) * w * 3]);
                            }
                        }
                        (7, TiffPixelFormat::Gray16Le) => {
                            for y in 0..h {
                                assert_eq!(row(y, w * 2), &g16le[y * w * 2..(y + 1) * w * 2]);
                            }
                        }
                        (8, TiffPixelFormat::Rgb48Le) => {
                            for y in 0..h {
                                assert_eq!(row(y, w * 6), &rgb48[y * w * 6..(y + 1) * w * 6]);
                            }
                        }
                        (5, TiffPixelFormat::Gray16Le) => {
                            for y in 0..h {
                                let got: Vec<u16> = row(y, w * 2)
                                    .chunks_exact(2)
                                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                    .collect();
                                let want: Vec<u16> =
                                    g12[y * w..(y + 1) * w].iter().map(|&v| (v << 4) | (v >> 8)).collect();
                                assert_eq!(got, want);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                assert!(four_two, "produced file failed to decode: {e}");
            }
        }
        // (4) one-byte mutation never panics the reader.
        if !tiff.is_empty() {
            let mut m = tiff.clone();
            let at = (mutate_at as usize * 7919) % m.len();
            m[at] ^= 0x5A;
            let _ = decode_tiff(&m);
        }
    }

    // (5) bare engine with arbitrary geometry.
    let precision = match byte(data, 3) % 4 {
        0 => 8,
        1 => 12,
        2 => 16,
        _ => byte(data, 2),
    };
    let frame = JpegFrame {
        width: width as u16,
        height: height as u16,
        precision,
        process,
    };
    let comp_w = (byte(data, 5) % 41) as usize + 1;
    let comp_h = (byte(data, 6) % 41) as usize + 1;
    let comp_samples: Vec<u16> = (0..comp_w * comp_h)
        .map(|i| u16::from_le_bytes([px(i), px(i + 1)]) & 0x0FFF)
        .collect();
    let comps = [JpegComponent {
        samples: &comp_samples,
        width: comp_w,
        height: comp_h,
        h: (flags & 3) + 1,
        v: ((flags >> 2) & 3) + 1,
        quant_id: 0,
        huff_id: 0,
    }];
    let mut t = JpegTableSet::default();
    t.quant[0] = Some(scaled_quant_table(&QUANT_LUMINANCE_K1, quality.max(1), 8));
    t.dc[0] = Some(HuffSpec::k3_dc_luminance());
    t.ac[0] = Some(HuffSpec::k5_ac_luminance());
    let _ = encode_frame(&frame, &comps, &t, true);
});
