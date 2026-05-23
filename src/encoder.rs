//! TIFF 6.0 encoder — single-IFD or multi-page (multi-IFD chain),
//! little-endian on-disk byte order ("II"), classic 32-bit offsets.
//!
//! The encoder targets the same baseline our decoder reads:
//!
//! * Photometric: WhiteIsZero (1-bit bilevel), BlackIsZero (greyscale
//!   8/16-bit), RGB (8-bit), Palette (8-bit indexed)
//! * Compression: 1 None, 2 CCITT Modified Huffman, 3 CCITT T.4 1-D
//!   (with optional T4Options bit 2 byte-aligned EOLs), 5 LZW,
//!   8 Deflate, 32773 PackBits
//! * Strip layout — a single strip for chunky pages, or one strip per
//!   component plane for `PlanarConfiguration = 2` pages (see
//!   [`EncodePage::planar`])
//! * Single-IFD or multi-IFD chain via [`encode_tiff_multi`]
//!
//! BigTIFF write, tile write, T.4 2-D / T.6 (Compression=4) encoding,
//! JPEG-in-TIFF, and YCbCr / CIELab / CMYK output are intentionally out
//! of scope for this round. The horizontal-differencing predictor
//! (`Predictor = 2`, TIFF 6.0 §14) is supported on encode via the
//! [`EncodePage::predictor`] flag, and `PlanarConfiguration = 2`
//! (separate component planes, §"PlanarConfiguration") via
//! [`EncodePage::planar`].

use crate::ccitt::{encode_ccitt, CcittVariant, FillOrder};
use crate::compress::{pack_deflate, pack_lzw, pack_packbits};
use crate::error::{Result, TiffError as Error};
use crate::types::*;

/// One palette entry as stored in the on-disk `ColorMap` tag (each
/// component is a 16-bit value, top-byte is the 8-bit colour).
pub type RgbColor = [u8; 3];

/// Description of one image page being written.
///
/// `pixels` is row-major, packed (no padding between rows). For
/// 16-bit grayscale pages the bytes are interpreted little-endian
/// regardless of the stored byte order — the encoder writes II
/// files and the input bytes are consumed verbatim.
#[derive(Debug, Clone)]
pub struct EncodePage<'a> {
    pub width: u32,
    pub height: u32,
    pub kind: EncodePixelFormat<'a>,
    pub compression: TiffCompression,
    /// Apply the TIFF 6.0 §14 horizontal-differencing predictor
    /// (`Predictor = 2`) to the sample data before compression. The
    /// encoder replaces each component with the difference from the
    /// previous pixel of the same component (offset `SamplesPerPixel`,
    /// per §14's "subtract red from red, green from green") and writes
    /// the `Predictor` tag (317) so the decoder reverses the step. Only
    /// meaningful for the lossless byte-aligned photometrics whose
    /// decode path supports it — `Gray8` (8-bit), `Gray16Le` (16-bit),
    /// `Rgb24` (8-bit × 3), and `Palette8` (8-bit indices). §14 ties
    /// the predictor to LZW/Deflate; combining it with the bilevel
    /// CCITT schemes (`Compression = 2 / 3`) or with `Bilevel` input is
    /// rejected with a precise error.
    pub predictor: bool,
    /// Write the image in `PlanarConfiguration = 2` (separate component
    /// planes) layout per TIFF 6.0 §"PlanarConfiguration" (page 38).
    /// When set, each sample component is stored in its own
    /// full-resolution strip (one strip per plane), and `StripOffsets`
    /// / `StripByteCounts` carry `SamplesPerPixel` entries ordered
    /// component-0, component-1, … — the spec's "SamplesPerPixel rows
    /// and StripsPerImage columns" array with StripsPerImage = 1. Only
    /// meaningful for multi-sample formats (`Rgb24`); §"PlanarConfiguration"
    /// notes the field "is irrelevant" when SamplesPerPixel is 1, so
    /// `planar` combined with a single-sample format (`Gray8` /
    /// `Gray16Le` / `Palette8` / `Bilevel`) is rejected with a precise
    /// error. The §14 predictor still applies when both flags are set:
    /// §14 says "If PlanarConfiguration is 2 … Differencing works the
    /// same as it does for grayscale data," so each plane is differenced
    /// independently with an offset of 1 sample.
    pub planar: bool,
}

