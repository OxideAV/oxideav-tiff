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
//! Value 1 is the canonical "as-stored == as-displayed" layout.
//! Values 2..=4 are in-place mirror / 180° permutations that keep the
//! stored (width, height) shape; values 5..=8 are transpose-family
//! permutations that swap width and height. The decoder re-orients the
//! fully-assembled image into display order so a downstream consumer
//! always sees the visually-correct geometry. Value 0 and values ≥ 9
//! are surfaced as invalid-data errors because the spec lists 1..=8
//! only.
//!
//! ## On-disk layout each test produces
//!
//! A minimal `w`×`h` Gray8 classic-II TIFF carrying the raw pixel
//! bytes in a single strip, optionally with an Orientation (274) entry.

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

/// IFD entry, LONG (field-type = 4) with a single inline value.
fn entry_long(tag: u16, value: u32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&4u16.to_le_bytes()); // LONG
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..12].copy_from_slice(&value.to_le_bytes());
    e
}

/// Assemble a minimal `w`×`h` Gray8 (BlackIsZero, Compression = None)
/// classic-II TIFF whose single strip holds `pixels` in row-major
/// storage order, optionally appending an Orientation entry (tag 274,
/// SHORT, count = 1). Entries are emitted in ascending tag order per
/// TIFF 6.0 §2.
fn build_gray8(w: u16, h: u16, pixels: &[u8], orientation: Option<u16>) -> Vec<u8> {
    assert_eq!(pixels.len(), w as usize * h as usize);
    let mut out = Vec::new();

    // Header: classic LE, IFD offset computed below. The strip lives
    // immediately after the 8-byte header at offset 8.
    let strip_off = 8u32;
    // IFD starts after the strip, padded to an even offset.
    let mut ifd_off = strip_off + pixels.len() as u32;
    if ifd_off % 2 == 1 {
        ifd_off += 1;
    }

    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_off.to_le_bytes());
    out.extend_from_slice(pixels);
    while out.len() < ifd_off as usize {
        out.push(0);
    }

    // IFD entry count (Orientation sorts between StripOffsets (273) and
    // SamplesPerPixel (277), inserted in tag order when present).
    let n: u16 = 9 + if orientation.is_some() { 1 } else { 0 };
    out.extend_from_slice(&n.to_le_bytes());

    out.extend_from_slice(&entry_short(256, w)); // ImageWidth
    out.extend_from_slice(&entry_short(257, h)); // ImageLength
    out.extend_from_slice(&entry_short(258, 8)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, 1)); // Photometric = BlackIsZero
    out.extend_from_slice(&entry_long(273, strip_off)); // StripOffsets
    if let Some(o) = orientation {
        out.extend_from_slice(&entry_short(274, o)); // Orientation
    }
    out.extend_from_slice(&entry_short(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, h)); // RowsPerStrip = full image
    out.extend_from_slice(&entry_long(279, pixels.len() as u32)); // StripByteCounts

    // next_ifd = 0 — single IFD.
    out.extend_from_slice(&0u32.to_le_bytes());

    out
}

/// The asymmetric 3×2 fixture used by the transform tests. Storage
/// order (w = 3, h = 2):
///
/// ```text
///   row 0: 10 20 30
///   row 1: 40 50 60
/// ```
const FW: u16 = 3;
const FH: u16 = 2;
const FIXTURE: [u8; 6] = [10, 20, 30, 40, 50, 60];

/// Decode the fixture under `orientation` and assert the resulting
/// (width, height) and row-major Gray8 plane bytes.
fn check(orientation: Option<u16>, exp_w: u32, exp_h: u32, exp: &[u8]) {
    let bytes = build_gray8(FW, FH, &FIXTURE, orientation);
    let d = decode_tiff(&bytes).expect("fixture must decode");
    assert_eq!(
        (d.width, d.height),
        (exp_w, exp_h),
        "orientation={orientation:?}: display dimensions"
    );
    assert_eq!(
        d.frame.planes[0].data, exp,
        "orientation={orientation:?}: display pixels"
    );
    // The single-plane Gray8 stride is exactly the display width.
    assert_eq!(d.frame.planes[0].stride, exp_w as usize);
}

