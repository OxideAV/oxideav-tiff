//! TIFF 6.0 §"Physical Dimensions" / `ResolutionUnit` (tag 296,
//! page 18) decoder coverage.
//!
//! The spec defines three values for the unit of measurement that
//! `XResolution` (tag 282) and `YResolution` (tag 283) are
//! denominated in:
//!
//!   1 = No absolute unit of measurement. Used for images that may
//!       have a non-square aspect ratio but no meaningful absolute
//!       dimensions.
//!   2 = Inch.
//!   3 = Centimeter.
//!
//! "Default = 2 (inch)." (TIFF 6.0 page 18.)
//!
//! The decoder does not dispatch on the resolution unit — the on-disk
//! pixel bytes are independent of the resolution metadata — but
//! malformed writers that produce values outside `1..=3` are surfaced
//! as `Error::InvalidData` rather than silently swallowed so a
//! spec-conformant reader stays a spec-conformant reader on the way
//! through.
//!
//! ## On-disk layout each test produces
//!
//! Same minimal 1x1 Gray8 classic-II TIFF the §SampleFormat and
//! §Orientation tests use, optionally with a `ResolutionUnit` (296)
//! entry on the end. Tag 296 sorts after every other Baseline tag in
//! the fixture so it is always appended last in tag-order — no
//! re-ordering of the existing entry stream is required.

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
/// header, optionally appending a `ResolutionUnit` entry (tag 296,
/// SHORT, count = 1) with the given value. Entries are emitted in
/// ascending tag order per TIFF 6.0 §2.
fn build_1x1_gray8(resolution_unit: Option<u16>) -> Vec<u8> {
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
    //
    // Tag 296 sorts after every other Baseline tag carried in this
    // fixture (256, 257, 258, 259, 262, 273, 277, 278, 279), so the
    // `ResolutionUnit` entry is always appended last when present.
    let n: u16 = 9 + if resolution_unit.is_some() { 1 } else { 0 };
    out.extend_from_slice(&n.to_le_bytes());

    // 18.. — entries, in ascending tag order.
    out.extend_from_slice(&entry_short(256, 1)); // ImageWidth
    out.extend_from_slice(&entry_short(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short(258, 8)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, 1)); // Photometric = BlackIsZero
    out.extend_from_slice(&entry_short(273, 8)); // StripOffsets → byte 8
    out.extend_from_slice(&entry_short(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_short(279, 1)); // StripByteCounts
    if let Some(u) = resolution_unit {
        out.extend_from_slice(&entry_short(296, u)); // ResolutionUnit
    }

    // next_ifd = 0 — single IFD.
    out.extend_from_slice(&0u32.to_le_bytes());

    out
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
fn resolution_unit_absent_decodes_with_default() {
    // §"Physical Dimensions" / `ResolutionUnit`: "Default = 2
    // (inch)." An IFD without the tag must decode unchanged through
    // the existing pixel path — the decoder treats the field as
    // metadata only.
    let bytes = build_1x1_gray8(None);
    let d = decode_tiff(&bytes).expect("baseline 1x1 Gray8 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

#[test]
fn resolution_unit_one_no_absolute_unit_decodes() {
    // `ResolutionUnit = 1` ("No absolute unit of measurement") is a
    // permitted value per §"Physical Dimensions". The decoder must
    // accept it.
    let bytes = build_1x1_gray8(Some(1));
    let d = decode_tiff(&bytes).expect("ResolutionUnit=1 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

#[test]
fn resolution_unit_two_inch_decodes() {
    // `ResolutionUnit = 2` ("Inch") is the spec default — even when
    // written explicitly the decoder must route it through the
    // unchanged pixel path. Output is identical to the absent-tag
    // case.
    let bytes = build_1x1_gray8(Some(2));
    let d = decode_tiff(&bytes).expect("ResolutionUnit=2 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

#[test]
fn resolution_unit_three_centimeter_decodes() {
    // `ResolutionUnit = 3` ("Centimeter") is the third spec-defined
    // value. The decoder must accept it.
    let bytes = build_1x1_gray8(Some(3));
    let d = decode_tiff(&bytes).expect("ResolutionUnit=3 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

#[test]
fn resolution_unit_zero_is_rejected_as_invalid() {
    // The spec defines `1..=3` only. Value 0 comes from a malformed
    // writer; the decoder rejects rather than silently routing it
    // through the default-inch path.
    let bytes = build_1x1_gray8(Some(0));
    expect_err_containing(&bytes, "ResolutionUnit=0");
}

#[test]
fn resolution_unit_four_is_rejected_as_invalid() {
    // The spec defines `1..=3` only. Value 4 is the lowest
    // out-of-range value a writer could emit; the decoder rejects
    // it as `InvalidData` rather than silently treating it as one of
    // the defined units.
    let bytes = build_1x1_gray8(Some(4));
    expect_err_containing(&bytes, "ResolutionUnit=4");
}

#[test]
fn resolution_unit_large_value_is_rejected_as_invalid() {
    // Highest single-SHORT value — same invalid-data outcome as 4.
    let bytes = build_1x1_gray8(Some(65535));
    expect_err_containing(&bytes, "ResolutionUnit=65535");
}
