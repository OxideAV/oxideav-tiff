//! Hostile-input hardening for the decode-side metadata + format-info
//! extractors ([`oxideav_tiff::TiffMetadata`] / `TiffFormatInfo`).
//!
//! The IFD parser already rejects out-of-bounds value offsets, so the
//! extractor layer's contract is narrower but sharp: an informational
//! tag that *parses* but is semantically malformed — wrong field type,
//! zero / short count, an out-of-range enum value, an unterminated or
//! non-UTF-8 ASCII string — must leave that one metadata field empty
//! and NEVER gate the pixel decode or panic.
//!
//! Every fixture is a hand-built 1×1 8-bit BlackIsZero TIFF whose IFD
//! parses cleanly; only the metadata entry under test is malformed.

use oxideav_tiff::ifd::{ByteOrder, Entry};
use oxideav_tiff::metadata::extract_metadata;
use oxideav_tiff::{decode_tiff, ResolutionUnit};

/// Build one raw IFD [`Entry`] for direct extractor unit tests (values
/// stored little-endian). Used for the tags whose *pixel decoder*
/// validation would reject the whole file before the metadata layer
/// runs — the extractor's totality is still the contract when it is
/// reached (e.g. after a valid decode, or from the fuzz harness).
fn raw(tag: u16, field_type: u16, count: u64, data: Vec<u8>) -> Entry {
    Entry {
        tag,
        field_type,
        count,
        data,
    }
}

fn entry(tag: u16, ty: u16, count: u32, value: [u8; 4]) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&ty.to_le_bytes());
    e[4..8].copy_from_slice(&count.to_le_bytes());
    e[8..12].copy_from_slice(&value);
    e
}

/// A 1×1 gray TIFF plus arbitrary extra IFD entries and out-of-line
/// blob bytes. Any entry with an out-of-line value must set its value
/// slot to the absolute file offset the caller computed for its blob.
fn build(extra: &[[u8; 12]], blobs: &[u8]) -> Vec<u8> {
    let mut core = vec![
        entry(256, 3, 1, 1u32.to_le_bytes()),
        entry(257, 3, 1, 1u32.to_le_bytes()),
        entry(258, 3, 1, 8u32.to_le_bytes()),
        entry(259, 3, 1, 1u32.to_le_bytes()),
        entry(262, 3, 1, 1u32.to_le_bytes()),
        entry(273, 4, 1, 0u32.to_le_bytes()), // StripOffsets (patched below)
        entry(278, 3, 1, 1u32.to_le_bytes()),
        entry(279, 4, 1, 1u32.to_le_bytes()),
        entry(277, 3, 1, 1u32.to_le_bytes()),
    ];
    core.extend_from_slice(extra);
    core.sort_by_key(|e| u16::from_le_bytes([e[0], e[1]]));
    let n = core.len() as u16;
    let pixel_off = 8u32 + 2 + n as u32 * 12 + 4;

    let mut v = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&n.to_le_bytes());
    for e in &core {
        let mut e = *e;
        if u16::from_le_bytes([e[0], e[1]]) == 273 {
            e[8..12].copy_from_slice(&pixel_off.to_le_bytes());
        }
        v.extend_from_slice(&e);
    }
    v.extend_from_slice(&0u32.to_le_bytes());
    v.push(0xAB); // the single pixel
    v.extend_from_slice(blobs);
    v
}

/// The pixel byte must always survive; assert it and return metadata.
fn decode_ok(bytes: &[u8]) -> oxideav_tiff::DecodedTiff {
    let d = decode_tiff(bytes).expect("hostile metadata must not gate pixel decode");
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
    d
}

#[test]
fn ascii_field_with_wrong_type_is_dropped() {
    // ImageDescription (270) declared as LONG instead of ASCII.
    let d = decode_ok(&build(&[entry(270, 4, 1, 42u32.to_le_bytes())], &[]));
    assert_eq!(d.metadata.image_description, None);
}

#[test]
fn resolution_wrong_type_is_dropped() {
    // XResolution (282) declared ASCII instead of RATIONAL.
    let d = decode_ok(&build(&[entry(282, 2, 1, [b'x', 0, 0, 0])], &[]));
    assert_eq!(d.metadata.x_resolution, None);
}

#[test]
fn resolution_unit_valid_still_reads() {
    let d = decode_ok(&build(&[entry(296, 3, 1, 3u32.to_le_bytes())], &[]));
    assert_eq!(d.metadata.resolution_unit, Some(ResolutionUnit::Centimeter));
}

// The following two tags are validated by the *pixel decoder* (an
// out-of-range Orientation / ResolutionUnit fails the whole decode by
// long-standing contract — see `decode_orientation.rs` /
// `decode_resolution_unit.rs`). The metadata extractor sits downstream
// and is nonetheless total: exercise it directly so a future decoder
// that tolerates these still gets an empty field, not a panic.

#[test]
fn extractor_drops_out_of_range_resolution_unit() {
    let entries = vec![raw(296, 3, 1, 99u16.to_le_bytes().to_vec())];
    let m = extract_metadata(&entries, ByteOrder::Little);
    assert_eq!(m.resolution_unit, None);
}

