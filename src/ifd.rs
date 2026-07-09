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
//!
//! BigTIFF (Adobe Pagemaker 6.0 BigTIFF design, magic = 43):
//!
//! * Header (16 bytes): II/MM, magic 43, 2-byte offset-bytesize
//!   (always 8), 2-byte constant 0, 8-byte offset to first IFD.
//! * IFD: 8-byte count, count × 20-byte entries (tag/type/8-byte
//!   count/8-byte value-or-offset), 8-byte next-IFD offset.
//! * Entry value lives inline if `count * sizeof(type)` is <= 8
//!   bytes (vs. 4 for classic TIFF).

use crate::error::{Result, TiffError as Error};
use crate::types::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn read_u64(self, b: &[u8]) -> u64 {
        let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        match self {
            ByteOrder::Little => u64::from_le_bytes(a),
            ByteOrder::Big => u64::from_be_bytes(a),
        }
    }
    pub fn read_i32(self, b: &[u8]) -> i32 {
        self.read_u32(b) as i32
    }
    /// Read an IEEE-754 single-precision float (TIFF type FLOAT,
    /// 4 bytes) in this byte order. Per TIFF 6.0 §SampleFormat the
    /// in-file representation of a `SampleFormat = 3` sample is IEEE
    /// 754, so the 32-bit big-/little-endian byte string maps straight
    /// onto `f32::from_*_bytes`.
    pub fn read_f32(self, b: &[u8]) -> f32 {
        let a = [b[0], b[1], b[2], b[3]];
        match self {
            ByteOrder::Little => f32::from_le_bytes(a),
            ByteOrder::Big => f32::from_be_bytes(a),
        }
    }
    /// Read an IEEE-754 double-precision float (TIFF type DOUBLE,
    /// 8 bytes) in this byte order — the 64-bit `SampleFormat = 3`
    /// width.
    pub fn read_f64(self, b: &[u8]) -> f64 {
        let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        match self {
            ByteOrder::Little => f64::from_le_bytes(a),
            ByteOrder::Big => f64::from_be_bytes(a),
        }
    }
}

/// Variant tag distinguishing classic TIFF from BigTIFF. Drives the
/// IFD entry layout (12 vs. 20 bytes) and the inline-value threshold
/// (4 vs. 8 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiffVariant {
    Classic,
    Big,
}

/// One IFD entry, with its raw value bytes already extracted (either
/// inline from the value/offset slot or dereferenced through the
/// offset).
#[derive(Debug, Clone)]
pub struct Entry {
    pub tag: u16,
    pub field_type: u16,
    pub count: u64,
    /// The raw bytes of all values for this entry, in file byte
    /// order. `count * type_size(field_type)` bytes long when the
    /// type is known, else empty (caller is expected to skip).
    pub data: Vec<u8>,
}