/// Pixel layouts the encoder knows how to write.
#[derive(Debug, Clone)]
pub enum EncodePixelFormat<'a> {
    /// 1-bit bilevel (1 sample per pixel, MSB-first byte packing).
    /// `pixels` must contain `ceil(width / 8) * height` bytes. The
    /// bit convention follows §10 / §11: bit value 0 = white,
    /// 1 = black. The `WhiteIsZero` (default) PhotometricInterpretation
    /// reads those bits straight through; combine with
    /// `BlackIsZero` by inverting the input first. Required for
    /// CCITT compression schemes ([`TiffCompression::CcittRle`],
    /// [`TiffCompression::CcittT4OneD`]).
    Bilevel { pixels: &'a [u8] },
    /// 8-bit greyscale (BlackIsZero, 1 sample per pixel).
    /// `pixels.len() == width * height`.
    Gray8 { pixels: &'a [u8] },
    /// 16-bit greyscale (BlackIsZero, 1 sample per pixel,
    /// little-endian on disk). `pixels.len() == width * height * 2`.
    Gray16Le { pixels: &'a [u8] },
    /// 8-bit packed RGB. `pixels.len() == width * height * 3`.
    Rgb24 { pixels: &'a [u8] },
    /// 8-bit indexed palette. `indices.len() == width * height`,
    /// `palette.len() <= 256` (any extras are ignored).
    Palette8 {
        indices: &'a [u8],
        palette: &'a [RgbColor],
    },
}

/// Compression scheme for an [`EncodePage`].
#[derive(Debug, Clone, Copy)]
pub enum TiffCompression {
    None,
    PackBits,
    Lzw,
    Deflate,
    /// Compression=2 — CCITT Modified Huffman (TIFF 6.0 §10). Bilevel
    /// only. Encoded as a sequence of white/black run-length codes
    /// from Tables 1/T.4 and 2/T.4. No EOL codes; rows align to byte
    /// boundaries.
    CcittRle,
    /// Compression=3 — CCITT T.4 1-D (TIFF 6.0 §11). Bilevel only.
    /// Each row is preceded by a 12-bit EOL prefix. With
    /// `eol_byte_aligned`, the EOL is byte-aligned (T4Options bit 2).
    /// 2-D coding (T4Options bit 0) is not yet supported on either
    /// the encode or the decode side.
    CcittT4OneD {
        eol_byte_aligned: bool,
    },
}

impl TiffCompression {
    fn tag_value(self) -> u16 {
        match self {
            TiffCompression::None => COMPRESSION_NONE,
            TiffCompression::PackBits => COMPRESSION_PACKBITS,
            TiffCompression::Lzw => COMPRESSION_LZW,
            TiffCompression::Deflate => COMPRESSION_DEFLATE_ADOBE,
            TiffCompression::CcittRle => COMPRESSION_CCITT_HUFFMAN,
            TiffCompression::CcittT4OneD { .. } => COMPRESSION_CCITT_T4,
        }
    }

    /// Compress `raw` per this scheme. For bilevel CCITT schemes the
    /// caller supplies the geometry; non-CCITT schemes ignore those
    /// arguments. Errors only on CCITT (the others are infallible).
    fn pack(self, raw: &[u8], width: u32, rows: u32) -> Result<Vec<u8>> {
        match self {
            TiffCompression::None => Ok(raw.to_vec()),
            TiffCompression::PackBits => Ok(pack_packbits(raw)),
            TiffCompression::Lzw => Ok(pack_lzw(raw)),
            TiffCompression::Deflate => Ok(pack_deflate(raw)),
            TiffCompression::CcittRle => encode_ccitt(
                raw,
                width,
                rows,
                CcittVariant::ModifiedHuffman,
                FillOrder::MsbFirst,
            ),
            TiffCompression::CcittT4OneD { eol_byte_aligned } => encode_ccitt(
                raw,
                width,
                rows,
                CcittVariant::T4OneD { eol_byte_aligned },
                FillOrder::MsbFirst,
            ),
        }
    }

    /// Bilevel CCITT schemes accept only [`EncodePixelFormat::Bilevel`].
    fn is_ccitt(self) -> bool {
        matches!(
            self,
            TiffCompression::CcittRle | TiffCompression::CcittT4OneD { .. }
        )
    }
}

/// One planned page with its compressed strip(s), IFD, and any external
/// blobs (BitsPerSample / ColorMap arrays that don't fit inline).
///
/// `strips` holds one entry for chunky pages and `SamplesPerPixel`
/// entries (one per component plane) for `PlanarConfiguration = 2`
/// pages, in plane order (component 0 first). The on-disk
/// `StripOffsets` / `StripByteCounts` arrays index into this list in
/// the same order.
struct PlannedPage {
    strips: Vec<Vec<u8>>,
    ifd: PageIfd,
    externals: Vec<(BlobId, Vec<u8>)>,
}

/// Encode a single-page TIFF file. Produces the complete byte
/// sequence. Convenience wrapper around [`encode_tiff_multi`].
pub fn encode_tiff(page: &EncodePage<'_>) -> Result<Vec<u8>> {
    encode_tiff_multi(std::slice::from_ref(page))
}

/// Encode a multi-page TIFF file (one IFD per page, chained via
/// the next-IFD pointer in file order). Produces the complete byte
/// sequence.
pub fn encode_tiff_multi(pages: &[EncodePage<'_>]) -> Result<Vec<u8>> {
    if pages.is_empty() {
        return Err(Error::invalid("TIFF encode: must supply at least one page"));
    }

    // Layout strategy:
    //
    // 1. Header (8 bytes): II + 42 + first-IFD-offset.
    // 2. For each page in order:
    //    a. Compressed strip data (one strip per page).
    //    b. Out-of-line value blobs that don't fit inline (BitsPerSample
    //       array for RGB, ColorMap for palette, StripOffsets/StripByteCounts
    //       arrays when count > 1 — currently always count = 1, fits inline).
    //    c. The IFD itself (count + 12 × entries + next-IFD).
    //
    // We compute layout in two passes: first sizing pass, then write
    // pass. The IFD offset of page N+1 is the next-IFD field of
    // page N's IFD.

    // ---- Sizing pass: per-page, derive the on-disk layout ----
    let mut planned: Vec<PlannedPage> = Vec::with_capacity(pages.len());
    for p in pages {
        let plan = plan_page_full(p)?;
        planned.push(plan);
    }

    // ---- Address assignment ----
    //
    // Start at byte 8 (after the header). For each page we lay out:
    // [compressed strip(s)][external blobs][StripOffsets/ByteCounts
    // LONG arrays, only when >1 strip][IFD].
    //
    // We need to know the IFD offset *before* writing it (it goes in
    // the previous IFD's next-IFD slot or in the header for the
    // first page), so we walk pages once to assign byte ranges. The
    // StripOffsets / StripByteCounts arrays are written out-of-line
    // only for multi-strip (planar) pages; a single-strip chunky page
    // keeps both LONGs inline in the IFD value slot.
    let mut cursor: u64 = 8;
    struct PlannedPageAddr {
        // One (offset, size) per strip, in plane order. For chunky
        // pages this has length 1.
        strips: Vec<(u64, u64)>,
        externals: Vec<(BlobId, u64, u64)>, // (id, offset, size)
        // File offsets of the out-of-line StripOffsets / StripByteCounts
        // LONG arrays (only Some when the page has >1 strip).
        strip_offsets_array: Option<u64>,
        strip_byte_counts_array: Option<u64>,
        ifd_offset: u64,
    }
    let mut addrs: Vec<PlannedPageAddr> = Vec::with_capacity(planned.len());
    for plan in &planned {
        // Strip payloads, laid out contiguously in plane order.
        let mut strip_addrs = Vec::with_capacity(plan.strips.len());
        for strip in &plan.strips {
            strip_addrs.push((cursor, strip.len() as u64));
            cursor += strip.len() as u64;
        }
        let mut ext_addrs = Vec::with_capacity(plan.externals.len());
        for (id, blob) in &plan.externals {
            // SHORT/LONG arrays must be 2-/4-byte aligned. Align
            // conservatively to 2 bytes — enough for SHORTs which
            // is what we emit (ColorMap = SHORTs, BitsPerSample =
            // SHORTs).
            if cursor % 2 != 0 {
                cursor += 1;
            }
            ext_addrs.push((*id, cursor, blob.len() as u64));
            cursor += blob.len() as u64;
        }
        // Out-of-line StripOffsets / StripByteCounts arrays for
        // multi-strip (planar) pages. Both are LONG arrays, so align
        // to 4 bytes. Each holds `strips.len()` LONGs.
        let (strip_offsets_array, strip_byte_counts_array) = if plan.strips.len() > 1 {
            if cursor % 4 != 0 {
                cursor += 4 - (cursor % 4);
            }
            let so = cursor;
            cursor += 4 * plan.strips.len() as u64;
            let sbc = cursor;
            cursor += 4 * plan.strips.len() as u64;
            (Some(so), Some(sbc))
        } else {
            (None, None)
        };
        // IFD must be 2-byte aligned (entries are u16/u32 mixes).
        if cursor % 2 != 0 {
            cursor += 1;
        }
        let ifd_offset = cursor;
        // count(2) + entries × 12 + next_ifd(4)
        let ifd_size = 2 + (plan.ifd.entries.len() as u64) * 12 + 4;
        cursor += ifd_size;
        addrs.push(PlannedPageAddr {
            strips: strip_addrs,
            externals: ext_addrs,
            strip_offsets_array,
            strip_byte_counts_array,
            ifd_offset,
        });
        if ifd_offset > u32::MAX as u64 || cursor > u32::MAX as u64 {
            return Err(Error::invalid(
                "TIFF encode: classic-TIFF 32-bit offset overflow (would need BigTIFF)",
            ));
        }
    }

    // ---- Write pass ----
    let total = cursor as usize;
    let mut out = vec![0u8; total];
    // Header.
    out[0] = b'I';
    out[1] = b'I';
    out[2..4].copy_from_slice(&42u16.to_le_bytes());
    out[4..8].copy_from_slice(&(addrs[0].ifd_offset as u32).to_le_bytes());

    for (i, (plan, addr)) in planned.iter().zip(addrs.iter()).enumerate() {
        // Strip payload(s), one per plane for planar pages.
        for (strip, (off, size)) in plan.strips.iter().zip(addr.strips.iter()) {
            assert_eq!(strip.len() as u64, *size);
            out[*off as usize..(*off + *size) as usize].copy_from_slice(strip);
        }
        // External blobs.
        for (j, (id, off, size)) in addr.externals.iter().enumerate() {
            assert_eq!(*id, plan.externals[j].0);
            let blob = &plan.externals[j].1;
            assert_eq!(blob.len() as u64, *size);
            out[*off as usize..(*off + *size) as usize].copy_from_slice(blob);
        }
        // Out-of-line StripOffsets / StripByteCounts LONG arrays (only
        // present for multi-strip planar pages). Each entry is the
        // corresponding strip's file offset / compressed length.
        if let Some(so) = addr.strip_offsets_array {
            for (k, (off, _)) in addr.strips.iter().enumerate() {
                let slot = so as usize + k * 4;
                out[slot..slot + 4].copy_from_slice(&(*off as u32).to_le_bytes());
            }
        }
        if let Some(sbc) = addr.strip_byte_counts_array {
            for (k, (_, size)) in addr.strips.iter().enumerate() {
                let slot = sbc as usize + k * 4;
                out[slot..slot + 4].copy_from_slice(&(*size as u32).to_le_bytes());
            }
        }
        // IFD.
        let ifd_off = addr.ifd_offset as usize;
        out[ifd_off..ifd_off + 2].copy_from_slice(&(plan.ifd.entries.len() as u16).to_le_bytes());
        let next_ifd_off = ifd_off + 2 + plan.ifd.entries.len() * 12;
        // Resolve each entry's value-or-offset slot.
        for (k, e) in plan.ifd.entries.iter().enumerate() {
            let entry_off = ifd_off + 2 + k * 12;
            out[entry_off..entry_off + 2].copy_from_slice(&e.tag.to_le_bytes());
            out[entry_off + 2..entry_off + 4].copy_from_slice(&e.field_type.to_le_bytes());
            out[entry_off + 4..entry_off + 8].copy_from_slice(&e.count.to_le_bytes());
            // Value-or-offset slot:
            let slot = &mut out[entry_off + 8..entry_off + 12];
            match &e.value {
                IfdValue::Inline(bytes) => {
                    let n = bytes.len();
                    slot[..n].copy_from_slice(bytes);
                    if n < 4 {
                        for b in &mut slot[n..] {
                            *b = 0;
                        }
                    }
                }
                IfdValue::StripOffsets => {
                    if let Some(so) = addr.strip_offsets_array {
                        // >1 strip: value slot holds the file offset of
                        // the out-of-line LONG array.
                        slot.copy_from_slice(&(so as u32).to_le_bytes());
                    } else {
                        // Single strip: the LONG offset fits inline.
                        slot.copy_from_slice(&(addr.strips[0].0 as u32).to_le_bytes());
                    }
                }
                IfdValue::StripByteCounts => {
                    if let Some(sbc) = addr.strip_byte_counts_array {
                        slot.copy_from_slice(&(sbc as u32).to_le_bytes());
                    } else {
                        slot.copy_from_slice(&(addr.strips[0].1 as u32).to_le_bytes());
                    }
                }
                IfdValue::ExternalBlob(id) => {
                    let (_, off, _) = addr
                        .externals
                        .iter()
                        .find(|(x, _, _)| *x == *id)
                        .ok_or_else(|| {
                            Error::invalid("TIFF encode: missing planned blob for entry")
                        })?;
                    slot.copy_from_slice(&(*off as u32).to_le_bytes());
                }
            }
        }
        // Next-IFD pointer.
        let next_offset: u32 = if i + 1 < addrs.len() {
            addrs[i + 1].ifd_offset as u32
        } else {
            0
        };
        out[next_ifd_off..next_ifd_off + 4].copy_from_slice(&next_offset.to_le_bytes());
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobId {
    BitsPerSample,
    ColorMapWords,
}

#[derive(Debug, Clone)]
enum IfdValue {
    /// Up to 4 bytes packed inline into the value/offset slot.
    Inline(Vec<u8>),
    /// Resolved at write time: the page's `StripOffsets` array. For a
    /// single strip (chunky) the LONG fits inline; for
    /// `PlanarConfiguration = 2` the per-plane offsets are written as a
    /// LONG array out-of-line and this slot holds its file offset.
    StripOffsets,
    /// Resolved at write time: the page's `StripByteCounts` array,
    /// inline for a single strip or an out-of-line LONG array for
    /// multiple strips.
    StripByteCounts,
    /// Reference to an external blob attached to this page.
    ExternalBlob(BlobId),
}

#[derive(Debug, Clone)]
struct PageIfdEntry {
    tag: u16,
    field_type: u16,
    count: u32,
    value: IfdValue,
}

#[derive(Debug)]
struct PageIfd {
    entries: Vec<PageIfdEntry>,
}

fn plan_page_full(p: &EncodePage<'_>) -> Result<PlannedPage> {
    // CCITT schemes are bilevel-only per TIFF 6.0 §10 / §11.
    if p.compression.is_ccitt() && !matches!(p.kind, EncodePixelFormat::Bilevel { .. }) {
        return Err(Error::invalid(
            "TIFF encode: CCITT compression (Compression=2/3) requires Bilevel input",
        ));
    }

    // PlanarConfiguration=2 (separate component planes) is only
    // meaningful when there is more than one sample per pixel; TIFF 6.0
    // §"PlanarConfiguration" (page 38): "If SamplesPerPixel is 1,
    // PlanarConfiguration is irrelevant." Reject the single-sample
    // formats and the bit-packed bilevel format up front. The only
    // multi-sample format the encoder writes is Rgb24 (SPP=3).
    if p.planar && !matches!(p.kind, EncodePixelFormat::Rgb24 { .. }) {
        return Err(Error::invalid(
            "TIFF encode: PlanarConfiguration=2 (separate planes) requires a multi-sample \
             format; TIFF 6.0 §\"PlanarConfiguration\" says the field is irrelevant when \
             SamplesPerPixel is 1 (Gray8 / Gray16Le / Palette8 / Bilevel)",
        ));
    }
    // CCITT schemes are bilevel-only (rejected above) so they never
    // reach the planar path; the predictor is handled per-plane below.

    // The §14 horizontal-differencing predictor operates on whole
    // sample components; it has no meaning for bit-packed bilevel data,
    // and §14 only defines it for the LZW-family lossless coders. Reject
    // the impossible combinations up front so they surface precisely
    // rather than producing a file the decoder can't reverse.
    if p.predictor {
        if matches!(p.kind, EncodePixelFormat::Bilevel { .. }) {
            return Err(Error::invalid(
                "TIFF encode: Predictor=2 (horizontal differencing) is undefined for Bilevel \
                 (1-bit) input — TIFF 6.0 §14 differences whole sample components",
            ));
        }
        if p.compression.is_ccitt() {
            return Err(Error::invalid(
                "TIFF encode: Predictor=2 cannot combine with CCITT compression \
                 (Compression=2/3); TIFF 6.0 §14 ties the predictor to the LZW family",
            ));
        }
    }

    let (samples_per_pixel, bits_per_sample, photometric, mut raw_pixels, color_map_words) =
        match &p.kind {
            EncodePixelFormat::Bilevel { pixels } => {
                let row_bytes = (p.width as usize).div_ceil(8);
                let want = row_bytes * (p.height as usize);
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Bilevel: pixel buffer is {} bytes, expected {want} \
                         (row_bytes={row_bytes}, height={})",
                        pixels.len(),
                        p.height
                    )));
                }
                // §10 / §11: "The 'normal' PhotometricInterpretation
                // for bilevel CCITT compressed data is WhiteIsZero".
                // We follow the same default for uncompressed bilevel
                // so a Bilevel input round-trips through any
                // compression scheme without changing meaning. The
                // decoder applies the photometric inversion on the
                // way to Gray8.
                (1u16, vec![1u16], PHOTO_WHITE_IS_ZERO, pixels.to_vec(), None)
            }
            EncodePixelFormat::Gray8 { pixels } => {
                let want = (p.width as usize) * (p.height as usize);
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Gray8: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (1u16, vec![8u16], PHOTO_BLACK_IS_ZERO, pixels.to_vec(), None)
            }
            EncodePixelFormat::Gray16Le { pixels } => {
                let want = (p.width as usize) * (p.height as usize) * 2;
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Gray16Le: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (
                    1u16,
                    vec![16u16],
                    PHOTO_BLACK_IS_ZERO,
                    pixels.to_vec(),
                    None,
                )
            }
            EncodePixelFormat::Rgb24 { pixels } => {
                let want = (p.width as usize) * (p.height as usize) * 3;
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Rgb24: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (3u16, vec![8u16, 8, 8], PHOTO_RGB, pixels.to_vec(), None)
            }
            EncodePixelFormat::Palette8 { indices, palette } => {
                let want = (p.width as usize) * (p.height as usize);
                if indices.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Palette8: index buffer is {} bytes, expected {want}",
                        indices.len()
                    )));
                }
                if palette.is_empty() || palette.len() > 256 {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Palette8: palette must have 1..=256 entries (got {})",
                        palette.len()
                    )));
                }
                // ColorMap stores 256 SHORTs per channel for an 8-bpp
                // palette per spec; pad missing entries with 0.
                let mut cm = vec![0u16; 256 * 3];
                for (i, c) in palette.iter().enumerate() {
                    // Replicate 8-bit channel into the high byte of
                    // the 16-bit ColorMap entry (upper bits =
                    // intensity). The canonical (v << 8) | v
                    // expansion ensures a 0xFF 8-bit value reads
                    // back as 0xFFFF in the 16-bit field.
                    cm[i] = ((c[0] as u16) << 8) | c[0] as u16;
                    cm[256 + i] = ((c[1] as u16) << 8) | c[1] as u16;
                    cm[512 + i] = ((c[2] as u16) << 8) | c[2] as u16;
                }
                (1u16, vec![8u16], PHOTO_PALETTE, indices.to_vec(), Some(cm))
            }
        };

    // Build the page's compressed strip(s). Chunky pages are a single
    // strip over the interleaved data; PlanarConfiguration=2 pages emit
    // one strip per component plane (full image height), in plane order
    // (component 0 first), matching the decoder's planar walker.
    let bps = bits_per_sample[0] as usize;
    let strips: Vec<Vec<u8>> = if p.planar {
        // De-interleave chunky RGBRGB… into separate R / G / B planes
        // (TIFF 6.0 §"PlanarConfiguration": "Red components in one
        // component plane, the Green in another, and the Blue in
        // another"). Only Rgb24 (SPP=3, 8 bits) reaches here.
        let spp = samples_per_pixel as usize;
        let bytes_per_sample = bps / 8;
        let pixels = (p.width as usize) * (p.height as usize);
        let plane_len = pixels * bytes_per_sample;
        let mut out_strips = Vec::with_capacity(spp);
        for plane in 0..spp {
            let mut plane_buf = vec![0u8; plane_len];
            for px in 0..pixels {
                let src = (px * spp + plane) * bytes_per_sample;
                let dst = px * bytes_per_sample;
                plane_buf[dst..dst + bytes_per_sample]
                    .copy_from_slice(&raw_pixels[src..src + bytes_per_sample]);
            }
            // §14: "If PlanarConfiguration is 2 … Differencing works the
            // same as it does for grayscale data." Each plane is a
            // single-component image, so the predictor runs with an
            // offset of one sample.
            if p.predictor {
                let plane_row_bytes = (p.width as usize) * bytes_per_sample;
                forward_horizontal_predictor(
                    &mut plane_buf,
                    p.width as usize,
                    p.height as usize,
                    1,
                    bps,
                    plane_row_bytes,
                )?;
            }
            out_strips.push(p.compression.pack(&plane_buf, p.width, p.height)?);
        }
        out_strips
    } else {
        // Apply the §14 horizontal-differencing predictor *before*
        // compression. The encoder stores first differences; the
        // decoder's cumulative left-to-right add reverses it exactly.
        // Chunky single-strip layout, so `row_bytes` is the packed
        // sample stride and the whole image is one differencing region.
        if p.predictor {
            let row_bytes = (p.width as usize) * (samples_per_pixel as usize) * (bps / 8);
            forward_horizontal_predictor(
                &mut raw_pixels,
                p.width as usize,
                p.height as usize,
                samples_per_pixel as usize,
                bps,
                row_bytes,
            )?;
        }
        vec![p.compression.pack(&raw_pixels, p.width, p.height)?]
    };
    let planar_config = if p.planar {
        PLANAR_SEPARATE
    } else {
        PLANAR_CHUNKY
    };
    let strip_count = strips.len() as u32;

    // Build the IFD entry list. Tags must appear in ascending order
    // per spec.
    let mut entries: Vec<PageIfdEntry> = Vec::new();
    let mut externals: Vec<(BlobId, Vec<u8>)> = Vec::new();

    // 254 NewSubfileType — 0 = full-resolution image (only flag we
    // know how to set safely; included so multi-page readers can
    // walk the chain confidently).
    entries.push(PageIfdEntry {
        tag: TAG_NEW_SUBFILE_TYPE,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::Inline(0u32.to_le_bytes().to_vec()),
    });
    // 256 ImageWidth (LONG)
    entries.push(PageIfdEntry {
        tag: TAG_IMAGE_WIDTH,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::Inline(p.width.to_le_bytes().to_vec()),
    });
    // 257 ImageLength (LONG)
    entries.push(PageIfdEntry {
        tag: TAG_IMAGE_LENGTH,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::Inline(p.height.to_le_bytes().to_vec()),
    });
    // 258 BitsPerSample (SHORT × samples_per_pixel)
    let bps_inline_bytes: Vec<u8> = bits_per_sample
        .iter()
        .flat_map(|b| b.to_le_bytes())
        .collect();
    if bps_inline_bytes.len() <= 4 {
        entries.push(PageIfdEntry {
            tag: TAG_BITS_PER_SAMPLE,
            field_type: TYPE_SHORT,
            count: bits_per_sample.len() as u32,
            value: IfdValue::Inline(bps_inline_bytes),
        });
    } else {
        externals.push((BlobId::BitsPerSample, bps_inline_bytes));
        entries.push(PageIfdEntry {
            tag: TAG_BITS_PER_SAMPLE,
            field_type: TYPE_SHORT,
            count: bits_per_sample.len() as u32,
            value: IfdValue::ExternalBlob(BlobId::BitsPerSample),
        });
    }
    // 259 Compression (SHORT)
    entries.push(PageIfdEntry {
        tag: TAG_COMPRESSION,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(p.compression.tag_value().to_le_bytes().to_vec()),
    });
    // 262 PhotometricInterpretation (SHORT)
    entries.push(PageIfdEntry {
        tag: TAG_PHOTOMETRIC_INTERPRETATION,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(photometric.to_le_bytes().to_vec()),
    });
    // 273 StripOffsets (LONG). count=1 for chunky (one strip), or
    // SamplesPerPixel for PlanarConfiguration=2 (one strip per plane).
    entries.push(PageIfdEntry {
        tag: TAG_STRIP_OFFSETS,
        field_type: TYPE_LONG,
        count: strip_count,
        value: IfdValue::StripOffsets,
    });
    // 277 SamplesPerPixel (SHORT)
    entries.push(PageIfdEntry {
        tag: TAG_SAMPLES_PER_PIXEL,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(samples_per_pixel.to_le_bytes().to_vec()),
    });
    // 278 RowsPerStrip (LONG)
    entries.push(PageIfdEntry {
        tag: TAG_ROWS_PER_STRIP,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::Inline(p.height.to_le_bytes().to_vec()),
    });
    // 279 StripByteCounts (LONG). count matches StripOffsets.
    entries.push(PageIfdEntry {
        tag: TAG_STRIP_BYTE_COUNTS,
        field_type: TYPE_LONG,
        count: strip_count,
        value: IfdValue::StripByteCounts,
    });
    // 284 PlanarConfiguration (SHORT) — 1 (chunky) or 2 (separate
    // planes) per the page's `planar` flag.
    entries.push(PageIfdEntry {
        tag: TAG_PLANAR_CONFIGURATION,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(planar_config.to_le_bytes().to_vec()),
    });
    // 292 T4Options (LONG) — only for Compression=3. Bit 0 (2D) and
    // bit 1 (uncompressed mode) are always clear for this encoder;
    // bit 2 (EOL byte-aligned) is set per the variant flag.
    if let TiffCompression::CcittT4OneD { eol_byte_aligned } = p.compression {
        let flags: u32 = if eol_byte_aligned {
            T4OPT_EOL_BYTE_ALIGNED
        } else {
            0
        };
        entries.push(PageIfdEntry {
            tag: TAG_T4_OPTIONS,
            field_type: TYPE_LONG,
            count: 1,
            value: IfdValue::Inline(flags.to_le_bytes().to_vec()),
        });
    }
    // 317 Predictor (SHORT) — only when horizontal differencing is on.
    // Default (Predictor=1, no prediction) is omitted; the decoder
    // treats an absent tag as 1.
    if p.predictor {
        entries.push(PageIfdEntry {
            tag: TAG_PREDICTOR,
            field_type: TYPE_SHORT,
            count: 1,
            value: IfdValue::Inline(PREDICTOR_HORIZONTAL.to_le_bytes().to_vec()),
        });
    }
    // 320 ColorMap (SHORT, 3*2^bps) — palette only.
    if let Some(cm) = color_map_words {
        let bytes: Vec<u8> = cm.iter().flat_map(|w| w.to_le_bytes()).collect();
        let count = cm.len() as u32;
        externals.push((BlobId::ColorMapWords, bytes));
        entries.push(PageIfdEntry {
            tag: TAG_COLOR_MAP,
            field_type: TYPE_SHORT,
            count,
            value: IfdValue::ExternalBlob(BlobId::ColorMapWords),
        });
    }

    // Spec: entries must be in ascending tag order. The pushes
    // above are already sorted (254/256/257/258/259/262/273/277/
    // 278/279/284/292?/317?/320?), but assert defensively.
    debug_assert!(entries.windows(2).all(|w| w[0].tag <= w[1].tag));

    Ok(PlannedPage {
        strips,
        ifd: PageIfd { entries },
        externals,
    })
}

