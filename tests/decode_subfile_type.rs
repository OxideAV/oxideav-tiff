//! NewSubfileType (tag 254, TIFF 6.0 §NewSubfileType page 36)
//! inspection. The decoder must:
//!
//!   * Return [`NewSubfileType::ZERO`] when the tag is absent (spec
//!     "Default is 0").
//!   * Round-trip the three spec-defined flag bits (reduced-resolution,
//!     page-of-multi-page, transparency-mask).
//!   * Reject any undefined high bit (spec "Unused bits are expected
//!     to be 0").
//!   * Reject bit 2 (transparency mask) without PhotometricInterpretation
//!     = 4 (spec "The PhotometricInterpretation value must be 4").
//!
//! All fixtures are binary-independent: each one is hand-built from
//! the writer or assembled byte-for-byte through the `EncodePage`
//! API so the tests are entirely free of external-tooling dependencies.

use oxideav_tiff::{
    decode_tiff_all, decode_tiff_subfile_types, encode_tiff, encode_tiff_multi, EncodePage,
    EncodePixelFormat, NewSubfileType, TiffCompression, TiffError,
};

/// Tiny 4×4 RGB pixel pattern used as the "page of multi-page" content.
fn rgb_4x4() -> Vec<u8> {
    let mut out = Vec::with_capacity(4 * 4 * 3);
    for y in 0..4u8 {
        for x in 0..4u8 {
            out.push(x.wrapping_mul(60));
            out.push(y.wrapping_mul(60));
            out.push(((x as u16 + y as u16) * 30) as u8);
        }
    }
    out
}

/// A 16×8 transparency mask: top half exterior (all 0-bits), bottom
/// half interior (all 1-bits).
fn mask_16x8() -> Vec<u8> {
    let row_bytes = 2usize;
    let mut bytes = vec![0u8; row_bytes * 8];
    for y in 4..8 {
        for b in 0..row_bytes {
            bytes[y * row_bytes + b] = 0xFF;
        }
    }
    bytes
}

fn encode_rgb_page(pixels: &[u8]) -> Vec<u8> {
    let page = EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::Rgb24 { pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    encode_tiff(&page).expect("encode RGB")
}

fn encode_mask_page(pixels: &[u8]) -> Vec<u8> {
    let page = EncodePage {
        width: 16,
        height: 8,
        kind: EncodePixelFormat::TransparencyMask { pixels },
        compression: TiffCompression::PackBits,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    };
    encode_tiff(&page).expect("encode mask")
}

fn make_rgb_page(pixels: &[u8]) -> EncodePage<'_> {
    EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::Rgb24 { pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    }
}

fn make_mask_page(pixels: &[u8]) -> EncodePage<'_> {
    EncodePage {
        width: 16,
        height: 8,
        kind: EncodePixelFormat::TransparencyMask { pixels },
        compression: TiffCompression::PackBits,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
    }
}

#[test]
fn single_page_no_extra_bits_reports_zero_flags() {
    // A vanilla single-page RGB TIFF written by our encoder includes
    // tag 254 with value 0 (the encoder's default). The accessor must
    // surface that as `NewSubfileType::ZERO`, and the three typed
    // bit accessors must all be false.
    let pixels = rgb_4x4();
    let bytes = encode_rgb_page(&pixels);
    let flags = decode_tiff_subfile_types(&bytes).expect("decode subfile types");
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0], NewSubfileType::ZERO);
    assert!(!flags[0].is_reduced_resolution());
    assert!(!flags[0].is_page_of_multipage());
    assert!(!flags[0].is_transparency_mask());
    assert_eq!(flags[0].raw(), 0);
}

#[test]
fn transparency_mask_page_sets_bit_2() {
    // The encoder's TransparencyMask page automatically sets
    // NewSubfileType bit 2 (per TIFF 6.0 §"NewSubfileType" page 36).
    // The decoder accessor must report that bit, and the cross-tag
    // Photometric=4 invariant must pass through silently because both
    // sides agree.
    let pixels = mask_16x8();
    let bytes = encode_mask_page(&pixels);
    let flags = decode_tiff_subfile_types(&bytes).expect("decode subfile types");
    assert_eq!(flags.len(), 1);
    assert!(flags[0].is_transparency_mask());
    assert!(!flags[0].is_reduced_resolution());
    assert!(!flags[0].is_page_of_multipage());
    assert_eq!(flags[0].raw(), 1 << 2);
}

#[test]
fn multi_page_reports_one_entry_per_page() {
    // Two pages: a normal RGB image followed by a transparency mask
    // describing the irregular interior of another image. The
    // accessor must walk the whole next-IFD chain.
    let rgb = rgb_4x4();
    let mask = mask_16x8();
    let pages = vec![make_rgb_page(&rgb), make_mask_page(&mask)];
    let bytes = encode_tiff_multi(&pages).expect("encode multi-page");
    let flags = decode_tiff_subfile_types(&bytes).expect("decode subfile types");
    assert_eq!(flags.len(), 2);
    assert_eq!(flags[0], NewSubfileType::ZERO);
    assert!(flags[1].is_transparency_mask());
    // Sanity: the matching decode_tiff_all walk also returns the same
    // page count, so the two functions see the same IFD chain.
    let pages = decode_tiff_all(&bytes).expect("decode pages");
    assert_eq!(pages.len(), 2);
}

