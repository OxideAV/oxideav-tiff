//! The `Metadata::exif` bridge: a page's Exif IFD (tag 34665) and GPS
//! IFD (tag 34853) as one standalone **Exif TIFF payload**, and back.
//!
//! The image-crate contract carries Exif as "a payload starting at the
//! TIFF header" (the shape PNG's `eXIf`, JPEG's `APP1` and HEIF's
//! `Exif` item all share). Inside a TIFF file the same information is
//! not a blob but two child IFDs reached through pointer tags, so this
//! module re-serialises them:
//!
//! * [`extract_exif_payload`] builds a little-endian classic TIFF
//!   (`II 2A 00`, first IFD at 8) whose 0th IFD holds only the
//!   `ExifIFDPointer` (34665) and, when the page has one, the
//!   `GPSInfoIFDPointer` (34853), each pointing at a copy of the
//!   page's child IFD entries with their values re-laid out (and
//!   byte-swapped from an `MM` source). Entries whose values are
//!   themselves file offsets (the IFD-typed `Interoperability` pointer
//!   40965 and any `IFD` / `IFD8`-typed entry) are dropped, because
//!   their targets are not carried; so are BigTIFF-only value types
//!   (classic entries cannot express them).
//! * [`parse_exif_payload`] reads such a payload (either byte order)
//!   back into owned entry lists for the encoder's `exif_ifd` /
//!   `gps_ifd`, converting the values to the little-endian layout the
//!   encoder's `AuxIfdEntry` requires.
//!
//! Only the TIFF 6.0 §2 IFD structure is interpreted — tag meanings
//! stay with the caller (the Exif catalogue is staged at
//! `docs/image/tiff/CIPA-DC-008-*.pdf`).

use crate::error::{Result, TiffError as Error};
use crate::ifd::{find, parse_header, parse_ifd, ByteOrder, Entry, ParsedHeader};
use crate::types::*;

/// Exif `Interoperability IFD Pointer` (0xA005): a child-IFD offset
/// inside the Exif IFD; dropped from the payload (its target is not
/// carried).
const TAG_INTEROPERABILITY_IFD: u16 = 40965;

/// One owned child-IFD entry with little-endian value bytes — the
/// owned counterpart of [`crate::AuxIfdEntry`], which borrows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedEntry {
    pub tag: u16,
    pub field_type: u16,
    pub count: u64,
    pub value: Vec<u8>,
}

