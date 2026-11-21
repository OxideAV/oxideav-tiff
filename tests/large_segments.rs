//! Segment sizes: a strip or tile is bounded by what the directory
//! declares and by `DecodeOptions::max_bytes`, not by a fixed cap.

use oxideav_tiff::{
    DecodeOptions, EncodePage, EncodePixelFormat, PageExtras, TiffCompression, decode_page_with,
    encode_page,
};

/// A single strip may be the whole image: a strip just over 64 MiB
/// decodes under Deflate and Zstandard (segments used to be capped at
/// 64 MiB regardless of what the directory declared), while the
/// caller's byte limit still applies before anything is inflated.
#[test]
fn one_strip_larger_than_64_mib_decodes() {
    let (w, h) = (8200u32, 8200u32);
    // Mostly flat (cheap to code in an unoptimised build), with marks
    // at both ends and in the part past the old cap.
    let mut gray = vec![0u8; w as usize * h as usize];
    assert!(gray.len() > 64 << 20);
    let n = gray.len();
    for at in [0, 1, n / 2, (64 << 20) + 5, n - 2, n - 1] {
        gray[at] = (at % 251) as u8 | 1;
    }
    // Deflate level 0 is stored blocks.
    for (compression, level) in [(TiffCompression::Deflate, 0), (TiffCompression::Zstd, 1)] {
        let page = EncodePage::new(
            w,
            h,
            EncodePixelFormat::Gray8 { pixels: &gray },
            compression,
            false,
            false,
            None,
            false,
            PageExtras::default().with_level(Some(level)),
        );
        let bytes = encode_page(&page).unwrap();
        let back = decode_page_with(&bytes, &DecodeOptions::default()).unwrap();
        assert!(
            back.image.as_bytes() == Some(gray.as_slice()),
            "{compression:?}"
        );
        let tight = DecodeOptions::default().with_max_bytes(1u64 << 20);
        assert!(decode_page_with(&bytes, &tight).is_err());
    }
}

/// `TileWidth` / `TileLength` are free LONGs: absurd values are an
/// error before any buffer is sized from them — at the default limits
/// and with every limit lifted.
#[test]
fn absurd_tile_dimensions_are_an_error_not_an_allocation() {
    let gray = [7u8; 256];
    let page = EncodePage::new(
        16,
        16,
        EncodePixelFormat::Gray8 { pixels: &gray },
        TiffCompression::Deflate,
        false,
        false,
        Some((16, 16)),
        false,
        PageExtras::default(),
    );
    let good = encode_page(&page).unwrap();
    assert!(decode_page_with(&good, &DecodeOptions::default()).is_ok());
    // Rewrite both tile-dimension entries (little-endian classic IFD)
    // as LONGs holding `dim`.
    let with_tile = |dim: u32| -> Vec<u8> {
        let mut b = good.clone();
        let ifd = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let n = u16::from_le_bytes([b[ifd], b[ifd + 1]]) as usize;
        let mut patched = 0;
        for i in 0..n {
            let at = ifd + 2 + 12 * i;
            let tag = u16::from_le_bytes([b[at], b[at + 1]]);
            if tag == 322 || tag == 323 {
                b[at + 2..at + 4].copy_from_slice(&4u16.to_le_bytes());
                b[at + 4..at + 8].copy_from_slice(&1u32.to_le_bytes());
                b[at + 8..at + 12].copy_from_slice(&dim.to_le_bytes());
                patched += 1;
            }
        }
        assert_eq!(patched, 2);
        b
    };
    let lifted = DecodeOptions::default().unlimited();
    for dim in [0xFFFF_FFF0u32, 0x8000_0000] {
        let bytes = with_tile(dim);
        assert!(decode_page_with(&bytes, &DecodeOptions::default()).is_err());
        assert!(decode_page_with(&bytes, &lifted).is_err());
    }
    // A 4 GiB tile is within the address space but past the default
    // byte limit.
    let e = decode_page_with(&with_tile(0x1_0000), &DecodeOptions::default()).unwrap_err();
    assert!(matches!(e, oxideav_tiff::Error::LimitExceeded(_)), "{e}");
}
