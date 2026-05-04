//! High-level TIFF 6.0 decode: parse the header + first IFD,
//! decompress every strip, assemble the image, apply the predictor
//! if any, expand palette / bilevel / 16-bit pixels into one of our
//! standard `TiffPixelFormat`s.

use crate::compress::{unpack_deflate, unpack_lzw, unpack_packbits};
use crate::error::{Result, TiffError as Error};
use crate::ifd::{find, parse_header, parse_ifd, ByteOrder};
use crate::image::{TiffImage, TiffPixelFormat, TiffPlane};
use crate::types::*;

/// Outcome of a successful decode: the image plus the resolved pixel
/// format and dimensions (handy for tests / containers).
///
/// Identical in shape to [`TiffImage`] — kept as a distinct alias so
/// the historical `DecodedTiff { frame, width, height, pixel_format }`
/// shape stays available to callers.
pub struct DecodedTiff {
    pub frame: TiffImage,
    pub width: u32,
    pub height: u32,
    pub pixel_format: TiffPixelFormat,
}

/// Decode a complete TIFF file into a [`TiffImage`] + metadata.
pub fn decode_tiff(input: &[u8]) -> Result<DecodedTiff> {
    let header = parse_header(input)?;
    let bo = header.byte_order;
    let (entries, _next_ifd) = parse_ifd(input, bo, header.first_ifd_offset)?;

    // ---- Mandatory tags ----
    let width = find(&entries, TAG_IMAGE_WIDTH)
        .ok_or_else(|| Error::invalid("TIFF: missing ImageWidth"))?
        .as_u32(bo)?;
    let height = find(&entries, TAG_IMAGE_LENGTH)
        .ok_or_else(|| Error::invalid("TIFF: missing ImageLength"))?
        .as_u32(bo)?;
    if width == 0 || height == 0 {
        return Err(Error::invalid("TIFF: zero dimension"));
    }

    let compression = find(&entries, TAG_COMPRESSION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(COMPRESSION_NONE as u32) as u16;
    let photometric = find(&entries, TAG_PHOTOMETRIC_INTERPRETATION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .ok_or_else(|| Error::invalid("TIFF: missing PhotometricInterpretation"))?
        as u16;
    let samples_per_pixel = find(&entries, TAG_SAMPLES_PER_PIXEL)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(1) as u16;
    let bits_per_sample =
        decode_bits_per_sample(find(&entries, TAG_BITS_PER_SAMPLE), bo, samples_per_pixel)?;
    let planar = find(&entries, TAG_PLANAR_CONFIGURATION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(PLANAR_CHUNKY as u32) as u16;
    if planar != PLANAR_CHUNKY {
        return Err(Error::invalid(
            "TIFF: PlanarConfiguration=2 (separate planes) not yet supported",
        ));
    }

    let predictor = find(&entries, TAG_PREDICTOR)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(PREDICTOR_NONE as u32) as u16;
    if predictor != PREDICTOR_NONE && predictor != PREDICTOR_HORIZONTAL {
        return Err(Error::invalid(format!(
            "TIFF: Predictor={predictor} not supported"
        )));
    }

    let rows_per_strip = find(&entries, TAG_ROWS_PER_STRIP)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(height); // default per spec is "the entire image is one strip"

    let strip_offsets = find(&entries, TAG_STRIP_OFFSETS)
        .ok_or_else(|| Error::invalid("TIFF: missing StripOffsets"))?
        .as_u32_vec(bo)?;
    let strip_byte_counts = find(&entries, TAG_STRIP_BYTE_COUNTS)
        .ok_or_else(|| Error::invalid("TIFF: missing StripByteCounts"))?
        .as_u32_vec(bo)?;
    if strip_offsets.len() != strip_byte_counts.len() {
        return Err(Error::invalid(
            "TIFF: StripOffsets / StripByteCounts length mismatch",
        ));
    }

    // ---- Decode all strips into one packed-row buffer ----
    let bps_first = bits_per_sample[0];
    if !bits_per_sample.iter().all(|&b| b == bps_first) {
        return Err(Error::invalid(
            "TIFF: per-channel BitsPerSample must be uniform in this build",
        ));
    }
    if bps_first != 1 && bps_first != 4 && bps_first != 8 && bps_first != 16 {
        return Err(Error::invalid(format!(
            "TIFF: BitsPerSample={bps_first} not supported"
        )));
    }

    // Stride of one decompressed row, in bytes. Per spec, rows are
    // packed tightly into bytes (Compression=1 caveat: each row is
    // padded to the next byte boundary).
    let bits_per_row = (width as u64) * (samples_per_pixel as u64) * (bps_first as u64);
    let row_bytes = bits_per_row.div_ceil(8) as usize;

    let mut pixel_buf: Vec<u8> = Vec::with_capacity(row_bytes * height as usize);
    let mut rows_done: u32 = 0;
    for (i, (&offset, &byte_count)) in strip_offsets
        .iter()
        .zip(strip_byte_counts.iter())
        .enumerate()
    {
        let start = offset as usize;
        let end = start
            .checked_add(byte_count as usize)
            .ok_or_else(|| Error::invalid(format!("TIFF: strip {i} length overflow")))?;
        if end > input.len() {
            return Err(Error::invalid(format!("TIFF: strip {i} extends past EOF")));
        }
        let raw = &input[start..end];
        let rows_this_strip = rows_per_strip.min(height - rows_done);
        let expected = row_bytes * rows_this_strip as usize;
        let decompressed = match compression {
            COMPRESSION_NONE => raw.to_vec(),
            COMPRESSION_PACKBITS => unpack_packbits(raw, expected)?,
            COMPRESSION_LZW => unpack_lzw(raw, expected)?,
            COMPRESSION_DEFLATE_ADOBE => unpack_deflate(raw, expected)?,
            other => {
                return Err(Error::invalid(format!(
                    "TIFF: Compression={other} not supported"
                )))
            }
        };
        if decompressed.len() < expected {
            return Err(Error::invalid(format!(
                "TIFF: strip {i} short after decompress: got {} bytes, expected {}",
                decompressed.len(),
                expected
            )));
        }
        // Apply predictor per-row if requested.
        let mut strip = decompressed[..expected].to_vec();
        if predictor == PREDICTOR_HORIZONTAL {
            apply_horizontal_predictor(
                &mut strip,
                width as usize,
                rows_this_strip as usize,
                samples_per_pixel as usize,
                bps_first as usize,
                row_bytes,
                bo,
            )?;
        }
        pixel_buf.extend_from_slice(&strip);
        rows_done += rows_this_strip;
        if rows_done >= height {
            break;
        }
    }
    if rows_done < height {
        return Err(Error::invalid("TIFF: strips did not cover full image"));
    }

    // ---- Convert into a standard TiffPixelFormat ----
    let (image, pf) = match (photometric, samples_per_pixel, bps_first) {
        (PHOTO_BLACK_IS_ZERO, 1, 1) | (PHOTO_WHITE_IS_ZERO, 1, 1) => {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            (
                build_gray8_from_1bpp(&pixel_buf, width, height, row_bytes, inv),
                TiffPixelFormat::Gray8,
            )
        }
        (PHOTO_BLACK_IS_ZERO, 1, 4) | (PHOTO_WHITE_IS_ZERO, 1, 4) => {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            (
                build_gray8_from_4bpp(&pixel_buf, width, height, row_bytes, inv),
                TiffPixelFormat::Gray8,
            )
        }
        (PHOTO_BLACK_IS_ZERO, 1, 8) | (PHOTO_WHITE_IS_ZERO, 1, 8) => {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            (
                build_gray8(&pixel_buf, width, height, inv),
                TiffPixelFormat::Gray8,
            )
        }
        (PHOTO_BLACK_IS_ZERO, 1, 16) | (PHOTO_WHITE_IS_ZERO, 1, 16) => {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            (
                build_gray16le(&pixel_buf, width, height, bo, inv),
                TiffPixelFormat::Gray16Le,
            )
        }
        (PHOTO_RGB, 3, 8) => (
            build_rgb24(&pixel_buf, width, height),
            TiffPixelFormat::Rgb24,
        ),
        (PHOTO_RGB, 3, 16) => (
            build_rgb48le(&pixel_buf, width, height, bo),
            TiffPixelFormat::Rgb48Le,
        ),
        (PHOTO_RGB, n, 8) if n >= 4 => {
            // Skip extra samples (e.g. RGBA where we don't expose Rgba32 here).
            (
                build_rgb_from_n_chunky_8bit(&pixel_buf, width, height, n as usize),
                TiffPixelFormat::Rgb24,
            )
        }
        (PHOTO_PALETTE, 1, b @ (4 | 8)) => {
            let cm = find(&entries, TAG_COLOR_MAP)
                .ok_or_else(|| Error::invalid("TIFF: palette image missing ColorMap"))?
                .as_u32_vec(bo)?;
            let palette = parse_colormap(&cm, b)?;
            (
                build_rgb24_from_palette(&pixel_buf, width, height, &palette, b, row_bytes),
                TiffPixelFormat::Rgb24,
            )
        }
        (p, s, b) => {
            return Err(Error::invalid(format!(
                "TIFF: photometric={p} samples_per_pixel={s} bits_per_sample={b} not supported"
            )))
        }
    };

    Ok(DecodedTiff {
        frame: TiffImage {
            width,
            height,
            pixel_format: pf,
            planes: image.planes,
        },
        width,
        height,
        pixel_format: pf,
    })
}

fn decode_bits_per_sample(
    entry: Option<&crate::ifd::Entry>,
    bo: ByteOrder,
    spp: u16,
) -> Result<Vec<u16>> {
    match entry {
        None => Ok(vec![1; spp as usize]), // default per spec
        Some(e) => {
            if e.count as u16 != spp {
                return Err(Error::invalid(format!(
                    "TIFF: BitsPerSample count {} != SamplesPerPixel {}",
                    e.count, spp
                )));
            }
            let v = e.as_u32_vec(bo)?;
            Ok(v.into_iter().map(|b| b as u16).collect())
        }
    }
}

/// Spec Section 14: subtract previous pixel of the same component.
/// Implementation note: when `SamplesPerPixel > 1`, the offset
/// between the source and the destination is `SamplesPerPixel`
/// components, NOT 1; per-component differencing is what compressors
/// and decompressors do (so red is differenced against red, green
/// against green, etc.).
fn apply_horizontal_predictor(
    buf: &mut [u8],
    width: usize,
    rows: usize,
    samples: usize,
    bps: usize,
    row_bytes: usize,
    bo: ByteOrder,
) -> Result<()> {
    if width == 0 || rows == 0 {
        return Ok(());
    }
    match bps {
        8 => {
            for r in 0..rows {
                let row = &mut buf[r * row_bytes..r * row_bytes + width * samples];
                for x in samples..(width * samples) {
                    row[x] = row[x].wrapping_add(row[x - samples]);
                }
            }
        }
        16 => {
            for r in 0..rows {
                let row = &mut buf[r * row_bytes..r * row_bytes + width * samples * 2];
                let pixels = width * samples;
                // Convert in place. Read using current byte order
                // (in-file), accumulate, write back same way.
                for x in samples..pixels {
                    let cur_off = x * 2;
                    let prev_off = (x - samples) * 2;
                    let cur = bo.read_u16(&row[cur_off..cur_off + 2]);
                    let prev = bo.read_u16(&row[prev_off..prev_off + 2]);
                    let new = cur.wrapping_add(prev);
                    let bytes = match bo {
                        ByteOrder::Little => new.to_le_bytes(),
                        ByteOrder::Big => new.to_be_bytes(),
                    };
                    row[cur_off] = bytes[0];
                    row[cur_off + 1] = bytes[1];
                }
            }
        }
        4 => {
            // Spec Section 14: expand to 8-bit, difference, repack.
            for r in 0..rows {
                let row_off = r * row_bytes;
                // Decode row to 8-bit per nibble.
                let mut tmp: Vec<u8> = Vec::with_capacity(width * samples);
                for x in 0..(width * samples) {
                    let byte = buf[row_off + x / 2];
                    let n = if x & 1 == 0 { byte >> 4 } else { byte & 0x0F };
                    tmp.push(n);
                }
                for x in samples..(width * samples) {
                    tmp[x] = tmp[x].wrapping_add(tmp[x - samples]) & 0x0F;
                }
                // Repack.
                for (x, b) in tmp.iter().enumerate() {
                    let off = row_off + x / 2;
                    if x & 1 == 0 {
                        buf[off] = (buf[off] & 0x0F) | ((b & 0x0F) << 4);
                    } else {
                        buf[off] = (buf[off] & 0xF0) | (b & 0x0F);
                    }
                }
            }
        }
        _ => {
            return Err(Error::invalid(format!(
                "TIFF: predictor at bits_per_sample={bps} unsupported"
            )))
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pixel-format conversions
// ---------------------------------------------------------------------------

fn build_gray8(src: &[u8], w: u32, h: u32, invert: bool) -> TiffImage {
    let stride = w as usize;
    let mut data = src[..stride * h as usize].to_vec();
    if invert {
        for b in data.iter_mut() {
            *b = 255 - *b;
        }
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Gray8,
        planes: vec![TiffPlane { stride, data }],
    }
}

fn build_gray8_from_4bpp(src: &[u8], w: u32, h: u32, row_bytes: usize, invert: bool) -> TiffImage {
    let stride = w as usize;
    let mut data = Vec::with_capacity(stride * h as usize);
    for y in 0..h as usize {
        let row = &src[y * row_bytes..y * row_bytes + row_bytes];
        for x in 0..w as usize {
            let byte = row[x / 2];
            let n = if x & 1 == 0 { byte >> 4 } else { byte & 0x0F };
            // Scale 4-bit into 8-bit: replicate nibble (0xF -> 0xFF).
            let v = (n << 4) | n;
            data.push(if invert { 255 - v } else { v });
        }
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Gray8,
        planes: vec![TiffPlane { stride, data }],
    }
}

fn build_gray8_from_1bpp(src: &[u8], w: u32, h: u32, row_bytes: usize, invert: bool) -> TiffImage {
    let stride = w as usize;
    let mut data = Vec::with_capacity(stride * h as usize);
    for y in 0..h as usize {
        let row = &src[y * row_bytes..y * row_bytes + row_bytes];
        for x in 0..w as usize {
            let byte = row[x / 8];
            let bit = (byte >> (7 - (x % 8))) & 1;
            // BlackIsZero: 0=black=0, 1=white=255. Invert flips.
            let v = if bit == 1 { 255 } else { 0 };
            data.push(if invert { 255 - v } else { v });
        }
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Gray8,
        planes: vec![TiffPlane { stride, data }],
    }
}

fn build_gray16le(src: &[u8], w: u32, h: u32, bo: ByteOrder, invert: bool) -> TiffImage {
    let stride = w as usize * 2;
    let n = (w * h) as usize;
    let mut data = Vec::with_capacity(stride * h as usize);
    for i in 0..n {
        let v = bo.read_u16(&src[i * 2..i * 2 + 2]);
        let v = if invert { 0xFFFF - v } else { v };
        data.extend_from_slice(&v.to_le_bytes());
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Gray16Le,
        planes: vec![TiffPlane { stride, data }],
    }
}

fn build_rgb24(src: &[u8], w: u32, h: u32) -> TiffImage {
    let stride = w as usize * 3;
    let data = src[..stride * h as usize].to_vec();
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Rgb24,
        planes: vec![TiffPlane { stride, data }],
    }
}

fn build_rgb_from_n_chunky_8bit(src: &[u8], w: u32, h: u32, n: usize) -> TiffImage {
    let stride = w as usize * 3;
    let mut data = Vec::with_capacity(stride * h as usize);
    for y in 0..h as usize {
        let row = &src[y * w as usize * n..(y + 1) * w as usize * n];
        for x in 0..w as usize {
            data.push(row[x * n]);
            data.push(row[x * n + 1]);
            data.push(row[x * n + 2]);
        }
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Rgb24,
        planes: vec![TiffPlane { stride, data }],
    }
}

fn build_rgb48le(src: &[u8], w: u32, h: u32, bo: ByteOrder) -> TiffImage {
    let stride = w as usize * 6;
    let pixels = (w * h) as usize;
    let mut data = Vec::with_capacity(stride * h as usize);
    for i in 0..pixels {
        let off = i * 6;
        for c in 0..3 {
            let v = bo.read_u16(&src[off + c * 2..off + c * 2 + 2]);
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Rgb48Le,
        planes: vec![TiffPlane { stride, data }],
    }
}

fn parse_colormap(words: &[u32], bps: u16) -> Result<Vec<[u8; 3]>> {
    // ColorMap: 3 * 2^BitsPerSample SHORTs. All red first, then
    // green, then blue. 0..=65535 represent the channel intensity.
    let entries = 1usize << bps;
    if words.len() < 3 * entries {
        return Err(Error::invalid(format!(
            "TIFF: ColorMap has {} entries, expected {}",
            words.len(),
            3 * entries
        )));
    }
    let mut out = Vec::with_capacity(entries);
    for i in 0..entries {
        let r = (words[i] >> 8) as u8;
        let g = (words[entries + i] >> 8) as u8;
        let b = (words[2 * entries + i] >> 8) as u8;
        out.push([r, g, b]);
    }
    Ok(out)
}

fn build_rgb24_from_palette(
    src: &[u8],
    w: u32,
    h: u32,
    palette: &[[u8; 3]],
    bps: u16,
    row_bytes: usize,
) -> TiffImage {
    let stride = w as usize * 3;
    let mut data = Vec::with_capacity(stride * h as usize);
    for y in 0..h as usize {
        let row = &src[y * row_bytes..y * row_bytes + row_bytes];
        for x in 0..w as usize {
            let idx = match bps {
                8 => row[x] as usize,
                4 => {
                    let byte = row[x / 2];
                    (if x & 1 == 0 { byte >> 4 } else { byte & 0x0F }) as usize
                }
                _ => 0,
            };
            let p = palette.get(idx).copied().unwrap_or([0, 0, 0]);
            data.push(p[0]);
            data.push(p[1]);
            data.push(p[2]);
        }
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Rgb24,
        planes: vec![TiffPlane { stride, data }],
    }
}