impl Entry {
    /// Convenience: decode the entry's values as `u64`s. Accepts
    /// BYTE / SHORT / LONG / LONG8 / IFD8 (per spec, "TIFF readers
    /// should accept BYTE, SHORT, or LONG values for any unsigned
    /// integer field"; BigTIFF adds LONG8/IFD8).
    pub fn as_u64_vec(&self, bo: ByteOrder) -> Result<Vec<u64>> {
        let n = self.count as usize;
        match self.field_type {
            TYPE_BYTE => {
                if self.data.len() < n {
                    return Err(Error::invalid("TIFF: BYTE entry truncated"));
                }
                Ok(self.data[..n].iter().map(|&b| b as u64).collect())
            }
            TYPE_SHORT => {
                if self.data.len() < n * 2 {
                    return Err(Error::invalid("TIFF: SHORT entry truncated"));
                }
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(bo.read_u16(&self.data[i * 2..i * 2 + 2]) as u64);
                }
                Ok(out)
            }
            TYPE_LONG | TYPE_IFD => {
                if self.data.len() < n * 4 {
                    return Err(Error::invalid("TIFF: LONG entry truncated"));
                }
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(bo.read_u32(&self.data[i * 4..i * 4 + 4]) as u64);
                }
                Ok(out)
            }
            TYPE_LONG8 | TYPE_IFD8 => {
                if self.data.len() < n * 8 {
                    return Err(Error::invalid("TIFF: LONG8 entry truncated"));
                }
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(bo.read_u64(&self.data[i * 8..i * 8 + 8]));
                }
                Ok(out)
            }
            t => Err(Error::invalid(format!(
                "TIFF: cannot read field type {t} as integer"
            ))),
        }
    }

    /// Convenience: decode the entry's values as `u32`s. Same accept
    /// list as [`Self::as_u64_vec`] (LONG8 values are downcast and
    /// will error if any individual value exceeds `u32::MAX`).
    pub fn as_u32_vec(&self, bo: ByteOrder) -> Result<Vec<u32>> {
        let v = self.as_u64_vec(bo)?;
        let mut out = Vec::with_capacity(v.len());
        for x in v {
            if x > u32::MAX as u64 {
                return Err(Error::invalid(format!("TIFF: value {x} exceeds u32 range")));
            }
            out.push(x as u32);
        }
        Ok(out)
    }

    /// Decode the entry's values as `f64`s. Accepts the IEEE float
    /// types (FLOAT, DOUBLE) plus the unsigned/signed integer and
    /// RATIONAL types — TIFF 6.0 §SampleFormat types SMinSampleValue /
    /// SMaxSampleValue (340 / 341) as "the field type that best matches
    /// the sample data", which for a `SampleFormat = 3` image is FLOAT
    /// or DOUBLE but which a writer may legitimately store as an integer
    /// type for an integer image, so the reader accepts the breadth the
    /// spec allows.
    pub fn as_f64_vec(&self, bo: ByteOrder) -> Result<Vec<f64>> {
        let n = self.count as usize;
        let need = n.saturating_mul(type_size(self.field_type) as usize);
        if self.data.len() < need {
            return Err(Error::invalid("TIFF: float entry truncated"));
        }
        let mut out = Vec::with_capacity(n);
        match self.field_type {
            TYPE_FLOAT => {
                for i in 0..n {
                    out.push(bo.read_f32(&self.data[i * 4..i * 4 + 4]) as f64);
                }
            }
            TYPE_DOUBLE => {
                for i in 0..n {
                    out.push(bo.read_f64(&self.data[i * 8..i * 8 + 8]));
                }
            }
            TYPE_RATIONAL => {
                for i in 0..n {
                    let num = bo.read_u32(&self.data[i * 8..i * 8 + 4]) as f64;
                    let den = bo.read_u32(&self.data[i * 8 + 4..i * 8 + 8]) as f64;
                    out.push(if den == 0.0 { 0.0 } else { num / den });
                }
            }
            TYPE_SRATIONAL => {
                for i in 0..n {
                    let num = bo.read_i32(&self.data[i * 8..i * 8 + 4]) as f64;
                    let den = bo.read_i32(&self.data[i * 8 + 4..i * 8 + 8]) as f64;
                    out.push(if den == 0.0 { 0.0 } else { num / den });
                }
            }
            TYPE_SBYTE => {
                for i in 0..n {
                    out.push(self.data[i] as i8 as f64);
                }
            }
            TYPE_SSHORT => {
                for i in 0..n {
                    out.push(bo.read_u16(&self.data[i * 2..i * 2 + 2]) as i16 as f64);
                }
            }
            TYPE_SLONG => {
                for i in 0..n {
                    out.push(bo.read_i32(&self.data[i * 4..i * 4 + 4]) as f64);
                }
            }
            // BYTE / SHORT / LONG / LONG8 fall through to the unsigned
            // integer reader.
            _ => {
                let v = self.as_u64_vec(bo)?;
                out.extend(v.into_iter().map(|x| x as f64));
            }
        }
        Ok(out)
    }

    /// First u32 value (single-value fields like ImageWidth /
    /// Compression / Photometric / Predictor / etc.)
    pub fn as_u32(&self, bo: ByteOrder) -> Result<u32> {
        let v = self.as_u32_vec(bo)?;
        v.into_iter()
            .next()
            .ok_or_else(|| Error::invalid("TIFF: empty integer entry"))
    }

    /// Decode an ASCII (`TYPE_ASCII`, code 2) field to a `String`.
    ///
    /// Per TIFF 6.0 §2 an ASCII field is a NUL-terminated 7-bit ASCII
    /// string; `count` includes the trailing NUL. This reader returns
    /// the first NUL-terminated string (a field may in principle store
    /// several, but every writer in the wild stores one), trimming the
    /// terminator. Bytes are interpreted lossily so a hostile field
    /// carrying non-ASCII / non-UTF-8 octets never fails the whole
    /// decode — the substitution character is substituted instead.
    ///
    /// Returns an error only if the field type is not `TYPE_ASCII`; a
    /// zero-length or unterminated payload yields an empty / best-effort
    /// string rather than an error, since descriptive metadata must
    /// never gate a pixel decode.
    pub fn as_ascii(&self) -> Result<String> {
        if self.field_type != TYPE_ASCII {
            return Err(Error::invalid(format!(
                "TIFF: tag {} is field type {}, not ASCII (2)",
                self.tag, self.field_type
            )));
        }
        let n = (self.count as usize).min(self.data.len());
        let slice = &self.data[..n];
        // Cut at the first NUL; if there is none, take the whole slice.
        let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        Ok(String::from_utf8_lossy(&slice[..end]).into_owned())
    }
}