#[test]
fn extractor_drops_orientation_beyond_u16() {
    // Orientation as LONG whose value overflows u16.
    let entries = vec![raw(274, 4, 1, 70_000u32.to_le_bytes().to_vec())];
    let m = extract_metadata(&entries, ByteOrder::Little);
    assert_eq!(m.orientation, None);
}

#[test]
fn extractor_drops_zero_count_rational() {
    // XResolution (282) RATIONAL with count 0 and empty data.
    let entries = vec![raw(282, 5, 0, Vec::new())];
    let m = extract_metadata(&entries, ByteOrder::Little);
    assert_eq!(m.x_resolution, None);
}

#[test]
fn extractor_drops_truncated_rational() {
    // RATIONAL claims one value but only 3 bytes are present.
    let entries = vec![raw(282, 5, 1, vec![1, 2, 3])];
    let m = extract_metadata(&entries, ByteOrder::Little);
    assert_eq!(m.x_resolution, None);
}

#[test]
fn extractor_ascii_count_larger_than_data_does_not_overrun() {
    // ASCII claims count 64 but only carries 3 bytes — as_ascii clamps
    // to the data length and never indexes past the end.
    let entries = vec![raw(270, 2, 64, b"Hi\0".to_vec())];
    let m = extract_metadata(&entries, ByteOrder::Little);
    assert_eq!(m.image_description.as_deref(), Some("Hi"));
}

#[test]
fn page_number_with_single_value_is_dropped() {
    // PageNumber (297) needs 2 SHORTs; a count-1 entry is malformed.
    let d = decode_ok(&build(&[entry(297, 3, 1, 4u32.to_le_bytes())], &[]));
    assert_eq!(d.metadata.page_number, None);
}

#[test]
fn page_number_with_two_values_reads() {
    // Two SHORTs pack into the 4-byte value slot: (1, 8).
    let mut val = [0u8; 4];
    val[0..2].copy_from_slice(&1u16.to_le_bytes());
    val[2..4].copy_from_slice(&8u16.to_le_bytes());
    let d = decode_ok(&build(&[entry(297, 3, 2, val)], &[]));
    assert_eq!(d.metadata.page_number, Some((1, 8)));
}

#[test]
fn ascii_without_nul_terminator_reads_whole_slice() {
    // ImageDescription "ABCD" (4 bytes, exactly the inline slot) with
    // no trailing NUL — the reader must take the whole slice, not run
    // off the end looking for a terminator.
    let d = decode_ok(&build(&[entry(270, 2, 4, *b"ABCD")], &[]));
    assert_eq!(d.metadata.image_description.as_deref(), Some("ABCD"));
}

#[test]
fn ascii_with_embedded_nul_truncates_at_first() {
    // "Hi\0X" — the reader returns "Hi".
    let d = decode_ok(&build(&[entry(270, 2, 4, *b"Hi\0X")], &[]));
    assert_eq!(d.metadata.image_description.as_deref(), Some("Hi"));
}

#[test]
fn ascii_with_non_utf8_bytes_is_lossy_not_a_failure() {
    // Software (305) out-of-line (>4 bytes so it does not pack inline)
    // with a stray 0xFF byte in the middle then NUL.
    let base = 8u32 + 2 + 10 * 12 + 4 + 1; // 9 core + 1 extra = 10 entries
    let blob = [0x41u8, 0xFF, 0x42, 0x43, 0x00]; // "A<bad>BC\0"
    let d = decode_ok(&build(&[entry(305, 2, 5, base.to_le_bytes())], &blob));
    let s = d.metadata.software.expect("lossy string still present");
    assert!(s.starts_with('A') && s.ends_with('C'), "got {s:?}");
}

#[test]
fn bits_per_sample_absent_defaults_to_one_in_format_info() {
    // A genuinely bilevel-ish IFD: drop BitsPerSample so the reader must
    // synthesise the spec default [1]. (This still decodes as 1-bit.)
    // Rebuild without tag 258.
    let mut core = vec![
        entry(256, 3, 1, 1u32.to_le_bytes()),
        entry(257, 3, 1, 1u32.to_le_bytes()),
        entry(259, 3, 1, 1u32.to_le_bytes()),
        entry(262, 3, 1, 0u32.to_le_bytes()), // WhiteIsZero
        entry(273, 4, 1, 0u32.to_le_bytes()),
        entry(278, 3, 1, 1u32.to_le_bytes()),
        entry(279, 4, 1, 1u32.to_le_bytes()),
        entry(277, 3, 1, 1u32.to_le_bytes()),
    ];
    core.sort_by_key(|e| u16::from_le_bytes([e[0], e[1]]));
    let n = core.len() as u16;
    let pixel_off = 8u32 + 2 + n as u32 * 12 + 4;
    let mut v = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&n.to_le_bytes());
    for e in &core {
        let mut e = *e;
        if u16::from_le_bytes([e[0], e[1]]) == 273 {
            e[8..12].copy_from_slice(&pixel_off.to_le_bytes());
        }
        v.extend_from_slice(&e);
    }
    v.extend_from_slice(&0u32.to_le_bytes());
    v.push(0x80);
    let d = decode_tiff(&v).expect("1-bit default-bits decode");
    assert_eq!(d.format.bits_per_sample, vec![1]);
}
