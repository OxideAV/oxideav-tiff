//! TIFF 6.0 §Orientation (tag 274, page 36) decoder coverage.
//!
//! The spec defines eight orientation values that describe how the
//! stored 0th row + 0th column map onto the displayed image:
//!
//!   1 = 0th row visual top,    0th column visual left   (canonical)
//!   2 = 0th row visual top,    0th column visual right
//!   3 = 0th row visual bottom, 0th column visual right
//!   4 = 0th row visual bottom, 0th column visual left
//!   5 = 0th row visual left,   0th column visual top
//!   6 = 0th row visual right,  0th column visual top
//!   7 = 0th row visual right,  0th column visual bottom
//!   8 = 0th row visual left,   0th column visual bottom
//!
//! The spec then states: "Default is 1. Support for orientations
//! other than 1 is not a Baseline TIFF requirement." This decoder
//! is one such Baseline-only reader — it surfaces pixels in storage
//! order without rotation or mirroring — so the canonical value 1
//! is the only orientation it can honour without silently
//! mis-rendering. Values 2..=8 are surfaced as precise typed errors
//! rather than silently treated as 1 (which would yield correctly
//! coloured but geometrically wrong output); value 0 and ≥ 9 are
//! surfaced as invalid-data errors per spec.
//!
//! ## On-disk layout each test produces
//!
//! Same minimal 1x1 Gray8 classic-II TIFF the §SampleFormat tests
//! use, optionally with an Orientation (274) entry on the end.

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
/// header, optionally appending an Orientation entry (tag 274,
/// SHORT, count = 1) with the given value. Entries are emitted in
/// ascending tag order per TIFF 6.0 §2.
fn build_1x1_gray8(orientation: Option<u16>) -> Vec<u8> {
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
    // The Orientation entry (tag 274) sorts between StripOffsets
    // (273) and SamplesPerPixel (277), so when present it is
    // inserted into the entry stream at the right position rather
    // than appended.
    let n: u16 = 9 + if orientation.is_some() { 1 } else { 0 };
    out.extend_from_slice(&n.to_le_bytes());

    // 18.. — entries, in ascending tag order.
    out.extend_from_slice(&entry_short(256, 1)); // ImageWidth
    out.extend_from_slice(&entry_short(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short(258, 8)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, 1)); // Photometric = BlackIsZero
    out.extend_from_slice(&entry_short(273, 8)); // StripOffsets → byte 8
    if let Some(o) = orientation {
        out.extend_from_slice(&entry_short(274, o)); // Orientation
    }
    out.extend_from_slice(&entry_short(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_short(279, 1)); // StripByteCounts

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
fn orientation_absent_decodes_as_canonical_layout() {
    // §Orientation: "Default is 1." An IFD without the tag must
    // decode without touching the Orientation path — pixels are
    // stored in display order.
    let bytes = build_1x1_gray8(None);
    let d = decode_tiff(&bytes).expect("baseline 1x1 Gray8 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

#[test]
fn orientation_one_decodes_as_canonical_layout() {
    // Orientation = 1 is the spec default; even when written
    // explicitly the decoder must accept it as the canonical
    // top-left layout. The output is identical to the absent-tag
    // case — pixels in storage order.
    let bytes = build_1x1_gray8(Some(1));
    let d = decode_tiff(&bytes).expect("Orientation=1 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}

#[test]
fn orientation_two_terminates_gracefully() {
    // Orientation = 2 (0th row visual top, 0th column visual
    // right — i.e. horizontal mirror) is a permitted value per
    // §Orientation but is not Baseline. The §Orientation reader
    // rule lets a Baseline-only reader refuse rather than silently
    // surface the bytes as if they were Orientation = 1. The
    // decoder surfaces a typed error instead.
    let bytes = build_1x1_gray8(Some(2));
    expect_err_containing(&bytes, "Orientation=2");
}

#[test]
fn orientation_three_terminates_gracefully() {
    // Orientation = 3 — 180° rotation. Same §Orientation rule.
    let bytes = build_1x1_gray8(Some(3));
    expect_err_containing(&bytes, "Orientation=3");
}

#[test]
fn orientation_four_terminates_gracefully() {
    // Orientation = 4 — vertical mirror. Same §Orientation rule.
    let bytes = build_1x1_gray8(Some(4));
    expect_err_containing(&bytes, "Orientation=4");
}

#[test]
fn orientation_five_terminates_gracefully() {
    // Orientation = 5 — transpose (rows/columns swapped). Same
    // §Orientation rule.
    let bytes = build_1x1_gray8(Some(5));
    expect_err_containing(&bytes, "Orientation=5");
}

#[test]
fn orientation_six_terminates_gracefully() {
    // Orientation = 6 — 90° CW (common from many digital cameras
    // for portrait shots). Same §Orientation rule.
    let bytes = build_1x1_gray8(Some(6));
    expect_err_containing(&bytes, "Orientation=6");
}

#[test]
fn orientation_seven_terminates_gracefully() {
    // Orientation = 7 — antitranspose. Same §Orientation rule.
    let bytes = build_1x1_gray8(Some(7));
    expect_err_containing(&bytes, "Orientation=7");
}

#[test]
fn orientation_eight_terminates_gracefully() {
    // Orientation = 8 — 270° CW. Same §Orientation rule.
    let bytes = build_1x1_gray8(Some(8));
    expect_err_containing(&bytes, "Orientation=8");
}

#[test]
fn orientation_zero_is_rejected_as_invalid() {
    // The spec defines 1..=8 only. Value 0 comes from a malformed
    // writer; the decoder rejects rather than silently routing it
    // through the Orientation = 1 path.
    let bytes = build_1x1_gray8(Some(0));
    expect_err_containing(&bytes, "Orientation=0");
}

#[test]
fn orientation_nine_is_rejected_as_invalid() {
    // The spec defines 1..=8 only. Value 9 comes from a malformed
    // or future-proofed writer; the decoder rejects rather than
    // silently routing it through one of the canonical orientations.
    let bytes = build_1x1_gray8(Some(9));
    expect_err_containing(&bytes, "Orientation=9");
}

#[test]
fn orientation_large_value_is_rejected_as_invalid() {
    // Highest single-SHORT value — same invalid-data outcome as 9.
    let bytes = build_1x1_gray8(Some(65535));
    expect_err_containing(&bytes, "Orientation=65535");
}