/// Result of parsing the file header.
pub struct ParsedHeader {
    pub byte_order: ByteOrder,
    pub variant: TiffVariant,
    /// Offset of the first IFD. For classic TIFF this is a `u32`
    /// stored in the low 32 bits; for BigTIFF it's a full `u64`.
    pub first_ifd_offset: u64,
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
    match magic {
        TIFF_MAGIC => {
            let first_ifd_offset = bo.read_u32(&input[4..8]) as u64;
            Ok(ParsedHeader {
                byte_order: bo,
                variant: TiffVariant::Classic,
                first_ifd_offset,
            })
        }
        BIGTIFF_MAGIC => {
            // BigTIFF header is 16 bytes: II/MM (2) + 43 (2) +
            // offset-bytesize (2, must be 8) + reserved (2, must be
            // 0) + first-IFD-offset (8).
            if input.len() < 16 {
                return Err(Error::invalid("BigTIFF: file shorter than 16-byte header"));
            }
            let off_size = bo.read_u16(&input[4..6]);
            let reserved = bo.read_u16(&input[6..8]);
            if off_size != 8 || reserved != 0 {
                return Err(Error::invalid(format!(
                    "BigTIFF: invalid header off_size={off_size} reserved={reserved}"
                )));
            }
            let first_ifd_offset = bo.read_u64(&input[8..16]);
            Ok(ParsedHeader {
                byte_order: bo,
                variant: TiffVariant::Big,
                first_ifd_offset,
            })
        }
        _ => Err(Error::invalid(format!(
            "TIFF: magic={magic} (expected 42 or 43)"
        ))),
    }
}

/// Parse the IFD at `offset`. Returns the entry list and the
/// next-IFD offset (0 if last in the chain). Variant chosen by
/// `variant`; classic uses 12-byte entries with a 4-byte next-IFD,
/// BigTIFF uses 20-byte entries with an 8-byte next-IFD.
pub fn parse_ifd(
    input: &[u8],
    bo: ByteOrder,
    variant: TiffVariant,
    offset: u64,
) -> Result<(Vec<Entry>, u64)> {
    match variant {
        TiffVariant::Classic => parse_ifd_classic(input, bo, offset),
        TiffVariant::Big => parse_ifd_big(input, bo, offset),
    }
}