/// Swap an entry's value bytes between big- and little-endian for its
/// field type (RATIONALs are two 4-byte words; byte-wide types are
/// unchanged).
fn to_little_endian(data: &[u8], field_type: u16, bo: ByteOrder) -> Vec<u8> {
    if bo == ByteOrder::Little {
        return data.to_vec();
    }
    let unit = match field_type {
        TYPE_SHORT | TYPE_SSHORT => 2,
        TYPE_LONG | TYPE_SLONG | TYPE_FLOAT | TYPE_IFD | TYPE_RATIONAL | TYPE_SRATIONAL => 4,
        TYPE_DOUBLE | TYPE_LONG8 | TYPE_SLONG8 | TYPE_IFD8 => 8,
        _ => 1,
    };
    if unit == 1 {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(unit) {
        out.extend(chunk.iter().rev());
    }
    out
}

/// An entry can travel in a classic little-endian payload: a known
/// classic value type, a complete value, a `u32` count, and not an
/// IFD pointer.
fn carryable(e: &Entry) -> bool {
    let unit = type_size(e.field_type) as usize;
    unit != 0
        && !matches!(
            e.field_type,
            TYPE_IFD | TYPE_IFD8 | TYPE_LONG8 | TYPE_SLONG8
        )
        && e.tag != TAG_INTEROPERABILITY_IFD
        && e.tag != TAG_EXIF_IFD
        && e.tag != TAG_GPS_IFD
        && e.count <= u32::MAX as u64
        && e.data.len() >= (e.count as usize).saturating_mul(unit)
}

/// Read the child IFD a pointer tag names, as owned little-endian
/// entries; `None` when the tag is absent or the IFD unreadable.
fn child_entries(
    input: &[u8],
    header: &ParsedHeader,
    entries: &[Entry],
    tag: u16,
) -> Option<Vec<OwnedEntry>> {
    let bo = header.byte_order;
    let off = find(entries, tag)?.as_u64_vec(bo).ok()?.first().copied()?;
    if off == 0 {
        return None;
    }
    let (child, _next) = parse_ifd(input, bo, header.variant, off).ok()?;
    let mut out: Vec<OwnedEntry> = child
        .iter()
        .filter(|e| carryable(e))
        .map(|e| OwnedEntry {
            tag: e.tag,
            field_type: e.field_type,
            count: e.count,
            value: to_little_endian(
                &e.data[..e.count as usize * type_size(e.field_type) as usize],
                e.field_type,
                bo,
            ),
        })
        .collect();
    out.sort_by_key(|e| e.tag);
    out.dedup_by_key(|e| e.tag);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Serialise one classic little-endian IFD at `at` (even), returning
/// the bytes to append there. Out-of-line values follow the IFD, each
/// at an even offset.
fn write_ifd(entries: &[OwnedEntry], at: u32) -> Vec<u8> {
    let n = entries.len() as u32;
    let ifd_len = 2 + 12 * n + 4;
    let mut out = Vec::new();
    out.extend_from_slice(&(n as u16).to_le_bytes());
    let mut tail: Vec<u8> = Vec::new();
    for e in entries {
        out.extend_from_slice(&e.tag.to_le_bytes());
        out.extend_from_slice(&e.field_type.to_le_bytes());
        out.extend_from_slice(&(e.count as u32).to_le_bytes());
        if e.value.len() <= 4 {
            let mut slot = [0u8; 4];
            slot[..e.value.len()].copy_from_slice(&e.value);
            out.extend_from_slice(&slot);
        } else {
            if tail.len() % 2 == 1 {
                tail.push(0);
            }
            let off = at + ifd_len + tail.len() as u32;
            out.extend_from_slice(&off.to_le_bytes());
            tail.extend_from_slice(&e.value);
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD
    out.extend_from_slice(&tail);
    if out.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// Build the Exif TIFF payload for a page: `None` when the page has
/// neither an Exif nor a GPS IFD (or both are empty / unreadable).
pub(crate) fn extract_exif_payload(
    input: &[u8],
    header: &ParsedHeader,
    entries: &[Entry],
) -> Option<Vec<u8>> {
    let exif = child_entries(input, header, entries, TAG_EXIF_IFD);
    let gps = child_entries(input, header, entries, TAG_GPS_IFD);
    if exif.is_none() && gps.is_none() {
        return None;
    }
    Some(build_exif_payload(exif.as_deref(), gps.as_deref()))
}

/// Serialise Exif / GPS entry lists into the Exif TIFF payload.
pub(crate) fn build_exif_payload(
    exif: Option<&[OwnedEntry]>,
    gps: Option<&[OwnedEntry]>,
) -> Vec<u8> {
    let mut pointers: Vec<(u16, &[OwnedEntry])> = Vec::new();
    if let Some(e) = exif.filter(|e| !e.is_empty()) {
        pointers.push((TAG_EXIF_IFD, e));
    }
    if let Some(g) = gps.filter(|g| !g.is_empty()) {
        pointers.push((TAG_GPS_IFD, g));
    }
    let n0 = pointers.len() as u32;
    let ifd0_len = 2 + 12 * n0 + 4;
    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    // Lay the child IFDs out after IFD0 to learn their offsets.
    let mut children: Vec<Vec<u8>> = Vec::new();
    let mut offsets: Vec<u32> = Vec::new();
    let mut cursor = 8 + ifd0_len;
    for (_, list) in &pointers {
        offsets.push(cursor);
        let bytes = write_ifd(list, cursor);
        cursor += bytes.len() as u32;
        children.push(bytes);
    }
    out.extend_from_slice(&(n0 as u16).to_le_bytes());
    for ((tag, _), off) in pointers.iter().zip(offsets.iter()) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&TYPE_LONG.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    for c in children {
        out.extend_from_slice(&c);
    }
    out
}

/// Parse an Exif TIFF payload (either byte order, classic or BigTIFF)
/// into `(exif_entries, gps_entries)` with little-endian values, ready
/// for the encoder's child IFDs. A payload whose 0th IFD carries
/// neither pointer yields two empty lists.
pub(crate) fn parse_exif_payload(blob: &[u8]) -> Result<(Vec<OwnedEntry>, Vec<OwnedEntry>)> {
    let header = parse_header(blob).map_err(|e| {
        Error::invalid(format!(
            "TIFF encode: metadata.exif is not a TIFF-headed Exif payload ({e})"
        ))
    })?;
    let (ifd0, _next) = parse_ifd(
        blob,
        header.byte_order,
        header.variant,
        header.first_ifd_offset,
    )?;
    let exif = child_entries(blob, &header, &ifd0, TAG_EXIF_IFD).unwrap_or_default();
    let gps = child_entries(blob, &header, &ifd0, TAG_GPS_IFD).unwrap_or_default();
    Ok((exif, gps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips_through_parse() {
        let exif = vec![
            OwnedEntry {
                tag: 0x9003, // DateTimeOriginal
                field_type: TYPE_ASCII,
                count: 20,
                value: b"2026:10:04 12:00:00\0".to_vec(),
            },
            OwnedEntry {
                tag: 0x829A, // ExposureTime
                field_type: TYPE_RATIONAL,
                count: 1,
                value: [1u32.to_le_bytes(), 125u32.to_le_bytes()].concat(),
            },
        ];
        let gps = vec![OwnedEntry {
            tag: 0, // GPSVersionID
            field_type: TYPE_BYTE,
            count: 4,
            value: vec![2, 3, 0, 0],
        }];
        let blob = build_exif_payload(Some(&exif), Some(&gps));
        assert_eq!(&blob[..4], b"II\x2a\x00");
        let (e2, g2) = parse_exif_payload(&blob).unwrap();
        let mut sorted = exif.clone();
        sorted.sort_by_key(|e| e.tag);
        assert_eq!(e2, sorted);
        assert_eq!(g2, gps);
    }

    #[test]
    fn big_endian_values_are_swapped_to_little() {
        assert_eq!(
            to_little_endian(&[0x12, 0x34, 0x56, 0x78], TYPE_SHORT, ByteOrder::Big),
            vec![0x34, 0x12, 0x78, 0x56]
        );
        assert_eq!(
            to_little_endian(&[0, 0, 0, 1, 0, 0, 0, 2], TYPE_RATIONAL, ByteOrder::Big),
            vec![1, 0, 0, 0, 2, 0, 0, 0]
        );
        assert_eq!(
            to_little_endian(b"abc", TYPE_ASCII, ByteOrder::Big),
            b"abc".to_vec()
        );
    }

    #[test]
    fn not_a_tiff_header_is_invalid_data() {
        assert!(matches!(
            parse_exif_payload(b"Exif\0\0"),
            Err(Error::InvalidData(_))
        ));
    }
}
