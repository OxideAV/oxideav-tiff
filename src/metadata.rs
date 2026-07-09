//! Decode-side metadata extraction.
//!
//! The pixel decoder in [`crate::decoder`] resolves an IFD into a
//! [`crate::image::TiffImage`]; this module gathers the *descriptive*
//! and *structural* tags that sit alongside the pixels — the TIFF 6.0
//! §8 ASCII information fields (ImageDescription, Software, Artist, …),
//! the resolution triple (XResolution / YResolution / ResolutionUnit),
//! and the page-level layout tags (Orientation, PageNumber, the two
//! SubfileType flavours). None of these steer pixel reconstruction, so
//! they were previously discarded; exposing them lets a caller read
//! back exactly what the encoder's [`crate::encoder::PageExtras`] wrote.
//!
//! Extraction is deliberately *total*: a malformed metadata entry
//! (wrong field type, truncated RATIONAL, unterminated ASCII) leaves
//! that one field `None` rather than failing the whole decode.
//! Descriptive metadata must never gate a pixel decode.

use crate::ifd::{find, ByteOrder, Entry};
use crate::types::*;

/// Resolution unit stored in tag 296 (ResolutionUnit), TIFF 6.0 §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionUnit {
    /// 1 — no absolute unit; X/YResolution express an aspect ratio only.
    None,
    /// 2 — inch (the TIFF 6.0 default).
    Inch,
    /// 3 — centimetre.
    Centimeter,
}

impl ResolutionUnit {
    fn from_tag(v: u32) -> Option<Self> {
        match v {
            1 => Some(ResolutionUnit::None),
            2 => Some(ResolutionUnit::Inch),
            3 => Some(ResolutionUnit::Centimeter),
            _ => None,
        }
    }
}

/// Descriptive + structural metadata gathered from one IFD.
///
/// Every field is optional: it is `Some` only when the corresponding
/// tag is present *and* well-formed. All fields default to `None`, so
/// [`TiffMetadata::default()`] is the "no metadata" value the standalone
/// [`crate::decode_tiff`] result carries when a file omits every
/// informational tag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TiffMetadata {
    // ---- TIFF 6.0 §8 ASCII descriptive fields ----
    /// DocumentName (tag 269).
    pub document_name: Option<String>,
    /// ImageDescription (tag 270).
    pub image_description: Option<String>,
    /// Make (tag 271) — scanner / camera manufacturer.
    pub make: Option<String>,
    /// Model (tag 272) — scanner / camera model.
    pub model: Option<String>,
    /// PageName (tag 285).
    pub page_name: Option<String>,
    /// Software (tag 305).
    pub software: Option<String>,
    /// DateTime (tag 306) — `"YYYY:MM:DD HH:MM:SS"` per §8; stored
    /// verbatim (no parse / validation — the raw ASCII round-trips).
    pub date_time: Option<String>,
    /// Artist (tag 315).
    pub artist: Option<String>,
    /// HostComputer (tag 316).
    pub host_computer: Option<String>,
    /// Copyright (tag 33432).
    pub copyright: Option<String>,

    // ---- Resolution (tags 282 / 283 / 296) ----
    /// XResolution (tag 282) as the raw `(numerator, denominator)`
    /// RATIONAL — the pixels-per-`resolution_unit` in the image width
    /// direction.
    pub x_resolution: Option<(u32, u32)>,
    /// YResolution (tag 283) as the raw `(numerator, denominator)`
    /// RATIONAL.
    pub y_resolution: Option<(u32, u32)>,
    /// ResolutionUnit (tag 296). Absent tag ⇒ `None` here (the spec
    /// default is `Inch`, but the reader reports only what was written
    /// so a round-trip can distinguish "omitted" from "explicit 2").
    pub resolution_unit: Option<ResolutionUnit>,

    // ---- Page-level structural tags ----
    /// Orientation (tag 274) — the *original* on-disk value. The pixel
    /// decoder applies the orientation transform to the returned image;
    /// this preserves the tag so a caller can tell what transform was
    /// applied.
    pub orientation: Option<u16>,
    /// PageNumber (tag 297) — `(page, total)`; `total == 0` means the
    /// writer left the count unknown, per §8.
    pub page_number: Option<(u16, u16)>,
    /// NewSubfileType (tag 254) — the 32-bit flag word (bit 0 =
    /// reduced-resolution, bit 1 = single page of a multi-page image,
    /// bit 2 = transparency mask), TIFF 6.0 §8.
    pub new_subfile_type: Option<u32>,
    /// SubfileType (tag 255) — the deprecated pre-6.0 SHORT enum.
    pub subfile_type: Option<u16>,
}

