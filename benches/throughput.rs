//! Decode / encode throughput for the common layouts, in megapixels
//! per second (median of several runs, one core unless a thread budget
//! is named).
//!
//! ```sh
//! cargo bench --bench throughput
//! ```
//!
//! Every raster is synthesised in-bench (a photo-like mix of smooth
//! gradients, edges and sensor-style noise; a text-like bilevel page
//! for CCITT), so nothing is read from disk.

use std::time::Instant;

use oxideav_tiff::{
    DecodeOptions, EncodePage, EncodePixelFormat, PageExtras, TiffCompression, decode_page_with,
    encode_page,
};

const W: u32 = 2048;
const H: u32 = 1536;

/// Photo-like 8-bit samples: low-frequency structure plus a little
/// noise, so LZW / Deflate see realistic matches (not a flat field,
/// not white noise).
fn photo(ch: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((W * H * ch) as usize);
    let mut seed = 0x1234_5678u32;
    for y in 0..H {
        for x in 0..W {
            for c in 0..ch {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (seed >> 29) as i32 - 4;
                let base = ((x * (3 + c) / 16 + y * (5 - c) / 24) & 0xFF) as i32;
                let edge = if (x / 97 + y / 61) % 2 == 0 { 24 } else { 0 };
                v.push((base + edge + noise).clamp(0, 255) as u8);
            }
        }
    }
    v
}

fn photo16(ch: u32) -> Vec<u8> {
    photo(ch)
        .iter()
        .enumerate()
        .flat_map(|(i, &b)| ((b as u16) << 8 | (i as u16 & 0x3F)).to_le_bytes())
        .collect()
}

/// Text-like bilevel page, packed 1 bit per pixel.
fn page_1bit() -> Vec<u8> {
    let row = (W as usize).div_ceil(8);
    let mut v = vec![0u8; row * H as usize];
    for y in 0..H as usize {
        if (y / 6) % 4 == 3 {
            continue;
        }
        for x in 0..W as usize {
            let glyph = (x / 5 + (y / 24) * 7) % 11;
            if glyph < 4 && (x % 5) < 3 && (y % 6) < 5 {
                v[y * row + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    v
}

fn median(mut t: Vec<f64>) -> f64 {
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    t[t.len() / 2]
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> f64 {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        std::hint::black_box(f());
        samples.push(t.elapsed().as_secs_f64());
    }
    median(samples)
}

/// `(name, pixels, scheme, predictor, tiling, RowsPerStrip, coder level)`.
type Case<'a> = (
    &'static str,
    EncodePixelFormat<'a>,
    TiffCompression,
    bool,
    Option<(u32, u32)>,
    Option<u32>,
    Option<u8>,
);

fn main() {
    let rgb = photo(3);
    let gray16 = photo16(1);
    let bw = page_1bit();
    let mp = (W as f64 * H as f64) / 1e6;
    let runs = 7;
    let cases: Vec<Case<'_>> = vec![
        (
            "RGB8 none, strips",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::None,
            false,
            None,
            Some(16),
            None,
        ),
        (
            "RGB8 PackBits, strips",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::PackBits,
            false,
            None,
            Some(16),
            None,
        ),
        (
            "RGB8 LZW, strips",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::Lzw,
            false,
            None,
            Some(16),
            None,
        ),
        (
            "RGB8 LZW + predictor, strips",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::Lzw,
            true,
            None,
            Some(16),
            None,
        ),
        (
            "RGB8 LZW + predictor, 256 tiles",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::Lzw,
            true,
            Some((256, 256)),
            None,
            None,
        ),
        (
            "RGB8 Deflate + predictor, strips",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::Deflate,
            true,
            None,
            Some(16),
            None,
        ),
        (
            "RGB8 Deflate level 1 + predictor, strips",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::Deflate,
            true,
            None,
            Some(16),
            Some(1),
        ),
        (
            "RGB8 Deflate + predictor, 256 tiles",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::Deflate,
            true,
            Some((256, 256)),
            None,
            None,
        ),
        (
            "RGB8 Zstd + predictor, strips",
            EncodePixelFormat::Rgb24 { pixels: &rgb },
            TiffCompression::Zstd,
            true,
            None,
            Some(16),
            None,
        ),
        (
            "Gray16 LZW + predictor, strips",
            EncodePixelFormat::Gray16Le { pixels: &gray16 },
            TiffCompression::Lzw,
            true,
            None,
            Some(16),
            None,
        ),
        (
            "Gray16 Deflate + predictor, strips",
            EncodePixelFormat::Gray16Le { pixels: &gray16 },
            TiffCompression::Deflate,
            true,
            None,
            Some(16),
            None,
        ),
        (
            "Bilevel CCITT G4, strips",
            EncodePixelFormat::Bilevel { pixels: &bw },
            TiffCompression::CcittT6 {
                uncompressed: false,
            },
            false,
            None,
            Some(64),
            None,
        ),
        (
            "Bilevel CCITT G3 1-D, strips",
            EncodePixelFormat::Bilevel { pixels: &bw },
            TiffCompression::CcittT4OneD {
                eol_byte_aligned: false,
            },
            false,
            None,
            Some(64),
            None,
        ),
    ];
    println!("{W}x{H} ({mp:.2} MP), median of {runs} runs\n");
    println!(
        "| Layout | bytes | encode MP/s | encode, 4 threads | decode MP/s | decode, 4 threads |"
    );
    println!("|---|---:|---:|---:|---:|---:|");
    for (name, kind, comp, pred, tiling, rps, level) in cases {
        let extras = PageExtras::default()
            .with_rows_per_strip(rps)
            .with_level(level);
        let page = EncodePage::new(
            W,
            H,
            kind.clone(),
            comp,
            pred,
            false,
            tiling,
            false,
            extras.clone(),
        );
        let page4 = EncodePage::new(
            W,
            H,
            kind,
            comp,
            pred,
            false,
            tiling,
            false,
            extras.with_threads(4),
        );
        let bytes = encode_page(&page).expect("encode");
        assert_eq!(
            encode_page(&page4).expect("encode"),
            bytes,
            "threads change nothing"
        );
        let enc = time(runs, || encode_page(&page).expect("encode"));
        let enc4 = time(runs, || encode_page(&page4).expect("encode"));
        let serial = DecodeOptions::default();
        let dec = time(runs, || decode_page_with(&bytes, &serial).expect("decode"));
        let threaded = DecodeOptions::default().with_threads(4);
        let dec4 = time(runs, || {
            decode_page_with(&bytes, &threaded).expect("decode")
        });
        println!(
            "| {name} | {} | {:.0} | {:.0} | {:.0} | {:.0} |",
            bytes.len(),
            mp / enc,
            mp / enc4,
            mp / dec,
            mp / dec4
        );
    }
}
