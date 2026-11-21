//! General integer-sample path.
//!
//! The fast paths in [`crate::decoder`] cover the layouts whose samples
//! are whole bytes in a shape one of the crate's packed formats stores
//! verbatim. Everything else an integer TIFF can declare — any
//! `BitsPerSample` from 1 to 64, signed samples, extra samples next to
//! any colour model, sub-byte samples in separate planes — is read
//! here: every segment (strip or tile, per plane for
//! `PlanarConfiguration = 2`) is decompressed, its rows unpacked to one
//! integer per sample, the TIFF 6.0 §14 horizontal differencing undone
//! on those integers, and the visible part stored in a chunky
//! `width × height × SamplesPerPixel` sample matrix. [`build_image`]
//! then maps the matrix onto the nearest native layout.

use crate::decoder::decompress_bytes;
use crate::error::{Error, Result};
use crate::ifd::{ByteOrder, Entry, find};
use crate::image::{Palette, Plane, TiffImage, TiffPixelFormat};
use crate::types::*;

/// How a general integer page maps onto a native layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenericLayout {
    /// Colour components the photometric defines (1, 3 or 4).
    pub color: usize,
    /// Sample index of the alpha component and whether it is
    /// associated (pre-multiplied) — the first §ExtraSamples entry of
    /// kind 1 or 2.
    pub alpha: Option<(usize, bool)>,
    /// Layout handed back.
    pub out: TiffPixelFormat,
}

/// Decide whether (and as what) the general path decodes a page.
/// `None` means the combination has no rule here.
pub(crate) fn classify(
    photometric: u16,
    spp: u16,
    bps: u16,
    sample_format: u16,
    extras: &[u16],
) -> Option<GenericLayout> {
    if !(1..=64).contains(&bps) {
        return None;
    }
    if sample_format != SAMPLE_FORMAT_UINT && sample_format != SAMPLE_FORMAT_SINT {
        return None;
    }
    let color = match photometric {
        PHOTO_WHITE_IS_ZERO | PHOTO_BLACK_IS_ZERO | PHOTO_PALETTE => 1usize,
        PHOTO_RGB => 3,
        PHOTO_CMYK => 4,
        _ => return None,
    };
    if (spp as usize) < color {
        return None;
    }
    let alpha = extras
        .iter()
        .take(spp as usize - color)
        .position(|&k| k == EXTRA_SAMPLE_ASSOCIATED_ALPHA || k == EXTRA_SAMPLE_UNASSOCIATED_ALPHA)
        .map(|i| (color + i, extras[i] == EXTRA_SAMPLE_ASSOCIATED_ALPHA));
    let deep = bps > 8;
    let out = match photometric {
        PHOTO_PALETTE => {
            // A ColorMap index is not an intensity: only unsigned
            // indices that fit the 8-bit `Pal8` layout decode.
            if deep || sample_format != SAMPLE_FORMAT_UINT {
                return None;
            }
            TiffPixelFormat::Pal8
        }
        PHOTO_CMYK => TiffPixelFormat::Cmyk,
        PHOTO_RGB => match (deep, alpha.is_some()) {
            (false, false) => TiffPixelFormat::Rgb24,
            (false, true) => TiffPixelFormat::Rgba,
            (true, false) => TiffPixelFormat::Rgb48Le,
            (true, true) => TiffPixelFormat::Rgba64Le,
        },
        _ => match (deep, alpha.is_some()) {
            (false, false) => TiffPixelFormat::Gray8,
            (false, true) => TiffPixelFormat::Ya8,
            (true, false) => TiffPixelFormat::Gray16Le,
            (true, true) => TiffPixelFormat::Ya16Le,
        },
    };
    // Palette and CMYK pages hand back no alpha plane.
    let alpha = match out {
        TiffPixelFormat::Pal8 | TiffPixelFormat::Cmyk => None,
        _ => alpha,
    };
    Some(GenericLayout { color, alpha, out })
}

/// All-ones mask of `bps` bits.
#[inline]
fn mask_of(bps: u32) -> u64 {
    if bps >= 64 {
        u64::MAX
    } else {
        (1u64 << bps) - 1
    }
}