// --- Failure paths: undefined high bits + bit-2 / Photometric=4 invariant ---
//
// These tests synthesize tiny hand-rolled TIFF files (II header,
// single IFD, no pixel data needed because the validation gate runs
// before the strip walker is reached).

/// Build a minimal II classic-TIFF file with a one-IFD payload whose
/// `extra_entries` are merged into the baseline 1×1 WhiteIsZero IFD
/// (the baseline carries StripOffsets/StripByteCounts pointing at a
/// 1-byte scratch strip so decode_tiff_all has something to read).
fn build_min_tiff(extra_entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
    // Baseline IFD: 1×1 WhiteIsZero bilevel image.
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, 3, 1, 1), // ImageWidth = 1
        (257, 3, 1, 1), // ImageLength = 1
        (258, 3, 1, 1), // BitsPerSample = 1
        (259, 3, 1, 1), // Compression = None
        (262, 3, 1, 0), // PhotometricInterpretation = WhiteIsZero
        (273, 4, 1, 0), // StripOffsets — patched after layout known
        (277, 3, 1, 1), // SamplesPerPixel = 1
        (278, 4, 1, 1), // RowsPerStrip = 1
        (279, 4, 1, 1), // StripByteCounts = 1
    ];
    entries.extend_from_slice(extra_entries);
    entries.sort_by_key(|e| e.0);

    let n = entries.len();
    let ifd_size = 2 + n * 12 + 4;
    let strip_offset = 8 + ifd_size;

    for e in entries.iter_mut() {
        if e.0 == 273 {
            e.3 = strip_offset as u32;
        }
    }

    let mut v = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes()); // first IFD at byte 8
    v.extend_from_slice(&(n as u16).to_le_bytes());
    for (tag, ty, cnt, val) in &entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&cnt.to_le_bytes());
        v.extend_from_slice(&val.to_le_bytes());
    }
    v.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
    v.push(0x00); // strip body
    v
}

#[test]
fn undefined_high_bit_is_rejected() {
    // Bit 3 (= 0x08) is not defined as of TIFF 6.0. The spec page-36
    // line "Unused bits are expected to be 0" is treated as
    // enforceable: writers that set undefined bits get a precise
    // InvalidData rather than silent acceptance.
    let bytes = build_min_tiff(&[(254, 4, 1, 0x08)]);
    let err = decode_tiff_subfile_types(&bytes).expect_err("should reject undefined bit");
    let msg = format!("{err:?}");
    assert!(msg.contains("undefined bits"), "{msg}");
    assert!(matches!(err, TiffError::InvalidData(_)));
}

#[test]
fn high_unused_bit_rejected_even_with_defined_bits_set() {
    // 0x04 (bit 2 = transparency mask) plus 0x10 (bit 4, undefined)
    // — must still reject because of the unused-bit set, even though
    // bit 2 is valid in isolation.
    let bytes = build_min_tiff(&[(254, 4, 1, 0x14)]);
    let err = decode_tiff_subfile_types(&bytes).expect_err("should reject high unused bit");
    let msg = format!("{err:?}");
    assert!(msg.contains("undefined bits"), "{msg}");
}

#[test]
fn transparency_mask_bit_without_photometric_4_rejected() {
    // Bit 2 set, but PhotometricInterpretation = 0 (WhiteIsZero) per
    // the base builder. Spec mandates Photometric MUST be 4 — surface
    // as a precise InvalidData so a downstream compositor doesn't
    // ever consume a "mask" that's actually a grayscale image.
    let bytes = build_min_tiff(&[(254, 4, 1, 1 << 2)]);
    let err = decode_tiff_subfile_types(&bytes).expect_err("should reject bit2 without photo=4");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("bit 2") && msg.contains("transparency mask"),
        "{msg}"
    );
    assert!(msg.contains("must be 4"), "{msg}");
}

#[test]
fn reduced_resolution_and_page_bits_accepted_in_combination() {
    // 0x03 = bit 0 (reduced-resolution) + bit 1 (page-of-multipage)
    // — both defined, combination is legal per the spec "These values
    // are defined as bit flags because they are independent of each
    // other."
    let bytes = build_min_tiff(&[(254, 4, 1, 0x03)]);
    let flags = decode_tiff_subfile_types(&bytes).expect("should accept defined bit combo");
    assert_eq!(flags.len(), 1);
    assert!(flags[0].is_reduced_resolution());
    assert!(flags[0].is_page_of_multipage());
    assert!(!flags[0].is_transparency_mask());
}

#[test]
fn raw_value_is_preserved_exactly() {
    // Bit 0 + bit 1 → 0x03. The raw accessor returns the bit pattern
    // as stored in the IFD, useful for archival workflows that need
    // byte-exact metadata preservation across a decode/re-encode loop.
    let bytes = build_min_tiff(&[(254, 4, 1, 0x03)]);
    let flags = decode_tiff_subfile_types(&bytes).expect("decode");
    assert_eq!(flags[0].raw(), 0x03);
}

#[test]
fn missing_tag_defaults_to_zero() {
    // No NewSubfileType entry at all — must default to ZERO per the
    // spec "Default is 0" line.
    let bytes = build_min_tiff(&[]);
    let flags = decode_tiff_subfile_types(&bytes).expect("decode");
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0], NewSubfileType::ZERO);
}
