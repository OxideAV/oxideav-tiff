//! High-level TIFF 6.0 decode: parse the header + first IFD,
//! decompress every strip, assemble the image, apply the predictor
//! if any, expand palette / bilevel / 16-bit pixels into one of our
//! standard `TiffPixelFormat`s.

use crate::ccitt::{decode_ccitt, CcittVariant, FillOrder};
use crate::compress::{unpack_deflate, unpack_lzw, unpack_packbits};
use crate::error::{Result, TiffError as Error};
use crate::ifd::{find, parse_header, parse_ifd, ByteOrder, Entry};
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

/// Decode the first IFD of a TIFF/BigTIFF file. Multi-page callers
/// should reach for [`decode_tiff_all`] instead.
pub fn decode_tiff(input: &[u8]) -> Result<DecodedTiff> {
    let header = parse_header(input)?;
    let bo = header.byte_order;
    let variant = header.variant;
    let (entries, _next_ifd) = parse_ifd(input, bo, variant, header.first_ifd_offset)?;
    let frame = decode_ifd(input, bo, &entries)?;
    let pf = frame.pixel_format;
    Ok(DecodedTiff {
        width: frame.width,
        height: frame.height,
        pixel_format: pf,
        frame,
    })
}

/// Decode every IFD in the file (all pages of a multi-page TIFF /
/// BigTIFF). Returns one [`TiffImage`] per IFD in file order.
pub fn decode_tiff_all(input: &[u8]) -> Result<Vec<TiffImage>> {
    let header = parse_header(input)?;
    let bo = header.byte_order;
    let variant = header.variant;
    let mut out = Vec::new();
    let mut next = header.first_ifd_offset;
    let mut visited: Vec<u64> = Vec::new();
    while next != 0 {
        // Cycle guard: the next-IFD chain must not loop. Unbounded
        // input is hostile data; check that we haven't already seen
        // this offset.
        if visited.contains(&next) {
            return Err(Error::invalid("TIFF: cyclic next-IFD pointer"));
        }
        visited.push(next);
        let (entries, n) = parse_ifd(input, bo, variant, next)?;
        out.push(decode_ifd(input, bo, &entries)?);
        next = n;
    }
    if out.is_empty() {
        return Err(Error::invalid("TIFF: no IFDs in file"));
    }
    Ok(out)
}