fn parse_ifd_classic(input: &[u8], bo: ByteOrder, offset: u64) -> Result<(Vec<Entry>, u64)> {
    // Classic TIFF stores `offset` as a 32-bit field, but the
    // `parse_ifd` public surface widens it to u64; an attacker can
    // pass `u64::MAX` via the BigTIFF-typed entry path or via a
    // hand-crafted `decode_tiff` driver, so all `off + N` arithmetic
    // must go through `checked_add`.
    let off = offset as usize;
    let off_plus_2 = off
        .checked_add(2)
        .ok_or_else(|| Error::invalid("TIFF: IFD offset overflow"))?;
    if off_plus_2 > input.len() {
        return Err(Error::invalid("TIFF: IFD start past EOF"));
    }
    let count = bo.read_u16(&input[off..off_plus_2]) as usize;
    let entries_start = off_plus_2;
    let entries_end = entries_start
        .checked_add(
            count
                .checked_mul(12)
                .ok_or_else(|| Error::invalid("TIFF: IFD entry count overflow"))?,
        )
        .ok_or_else(|| Error::invalid("TIFF: IFD entry length overflow"))?;
    let entries_end_plus_4 = entries_end
        .checked_add(4)
        .ok_or_else(|| Error::invalid("TIFF: IFD next-IFD slot overflow"))?;
    if entries_end_plus_4 > input.len() {
        return Err(Error::invalid("TIFF: IFD truncated"));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = entries_start + i * 12;
        let tag = bo.read_u16(&input[base..base + 2]);
        let field_type = bo.read_u16(&input[base + 2..base + 4]);
        let cnt = bo.read_u32(&input[base + 4..base + 8]) as u64;
        let val_off_slot = &input[base + 8..base + 12];
        let ts = type_size(field_type);
        let data = if ts == 0 {
            // Unknown type — keep the inline 4 bytes verbatim and
            // let the caller skip. Per spec, readers must skip
            // unknown field types gracefully.
            val_off_slot.to_vec()
        } else {
            // `cnt` (u32) * `ts` (≤16) fits in u64 without overflow,
            // but the `as usize` cast can truncate on 32-bit hosts.
            // Use `usize::try_from` so the truncation case errors out
            // rather than silently returning a small wrapped length.
            let total_u64 = (ts as u64) * cnt;
            if total_u64 <= 4 {
                val_off_slot[..total_u64 as usize].to_vec()
            } else {
                let p = bo.read_u32(val_off_slot) as usize;
                let total = usize::try_from(total_u64)
                    .map_err(|_| Error::invalid("TIFF: entry total bytes exceed usize"))?;
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
    let next_ifd = bo.read_u32(&input[entries_end..entries_end + 4]) as u64;
    Ok((out, next_ifd))
}

fn parse_ifd_big(input: &[u8], bo: ByteOrder, offset: u64) -> Result<(Vec<Entry>, u64)> {
    // `offset` is u64 from the BigTIFF header / next-IFD chain; on
    // 64-bit hosts `u64 as usize` is identity, so the post-cast
    // checks below must use `checked_add` to catch the attacker
    // case where `offset == u64::MAX` and `off + 8` would overflow.
    let off = offset as usize;
    let off_plus_8 = off
        .checked_add(8)
        .ok_or_else(|| Error::invalid("BigTIFF: IFD offset overflow"))?;
    if off_plus_8 > input.len() {
        return Err(Error::invalid("BigTIFF: IFD start past EOF"));
    }
    let count = bo.read_u64(&input[off..off_plus_8]);
    let count_us = count as usize;
    let entries_start = off_plus_8;
    let entries_end = entries_start
        .checked_add(
            count_us
                .checked_mul(20)
                .ok_or_else(|| Error::invalid("BigTIFF: IFD entry count overflow"))?,
        )
        .ok_or_else(|| Error::invalid("BigTIFF: IFD entry length overflow"))?;
    let entries_end_plus_8 = entries_end
        .checked_add(8)
        .ok_or_else(|| Error::invalid("BigTIFF: IFD next-IFD slot overflow"))?;
    if entries_end_plus_8 > input.len() {
        return Err(Error::invalid("BigTIFF: IFD truncated"));
    }
    // Cap the upfront entry-list reservation by the smaller of
    // `count_us` (attacker-claimed) and what the input could actually
    // back (each entry is 20 bytes); the loop below will reject any
    // genuinely-truncated tail anyway, but the cap prevents a
    // 16-byte BigTIFF header from forcing a multi-GiB reserve.
    let max_possible = input.len() / 20 + 1;
    let mut out = Vec::with_capacity(count_us.min(max_possible));
    for i in 0..count_us {
        let base = entries_start + i * 20;
        let tag = bo.read_u16(&input[base..base + 2]);
        let field_type = bo.read_u16(&input[base + 2..base + 4]);
        let cnt = bo.read_u64(&input[base + 4..base + 12]);
        let val_off_slot = &input[base + 12..base + 20];
        let ts = type_size(field_type);
        let data = if ts == 0 {
            val_off_slot.to_vec()
        } else {
            // `ts <= 16` but `cnt` is attacker-controlled u64; use
            // `checked_mul` so a crafted `cnt == u64::MAX` value
            // can't wrap to a small number that bypasses the
            // `<= 8` inline-vs-offset split below.
            let total = (ts as u64)
                .checked_mul(cnt)
                .ok_or_else(|| Error::invalid("BigTIFF: entry total bytes overflow"))?;
            if total <= 8 {
                val_off_slot[..total as usize].to_vec()
            } else {
                let p = bo.read_u64(val_off_slot) as usize;
                let total = usize::try_from(total)
                    .map_err(|_| Error::invalid("BigTIFF: entry total bytes exceed usize"))?;
                if p.checked_add(total).map_or(true, |end| end > input.len()) {
                    return Err(Error::invalid(format!(
                        "BigTIFF: entry tag={tag} value offset past EOF"
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
    let next_ifd = bo.read_u64(&input[entries_end..entries_end + 8]);
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
        assert_eq!(p.variant, TiffVariant::Classic);
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
    fn header_bigtiff_le() {
        let mut v = Vec::new();
        v.extend_from_slice(b"II");
        v.extend_from_slice(&43u16.to_le_bytes()); // BigTIFF magic
        v.extend_from_slice(&8u16.to_le_bytes()); // off_size = 8
        v.extend_from_slice(&0u16.to_le_bytes()); // reserved
        v.extend_from_slice(&16u64.to_le_bytes()); // first IFD at byte 16
        let p = parse_header(&v).unwrap();
        assert_eq!(p.variant, TiffVariant::Big);
        assert_eq!(p.first_ifd_offset, 16);
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
        let (entries, next) = parse_ifd(&v, h.byte_order, h.variant, h.first_ifd_offset).unwrap();
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
        let (entries, _next) = parse_ifd(&v, h.byte_order, h.variant, h.first_ifd_offset).unwrap();
        let vs = entries[0].as_u32_vec(h.byte_order).unwrap();
        assert_eq!(vs, vec![100, 200]);
    }

    #[test]
    fn bigtiff_ifd_offset_max_does_not_overflow_usize() {
        // Fuzz reproducer (r126): a BigTIFF header whose
        // `first_ifd_offset` is `u64::MAX` casts to `usize::MAX` on
        // 64-bit hosts; the subsequent `off + 8` would debug-panic
        // with "attempt to add with overflow". Must surface as a
        // clean `Err`.
        let mut v = Vec::new();
        v.extend_from_slice(b"II");
        v.extend_from_slice(&43u16.to_le_bytes()); // BigTIFF magic
        v.extend_from_slice(&8u16.to_le_bytes()); // offset bytesize
        v.extend_from_slice(&0u16.to_le_bytes()); // reserved
        v.extend_from_slice(&u64::MAX.to_le_bytes()); // first_ifd_offset
        let h = parse_header(&v).unwrap();
        let err = parse_ifd(&v, h.byte_order, h.variant, h.first_ifd_offset).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("overflow") || msg.contains("past EOF"),
            "{msg}"
        );
    }

    #[test]
    fn bigtiff_entry_total_bytes_overflow() {
        // Fuzz-shape input: BigTIFF entry with `field_type = LONG8`
        // (size 8) and `count = u64::MAX`. The naive
        // `ts * cnt` u64 multiply wraps to a small value that
        // bypasses the `total <= 8` inline check; the fixed
        // `checked_mul` path must surface this as a clean `Err`.
        let mut v = Vec::new();
        v.extend_from_slice(b"II");
        v.extend_from_slice(&43u16.to_le_bytes()); // BigTIFF magic
        v.extend_from_slice(&8u16.to_le_bytes()); // offset bytesize
        v.extend_from_slice(&0u16.to_le_bytes()); // reserved
        v.extend_from_slice(&16u64.to_le_bytes()); // first_ifd_offset → just after header
                                                   // IFD at offset 16: 8 bytes count + N*20 entries + 8 bytes next.
        v.extend_from_slice(&1u64.to_le_bytes()); // count = 1
                                                  // Entry: tag=0, field_type=16 (LONG8), count=u64::MAX
        v.extend_from_slice(&0u16.to_le_bytes()); // tag
        v.extend_from_slice(&16u16.to_le_bytes()); // LONG8
        v.extend_from_slice(&u64::MAX.to_le_bytes()); // count
        v.extend_from_slice(&0u64.to_le_bytes()); // value offset slot
        v.extend_from_slice(&0u64.to_le_bytes()); // next IFD
        let h = parse_header(&v).unwrap();
        let err = parse_ifd(&v, h.byte_order, h.variant, h.first_ifd_offset).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("overflow"), "{msg}");
    }
}
