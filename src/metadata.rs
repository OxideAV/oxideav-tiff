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

/// Raw structural / codec tags describing *how* the image is stored —
/// the introspection surface a CLI or transcoder needs ("8-bit RGB,
/// LZW, tiled 256×256") without re-walking the IFD.
///
/// These are the *on-disk* tag values, not the decoder's interpreted
/// pixel format: `photometric` / `compression` are the raw tag 262 /
/// 259 codes, `bits_per_sample` is the per-sample list, etc. A tag that
/// is absent (and has a spec default) is reported as its resolved
/// value where the default is unambiguous (`samples_per_pixel` default
/// 1, `bits_per_sample` default `[1]`), else `None` / empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TiffFormatInfo {
    /// PhotometricInterpretation (tag 262) — raw code.
    pub photometric: Option<u16>,
    /// Compression (tag 259) — raw code (1 = none, 5 = LZW, …). Absent
    /// tag resolves to the spec default 1 (no compression).
    pub compression: Option<u16>,
    /// BitsPerSample (tag 258) — one entry per sample; empty only for a
    /// malformed entry (the spec default is `[1]`, filled in here).
    pub bits_per_sample: Vec<u16>,
    /// SamplesPerPixel (tag 277) — resolved (default 1).
    pub samples_per_pixel: u16,
    /// PlanarConfiguration (tag 284) — 1 = chunky, 2 = planar.
    pub planar_config: Option<u16>,
    /// Predictor (tag 317) — 1 = none, 2 = horizontal, 3 = float.
    pub predictor: Option<u16>,
    /// FillOrder (tag 266) — 1 = MSB-first, 2 = LSB-first.
    pub fill_order: Option<u16>,
    /// SampleFormat (tag 339) — one entry per sample (1 = uint, 2 = int,
    /// 3 = IEEE float); empty when the tag is absent (spec default uint).
    pub sample_format: Vec<u16>,
    /// `true` when the IFD stores tiles (has TileWidth / TileLength)
    /// rather than strips.
    pub tiled: bool,
    /// TileWidth × TileLength (tags 322 / 323) when tiled.
    pub tile_size: Option<(u32, u32)>,
    /// RowsPerStrip (tag 278) for a stripped image.
    pub rows_per_strip: Option<u32>,
}

/// Gather the raw structural / codec tags from a parsed IFD. Total: a
/// malformed entry is dropped, never propagated.
pub fn extract_format_info(entries: &[Entry], bo: ByteOrder) -> TiffFormatInfo {
    let short = |tag: u16| -> Option<u16> {
        find(entries, tag)
            .and_then(|e| e.as_u32(bo).ok())
            .and_then(|v| u16::try_from(v).ok())
    };
    let short_vec = |tag: u16| -> Vec<u16> {
        find(entries, tag)
            .and_then(|e| e.as_u32_vec(bo).ok())
            .map(|v| {
                v.into_iter()
                    .filter_map(|x| u16::try_from(x).ok())
                    .collect()
            })
            .unwrap_or_default()
    };

    let samples_per_pixel = short(TAG_SAMPLES_PER_PIXEL).unwrap_or(1);
    let mut bits_per_sample = short_vec(TAG_BITS_PER_SAMPLE);
    if bits_per_sample.is_empty() {
        // Spec default is 1 bit per sample.
        bits_per_sample = vec![1];
    }
    let tile_w = find(entries, TAG_TILE_WIDTH).and_then(|e| e.as_u32(bo).ok());
    let tile_h = find(entries, TAG_TILE_LENGTH).and_then(|e| e.as_u32(bo).ok());
    let tile_size = match (tile_w, tile_h) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    };

    TiffFormatInfo {
        photometric: short(TAG_PHOTOMETRIC_INTERPRETATION),
        compression: Some(short(TAG_COMPRESSION).unwrap_or(1)),
        bits_per_sample,
        samples_per_pixel,
        planar_config: short(TAG_PLANAR_CONFIGURATION),
        predictor: short(TAG_PREDICTOR),
        fill_order: short(TAG_FILL_ORDER),
        sample_format: short_vec(TAG_SAMPLE_FORMAT),
        tiled: tile_size.is_some(),
        tile_size,
        rows_per_strip: find(entries, TAG_ROWS_PER_STRIP).and_then(|e| e.as_u32(bo).ok()),
    }
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