/// Forward horizontal-differencing predictor (TIFF 6.0 §14): replace
/// each component with the difference from the previous pixel of the
/// same component, in place. The inverse of the decoder's
/// `apply_horizontal_predictor`: that routine adds left-to-right, so
/// the encoder subtracts right-to-left to keep each "previous" value
/// at its *original* magnitude while the difference is taken. §14:
/// "we will do our horizontal differences with an offset of
/// SamplesPerPixel ... subtract red from red, green from green, and
/// blue from blue." Encoder output is always II (little-endian), so
/// 16-bit components are read/written little-endian.
///
/// `row_bytes` is the packed sample stride (chunky, single-strip). The
/// "ignore the overflow bits" wrap-around §14 relies on is exactly
/// two's-complement `wrapping_sub`.
fn forward_horizontal_predictor(
    buf: &mut [u8],
    width: usize,
    rows: usize,
    samples: usize,
    bps: usize,
    row_bytes: usize,
) -> Result<()> {
    if width == 0 || rows == 0 {
        return Ok(());
    }
    match bps {
        8 => {
            for r in 0..rows {
                let row = &mut buf[r * row_bytes..r * row_bytes + width * samples];
                // Right-to-left so row[x - samples] is still the
                // original sample when we difference row[x].
                for x in (samples..(width * samples)).rev() {
                    row[x] = row[x].wrapping_sub(row[x - samples]);
                }
            }
        }
        16 => {
            for r in 0..rows {
                let row = &mut buf[r * row_bytes..r * row_bytes + width * samples * 2];
                let pixels = width * samples;
                for x in (samples..pixels).rev() {
                    let cur_off = x * 2;
                    let prev_off = (x - samples) * 2;
                    let cur = u16::from_le_bytes([row[cur_off], row[cur_off + 1]]);
                    let prev = u16::from_le_bytes([row[prev_off], row[prev_off + 1]]);
                    let new = cur.wrapping_sub(prev);
                    let bytes = new.to_le_bytes();
                    row[cur_off] = bytes[0];
                    row[cur_off + 1] = bytes[1];
                }
            }
        }
        _ => {
            // The encoder only emits 8- and 16-bit components, so this
            // is unreachable from the public API; keep the precise
            // error for defensiveness / future bit depths.
            return Err(Error::invalid(format!(
                "TIFF encode: Predictor=2 at bits_per_sample={bps} unsupported"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_tiff;

    fn ramp_gray8(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                v.push(((x + y) & 0xFF) as u8);
            }
        }
        v
    }

    fn pattern_rgb(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h as u8 {
            for x in 0..w as u8 {
                v.push(x.wrapping_mul(7));
                v.push(y.wrapping_mul(11));
                v.push((x ^ y).wrapping_mul(13));
            }
        }
        v
    }

    #[test]
    fn encode_gray8_uncompressed_roundtrip() {
        let pixels = ramp_gray8(32, 32);
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (32, 32));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_gray16_packbits_roundtrip() {
        let mut pixels = Vec::with_capacity(16 * 16 * 2);
        for y in 0u16..16 {
            for x in 0u16..16 {
                let v = (x.wrapping_mul(257)).wrapping_add(y.wrapping_mul(513));
                pixels.extend_from_slice(&v.to_le_bytes());
            }
        }
        let page = EncodePage {
            width: 16,
            height: 16,
            kind: EncodePixelFormat::Gray16Le { pixels: &pixels },
            compression: TiffCompression::PackBits,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (16, 16));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_rgb24_lzw_roundtrip() {
        let pixels = pattern_rgb(20, 20);
        let page = EncodePage {
            width: 20,
            height: 20,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (20, 20));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_rgb24_deflate_roundtrip() {
        let pixels = pattern_rgb(48, 24);
        let page = EncodePage {
            width: 48,
            height: 24,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::Deflate,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (48, 24));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_palette_roundtrip() {
        // 4-color palette: black, red, green, white.
        let palette = vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [255, 255, 255]];
        let mut indices = Vec::with_capacity(8 * 8);
        for y in 0..8 {
            for x in 0..8 {
                indices.push(((x ^ y) & 0x3) as u8);
            }
        }
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Palette8 {
                indices: &indices,
                palette: &palette,
            },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        // Decoder expands palette → Rgb24.
        let mut want = Vec::with_capacity(8 * 8 * 3);
        for &idx in &indices {
            let p = palette[idx as usize];
            want.extend_from_slice(&p);
        }
        assert_eq!(d.frame.planes[0].data, want);
    }

    fn bilevel_checkerboard(w: u32, h: u32) -> Vec<u8> {
        // Pack an MSB-first 1-bit bilevel buffer with a 1-pixel
        // checkerboard pattern. Used to exercise the run-length coder
        // on the worst-case input (every pixel is a run boundary).
        let row_bytes = (w as usize).div_ceil(8);
        let mut out = vec![0u8; row_bytes * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let on = ((x ^ y) & 1) == 1;
                if on {
                    out[y * row_bytes + x / 8] |= 1 << (7 - (x % 8));
                }
            }
        }
        out
    }

    fn bilevel_stripes(w: u32, h: u32, period: u32) -> Vec<u8> {
        // Wider runs to exercise the make-up-code paths.
        let row_bytes = (w as usize).div_ceil(8);
        let mut out = vec![0u8; row_bytes * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let on = ((x as u32) / period) & 1 == 1;
                if on {
                    out[y * row_bytes + x / 8] |= 1 << (7 - (x % 8));
                }
            }
        }
        out
    }

    /// Inflate a packed MSB-first bilevel buffer to Gray8 the same
    /// way the decoder would, with the WhiteIsZero convention the
    /// `Bilevel` encoder emits (bit 0 = white = 0xFF in Gray8).
    fn bilevel_to_gray8(packed: &[u8], w: u32, h: u32) -> Vec<u8> {
        let row_bytes = (w as usize).div_ceil(8);
        let mut out = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            let row = &packed[y * row_bytes..(y + 1) * row_bytes];
            for x in 0..w as usize {
                let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                // WhiteIsZero photometric: bit 0 -> white -> 0xFF.
                out.push(if bit == 0 { 0xFF } else { 0x00 });
            }
        }
        out
    }

    #[test]
    fn encode_bilevel_uncompressed_roundtrip() {
        let packed = bilevel_checkerboard(24, 16);
        let page = EncodePage {
            width: 24,
            height: 16,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (24, 16));
        let want = bilevel_to_gray8(&packed, 24, 16);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_bilevel_ccitt_rle_roundtrip_checkerboard() {
        let packed = bilevel_checkerboard(16, 8);
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let want = bilevel_to_gray8(&packed, 16, 8);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_bilevel_ccitt_rle_roundtrip_stripes() {
        // Width 128 with a 16-pixel stripe period exercises the
        // make-up-code path on every run (each run is exactly 16
        // pixels = white-terminating or black-terminating directly).
        let packed = bilevel_stripes(128, 4, 16);
        let page = EncodePage {
            width: 128,
            height: 4,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let want = bilevel_to_gray8(&packed, 128, 4);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_bilevel_ccitt_t4_1d_roundtrip() {
        let packed = bilevel_stripes(96, 6, 8);
        let page = EncodePage {
            width: 96,
            height: 6,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittT4OneD {
                eol_byte_aligned: false,
            },
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let want = bilevel_to_gray8(&packed, 96, 6);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_bilevel_ccitt_t4_1d_byte_aligned_roundtrip() {
        // T4Options bit 2 must end up in the IFD and the decoder
        // must read it back to find the byte-aligned EOLs.
        let packed = bilevel_stripes(64, 8, 4);
        let page = EncodePage {
            width: 64,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittT4OneD {
                eol_byte_aligned: true,
            },
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let want = bilevel_to_gray8(&packed, 64, 8);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_ccitt_rejects_non_bilevel() {
        // Asking for CCITT compression with a Gray8 input must be a
        // clean error, not a silent mis-encode.
        let pixels = ramp_gray8(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
        };
        let r = encode_tiff(&page);
        assert!(r.is_err());
    }

    #[test]
    fn encode_multi_page_chain() {
        let p1 = ramp_gray8(8, 8);
        let p2 = pattern_rgb(8, 8);
        let pages = vec![
            EncodePage {
                width: 8,
                height: 8,
                kind: EncodePixelFormat::Gray8 { pixels: &p1 },
                compression: TiffCompression::None,
                predictor: false,
                planar: false,
            },
            EncodePage {
                width: 8,
                height: 8,
                kind: EncodePixelFormat::Rgb24 { pixels: &p2 },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
            },
        ];
        let bytes = encode_tiff_multi(&pages).unwrap();
        let imgs = crate::decoder::decode_tiff_all(&bytes).unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].width, 8);
        assert_eq!(imgs[0].planes[0].data, p1);
        assert_eq!(imgs[1].width, 8);
        assert_eq!(imgs[1].planes[0].data, p2);
    }

    // ---- Predictor=2 (horizontal differencing, TIFF 6.0 §14) ----

    fn pattern_gray16(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 2) as usize);
        for y in 0..h {
            for x in 0..w {
                // Smoothly-varying so differences are small; the
                // predictor's correctness is independent of magnitude
                // (two's-complement wrap), but a ramp is the realistic
                // case §14 targets.
                let s = (x.wrapping_mul(311)).wrapping_add(y.wrapping_mul(101)) as u16;
                v.extend_from_slice(&s.to_le_bytes());
            }
        }
        v
    }

    /// Helper: encode `kind` with Predictor=2 + `comp`, decode, and
    /// assert the round-trip is bit-exact.
    fn predictor_roundtrip(
        width: u32,
        height: u32,
        kind: EncodePixelFormat<'_>,
        comp: TiffCompression,
    ) -> Vec<u8> {
        let page = EncodePage {
            width,
            height,
            kind,
            compression: comp,
            predictor: true,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (width, height));
        d.frame.planes[0].data.clone()
    }

    #[test]
    fn encode_gray8_predictor_lzw_roundtrip() {
        let pixels = ramp_gray8(40, 24);
        let out = predictor_roundtrip(
            40,
            24,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::Lzw,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray8_predictor_deflate_roundtrip() {
        let pixels = ramp_gray8(33, 17);
        let out = predictor_roundtrip(
            33,
            17,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::Deflate,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray8_predictor_none_roundtrip() {
        // §14 ties the predictor to LZW, but the tag is orthogonal to
        // the compressor; Compression=1 + Predictor=2 must still
        // round-trip (the decoder reverses the differencing regardless).
        let pixels = ramp_gray8(16, 16);
        let out = predictor_roundtrip(
            16,
            16,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::None,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray16_predictor_lzw_roundtrip() {
        let pixels = pattern_gray16(24, 20);
        let out = predictor_roundtrip(
            24,
            20,
            EncodePixelFormat::Gray16Le { pixels: &pixels },
            TiffCompression::Lzw,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray16_predictor_deflate_roundtrip() {
        let pixels = pattern_gray16(15, 9);
        let out = predictor_roundtrip(
            15,
            9,
            EncodePixelFormat::Gray16Le { pixels: &pixels },
            TiffCompression::Deflate,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_rgb24_predictor_lzw_roundtrip() {
        // §14: per-component differencing with an offset of
        // SamplesPerPixel (3). A pattern where R/G/B differ ensures a
        // plane swap or wrong offset would corrupt the round-trip.
        let pixels = pattern_rgb(28, 19);
        let out = predictor_roundtrip(
            28,
            19,
            EncodePixelFormat::Rgb24 { pixels: &pixels },
            TiffCompression::Lzw,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_rgb24_predictor_deflate_roundtrip() {
        let pixels = pattern_rgb(11, 13);
        let out = predictor_roundtrip(
            11,
            13,
            EncodePixelFormat::Rgb24 { pixels: &pixels },
            TiffCompression::Deflate,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_rgb24_predictor_packbits_roundtrip() {
        let pixels = pattern_rgb(9, 7);
        let out = predictor_roundtrip(
            9,
            7,
            EncodePixelFormat::Rgb24 { pixels: &pixels },
            TiffCompression::PackBits,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_palette_predictor_roundtrip() {
        // Palette indices are single-component 8-bit values; §14
        // differencing applies as for grayscale. The decoder expands
        // the (reversed) indices through the colormap to Rgb24.
        let palette = vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [255, 255, 255]];
        let mut indices = Vec::with_capacity(12 * 8);
        for y in 0..8u32 {
            for x in 0..12u32 {
                indices.push(((x + y) & 0x3) as u8);
            }
        }
        let page = EncodePage {
            width: 12,
            height: 8,
            kind: EncodePixelFormat::Palette8 {
                indices: &indices,
                palette: &palette,
            },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let mut want = Vec::with_capacity(12 * 8 * 3);
        for &idx in &indices {
            want.extend_from_slice(&palette[idx as usize]);
        }
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_predictor_emits_tag_317() {
        // The encoded file must carry Predictor=2 (tag 317, SHORT) so a
        // third-party reader reverses the differencing. Walk the
        // single IFD looking for the 12-byte entry whose tag is 317.
        let pixels = ramp_gray8(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
        };
        let b = encode_tiff(&page).unwrap();
        let ifd_off = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let count = u16::from_le_bytes([b[ifd_off], b[ifd_off + 1]]) as usize;
        let mut found = None;
        for k in 0..count {
            let e = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([b[e], b[e + 1]]);
            if tag == TAG_PREDICTOR {
                let ty = u16::from_le_bytes([b[e + 2], b[e + 3]]);
                let val = u16::from_le_bytes([b[e + 8], b[e + 9]]);
                found = Some((ty, val));
            }
        }
        assert_eq!(found, Some((TYPE_SHORT, PREDICTOR_HORIZONTAL)));

        // No-predictor encode must omit the tag entirely (decoder
        // defaults to Predictor=1).
        let page2 = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
        };
        let b2 = encode_tiff(&page2).unwrap();
        let ifd2 = u32::from_le_bytes([b2[4], b2[5], b2[6], b2[7]]) as usize;
        let count2 = u16::from_le_bytes([b2[ifd2], b2[ifd2 + 1]]) as usize;
        for k in 0..count2 {
            let e = ifd2 + 2 + k * 12;
            let tag = u16::from_le_bytes([b2[e], b2[e + 1]]);
            assert_ne!(tag, TAG_PREDICTOR);
        }
    }

    #[test]
    fn encode_predictor_rejects_bilevel() {
        let packed = bilevel_checkerboard(16, 8);
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
        };
        assert!(encode_tiff(&page).is_err());
    }

    #[test]
    fn encode_predictor_rejects_ccitt() {
        let packed = bilevel_checkerboard(16, 8);
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittRle,
            predictor: true,
            planar: false,
        };
        assert!(encode_tiff(&page).is_err());
    }

    #[test]
    fn forward_predictor_inverts_decoder_add_gray8() {
        // Direct unit check that forward differencing is the exact
        // inverse of the decoder's cumulative add for a known row.
        let mut row = vec![10u8, 12, 9, 9, 200, 201];
        let orig = row.clone();
        forward_horizontal_predictor(&mut row, 6, 1, 1, 8, 6).unwrap();
        // First sample unchanged; rest are first differences.
        assert_eq!(row[0], 10);
        assert_eq!(row[1], 12u8.wrapping_sub(10));
        assert_eq!(row[5], 201u8.wrapping_sub(200));
        // Reverse via the decoder's algorithm (left-to-right add).
        for x in 1..6 {
            row[x] = row[x].wrapping_add(row[x - 1]);
        }
        assert_eq!(row, orig);
    }

    // ---- PlanarConfiguration = 2 (separate planes) encode ----

    fn planar_roundtrip(w: u32, h: u32, compression: TiffCompression, predictor: bool) {
        let pixels = pattern_rgb(w, h);
        let page = EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression,
            predictor,
            planar: true,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (w, h));
        // Decoder re-interleaves the planes into chunky order, so the
        // output must match the original chunky RGB input bit-exactly.
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_rgb24_planar_none_roundtrip() {
        planar_roundtrip(20, 16, TiffCompression::None, false);
    }

    #[test]
    fn encode_rgb24_planar_packbits_roundtrip() {
        planar_roundtrip(33, 9, TiffCompression::PackBits, false);
    }

    #[test]
    fn encode_rgb24_planar_lzw_roundtrip() {
        planar_roundtrip(48, 24, TiffCompression::Lzw, false);
    }

    #[test]
    fn encode_rgb24_planar_deflate_roundtrip() {
        planar_roundtrip(17, 31, TiffCompression::Deflate, false);
    }

    #[test]
    fn encode_rgb24_planar_predictor_lzw_roundtrip() {
        // §14 + PlanarConfiguration=2: each plane is differenced
        // independently with an offset of one sample.
        planar_roundtrip(40, 20, TiffCompression::Lzw, true);
    }

    #[test]
    fn encode_rgb24_planar_predictor_deflate_roundtrip() {
        planar_roundtrip(28, 28, TiffCompression::Deflate, true);
    }

    /// Inspect the encoded IFD: PlanarConfiguration must read 2, and
    /// StripOffsets / StripByteCounts must each carry SamplesPerPixel
    /// (= 3) entries — the spec's "SamplesPerPixel rows and
    /// StripsPerImage columns" array with StripsPerImage = 1.
    #[test]
    fn encode_planar_emits_three_strips_and_config_2() {
        let pixels = pattern_rgb(16, 8);
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: true,
        };
        let bytes = encode_tiff(&page).unwrap();

        // Walk the IFD by hand (II classic TIFF).
        assert_eq!(&bytes[0..2], b"II");
        let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
        let mut planar_cfg = None;
        let mut strip_offsets_count = None;
        let mut strip_byte_counts_count = None;
        for k in 0..count {
            let e = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
            let cnt = u32::from_le_bytes([bytes[e + 4], bytes[e + 5], bytes[e + 6], bytes[e + 7]]);
            match tag {
                TAG_PLANAR_CONFIGURATION => {
                    planar_cfg = Some(u16::from_le_bytes([bytes[e + 8], bytes[e + 9]]));
                }
                TAG_STRIP_OFFSETS => strip_offsets_count = Some(cnt),
                TAG_STRIP_BYTE_COUNTS => strip_byte_counts_count = Some(cnt),
                _ => {}
            }
        }
        assert_eq!(planar_cfg, Some(PLANAR_SEPARATE));
        assert_eq!(strip_offsets_count, Some(3));
        assert_eq!(strip_byte_counts_count, Some(3));
    }

    /// `planar = true` requires a multi-sample format; the single-sample
    /// formats (where the spec says PlanarConfiguration is irrelevant)
    /// must be rejected with a precise error rather than silently
    /// emitting a meaningless `PlanarConfiguration = 2`.
    #[test]
    fn encode_planar_rejects_single_sample_formats() {
        let g = ramp_gray8(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &g },
            compression: TiffCompression::None,
            predictor: false,
            planar: true,
        };
        assert!(encode_tiff(&page).is_err());

        let palette = vec![[0u8, 0, 0], [255, 255, 255]];
        let indices = vec![0u8; 64];
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Palette8 {
                indices: &indices,
                palette: &palette,
            },
            compression: TiffCompression::None,
            predictor: false,
            planar: true,
        };
        assert!(encode_tiff(&page).is_err());
    }

    /// Chunky output stays single-strip (PlanarConfiguration = 1) when
    /// `planar` is off — the planar refactor must not regress the
    /// default layout.
    #[test]
    fn encode_chunky_still_single_strip_config_1() {
        let pixels = pattern_rgb(12, 6);
        let page = EncodePage {
            width: 12,
            height: 6,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
        for k in 0..count {
            let e = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
            let cnt = u32::from_le_bytes([bytes[e + 4], bytes[e + 5], bytes[e + 6], bytes[e + 7]]);
            if tag == TAG_PLANAR_CONFIGURATION {
                assert_eq!(
                    u16::from_le_bytes([bytes[e + 8], bytes[e + 9]]),
                    PLANAR_CHUNKY
                );
            }
            if tag == TAG_STRIP_OFFSETS || tag == TAG_STRIP_BYTE_COUNTS {
                assert_eq!(cnt, 1);
            }
        }
    }
}