/// Unpack `out.len()` samples of `bps` bits (1..=64) from one
/// byte-aligned row. Whole-byte samples (8, 16, 24, … 64 bits) are
/// integers in the file's byte order; every other width is a
/// most-significant-bit-first bit string (TIFF 6.0 §"Compression"
/// value 1: rows are packed "as tightly as possible" and start on a
/// byte boundary).
pub(crate) fn unpack_row(row: &[u8], bps: u32, bo: ByteOrder, out: &mut [u64]) {
    match bps {
        8 => {
            for (o, &b) in out.iter_mut().zip(row) {
                *o = b as u64;
            }
        }
        16 => {
            for (o, c) in out.iter_mut().zip(row.as_chunks::<2>().0.iter()) {
                *o = bo.read_u16(c) as u64;
            }
        }
        32 => {
            for (o, c) in out.iter_mut().zip(row.as_chunks::<4>().0.iter()) {
                *o = bo.read_u32(c) as u64;
            }
        }
        b if b.is_multiple_of(8) => {
            let n = (b / 8) as usize;
            for (o, c) in out.iter_mut().zip(row.chunks_exact(n)) {
                *o = match bo {
                    ByteOrder::Big => c.iter().fold(0u64, |v, &x| v << 8 | x as u64),
                    ByteOrder::Little => c.iter().rev().fold(0u64, |v, &x| v << 8 | x as u64),
                };
            }
        }
        _ => {
            let mut acc: u128 = 0;
            let mut have: u32 = 0;
            let mut bytes = row.iter();
            let mask = mask_of(bps) as u128;
            for o in out.iter_mut() {
                while have < bps {
                    acc = (acc << 8) | *bytes.next().unwrap_or(&0) as u128;
                    have += 8;
                }
                have -= bps;
                *o = ((acc >> have) & mask) as u64;
            }
        }
    }
}

/// Inverse of [`unpack_row`]: pack `vals` into `row`.
pub(crate) fn pack_row(vals: &[u64], bps: u32, bo: ByteOrder, row: &mut [u8]) {
    if bps.is_multiple_of(8) {
        let n = (bps / 8) as usize;
        for (c, &v) in row.chunks_exact_mut(n).zip(vals) {
            let be = v.to_be_bytes();
            match bo {
                ByteOrder::Big => c.copy_from_slice(&be[8 - n..]),
                ByteOrder::Little => {
                    for (dst, src) in c.iter_mut().zip(be[8 - n..].iter().rev()) {
                        *dst = *src;
                    }
                }
            }
        }
        return;
    }
    let mut acc: u128 = 0;
    let mut have: u32 = 0;
    let mut at = 0usize;
    for &v in vals {
        acc = (acc << bps) | (v & mask_of(bps)) as u128;
        have += bps;
        while have >= 8 {
            have -= 8;
            row[at] = (acc >> have) as u8;
            at += 1;
        }
    }
    if have > 0 {
        row[at] = ((acc << (8 - have)) & 0xFF) as u8;
    }
}

/// Undo TIFF 6.0 §14 horizontal differencing on one unpacked row:
/// every sample is the modular sum of itself and the sample `comps`
/// positions earlier ("an offset of SamplesPerPixel"), the overflow
/// bits ignored.
pub(crate) fn undo_differencing(vals: &mut [u64], comps: usize, bps: u32) {
    let mask = mask_of(bps);
    for i in comps..vals.len() {
        vals[i] = vals[i].wrapping_add(vals[i - comps]) & mask;
    }
}

/// Byte length of a row of `samples` samples at `bps` bits.
pub(crate) fn row_len(samples: usize, bps: u32) -> usize {
    (samples * bps as usize).div_ceil(8)
}

/// Width of the values [`read_samples`] hands back: the sample width,
/// capped at 32 bits (a wider sample keeps its top 32 bits).
pub(crate) fn matrix_bits(bps: u16) -> u16 {
    bps.min(32)
}