/// Decode one IFD (already parsed into `entries`) into a [`TiffImage`].
fn decode_ifd(input: &[u8], bo: ByteOrder, entries: &[Entry]) -> Result<TiffImage> {
    // ---- Mandatory tags ----
    let width = find(entries, TAG_IMAGE_WIDTH)
        .ok_or_else(|| Error::invalid("TIFF: missing ImageWidth"))?
        .as_u32(bo)?;
    let height = find(entries, TAG_IMAGE_LENGTH)
        .ok_or_else(|| Error::invalid("TIFF: missing ImageLength"))?
        .as_u32(bo)?;
    if width == 0 || height == 0 {
        return Err(Error::invalid("TIFF: zero dimension"));
    }

    let compression = find(entries, TAG_COMPRESSION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(COMPRESSION_NONE as u32) as u16;
    let photometric = find(entries, TAG_PHOTOMETRIC_INTERPRETATION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .ok_or_else(|| Error::invalid("TIFF: missing PhotometricInterpretation"))?
        as u16;
    let samples_per_pixel = find(entries, TAG_SAMPLES_PER_PIXEL)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(1) as u16;
    let bits_per_sample =
        decode_bits_per_sample(find(entries, TAG_BITS_PER_SAMPLE), bo, samples_per_pixel)?;
    let planar = find(entries, TAG_PLANAR_CONFIGURATION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(PLANAR_CHUNKY as u32) as u16;
    if planar != PLANAR_CHUNKY {
        return Err(Error::invalid(
            "TIFF: PlanarConfiguration=2 (separate planes) not yet supported",
        ));
    }

    let predictor = find(entries, TAG_PREDICTOR)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(PREDICTOR_NONE as u32) as u16;
    if predictor != PREDICTOR_NONE && predictor != PREDICTOR_HORIZONTAL {
        return Err(Error::invalid(format!(
            "TIFF: Predictor={predictor} not supported"
        )));
    }

    // ---- Tiles vs. strips ----
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

    // Extract CCITT-relevant IFD fields once; the strip/tile paths
    // use them only when `compression` is 2 or 3.
    let fill_order = find(entries, TAG_FILL_ORDER)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(1) as u16;
    if matches!(
        compression,
        COMPRESSION_CCITT_HUFFMAN | COMPRESSION_CCITT_T4 | COMPRESSION_CCITT_T6
    ) && fill_order != 1
    {
        return Err(Error::invalid(format!(
            "TIFF: FillOrder={fill_order} with CCITT compression not supported (need MSB-first)"
        )));
    }
    let t4_options = find(entries, TAG_T4_OPTIONS)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(0);

    let pixel_buf = if find(entries, TAG_TILE_WIDTH).is_some() {
        decode_tiles(
            input,
            entries,
            bo,
            width,
            height,
            samples_per_pixel,
            bps_first,
            compression,
            predictor,
            t4_options,
        )?
    } else {
        decode_strips(
            input,
            entries,
            bo,
            width,
            height,
            samples_per_pixel,
            bps_first,
            compression,
            predictor,
            t4_options,
        )?
    };

    // Note on CCITT photometric handling: §10 / §11 describe the
    // codec as "self-photometric in terms of white and black runs",
    // and TIFF 6.0 says a BlackIsZero reader must "reverse the
    // meaning of white and black when displaying". In practice
    // libtiff (and the spec's wire format) treats the codec as
    // bit-transparent: the encoder emits white runs for input bits
    // of 0 and black runs for input bits of 1, irrespective of
    // PhotometricInterpretation; the reader returns the raw
    // 1-bit-per-pixel buffer untouched and lets the downstream
    // bilevel-to-Gray8 conversion apply the photometric inversion
    // (which `build_gray8_from_1bpp` already does via its `invert`
    // argument). No extra inversion is needed here.

    // ---- Convert into a standard TiffPixelFormat ----
    let (image, _pf) = match (photometric, samples_per_pixel, bps_first) {
        (PHOTO_BLACK_IS_ZERO, 1, 1) | (PHOTO_WHITE_IS_ZERO, 1, 1) => {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            let row_bytes = ((width as u64).div_ceil(8)) as usize;
            (
                build_gray8_from_1bpp(&pixel_buf, width, height, row_bytes, inv),
                TiffPixelFormat::Gray8,
            )
        }
        (PHOTO_BLACK_IS_ZERO, 1, 4) | (PHOTO_WHITE_IS_ZERO, 1, 4) => {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            let row_bytes = ((width as u64).div_ceil(2)) as usize;
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
            let cm = find(entries, TAG_COLOR_MAP)
                .ok_or_else(|| Error::invalid("TIFF: palette image missing ColorMap"))?
                .as_u32_vec(bo)?;
            let palette = parse_colormap(&cm, b)?;
            let row_bytes = if b == 8 {
                width as usize
            } else {
                ((width as u64).div_ceil(2)) as usize
            };
            (
                build_rgb24_from_palette(&pixel_buf, width, height, &palette, b, row_bytes),
                TiffPixelFormat::Rgb24,
            )
        }
        (PHOTO_CMYK, 4, 8) => (
            build_rgb24_from_cmyk(&pixel_buf, width, height),
            TiffPixelFormat::Rgb24,
        ),
        (PHOTO_YCBCR, 3, 8) => {
            // Subsampling defaults: 2 horizontal / 2 vertical per
            // TIFF 6.0 §22.
            let (sh, sv) = match find(entries, TAG_YCBCR_SUBSAMPLING) {
                Some(e) => {
                    let v = e.as_u32_vec(bo)?;
                    if v.len() < 2 {
                        return Err(Error::invalid("TIFF: YCbCrSubSampling too short"));
                    }
                    (v[0] as u16, v[1] as u16)
                }
                None => (2, 2),
            };
            (
                build_rgb24_from_ycbcr(&pixel_buf, width, height, sh, sv)?,
                TiffPixelFormat::Rgb24,
            )
        }
        (p, s, b) => {
            return Err(Error::invalid(format!(
                "TIFF: photometric={p} samples_per_pixel={s} bits_per_sample={b} not supported"
            )))
        }
    };

    Ok(image)
}

#[allow(clippy::too_many_arguments)]
fn decode_strips(
    input: &[u8],
    entries: &[Entry],
    bo: ByteOrder,
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bps_first: u16,
    compression: u16,
    predictor: u16,
    t4_options: u32,
) -> Result<Vec<u8>> {
    let rows_per_strip = find(entries, TAG_ROWS_PER_STRIP)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(height); // default per spec is "the entire image is one strip"

    let strip_offsets = find(entries, TAG_STRIP_OFFSETS)
        .ok_or_else(|| Error::invalid("TIFF: missing StripOffsets"))?
        .as_u64_vec(bo)?;
    let strip_byte_counts = find(entries, TAG_STRIP_BYTE_COUNTS)
        .ok_or_else(|| Error::invalid("TIFF: missing StripByteCounts"))?
        .as_u64_vec(bo)?;
    if strip_offsets.len() != strip_byte_counts.len() {
        return Err(Error::invalid(
            "TIFF: StripOffsets / StripByteCounts length mismatch",
        ));
    }

    // For sub-byte bit depths, rows are padded to byte boundaries
    // per TIFF 6.0 spec.
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
        let ccitt = if matches!(
            compression,
            COMPRESSION_CCITT_HUFFMAN | COMPRESSION_CCITT_T4 | COMPRESSION_CCITT_T6
        ) {
            Some(CcittParams {
                width,
                rows: rows_this_strip,
                fill: FillOrder::MsbFirst,
                t4_options,
            })
        } else {
            None
        };
        let decompressed = decompress_block(raw, expected, compression, ccitt)?;
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
    Ok(pixel_buf)
}

#[allow(clippy::too_many_arguments)]
fn decode_tiles(
    input: &[u8],
    entries: &[Entry],
    bo: ByteOrder,
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bps_first: u16,
    compression: u16,
    predictor: u16,
    t4_options: u32,
) -> Result<Vec<u8>> {
    let tile_w = find(entries, TAG_TILE_WIDTH)
        .ok_or_else(|| Error::invalid("TIFF: missing TileWidth"))?
        .as_u32(bo)?;
    let tile_h = find(entries, TAG_TILE_LENGTH)
        .ok_or_else(|| Error::invalid("TIFF: missing TileLength"))?
        .as_u32(bo)?;
    if tile_w == 0 || tile_h == 0 {
        return Err(Error::invalid("TIFF: zero tile dimension"));
    }
    let tile_offsets = find(entries, TAG_TILE_OFFSETS)
        .ok_or_else(|| Error::invalid("TIFF: missing TileOffsets"))?
        .as_u64_vec(bo)?;
    let tile_byte_counts = find(entries, TAG_TILE_BYTE_COUNTS)
        .ok_or_else(|| Error::invalid("TIFF: missing TileByteCounts"))?
        .as_u64_vec(bo)?;
    if tile_offsets.len() != tile_byte_counts.len() {
        return Err(Error::invalid(
            "TIFF: TileOffsets / TileByteCounts length mismatch",
        ));
    }

    let tiles_across = (width as u64).div_ceil(tile_w as u64) as u32;
    let tiles_down = (height as u64).div_ceil(tile_h as u64) as u32;
    let expected_tiles = (tiles_across as usize) * (tiles_down as usize);
    if tile_offsets.len() != expected_tiles {
        return Err(Error::invalid(format!(
            "TIFF: TileOffsets length {} != expected {expected_tiles}",
            tile_offsets.len()
        )));
    }

    let bits_per_sample = bps_first as u64;
    let bits_per_tile_row = (tile_w as u64) * (samples_per_pixel as u64) * bits_per_sample;
    let tile_row_bytes = bits_per_tile_row.div_ceil(8) as usize;
    let tile_size_bytes = tile_row_bytes * tile_h as usize;

    let bits_per_image_row = (width as u64) * (samples_per_pixel as u64) * bits_per_sample;
    let image_row_bytes = bits_per_image_row.div_ceil(8) as usize;
    let mut out = vec![0u8; image_row_bytes * height as usize];

    for ty in 0..tiles_down {
        for tx in 0..tiles_across {
            let idx = (ty * tiles_across + tx) as usize;
            let off = tile_offsets[idx] as usize;
            let bc = tile_byte_counts[idx] as usize;
            let end = off
                .checked_add(bc)
                .ok_or_else(|| Error::invalid("TIFF: tile length overflow"))?;
            if end > input.len() {
                return Err(Error::invalid("TIFF: tile extends past EOF"));
            }
            let raw = &input[off..end];
            let ccitt = if matches!(
                compression,
                COMPRESSION_CCITT_HUFFMAN | COMPRESSION_CCITT_T4 | COMPRESSION_CCITT_T6
            ) {
                Some(CcittParams {
                    width: tile_w,
                    rows: tile_h,
                    fill: FillOrder::MsbFirst,
                    t4_options,
                })
            } else {
                None
            };
            let mut tile = decompress_block(raw, tile_size_bytes, compression, ccitt)?;
            if tile.len() < tile_size_bytes {
                return Err(Error::invalid("TIFF: tile short after decompress"));
            }
            tile.truncate(tile_size_bytes);
            if predictor == PREDICTOR_HORIZONTAL {
                apply_horizontal_predictor(
                    &mut tile,
                    tile_w as usize,
                    tile_h as usize,
                    samples_per_pixel as usize,
                    bps_first as usize,
                    tile_row_bytes,
                    bo,
                )?;
            }

            // Copy the visible portion of the tile into the output
            // buffer. Tiles at the right/bottom edge may extend past
            // the image; those samples are simply dropped.
            // Sub-byte bit depths require bit-level slicing, which
            // means we only support tiled images at byte-aligned
            // bit depths in this build.
            if bps_first % 8 != 0 {
                return Err(Error::invalid(
                    "TIFF: tiled images at sub-byte bit depths not yet supported",
                ));
            }
            let sample_bytes = (bps_first / 8) as usize;
            let pixel_bytes = sample_bytes * samples_per_pixel as usize;
            let visible_w =
                ((width as i64) - (tx as i64) * (tile_w as i64)).min(tile_w as i64) as usize;
            let visible_h =
                ((height as i64) - (ty as i64) * (tile_h as i64)).min(tile_h as i64) as usize;
            let visible_row_bytes = visible_w * pixel_bytes;
            let dst_origin_x = tx as usize * tile_w as usize * pixel_bytes;
            let dst_origin_y = ty as usize * tile_h as usize;
            for r in 0..visible_h {
                let src_off = r * tile_row_bytes;
                let dst_off = (dst_origin_y + r) * image_row_bytes + dst_origin_x;
                out[dst_off..dst_off + visible_row_bytes]
                    .copy_from_slice(&tile[src_off..src_off + visible_row_bytes]);
            }
        }
    }

    Ok(out)
}

/// Parameters needed for CCITT compression schemes (2 / 3). Carries
/// the per-block geometry plus the codec-shape options decoded out
/// of the IFD (FillOrder + T4Options).
#[derive(Debug, Clone, Copy)]
struct CcittParams {
    width: u32,
    rows: u32,
    fill: FillOrder,
    t4_options: u32,
}

fn decompress_block(
    raw: &[u8],
    expected: usize,
    compression: u16,
    ccitt: Option<CcittParams>,
) -> Result<Vec<u8>> {
    match compression {
        COMPRESSION_NONE => Ok(raw.to_vec()),
        COMPRESSION_PACKBITS => unpack_packbits(raw, expected),
        COMPRESSION_LZW => unpack_lzw(raw, expected),
        COMPRESSION_DEFLATE_ADOBE => unpack_deflate(raw, expected),
        COMPRESSION_CCITT_HUFFMAN => {
            let p = ccitt
                .ok_or_else(|| Error::invalid("TIFF: CCITT compression requires CcittParams"))?;
            decode_ccitt(raw, p.width, p.rows, CcittVariant::ModifiedHuffman, p.fill)
        }
        COMPRESSION_CCITT_T4 => {
            let p = ccitt
                .ok_or_else(|| Error::invalid("TIFF: CCITT-T4 compression requires CcittParams"))?;
            if p.t4_options & T4OPT_2D_CODING != 0 {
                return Err(Error::invalid(
                    "TIFF: T4 2-D coding (T4Options bit 0) not supported (docs lack 2D mode codes)",
                ));
            }
            if p.t4_options & T4OPT_UNCOMPRESSED != 0 {
                return Err(Error::invalid(
                    "TIFF: T4 uncompressed-mode extension (T4Options bit 1) not supported",
                ));
            }
            let eol_byte_aligned = p.t4_options & T4OPT_EOL_BYTE_ALIGNED != 0;
            decode_ccitt(
                raw,
                p.width,
                p.rows,
                CcittVariant::T4OneD { eol_byte_aligned },
                p.fill,
            )
        }
        COMPRESSION_CCITT_T6 => Err(Error::invalid(
            "TIFF: T.6 (Compression=4) not supported (docs lack 2D mode codes)",
        )),
        other => Err(Error::invalid(format!(
            "TIFF: Compression={other} not supported"
        ))),
    }
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

/// CMYK (TIFF 6.0 §16): 4 chunky bytes per pixel ordered C, M, Y, K
/// where each component is the *complement* of its dye coverage
/// (255 = no dye). Convert into the customary additive RGB the
/// crate emits: R = (1-C)(1-K), G = (1-M)(1-K), B = (1-Y)(1-K), all
/// scaled to 8-bit. This matches `tiffinfo`'s reference rendering
/// and is what callers expect for screen display.
fn build_rgb24_from_cmyk(src: &[u8], w: u32, h: u32) -> TiffImage {
    let stride = w as usize * 3;
    let pixels = (w * h) as usize;
    let mut data = Vec::with_capacity(stride * h as usize);
    for i in 0..pixels {
        let off = i * 4;
        let c = src[off] as u32;
        let m = src[off + 1] as u32;
        let y = src[off + 2] as u32;
        let k = src[off + 3] as u32;
        // Inversion is in the spec: stored values are the *amount*
        // of dye, so larger = darker. Compose multiplicatively.
        let r = ((255 - c) * (255 - k) / 255) as u8;
        let g = ((255 - m) * (255 - k) / 255) as u8;
        let b = ((255 - y) * (255 - k) / 255) as u8;
        data.push(r);
        data.push(g);
        data.push(b);
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Rgb24,
        planes: vec![TiffPlane { stride, data }],
    }
}

/// YCbCr → RGB conversion per TIFF 6.0 §22 / ITU-R BT.601 (the
/// classical SDTV coefficients TIFF lists as the default reference).
/// Subsampling: only chunky `(sh, sv)` configurations are supported
/// here. The on-disk layout in chunky mode is documented in §22 as
/// "data unit" ordering: each block is (sh*sv) Y samples followed
/// by 1 Cb and 1 Cr sample. We re-tile that into per-pixel YCbCr
/// then apply the matrix.
fn build_rgb24_from_ycbcr(src: &[u8], w: u32, h: u32, sh: u16, sv: u16) -> Result<TiffImage> {
    if sh == 0 || sv == 0 {
        return Err(Error::invalid("TIFF: YCbCrSubSampling must be > 0"));
    }
    if !matches!(
        (sh, sv),
        (1, 1) | (2, 1) | (2, 2) | (1, 2) | (4, 1) | (4, 2)
    ) {
        return Err(Error::invalid(format!(
            "TIFF: YCbCrSubSampling=({sh},{sv}) not supported"
        )));
    }
    let block_w = (w as usize).div_ceil(sh as usize);
    let block_h = (h as usize).div_ceil(sv as usize);
    let block_size_bytes = (sh as usize) * (sv as usize) + 2; // Y*Y + Cb + Cr
    let expected = block_w * block_h * block_size_bytes;
    if src.len() < expected {
        return Err(Error::invalid(format!(
            "TIFF/YCbCr: pixel buffer too small (got {}, need {expected})",
            src.len()
        )));
    }

    // Decode block by block, splatting Cb/Cr to each Y sample's
    // position. Pixels outside (w, h) are dropped.
    let mut data = vec![0u8; (w as usize) * 3 * h as usize];
    for by in 0..block_h {
        for bx in 0..block_w {
            let block_off = (by * block_w + bx) * block_size_bytes;
            let cb = src[block_off + (sh as usize) * (sv as usize)] as i32;
            let cr = src[block_off + (sh as usize) * (sv as usize) + 1] as i32;
            for sy in 0..(sv as usize) {
                for sx in 0..(sh as usize) {
                    let y_val = src[block_off + sy * (sh as usize) + sx] as i32;
                    let px = bx * (sh as usize) + sx;
                    let py = by * (sv as usize) + sy;
                    if px >= w as usize || py >= h as usize {
                        continue;
                    }
                    let (r, g, b) = ycbcr_to_rgb(y_val, cb, cr);
                    let dst = (py * (w as usize) + px) * 3;
                    data[dst] = r;
                    data[dst + 1] = g;
                    data[dst + 2] = b;
                }
            }
        }
    }
    Ok(TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Rgb24,
        planes: vec![TiffPlane {
            stride: w as usize * 3,
            data,
        }],
    })
}

/// ITU-R BT.601 inverse-matrix YCbCr → RGB with the canonical
/// rounded integer coefficients (matches libjpeg / libtiff at
/// integer precision; off-by-one differences from floating-point
/// reference impls are within tolerance).
fn ycbcr_to_rgb(y: i32, cb: i32, cr: i32) -> (u8, u8, u8) {
    let cb = cb - 128;
    let cr = cr - 128;
    // Coefficients × 65536 (Q16). Same constants libjpeg uses for
    // the JFIF inverse transform; we use them here because TIFF
    // 6.0 §22 default `YCbCrCoefficients` are exactly the BT.601
    // luma weights {0.299, 0.587, 0.114} that yield this matrix.
    let r = y + ((91881 * cr + 32768) >> 16);
    let g = y - ((22554 * cb + 46802 * cr + 32768) >> 16);
    let b = y + ((116130 * cb + 32768) >> 16);
    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}
