//! TIFF 6.0 §SampleFormat (tag 339, page 80) decoder coverage.
//!
//! The spec lists four interpretations a writer may declare for the
//! per-sample numeric type — `1` (unsigned integer, the Baseline
//! default), `2` (two's-complement signed integer), `3` (IEEE
//! floating-point) and `4` (undefined) — and concludes with the
//! reader rule: "If the SampleFormat field is present and the value
//! is not 1, a Baseline TIFF reader that cannot handle the
//! SampleFormat value must terminate the import process gracefully."
//!
//! These tests build minimal hand-crafted classic-TIFF byte strings
//! (8-byte header + one IFD + a one-pixel strip) and exercise each
//! relevant `SampleFormat` value through the public `decode_tiff`
//! entry point.
//!
//! ## On-disk layout each test produces
//!
//! ```text
//! offset 0   "II" + 42        little-endian classic TIFF header
//! offset 4   first_ifd = 16   4-byte u32 offset (header is 8 bytes,
//!                             so the IFD starts at 16; bytes 8..16
//!                             hold the one-byte strip plus padding)
//! offset 8   pixel byte       the single sample for our 1x1 image
//! offset 16  entry_count: u16 number of IFD entries
//! offset 18  entries[]        12 bytes each (tag u16, type u16,
//!                             count u32, value-or-offset u32)
//! offset N   next_ifd: u32    0 — single IFD
//! ```
//!
//! IFD entries used (all SHORT, inline) for the baseline 1x1 Gray8:
//!
//!   ImageWidth (256)         = 1
//!   ImageLength (257)        = 1
//!   BitsPerSample (258)      = 8
//!   Compression (259)        = 1   (None)
//!   PhotometricInterpretation (262) = 1   (BlackIsZero)
//!   StripOffsets (273)       = 8   (points back at the pixel byte)
//!   SamplesPerPixel (277)    = 1
//!   RowsPerStrip (278)       = 1
//!   StripByteCounts (279)    = 1
//!
//! Some tests add a SampleFormat (339) entry on the end to drive the
//! §SampleFormat reader rule under test.

use oxideav_tiff::decode_tiff;

/// IFD entry, SHORT (field-type = 3) with a single inline value.
fn entry_short(tag: u16, value: u16) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..10].copy_from_slice(&value.to_le_bytes());
    e
}

/// Assemble the minimal 1x1 Gray8 TIFF described in the module
/// header, optionally appending a SampleFormat (339, SHORT, count 1)
/// entry with the given value.
fn build_1x1_gray8(sample_format: Option<u16>) -> Vec<u8> {
    let mut out = Vec::new();

    // 0..8 — classic LE header pointing at IFD = 16.
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&16u32.to_le_bytes());
    // 8 — the strip's one pixel byte (so StripOffsets = 8 lines up).
    out.push(0xAB);
    // 9..16 — padding so the IFD starts at offset 16.
    out.extend_from_slice(&[0u8; 7]);

    // 16 — IFD entry count.
    let n: u16 = 9 + if sample_format.is_some() { 1 } else { 0 };
    out.extend_from_slice(&n.to_le_bytes());

    // 18.. — entries, in ascending tag order (TIFF 6.0 §2 "sorted by
    // tag number"). All values fit in a SHORT (≤ 65535) for this 1x1
    // image, so every entry is inline.
    out.extend_from_slice(&entry_short(256, 1)); // ImageWidth
    out.extend_from_slice(&entry_short(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short(258, 8)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, 1)); // Photometric = BlackIsZero
    out.extend_from_slice(&entry_short(273, 8)); // StripOffsets → byte 8
    out.extend_from_slice(&entry_short(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_short(279, 1)); // StripByteCounts
    if let Some(fmt) = sample_format {
        out.extend_from_slice(&entry_short(339, fmt));
    }

    // next_ifd = 0 — single IFD.
    out.extend_from_slice(&0u32.to_le_bytes());

    out
}

#[test]
fn sample_format_absent_decodes_as_unsigned() {
    // The §SampleFormat default paragraph: "Default is 1, unsigned
    // integer data." An IFD without the tag must decode without
    // touching the SampleFormat path.
    let bytes = build_1x1_gray8(None);
    let d = decode_tiff(&bytes).expect("baseline 1x1 Gray8 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

#[test]
fn sample_format_uint_decodes_as_unsigned() {
    // SampleFormat = 1 is the spec default; even when written
    // explicitly the decoder must accept it as unsigned data.
    let bytes = build_1x1_gray8(Some(1));
    let d = decode_tiff(&bytes).expect("SampleFormat=1 must decode");
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

#[test]
fn sample_format_undefined_falls_back_to_unsigned() {
    // §SampleFormat: "A reader would typically treat an image with
    // 'undefined' data as if the field were not present (i.e. as
    // unsigned integer data)." We follow the spec's recommendation
    // and decode value 4 as unsigned.
    let bytes = build_1x1_gray8(Some(4));
    let d = decode_tiff(&bytes).expect("SampleFormat=4 must fall back to unsigned");
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

/// Helper: assert `decode_tiff` returned an error whose Display
/// includes the given substring. `DecodedTiff` does not implement
/// `Debug`, so we can't use `.unwrap_err()` and instead drive the
/// outcome through a `match`.
fn expect_err_containing(bytes: &[u8], needle: &str) {
    match decode_tiff(bytes) {
        Ok(_) => panic!("expected an error containing {needle:?}, got Ok(..)"),
        Err(e) => {
            let msg = format!("{e}");
            assert!(
                msg.contains(needle),
                "expected error to contain {needle:?}, got: {msg}"
            );
        }
    }
}

#[test]
fn sample_format_signed_int_terminates_gracefully() {
    // §SampleFormat reader rule: a value of 2 the decoder cannot
    // handle "must terminate the import process gracefully" — i.e.
    // surface a typed error rather than silently re-interpreting the
    // bytes as unsigned.
    let bytes = build_1x1_gray8(Some(2));
    expect_err_containing(&bytes, "SampleFormat=2");
}

#[test]
fn sample_format_ieee_fp_terminates_gracefully() {
    // Same §SampleFormat reader rule for value 3 (IEEE
    // floating-point). The bit pattern of a u8 0xAB has no meaning as
    // an IEEE float, so decoding it as unsigned would be silent
    // garbage; the decoder must refuse.
    let bytes = build_1x1_gray8(Some(3));
    expect_err_containing(&bytes, "SampleFormat=3");
}

#[test]
fn sample_format_unknown_value_is_rejected() {
    // The spec defines 1..=4 only. Any other value comes from a
    // malformed or future-proofed writer; the decoder rejects rather
    // than silently routing it through the unsigned path.
    let bytes = build_1x1_gray8(Some(99));
    expect_err_containing(&bytes, "SampleFormat=99");
}