/// Helper: assert `decode_tiff` returned an error whose Display
/// includes the given substring. `DecodedTiff` does not implement
/// `Debug`, so we drive the outcome through a `match`.
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
    // §Orientation: "Default is 1." An IFD without the tag decodes in
    // storage order, which is also display order.
    check(None, FW as u32, FH as u32, &FIXTURE);
}

#[test]
fn orientation_one_is_identity() {
    // Orientation = 1 — canonical top-left; identical to the absent
    // case.
    check(Some(1), FW as u32, FH as u32, &FIXTURE);
}

#[test]
fn orientation_two_mirrors_horizontally() {
    // 0th column → visual right: each row is reversed left-to-right.
    //   10 20 30        30 20 10
    //   40 50 60   →    60 50 40
    check(Some(2), 3, 2, &[30, 20, 10, 60, 50, 40]);
}

#[test]
fn orientation_three_rotates_180() {
    // 0th row → bottom, 0th column → right: 180° rotation.
    //   10 20 30        60 50 40
    //   40 50 60   →    30 20 10
    check(Some(3), 3, 2, &[60, 50, 40, 30, 20, 10]);
}

#[test]
fn orientation_four_mirrors_vertically() {
    // 0th row → bottom: rows reversed top-to-bottom.
    //   10 20 30        40 50 60
    //   40 50 60   →    10 20 30
    check(Some(4), 3, 2, &[40, 50, 60, 10, 20, 30]);
}

#[test]
fn orientation_five_transposes() {
    // 0th row → left, 0th column → top: transpose (w↔h).
    //   10 20 30        10 40
    //   40 50 60   →    20 50
    //                   30 60
    check(Some(5), 2, 3, &[10, 40, 20, 50, 30, 60]);
}

#[test]
fn orientation_six_rotates_90_cw() {
    // 0th row → right, 0th column → top: 90° clockwise (w↔h). The
    // stored bottom-left pixel (40) becomes the display top-left.
    //   10 20 30        40 10
    //   40 50 60   →    50 20
    //                   60 30
    check(Some(6), 2, 3, &[40, 10, 50, 20, 60, 30]);
}

#[test]
fn orientation_seven_transposes_anti() {
    // 0th row → right, 0th column → bottom: antitranspose (w↔h).
    //   10 20 30        60 30
    //   40 50 60   →    50 20
    //                   40 10
    check(Some(7), 2, 3, &[60, 30, 50, 20, 40, 10]);
}

#[test]
fn orientation_eight_rotates_90_ccw() {
    // 0th row → left, 0th column → bottom: 90° counter-clockwise
    // (w↔h). The stored top-right pixel (30) becomes the display
    // top-left.
    //   10 20 30        30 60
    //   40 50 60   →    20 50
    //                   10 40
    check(Some(8), 2, 3, &[30, 60, 20, 50, 10, 40]);
}

#[test]
fn orientation_round_trip_chain_is_consistent() {
    // Applying orientation 2 (horizontal mirror) twice via the decoder
    // would restore the original; here we just confirm 2 and 4 are
    // distinct from each other and from the identity, guarding against
    // a remap that collapses cases.
    let id = decode_tiff(&build_gray8(FW, FH, &FIXTURE, Some(1))).unwrap();
    let two = decode_tiff(&build_gray8(FW, FH, &FIXTURE, Some(2))).unwrap();
    let four = decode_tiff(&build_gray8(FW, FH, &FIXTURE, Some(4))).unwrap();
    assert_ne!(id.frame.planes[0].data, two.frame.planes[0].data);
    assert_ne!(id.frame.planes[0].data, four.frame.planes[0].data);
    assert_ne!(two.frame.planes[0].data, four.frame.planes[0].data);
}

