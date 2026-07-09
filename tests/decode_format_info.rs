//! Structural / codec introspection ([`oxideav_tiff::TiffFormatInfo`]).
//!
//! `DecodedTiff::format` reports the raw on-disk layout tags —
//! photometric, compression, per-sample bit depth, planar / tiled
//! layout — the values a transcoder or `tiffinfo`-style CLI needs
//! without re-walking the IFD. Oracle: encode with a known
//! configuration, decode, confirm the reported tags match.

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, PageExtras, TiffCompression,
};

fn base<'a>(w: u32, h: u32, kind: EncodePixelFormat<'a>) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind,
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
    }
}

#[test]
fn gray8_none_stripped_reports_baseline_layout() {
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let tiff = encode_tiff(&base(4, 4, EncodePixelFormat::Gray8 { pixels: &px })).unwrap();
    let f = decode_tiff(&tiff).unwrap().format;
    assert_eq!(f.photometric, Some(1)); // BlackIsZero
    assert_eq!(f.compression, Some(1)); // none
    assert_eq!(f.bits_per_sample, vec![8]);
    assert_eq!(f.samples_per_pixel, 1);
    assert!(!f.tiled);
    assert_eq!(f.tile_size, None);
}

#[test]
fn rgb24_lzw_reports_photometric_compression_and_bits() {
    let px: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 5) as u8).collect();
    let mut page = base(4, 4, EncodePixelFormat::Rgb24 { pixels: &px });
    page.compression = TiffCompression::Lzw;
    let tiff = encode_tiff(&page).unwrap();
    let f = decode_tiff(&tiff).unwrap().format;
    assert_eq!(f.photometric, Some(2)); // RGB
    assert_eq!(f.compression, Some(5)); // LZW
    assert_eq!(f.bits_per_sample, vec![8, 8, 8]);
    assert_eq!(f.samples_per_pixel, 3);
}

#[test]
fn rgb_deflate_predictor_reports_predictor_tag() {
    let px: Vec<u8> = (0..8 * 2 * 3).map(|i| (i * 3 + 1) as u8).collect();
    let mut page = base(8, 2, EncodePixelFormat::Rgb24 { pixels: &px });
    page.compression = TiffCompression::Deflate;
    page.predictor = true;
    let tiff = encode_tiff(&page).unwrap();
    let f = decode_tiff(&tiff).unwrap().format;
    assert_eq!(f.compression, Some(8)); // Adobe Deflate
    assert_eq!(f.predictor, Some(2)); // horizontal differencing
}

#[test]
fn tiled_rgb_reports_tile_geometry() {
    // A 32x32 RGB image tiled 16x16.
    let px: Vec<u8> = (0..32 * 32 * 3).map(|i| (i % 251) as u8).collect();
    let mut page = base(32, 32, EncodePixelFormat::Rgb24 { pixels: &px });
    page.tiling = Some((16, 16));
    let tiff = encode_tiff(&page).unwrap();
    let f = decode_tiff(&tiff).unwrap().format;
    assert!(f.tiled);
    assert_eq!(f.tile_size, Some((16, 16)));
    assert_eq!(f.rows_per_strip, None);
}

#[test]
fn planar_rgb_reports_planar_config_2() {
    let px: Vec<u8> = (0..4 * 4 * 3).map(|i| (i * 7) as u8).collect();
    let mut page = base(4, 4, EncodePixelFormat::Rgb24 { pixels: &px });
    page.planar = true;
    let tiff = encode_tiff(&page).unwrap();
    let f = decode_tiff(&tiff).unwrap().format;
    assert_eq!(f.planar_config, Some(2)); // separate planes
}

#[test]
fn gray16_reports_16_bit_depth() {
    let px: Vec<u8> = (0..4 * 4 * 2).map(|i| (i * 9) as u8).collect();
    let tiff = encode_tiff(&base(4, 4, EncodePixelFormat::Gray16Le { pixels: &px })).unwrap();
    let f = decode_tiff(&tiff).unwrap().format;
    assert_eq!(f.bits_per_sample, vec![16]);
    assert_eq!(f.samples_per_pixel, 1);
}

#[test]
fn signed_gray_reports_sample_format_int() {
    let px: Vec<i8> = (0..16i32).map(|i| (i - 8) as i8).collect();
    let tiff = encode_tiff(&base(4, 4, EncodePixelFormat::GrayI8 { pixels: &px })).unwrap();
    let f = decode_tiff(&tiff).unwrap().format;
    // SampleFormat = 2 (two's-complement signed integer), TIFF 6.0 §19.
    assert_eq!(f.sample_format, vec![2]);
}