/// Read a two-`u32` RATIONAL (or single unsigned SHORT/LONG treated as
/// `x/1`) from an entry, guarding the payload length. Returns `None`
/// for a malformed / wrong-type entry.
fn rational(e: &Entry, bo: ByteOrder) -> Option<(u32, u32)> {
    match e.field_type {
        TYPE_RATIONAL if e.count >= 1 && e.data.len() >= 8 => {
            let num = bo.read_u32(&e.data[0..4]);
            let den = bo.read_u32(&e.data[4..8]);
            Some((num, den))
        }
        // A writer may legitimately store a whole-number resolution as
        // a SHORT / LONG; represent it as n/1.
        TYPE_SHORT | TYPE_LONG => e.as_u32(bo).ok().map(|n| (n, 1)),
        _ => None,
    }
}

/// Best-effort ASCII read: `None` unless the tag is present and its
/// (lossy-decoded) string is non-empty.
fn ascii(entries: &[Entry], tag: u16) -> Option<String> {
    let s = find(entries, tag)?.as_ascii().ok()?;
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Gather the descriptive + structural metadata from a parsed IFD.
///
/// Never fails: a malformed field is dropped, not propagated.
pub fn extract_metadata(entries: &[Entry], bo: ByteOrder) -> TiffMetadata {
    let short = |tag: u16| -> Option<u16> {
        find(entries, tag)
            .and_then(|e| e.as_u32(bo).ok())
            .and_then(|v| u16::try_from(v).ok())
    };

    let page_number = find(entries, TAG_PAGE_NUMBER).and_then(|e| {
        let v = e.as_u32_vec(bo).ok()?;
        if v.len() >= 2 {
            Some((v[0] as u16, v[1] as u16))
        } else {
            None
        }
    });

    TiffMetadata {
        document_name: ascii(entries, TAG_DOCUMENT_NAME),
        image_description: ascii(entries, TAG_IMAGE_DESCRIPTION),
        make: ascii(entries, TAG_MAKE),
        model: ascii(entries, TAG_MODEL),
        page_name: ascii(entries, TAG_PAGE_NAME),
        software: ascii(entries, TAG_SOFTWARE),
        date_time: ascii(entries, TAG_DATE_TIME),
        artist: ascii(entries, TAG_ARTIST),
        host_computer: ascii(entries, TAG_HOST_COMPUTER),
        copyright: ascii(entries, TAG_COPYRIGHT),

        x_resolution: find(entries, TAG_X_RESOLUTION).and_then(|e| rational(e, bo)),
        y_resolution: find(entries, TAG_Y_RESOLUTION).and_then(|e| rational(e, bo)),
        resolution_unit: find(entries, TAG_RESOLUTION_UNIT)
            .and_then(|e| e.as_u32(bo).ok())
            .and_then(ResolutionUnit::from_tag),

        orientation: short(TAG_ORIENTATION),
        page_number,
        new_subfile_type: find(entries, TAG_NEW_SUBFILE_TYPE).and_then(|e| e.as_u32(bo).ok()),
        subfile_type: short(TAG_SUBFILE_TYPE),
    }
}