/// Assemble a minimal `w`×`h` RGB (3-channel, 8-bit, BlackIsZero
/// semantics via PhotometricInterpretation = 2) classic-II TIFF whose
/// single chunky strip holds `pixels` (3 bytes per pixel, row-major),
/// optionally appending an Orientation entry. Exercises the
/// multi-byte-per-pixel cell remap (`bpp = 3`).
fn build_rgb24(w: u16, h: u16, pixels: &[u8], orientation: Option<u16>) -> Vec<u8> {
    assert_eq!(pixels.len(), w as usize * h as usize * 3);
    let mut out = Vec::new();

    let strip_off = 8u32;
    let mut ifd_off = strip_off + pixels.len() as u32;
    if ifd_off % 2 == 1 {
        ifd_off += 1;
    }

    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_off.to_le_bytes());
    out.extend_from_slice(pixels);
    while out.len() < ifd_off as usize {
        out.push(0);
    }

    // BitsPerSample is a 3-entry SHORT array; with each value (8) ≤ the
    // 2-byte inline budget it does not fit inline (3×2 = 6 > 4), so it
    // is written out-of-line. Place it right after the IFD.
    let n: u16 = 9 + if orientation.is_some() { 1 } else { 0 };
    let ifd_len = 2 + n as usize * 12 + 4;
    let bps_off = ifd_off as usize + ifd_len;

    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&entry_short(256, w)); // ImageWidth
    out.extend_from_slice(&entry_short(257, h)); // ImageLength
                                                 // BitsPerSample: SHORT, count 3, offset → bps_off.
    let mut bps = [0u8; 12];
    bps[0..2].copy_from_slice(&258u16.to_le_bytes());
    bps[2..4].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    bps[4..8].copy_from_slice(&3u32.to_le_bytes()); // count = 3
    bps[8..12].copy_from_slice(&(bps_off as u32).to_le_bytes());
    out.extend_from_slice(&bps);
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, 2)); // Photometric = RGB
    out.extend_from_slice(&entry_long(273, strip_off)); // StripOffsets
    if let Some(o) = orientation {
        out.extend_from_slice(&entry_short(274, o)); // Orientation
    }
    out.extend_from_slice(&entry_short(277, 3)); // SamplesPerPixel = 3
    out.extend_from_slice(&entry_short(278, h)); // RowsPerStrip
    out.extend_from_slice(&entry_long(279, pixels.len() as u32)); // StripByteCounts
    out.extend_from_slice(&0u32.to_le_bytes()); // next_ifd = 0

    // BitsPerSample out-of-line payload: three SHORTs = (8, 8, 8).
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());

    out
}

#[test]
fn orientation_rgb24_rotates_90_cw() {
    // bpp = 3 cell remap. A 2×1 (w = 2, h = 1) image:
    //   stored: [R0 G0 B0][R1 G1 B1]
    // Orientation 6 (90° CW) → display dims 1×2:
    //   row 0: [R0 G0 B0]
    //   row 1: [R1 G1 B1]
    // (stored bottom-left == top-left for a single row, so the column
    // order is preserved top-to-bottom).
    let pixels = [11u8, 12, 13, 21, 22, 23];
    let bytes = build_rgb24(2, 1, &pixels, Some(6));
    let d = decode_tiff(&bytes).expect("rgb fixture must decode");
    assert_eq!((d.width, d.height), (1, 2));
    assert_eq!(d.frame.planes[0].data, vec![11, 12, 13, 21, 22, 23]);
    assert_eq!(d.frame.planes[0].stride, 3);
}

#[test]
fn orientation_rgb24_mirrors_horizontally() {
    // bpp = 3, Orientation 2 on a 2×1 image swaps the two pixels.
    let pixels = [11u8, 12, 13, 21, 22, 23];
    let bytes = build_rgb24(2, 1, &pixels, Some(2));
    let d = decode_tiff(&bytes).expect("rgb fixture must decode");
    assert_eq!((d.width, d.height), (2, 1));
    assert_eq!(d.frame.planes[0].data, vec![21, 22, 23, 11, 12, 13]);
}

#[test]
fn orientation_zero_is_rejected_as_invalid() {
    // The spec defines 1..=8 only. Value 0 comes from a malformed
    // writer; the decoder rejects rather than silently routing it
    // through the Orientation = 1 path.
    let bytes = build_gray8(FW, FH, &FIXTURE, Some(0));
    expect_err_containing(&bytes, "Orientation=0");
}

#[test]
fn orientation_nine_is_rejected_as_invalid() {
    // The spec defines 1..=8 only. Value 9 comes from a malformed or
    // future-proofed writer.
    let bytes = build_gray8(FW, FH, &FIXTURE, Some(9));
    expect_err_containing(&bytes, "Orientation=9");
}

#[test]
fn orientation_large_value_is_rejected_as_invalid() {
    // Highest single-SHORT value — same invalid-data outcome as 9.
    let bytes = build_gray8(FW, FH, &FIXTURE, Some(65535));
    expect_err_containing(&bytes, "Orientation=65535");
}
