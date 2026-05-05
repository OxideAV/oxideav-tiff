//! TIFF 6.0 encoder — single-IFD or multi-page (multi-IFD chain),
//! little-endian on-disk byte order ("II"), classic 32-bit offsets.
//!
//! The encoder targets the same baseline our decoder reads:
//!
//! * Photometric: BlackIsZero (greyscale 8/16-bit), RGB (8-bit),
//!   Palette (8-bit indexed)
//! * Compression: 1 None, 32773 PackBits, 5 LZW, 8 Deflate
//! * Strip layout (single strip per page)
//! * Single-IFD or multi-IFD chain via [`encode_tiff_multi`]
//!
//! BigTIFF write, tile write, CCITT and JPEG-in-TIFF compression,
//! YCbCr / CIELab / CMYK output, and predictor encoding are
//! intentionally out of scope for this round.

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
}

/// Pixel layouts the encoder knows how to write.
#[derive(Debug, Clone)]
pub enum EncodePixelFormat<'a> {
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
}

impl TiffCompression {
    fn tag_value(self) -> u16 {
        match self {
            TiffCompression::None => COMPRESSION_NONE,
            TiffCompression::PackBits => COMPRESSION_PACKBITS,
            TiffCompression::Lzw => COMPRESSION_LZW,
            TiffCompression::Deflate => COMPRESSION_DEFLATE_ADOBE,
        }
    }

    fn pack(self, raw: &[u8]) -> Vec<u8> {
        match self {
            TiffCompression::None => raw.to_vec(),
            TiffCompression::PackBits => pack_packbits(raw),
            TiffCompression::Lzw => pack_lzw(raw),
            TiffCompression::Deflate => pack_deflate(raw),
        }
    }
}

/// One planned page with compressed bytes, IFD, and any external
/// blobs (BitsPerSample / ColorMap arrays that don't fit inline).
struct PlannedPage {
    compressed: Vec<u8>,
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
    // [compressed strip][external blobs][IFD].
    //
    // We need to know the IFD offset *before* writing it (it goes in
    // the previous IFD's next-IFD slot or in the header for the
    // first page), so we walk pages once to assign byte ranges.
    let mut cursor: u64 = 8;
    struct PlannedPageAddr {
        strip_offset: u64,
        strip_size: u64,
        externals: Vec<(BlobId, u64, u64)>, // (id, offset, size)
        ifd_offset: u64,
    }
    let mut addrs: Vec<PlannedPageAddr> = Vec::with_capacity(planned.len());
    for plan in &planned {
        let strip_offset = cursor;
        let strip_size = plan.compressed.len() as u64;
        cursor += strip_size;
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
        // IFD must be 2-byte aligned (entries are u16/u32 mixes).
        if cursor % 2 != 0 {
            cursor += 1;
        }
        let ifd_offset = cursor;
        // count(2) + entries × 12 + next_ifd(4)
        let ifd_size = 2 + (plan.ifd.entries.len() as u64) * 12 + 4;
        cursor += ifd_size;
        addrs.push(PlannedPageAddr {
            strip_offset,
            strip_size,
            externals: ext_addrs,
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
        // Strip payload.
        out[addr.strip_offset as usize..(addr.strip_offset + addr.strip_size) as usize]
            .copy_from_slice(&plan.compressed);
        // External blobs.
        for (j, (id, off, size)) in addr.externals.iter().enumerate() {
            assert_eq!(*id, plan.externals[j].0);
            let blob = &plan.externals[j].1;
            assert_eq!(blob.len() as u64, *size);
            out[*off as usize..(*off + *size) as usize].copy_from_slice(blob);
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
                IfdValue::StripOffset => {
                    slot.copy_from_slice(&(addr.strip_offset as u32).to_le_bytes());
                }
                IfdValue::StripByteCount => {
                    slot.copy_from_slice(&(addr.strip_size as u32).to_le_bytes());
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
    /// Fixed value resolved at write time: offset of this page's
    /// strip data.
    StripOffset,
    /// Fixed value resolved at write time: size of this page's
    /// compressed strip.
    StripByteCount,
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
    let (samples_per_pixel, bits_per_sample, photometric, raw_pixels, color_map_words) =
        match &p.kind {
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
                    // intensity). libtiff and ImageMagick use the
                    // canonical (v << 8) | v expansion so a 0xFF
                    // 8-bit value reads back as 0xFFFF.
                    cm[i] = ((c[0] as u16) << 8) | c[0] as u16;
                    cm[256 + i] = ((c[1] as u16) << 8) | c[1] as u16;
                    cm[512 + i] = ((c[2] as u16) << 8) | c[2] as u16;
                }
                (1u16, vec![8u16], PHOTO_PALETTE, indices.to_vec(), Some(cm))
            }
        };

    let compressed = p.compression.pack(&raw_pixels);

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
    // 273 StripOffsets (LONG, count=1)
    entries.push(PageIfdEntry {
        tag: TAG_STRIP_OFFSETS,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::StripOffset,
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
    // 279 StripByteCounts (LONG, count=1)
    entries.push(PageIfdEntry {
        tag: TAG_STRIP_BYTE_COUNTS,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::StripByteCount,
    });
    // 284 PlanarConfiguration (SHORT) = 1 (chunky)
    entries.push(PageIfdEntry {
        tag: TAG_PLANAR_CONFIGURATION,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(PLANAR_CHUNKY.to_le_bytes().to_vec()),
    });
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
    // 278/279/284/320), but assert defensively.
    debug_assert!(entries.windows(2).all(|w| w[0].tag <= w[1].tag));

    Ok(PlannedPage {
        compressed,
        ifd: PageIfd { entries },
        externals,
    })
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
            },
            EncodePage {
                width: 8,
                height: 8,
                kind: EncodePixelFormat::Rgb24 { pixels: &p2 },
                compression: TiffCompression::Lzw,
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
}
