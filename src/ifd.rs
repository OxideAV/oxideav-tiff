//! TIFF Image File Directory (IFD) parsing.
//!
//! Per spec Section 2:
//!
//! * Header (8 bytes): byte-order indicator (II=4949h or MM=4D4Dh),
//!   magic 42, 4-byte offset to the first IFD.
//! * IFD: 2-byte count of entries, then count x 12-byte entries
//!   sorted ascending by tag, then a 4-byte next-IFD offset (0 if
//!   none).
//! * Entry: 2-byte tag, 2-byte field type, 4-byte count, 4-byte
//!   value-or-offset. The value lives inline if `count *
//!   sizeof(type)` is <= 4 bytes; otherwise the 4-byte field is an
//!   absolute offset into the file where the values begin.
//!
//! All multi-byte ints in the file (including offsets) are read with
//! the byte order from the header.

use oxideav_core::{Error, Result};

use crate::types::*;

#[derive(Debug, Clone, Copy)]
pub enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    pub fn read_u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        match self {
            ByteOrder::Little => u16::from_le_bytes(a),
            ByteOrder::Big => u16::from_be_bytes(a),
        }
    }
    pub fn read_u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        match self {
            ByteOrder::Little => u32::from_le_bytes(a),
            ByteOrder::Big => u32::from_be_bytes(a),
        }
    }
    pub fn read_i32(self, b: &[u8]) -> i32 {
        self.read_u32(b) as i32
    }
}

/// One IFD entry, with its raw value bytes already extracted (either
/// inline from the value/offset slot or dereferenced through the
/// offset).
#[derive(Debug, Clone)]
pub struct Entry {
    pub tag: u16,
    pub field_type: u16,
    pub count: u32,
    /// The raw bytes of all values for this entry, in file byte
    /// order. `count * type_size(field_type)` bytes long when the
    /// type is known, else empty (caller is expected to skip).
    pub data: Vec<u8>,
}

impl Entry {
    /// Convenience: decode the entry's values as `u32`s. Accepts
    /// BYTE / SHORT / LONG (per spec, "TIFF readers should accept
    /// BYTE, SHORT, or LONG values for any unsigned integer field").
    pub fn as_u32_vec(&self, bo: ByteOrder) -> Result<Vec<u32>> {
        let n = self.count as usize;
        match self.field_type {
            TYPE_BYTE => {
                if self.data.len() < n {
                    return Err(Error::invalid("TIFF: BYTE entry truncated"));
                }
                Ok(self.data[..n].iter().map(|&b| b as u32).collect())
            }
            TYPE_SHORT => {
                if self.data.len() < n * 2 {
                    return Err(Error::invalid("TIFF: SHORT entry truncated"));
                }
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(bo.read_u16(&self.data[i * 2..i * 2 + 2]) as u32);
                }
                Ok(out)
            }
            TYPE_LONG => {
                if self.data.len() < n * 4 {
                    return Err(Error::invalid("TIFF: LONG entry truncated"));
                }
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(bo.read_u32(&self.data[i * 4..i * 4 + 4]));
                }
                Ok(out)
            }
            t => Err(Error::invalid(format!(
                "TIFF: cannot read field type {t} as integer"
            ))),
        }
    }

    /// First u32 value (single-value fields like ImageWidth /
    /// Compression / Photometric / Predictor / etc.)
    pub fn as_u32(&self, bo: ByteOrder) -> Result<u32> {
        let v = self.as_u32_vec(bo)?;
        v.into_iter()
            .next()
            .ok_or_else(|| Error::invalid("TIFF: empty integer entry"))
    }
}

/// Result of parsing the file header + first IFD.
pub struct ParsedHeader {
    pub byte_order: ByteOrder,
    pub first_ifd_offset: u32,
}

pub fn parse_header(input: &[u8]) -> Result<ParsedHeader> {
    if input.len() < 8 {
        return Err(Error::invalid("TIFF: file shorter than 8-byte header"));
    }
    let bo = match u16::from_le_bytes([input[0], input[1]]) {
        BYTE_ORDER_LE => ByteOrder::Little,
        BYTE_ORDER_BE => ByteOrder::Big,
        _ => return Err(Error::invalid("TIFF: missing II/MM byte-order tag")),
    };
    let magic = bo.read_u16(&input[2..4]);
    if magic != TIFF_MAGIC {
        return Err(Error::invalid(format!("TIFF: magic={magic} (expected 42)")));
    }
    let first_ifd_offset = bo.read_u32(&input[4..8]);
    Ok(ParsedHeader {
        byte_order: bo,
        first_ifd_offset,
    })
}