/// Read every sample of an integer page into a chunky
/// `width × height × spp` matrix of raw (unscaled) values, each
/// [`matrix_bits`] wide.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_samples(
    input: &[u8],
    entries: &[Entry],
    bo: ByteOrder,
    width: u32,
    height: u32,
    spp: u16,
    bps: u16,
    compression: u16,
    predictor: u16,
    planar: bool,
    lsb_first: bool,
) -> Result<Vec<u32>> {
    if !matches!(
        compression,
        COMPRESSION_NONE
            | COMPRESSION_PACKBITS
            | COMPRESSION_LZW
            | COMPRESSION_DEFLATE_ADOBE
            | COMPRESSION_DEFLATE_LEGACY
            | COMPRESSION_ZSTD
    ) {
        return Err(Error::Unsupported(format!(
            "TIFF: Compression={compression} with BitsPerSample={bps} / \
             SamplesPerPixel={spp} is not supported"
        )));
    }
    if predictor != PREDICTOR_NONE && predictor != PREDICTOR_HORIZONTAL {
        return Err(Error::Unsupported(format!(
            "TIFF: Predictor={predictor} over integer samples is not defined"
        )));
    }
    let (w, h, spp_n, bps) = (width as usize, height as usize, spp as usize, bps as u32);
    let tiled = find(entries, TAG_TILE_WIDTH).is_some();
    let (seg_w, seg_h, offsets, counts) = if tiled {
        let tw = find(entries, TAG_TILE_WIDTH)
            .ok_or_else(|| Error::invalid("TIFF: missing TileWidth"))?
            .as_u32(bo)?;
        let tl = find(entries, TAG_TILE_LENGTH)
            .ok_or_else(|| Error::invalid("TIFF: missing TileLength"))?
            .as_u32(bo)?;
        if tw == 0 || tl == 0 {
            return Err(Error::invalid("TIFF: zero tile dimension"));
        }
        let o = find(entries, TAG_TILE_OFFSETS)
            .ok_or_else(|| Error::invalid("TIFF: missing TileOffsets"))?
            .as_u64_vec(bo)?;
        let c = find(entries, TAG_TILE_BYTE_COUNTS)
            .ok_or_else(|| Error::invalid("TIFF: missing TileByteCounts"))?
            .as_u64_vec(bo)?;
        (tw as usize, tl as usize, o, c)
    } else {
        let rps = find(entries, TAG_ROWS_PER_STRIP)
            .map(|e| e.as_u32(bo))
            .transpose()?
            .unwrap_or(height)
            .clamp(1, height);
        let o = find(entries, TAG_STRIP_OFFSETS)
            .ok_or_else(|| Error::invalid("TIFF: missing StripOffsets"))?
            .as_u64_vec(bo)?;
        let c = find(entries, TAG_STRIP_BYTE_COUNTS)
            .ok_or_else(|| Error::invalid("TIFF: missing StripByteCounts"))?
            .as_u64_vec(bo)?;
        (w, rps as usize, o, c)
    };
    if offsets.len() != counts.len() {
        return Err(Error::invalid(
            "TIFF: segment offset / byte-count length mismatch",
        ));
    }
    let across = w.div_ceil(seg_w);
    let down = h.div_ceil(seg_h);
    let planes = if planar { spp_n } else { 1 };
    let comps = if planar { 1 } else { spp_n };
    let per_plane = across
        .checked_mul(down)
        .ok_or_else(|| Error::invalid("TIFF: segment count overflow"))?;
    if offsets.len() < per_plane.saturating_mul(planes) {
        return Err(Error::invalid(format!(
            "TIFF: {} segments present, {} needed",
            offsets.len(),
            per_plane * planes
        )));
    }
    // A bit-packed tile row is byte-padded at the tile's own width.
    let seg_row = row_len(seg_w * comps, bps);
    let mut out = vec![0u32; w * h * spp_n];
    let mut vals = vec![0u64; seg_w * comps];
    // A sample wider than 32 bits keeps its top 32.
    let narrow = bps.saturating_sub(32);
    for plane in 0..planes {
        for sy in 0..down {
            let y0 = sy * seg_h;
            // A strip holds only the rows that remain; a tile is always
            // full size (TIFF 6.0 §15, padded past the image edge).
            let rows = if tiled { seg_h } else { seg_h.min(h - y0) };
            for sx in 0..across {
                let x0 = sx * seg_w;
                let idx = plane * per_plane + sy * across + sx;
                let start = offsets[idx] as usize;
                let end = start
                    .checked_add(counts[idx] as usize)
                    .filter(|&e| e <= input.len())
                    .ok_or_else(|| Error::invalid(format!("TIFF: segment {idx} past EOF")))?;
                let expected = seg_row
                    .checked_mul(rows)
                    .ok_or_else(|| Error::invalid("TIFF: segment size overflow"))?;
                let mut data = decompress_bytes(&input[start..end], expected, compression)?;
                if lsb_first {
                    // FillOrder = 2 (1-bit uncompressed data only): the
                    // pixel order inside every byte is reversed.
                    for b in data.iter_mut() {
                        *b = b.reverse_bits();
                    }
                }
                if data.len() < expected {
                    return Err(Error::invalid(format!(
                        "TIFF: segment {idx} short after decompress: got {} bytes, expected {expected}",
                        data.len()
                    )));
                }
                let vis_w = seg_w.min(w - x0);
                let vis_h = rows.min(h - y0);
                for r in 0..vis_h {
                    unpack_row(&data[r * seg_row..(r + 1) * seg_row], bps, bo, &mut vals);
                    if predictor == PREDICTOR_HORIZONTAL {
                        undo_differencing(&mut vals, comps, bps);
                    }
                    let dst_row = (y0 + r) * w + x0;
                    if planar {
                        for (x, &v) in vals[..vis_w].iter().enumerate() {
                            out[(dst_row + x) * spp_n + plane] = (v >> narrow) as u32;
                        }
                    } else {
                        for (o, &v) in out[dst_row * spp_n..(dst_row + vis_w) * spp_n]
                            .iter_mut()
                            .zip(&vals[..vis_w * spp_n])
                        {
                            *o = (v >> narrow) as u32;
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Widen an `bps`-bit value onto `to` bits (8 or 16) by bit
/// replication, or keep the top `to` bits of a wider one.
#[inline]
fn rescale(v: u32, bps: u32, to: u32) -> u32 {
    if bps == to {
        v
    } else if bps > to {
        v >> (bps - to)
    } else {
        // Left-align, then fill the low bits with the pattern repeated.
        let mut out = v << (to - bps);
        let mut shift = bps;
        while shift < to {
            out |= out >> shift;
            shift *= 2;
        }
        out & ((1u32 << to) - 1)
    }
}

/// Straight colour from a pre-multiplied one (TIFF 6.0 §18:
/// associated alpha stores `colour × alpha`): `c × max / a`, rounded,
/// clamped; a fully transparent pixel has no colour.
#[inline]
fn unassociate(c: u32, a: u32, max: u32) -> u32 {
    if a == 0 {
        0
    } else if a >= max {
        c
    } else {
        (((c as u64) * (max as u64) + (a as u64) / 2) / (a as u64)).min(max as u64) as u32
    }
}

/// Map a raw sample matrix onto the native layout chosen by
/// [`classify`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_image(
    vals: &[u32],
    width: u32,
    height: u32,
    spp: u16,
    bps: u16,
    photometric: u16,
    signed: bool,
    layout: GenericLayout,
    palette: Option<Palette>,
) -> Result<TiffImage> {
    let (spp, bps) = (spp as usize, bps as u32);
    let fmt = layout.out;
    let bpp = fmt.bytes_per_pixel();
    let n = width as usize * height as usize;
    let mut data = Vec::with_capacity(n * bpp);
    let sign_bit = 1u32 << (bps - 1);
    let invert = photometric == PHOTO_WHITE_IS_ZERO;
    if fmt == TiffPixelFormat::Pal8 {
        // Indices are stored verbatim (never scaled).
        data.extend(vals.chunks_exact(spp).map(|px| px[0] as u8));
        let mut img = TiffImage::from_parts(
            width,
            height,
            fmt,
            vec![Plane {
                stride: width as usize,
                data,
            }],
        );
        img.palette = palette;
        return Ok(img);
    }
    let to = if fmt.bits_per_sample() == 16 { 16 } else { 8 };
    let max = (1u32 << to) - 1;
    for px in vals.chunks_exact(spp) {
        let norm = |i: usize| -> u32 {
            let v = if signed { px[i] ^ sign_bit } else { px[i] };
            rescale(v, bps, to)
        };
        let alpha = layout.alpha.map(|(i, assoc)| (norm(i), assoc));
        let mut put = |v: u32| {
            if to == 16 {
                data.extend_from_slice(&(v as u16).to_le_bytes());
            } else {
                data.push(v as u8);
            }
        };
        for c in 0..layout.color {
            let mut v = norm(c);
            if let Some((a, true)) = alpha {
                v = unassociate(v, a, max);
            }
            if invert {
                v = max - v;
            }
            put(v);
        }
        if let Some((a, _)) = alpha {
            put(a);
        }
    }
    Ok(TiffImage::from_parts(
        width,
        height,
        fmt,
        vec![Plane {
            stride: width as usize * bpp,
            data,
        }],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_round_trip_every_width() {
        for bps in 1..=64u32 {
            for bo in [ByteOrder::Little, ByteOrder::Big] {
                let vals: Vec<u64> = (0..37u64)
                    .map(|i| {
                        i.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(i as u32) & mask_of(bps)
                    })
                    .collect();
                let mut row = vec![0u8; row_len(vals.len(), bps)];
                pack_row(&vals, bps, bo, &mut row);
                let mut back = vec![0u64; vals.len()];
                unpack_row(&row, bps, bo, &mut back);
                assert_eq!(back, vals, "bps {bps}");
            }
        }
    }

    #[test]
    fn rescale_hits_both_ends() {
        for bps in 1..=16u32 {
            for to in [8u32, 16] {
                if bps > to {
                    continue;
                }
                assert_eq!(rescale(0, bps, to), 0);
                assert_eq!(rescale((1 << bps) - 1, bps, to), (1 << to) - 1);
            }
        }
        assert_eq!(rescale(0xABCD_1234, 32, 16), 0xABCD);
        assert_eq!(rescale(0b01, 2, 8), 0b0101_0101);
    }

    #[test]
    fn unassociate_edges() {
        assert_eq!(unassociate(100, 0, 255), 0);
        assert_eq!(unassociate(100, 255, 255), 100);
        assert_eq!(unassociate(64, 128, 255), 128);
        assert_eq!(unassociate(200, 100, 255), 255);
    }
}
