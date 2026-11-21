//! The opt-in thread budget (`DecodeOptions::with_threads`,
//! `PageExtras::with_threads`, `EncodeOptions::with_threads`) changes
//! the schedule, never the result; the coder level
//! (`PageExtras::with_level`) changes the size, never the pixels.

use oxideav_tiff::{
    DecodeOptions, EncodeOptions, EncodePage, EncodePixelFormat, PageExtras, TiffCompression,
    TiffImage, decode, decode_page_with, encode, encode_page,
};

const W: u32 = 150;
const H: u32 = 97;

fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|i| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Runs, ramps and noise, so every coder has work to do.
            match (i / 400) % 3 {
                0 => (i / 7) as u8,
                1 => 0x55,
                _ => (s >> 24) as u8,
            }
        })
        .collect()
}

/// `(name, pixels, scheme, predictor)`.
type Kind<'a> = (&'static str, EncodePixelFormat<'a>, TiffCompression, bool);

struct Sources {
    gray8: Vec<u8>,
    gray16: Vec<u8>,
    rgb8: Vec<u8>,
    bilevel: Vec<u8>,
    floats: Vec<f32>,
}

impl Sources {
    fn new() -> Self {
        let px = (W * H) as usize;
        Self {
            gray8: noise(px, 1),
            gray16: noise(px * 2, 2),
            rgb8: noise(px * 3, 3),
            bilevel: noise((W as usize).div_ceil(8) * H as usize, 4),
            floats: noise(px * 3, 5)
                .iter()
                .map(|&b| b as f32 / 51.0 - 1.0)
                .collect(),
        }
    }

    /// `(name, pixels, scheme, predictor)` for the chunky strip / tile
    /// paths the budget applies to.
    fn kinds(&self) -> Vec<Kind<'_>> {
        use EncodePixelFormat as E;
        use TiffCompression as C;
        vec![
            (
                "gray8 none",
                E::Gray8 {
                    pixels: &self.gray8,
                },
                C::None,
                false,
            ),
            (
                "gray8 packbits",
                E::Gray8 {
                    pixels: &self.gray8,
                },
                C::PackBits,
                false,
            ),
            (
                "gray16 lzw+pred",
                E::Gray16Le {
                    pixels: &self.gray16,
                },
                C::Lzw,
                true,
            ),
            (
                "rgb8 lzw+pred",
                E::Rgb24 { pixels: &self.rgb8 },
                C::Lzw,
                true,
            ),
            (
                "rgb8 deflate",
                E::Rgb24 { pixels: &self.rgb8 },
                C::Deflate,
                false,
            ),
            (
                "rgb8 zstd+pred",
                E::Rgb24 { pixels: &self.rgb8 },
                C::Zstd,
                true,
            ),
            (
                "rgbf32 deflate+pred",
                E::RgbF32 {
                    pixels: &self.floats,
                },
                C::Deflate,
                true,
            ),
            (
                "bilevel g4",
                E::Bilevel {
                    pixels: &self.bilevel,
                },
                C::CcittT6 {
                    uncompressed: false,
                },
                false,
            ),
            (
                "bilevel g3",
                E::Bilevel {
                    pixels: &self.bilevel,
                },
                C::CcittT4OneD {
                    eol_byte_aligned: true,
                },
                false,
            ),
        ]
    }
}

/// `(tiling, RowsPerStrip)`.
type Layout = (Option<(u32, u32)>, Option<u32>);

const LAYOUTS: [Layout; 4] = [
    (None, None),
    (None, Some(1)),
    (None, Some(13)),
    (Some((32, 48)), None),
];

#[test]
fn every_thread_budget_writes_the_same_file_and_decodes_the_same_page() {
    let src = Sources::new();
    for (name, kind, comp, pred) in src.kinds() {
        for (tiling, rps) in LAYOUTS {
            let page = |threads: usize| {
                EncodePage::new(
                    W,
                    H,
                    kind.clone(),
                    comp,
                    pred,
                    false,
                    tiling,
                    false,
                    PageExtras::default()
                        .with_rows_per_strip(rps)
                        .with_threads(threads),
                )
            };
            let serial = encode_page(&page(1)).unwrap_or_else(|e| panic!("{name}: {e}"));
            for threads in [0, 2, 3, 8, 500] {
                assert_eq!(
                    encode_page(&page(threads)).unwrap(),
                    serial,
                    "{name} {tiling:?} {rps:?}: encode with {threads} threads"
                );
            }
            let reference = decode_page_with(&serial, &DecodeOptions::default()).unwrap();
            for threads in [0, 2, 3, 8, 500] {
                let opts = DecodeOptions::default().with_threads(threads);
                assert_eq!(
                    decode_page_with(&serial, &opts).unwrap(),
                    reference,
                    "{name} {tiling:?} {rps:?}: decode with {threads} threads"
                );
            }
        }
    }
}

#[test]
fn a_damaged_segment_fails_the_same_way_at_every_budget() {
    let src = Sources::new();
    let page = EncodePage::new(
        W,
        H,
        EncodePixelFormat::Rgb24 { pixels: &src.rgb8 },
        TiffCompression::Deflate,
        false,
        false,
        None,
        false,
        PageExtras::default().with_rows_per_strip(Some(9)),
    );
    let mut bytes = encode_page(&page).unwrap();
    // Wreck the middle of the strip data (the directory sits at the
    // end of the file and stays intact).
    let mid = bytes.len() / 3;
    for b in &mut bytes[mid..mid + 64] {
        *b = 0xFF;
    }
    let serial = decode_page_with(&bytes, &DecodeOptions::default())
        .expect_err("a wrecked zlib stream must not decode")
        .to_string();
    for threads in [2, 4, 16] {
        let e = decode_page_with(&bytes, &DecodeOptions::default().with_threads(threads))
            .expect_err("still an error")
            .to_string();
        assert_eq!(e, serial, "{threads} threads");
    }
}

#[test]
fn the_contract_options_carry_the_budget_and_the_level() {
    let src = Sources::new();
    let img = TiffImage::from_rgb8(W, H, src.rgb8.clone()).unwrap();
    let base = EncodeOptions::default()
        .with_compression(TiffCompression::Deflate)
        .with_predictor(true)
        .with_rows_per_strip(8);
    let serial = encode(&img, &base).unwrap();
    assert_eq!(encode(&img, &base.clone().with_threads(4)).unwrap(), serial);
    // Level: default is 6; 1 is larger, 9 no larger, all decode alike.
    let fast = encode(&img, &base.clone().with_level(1)).unwrap();
    let best = encode(&img, &base.clone().with_level(9)).unwrap();
    assert_eq!(encode(&img, &base.clone().with_level(6)).unwrap(), serial);
    assert!(
        fast.len() > serial.len(),
        "{} vs {}",
        fast.len(),
        serial.len()
    );
    assert!(best.len() <= serial.len());
    for file in [&serial, &fast, &best] {
        assert_eq!(decode(file).unwrap(), img);
    }
    // Zstandard takes a level too; schemes without one ignore it.
    let zstd = base.clone().with_compression(TiffCompression::Zstd);
    let z3 = encode(&img, &zstd).unwrap();
    let z19 = encode(&img, &zstd.clone().with_level(19)).unwrap();
    assert!(z19.len() < z3.len(), "{} vs {}", z19.len(), z3.len());
    assert_eq!(decode(&z19).unwrap(), img);
    let lzw = base.clone().with_compression(TiffCompression::Lzw);
    assert_eq!(
        encode(&img, &lzw.clone().with_level(1)).unwrap(),
        encode(&img, &lzw).unwrap()
    );
}