/// Parse the IFD at `offset`. Returns the entry list. The next-IFD
/// offset that follows is also returned but ignored by round-1
/// callers (single-page).
pub fn parse_ifd(input: &[u8], bo: ByteOrder, offset: u32) -> Result<(Vec<Entry>, u32)> {
    let off = offset as usize;
    if off + 2 > input.len() {
        return Err(Error::invalid("TIFF: IFD start past EOF"));
    }
    let count = bo.read_u16(&input[off..off + 2]) as usize;
    let entries_start = off + 2;
    let entries_end = entries_start + count * 12;
    if entries_end + 4 > input.len() {
        return Err(Error::invalid("TIFF: IFD truncated"));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = entries_start + i * 12;
        let tag = bo.read_u16(&input[base..base + 2]);
        let field_type = bo.read_u16(&input[base + 2..base + 4]);
        let cnt = bo.read_u32(&input[base + 4..base + 8]);
        let val_off_slot = &input[base + 8..base + 12];
        let ts = type_size(field_type);
        let data = if ts == 0 {
            // Unknown type — keep the inline 4 bytes verbatim and
            // let the caller skip. Per spec, readers must skip
            // unknown field types gracefully.
            val_off_slot.to_vec()
        } else {
            let total = ts as u64 * cnt as u64;
            if total <= 4 {
                val_off_slot[..total as usize].to_vec()
            } else {
                let p = bo.read_u32(val_off_slot) as usize;
                let total = total as usize;
                if p.checked_add(total).map_or(true, |end| end > input.len()) {
                    return Err(Error::invalid(format!(
                        "TIFF: entry tag={tag} value offset past EOF"
                    )));
                }
                input[p..p + total].to_vec()
            }
        };
        out.push(Entry {
            tag,
            field_type,
            count: cnt,
            data,
        });
    }
    let next_ifd = bo.read_u32(&input[entries_end..entries_end + 4]);
    Ok((out, next_ifd))
}

/// Find the first entry with the given tag. None if absent.
pub fn find(entries: &[Entry], tag: u16) -> Option<&Entry> {
    entries.iter().find(|e| e.tag == tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(le: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if le {
            v.extend_from_slice(b"II");
            v.extend_from_slice(&42u16.to_le_bytes());
            v.extend_from_slice(&8u32.to_le_bytes());
        } else {
            v.extend_from_slice(b"MM");
            v.extend_from_slice(&42u16.to_be_bytes());
            v.extend_from_slice(&8u32.to_be_bytes());
        }
        v
    }

    #[test]
    fn header_little_endian() {
        let h = header(true);
        let p = parse_header(&h).unwrap();
        assert!(matches!(p.byte_order, ByteOrder::Little));
        assert_eq!(p.first_ifd_offset, 8);
    }

    #[test]
    fn header_big_endian() {
        let h = header(false);
        let p = parse_header(&h).unwrap();
        assert!(matches!(p.byte_order, ByteOrder::Big));
        assert_eq!(p.first_ifd_offset, 8);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut h = header(true);
        h[2] = 99;
        assert!(parse_header(&h).is_err());
    }

    #[test]
    fn entry_inline_short() {
        // II header, 1 IFD entry with one SHORT (count=1, fits
        // inline), then next-IFD = 0.
        let mut v = Vec::new();
        v.extend_from_slice(b"II");
        v.extend_from_slice(&42u16.to_le_bytes());
        v.extend_from_slice(&8u32.to_le_bytes());
        // IFD count
        v.extend_from_slice(&1u16.to_le_bytes());
        // entry
        v.extend_from_slice(&256u16.to_le_bytes()); // tag = ImageWidth
        v.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        v.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        v.extend_from_slice(&[0x40, 0x00, 0x00, 0x00]); // value 0x40 inline
                                                        // next-IFD
        v.extend_from_slice(&0u32.to_le_bytes());
        let h = parse_header(&v).unwrap();
        let (entries, next) = parse_ifd(&v, h.byte_order, h.first_ifd_offset).unwrap();
        assert_eq!(next, 0);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.tag, 256);
        assert_eq!(e.field_type, 3);
        assert_eq!(e.count, 1);
        assert_eq!(e.as_u32(h.byte_order).unwrap(), 0x40);
    }

    #[test]
    fn entry_offset_long_array() {
        // II header, 1 IFD entry with 2 LONGs (8 bytes — needs
        // dereference). Values stored after the IFD.
        let mut v = Vec::new();
        v.extend_from_slice(b"II");
        v.extend_from_slice(&42u16.to_le_bytes());
        v.extend_from_slice(&8u32.to_le_bytes());
        // IFD: count + 1 entry + next-IFD = 2 + 12 + 4 = 18 bytes; values
        // begin at file offset 26.
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&273u16.to_le_bytes()); // StripOffsets
        v.extend_from_slice(&4u16.to_le_bytes()); // LONG
        v.extend_from_slice(&2u32.to_le_bytes()); // count = 2
        v.extend_from_slice(&26u32.to_le_bytes()); // offset
        v.extend_from_slice(&0u32.to_le_bytes()); // next-IFD
        v.extend_from_slice(&100u32.to_le_bytes());
        v.extend_from_slice(&200u32.to_le_bytes());
        let h = parse_header(&v).unwrap();
        let (entries, _next) = parse_ifd(&v, h.byte_order, h.first_ifd_offset).unwrap();
        let vs = entries[0].as_u32_vec(h.byte_order).unwrap();
        assert_eq!(vs, vec![100, 200]);
    }
}
