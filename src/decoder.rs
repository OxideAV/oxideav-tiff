//! High-level TIFF 6.0 decode: parse the header + first IFD,
//! decompress every strip, assemble the image, apply the predictor
//! if any, expand palette / bilevel / 16-bit pixels into one of our
//! standard `TiffPixelFormat`s.

use crate::ccitt::{decode_ccitt, reverse_bits_in_place, CcittVariant, FillOrder};
use crate::compress::{unpack_deflate, unpack_lzw, unpack_packbits};
use crate::error::{Result, TiffError as Error};
use crate::ifd::{find, parse_header, parse_ifd, ByteOrder, Entry};
use crate::image::{TiffImage, TiffPixelFormat, TiffPlane};
use crate::types::*;

/// Maximum total pixels (`ImageWidth * ImageLength`) the decoder will
/// accept on a single IFD. Computed up-front from the IFD's
/// `ImageWidth` / `ImageLength` tags before any strip or tile buffer
/// is allocated, so a 16-byte attacker-crafted IFD claiming
/// `4294967295 * 4294967295` pixels can't drive a multi-petabyte
/// upfront `Vec::with_capacity`. 256 megapixels covers every
/// legitimate single-image TIFF in the wild (it is ~70% larger than
/// a "Hubble Ultra-Deep Field" mosaic at full resolution); the cap
/// can be lifted by a forward-compatible release if a real workflow
/// ever needs it.
const MAX_IMAGE_PIXELS: u64 = 256 * 1024 * 1024;

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
    // Sanity gate: reject claims that exceed `MAX_IMAGE_PIXELS` up
    // front so the downstream `row_bytes * height` allocations
    // can't be steered into multi-gibibyte territory by a 16-byte
    // attacker-crafted IFD. 256 megapixels covers every legitimate
    // single-image TIFF (a 16384x16384 RGB16 file is 1.5 GiB raw
    // but only 268 megapixels) while bounding the worst-case
    // upfront allocation to ~256 MiB even at 16-bit-per-component
    // RGB.
    if (width as u64).saturating_mul(height as u64) > MAX_IMAGE_PIXELS {
        return Err(Error::invalid(format!(
            "TIFF: image too large ({width}x{height} > {MAX_IMAGE_PIXELS} pixels)"
        )));
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
    if planar != PLANAR_CHUNKY && planar != PLANAR_SEPARATE {
        return Err(Error::invalid(format!(
            "TIFF: PlanarConfiguration={planar} unknown (spec defines only 1 and 2)"
        )));
    }
    // Spec page 38 ("PlanarConfiguration"): "If SamplesPerPixel is 1,
    // PlanarConfiguration is irrelevant, and need not be included."
    // A SPP=1 file tagged PlanarConfiguration=2 is well-formed and the
    // on-disk layout is identical to chunky — collapse the two cases
    // up front so the planar walker only has to handle the
    // multi-component branch.
    let planar = if samples_per_pixel == 1 {
        PLANAR_CHUNKY
    } else {
        planar
    };

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

    // Extract CCITT/FillOrder-relevant IFD fields once.
    //
    // FillOrder (tag 266, TIFF 6.0 §FillOrder page 32): 1 = pixels
    // with lower column values are in the high-order bits (default,
    // canonical), 2 = pixels with lower column values are in the
    // low-order bits (only meaningful when BitsPerSample=1 and the
    // data is uncompressed or CCITT-compressed; spec says
    // explicitly: "FillOrder = 2 should be used only when
    // BitsPerSample = 1 and the data is either uncompressed or
    // compressed using CCITT 1D or 2D compression").
    let fill_order_raw = find(entries, TAG_FILL_ORDER)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(1) as u16;
    let fill_order = match fill_order_raw {
        1 => FillOrder::MsbFirst,
        2 => {
            // Spec page 32: FillOrder=2 is only valid for BPS=1
            // (uncompressed or CCITT-compressed). Reject other
            // combinations rather than silently mis-decode.
            let allowed_compression = matches!(
                compression,
                COMPRESSION_NONE
                    | COMPRESSION_CCITT_HUFFMAN
                    | COMPRESSION_CCITT_T4
                    | COMPRESSION_CCITT_T6
            );
            if bps_first != 1 || !allowed_compression {
                return Err(Error::invalid(format!(
                    "TIFF: FillOrder=2 only valid for BitsPerSample=1 uncompressed/CCITT \
                     (got bps={bps_first}, compression={compression})"
                )));
            }
            FillOrder::LsbFirst
        }
        n => {
            return Err(Error::invalid(format!(
                "TIFF: FillOrder={n} unknown (spec defines only 1 and 2)"
            )));
        }
    };
    let t4_options = find(entries, TAG_T4_OPTIONS)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(0);

    // JPEG-in-TIFF (Compression = 7) takes its own path. Each strip
    // or tile is a freestanding JPEG datastream; merging the optional
    // `JPEGTables` blob in front and feeding the result to the JPEG
    // codec gives us decoded planes directly, so the strip / tile
    // walker bypasses the chunky `pixel_buf` intermediate entirely.
    #[cfg(feature = "registry")]
    if compression == COMPRESSION_JPEG_NEW {
        // TN2 "Special considerations for PlanarConfiguration 2":
        // each image segment carries one component plane only, and
        // chroma-subsampled JPEG segments must restate their
        // dimensions in absolute pixel terms. Our JPEG path is
        // chunky-only; reject planar=2 with a precise error so we
        // don't silently mis-decode.
        if planar == PLANAR_SEPARATE {
            return Err(Error::Unsupported(
                "TIFF/JPEG: PlanarConfiguration=2 (separate planes) not supported".into(),
            ));
        }
        return decode_ifd_jpeg(
            input,
            entries,
            bo,
            width,
            height,
            samples_per_pixel,
            bps_first,
            photometric,
        );
    }
    // Without the `registry` feature, JPEG-in-TIFF is unavailable —
    // the JPEG codec lives in `oxideav-mjpeg`, which is gated on the
    // same feature. Fail with a precise error rather than silently
    // mis-decoding.
    #[cfg(not(feature = "registry"))]
    if compression == COMPRESSION_JPEG_NEW {
        return Err(Error::Unsupported(
            "TIFF: Compression=7 (JPEG-in-TIFF) requires the `registry` feature".into(),
        ));
    }
    // The TIFF 6.0 §22 "old-style" JPEG (Compression=6) is officially
    // deprecated by Tech Note 2 and we don't attempt it.
    if compression == COMPRESSION_JPEG_OLD {
        return Err(Error::Unsupported(
            "TIFF: Compression=6 (old-style JPEG) is deprecated by TIFF Tech Note 2; \
             writers should emit Compression=7 instead"
                .into(),
        ));
    }

    let pixel_buf = if find(entries, TAG_TILE_WIDTH).is_some() {
        if planar == PLANAR_SEPARATE {
            decode_tiles_planar(
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
                fill_order,
            )?
        } else {
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
                fill_order,
            )?
        }
    } else if planar == PLANAR_SEPARATE {
        decode_strips_planar(
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
            fill_order,
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
            fill_order,
        )?
    };

    // Note on CCITT photometric handling: §10 / §11 describe the
    // codec as "self-photometric in terms of white and black runs",
    // and TIFF 6.0 says a BlackIsZero reader must "reverse the
    // meaning of white and black when displaying". In practice
    // every conformant reader treats the codec as bit-transparent
    // on the wire: the encoder emits white runs for input bits
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
        // PhotometricInterpretation = 4 (Transparency Mask), TIFF 6.0
        // page 37: "This means that the image is used to define an
        // irregularly shaped region of another image in the same TIFF
        // file. SamplesPerPixel and BitsPerSample must be 1. PackBits
        // compression is recommended. The 1-bits define the interior
        // of the region; the 0-bits define the exterior of the region."
        //
        // The spec ties the mask's polarity to bit value (1 = interior /
        // visible, 0 = exterior), independent of FillOrder or
        // PhotometricInterpretation inversion. We expose it as a
        // Gray8 plane where interior pixels are 0xFF and exterior
        // pixels are 0x00 — i.e. the same byte layout a downstream
        // compositor would multiply with the main image. Strip /
        // tile / FillOrder / compression handling is identical to
        // the BlackIsZero bilevel path, so we route through the
        // same expander with `invert = false` (bit 1 -> 0xFF).
        (PHOTO_TRANSPARENCY_MASK, 1, 1) => {
            let row_bytes = ((width as u64).div_ceil(8)) as usize;
            (
                build_gray8_from_1bpp(&pixel_buf, width, height, row_bytes, false),
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
        // PhotometricInterpretation = 8 (1976 CIE L*a*b*), TIFF 6.0
        // §23 "CIE L*a*b* Images" (page 110). Three 8-bit chunky
        // samples per pixel: L* in 0..255 mapping linearly to the
        // 0..100 perceptual-lightness scale, a* and b* as
        // two's-complement signed 8-bit values in -128..127
        // representing the red/green and yellow/blue chrominance
        // channels (§23: "L* range is from 0 ... to 100 ... The
        // a* and b* ranges will be represented as signed 8 bit
        // values having the range -127 to +127"). The decoder
        // colorimetrically converts via Lab -> XYZ (D65 reference
        // white) -> linear NTSC RGB (§23's stated forward matrix,
        // inverted analytically) -> sRGB-encoded 8-bit, matching
        // the "(perfect reflecting diffuser ... D65 illumination)"
        // reference white the spec mandates. This is a display-
        // ready render path consistent with how the existing
        // YCbCr and CMYK photometrics collapse to Rgb24.
        (PHOTO_CIELAB, 3, 8) => (
            build_rgb24_from_cielab(&pixel_buf, width, height),
            TiffPixelFormat::Rgb24,
        ),
        // §23: "SamplesPerPixel - ExtraSamples: 3 for L*a*b*, 1
        // implies L* only, for monochrome data". An L*-only CIELab
        // image is a perceptual grayscale: the stored 0..255 byte
        // maps linearly to L* 0..100, which we re-encode as
        // gamma-corrected sRGB-luminance so the 8-bit output is
        // ready for display.
        (PHOTO_CIELAB, 1, 8) => (
            build_gray8_from_cielab_l(&pixel_buf, width, height),
            TiffPixelFormat::Gray8,
        ),
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
    fill_order: FillOrder,
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
                fill: fill_order,
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
        // Uncompressed bilevel data carried with FillOrder=2 has the
        // bit order reversed in every byte; canonicalise to MSB-first
        // here so the bilevel-to-Gray8 expander downstream sees the
        // same layout regardless of FillOrder. CCITT-compressed data
        // is already normalised inside `decode_ccitt`, so we only
        // touch the Compression=None path.
        if compression == COMPRESSION_NONE && fill_order == FillOrder::LsbFirst && bps_first == 1 {
            reverse_bits_in_place(&mut strip);
        }
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
    fill_order: FillOrder,
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
                    fill: fill_order,
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
            // Uncompressed bilevel tile path: see the strip-path
            // companion comment. Tiles are gated to bps_first % 8 ==
            // 0 above (see the explicit error a few lines below), so
            // in practice bps_first==1 + tiles is rejected, but we
            // keep the normalisation here for symmetry / safety.
            if compression == COMPRESSION_NONE
                && fill_order == FillOrder::LsbFirst
                && bps_first == 1
            {
                reverse_bits_in_place(&mut tile);
            }
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

/// Decode strips for `PlanarConfiguration = 2` (separate component
/// planes), per TIFF 6.0 §"PlanarConfiguration" (page 38) and §22
/// "YCbCr Images / Storage Order".
///
/// Layout, per spec verbatim: "The values in StripOffsets and
/// StripByteCounts are then arranged as a 2-dimensional array, with
/// SamplesPerPixel rows and StripsPerImage columns. (All of the
/// columns for row 0 are stored first, followed by the columns of
/// row 1, and so on.)" So the entry arrays carry
/// `SamplesPerPixel * StripsPerImage` values; the first
/// `StripsPerImage` entries describe component 0 (e.g. Red), the next
/// `StripsPerImage` describe component 1 (Green), and so on.
///
/// Each plane decodes to a single-component image of the full
/// `width × height`. We then re-interleave the planes into chunky
/// `width × samples_per_pixel × height` ordering so the downstream
/// pixel-format conversion paths (which all assume chunky input)
/// don't need a planar duplicate.
#[allow(clippy::too_many_arguments)]
fn decode_strips_planar(
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
    fill_order: FillOrder,
) -> Result<Vec<u8>> {
    if samples_per_pixel < 2 {
        // SPP=1 routes through the chunky walker by construction
        // (see `planar = if samples_per_pixel == 1 { ... }` in
        // `decode_ifd`), so reaching here with SPP<2 is a programming
        // error.
        return Err(Error::invalid(
            "TIFF: PlanarConfiguration=2 with SamplesPerPixel<2 is meaningless",
        ));
    }
    if bps_first % 8 != 0 {
        // Sub-byte component planes would need bit-level
        // interleaving in the chunky-rebuild step. None of the
        // photometrics that use planar layout (RGB, YCbCr, CMYK)
        // ship sub-byte samples in any deployed encoder, so the
        // value of supporting that case is low. Reject precisely
        // rather than silently mis-decode.
        return Err(Error::invalid(
            "TIFF: PlanarConfiguration=2 at sub-byte bit depths not supported",
        ));
    }
    let rows_per_strip = find(entries, TAG_ROWS_PER_STRIP)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(height);
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
    let strips_per_image = (height as u64).div_ceil(rows_per_strip as u64) as usize;
    let expected_entries = strips_per_image * samples_per_pixel as usize;
    if strip_offsets.len() != expected_entries {
        return Err(Error::invalid(format!(
            "TIFF: PlanarConfiguration=2 expects {expected_entries} strip entries \
             (SamplesPerPixel={samples_per_pixel} × StripsPerImage={strips_per_image}), got {}",
            strip_offsets.len()
        )));
    }

    // Per-plane row stride: one component, full width, native bit
    // depth. `bits_per_sample[0] == bps_first` is enforced earlier
    // (the uniform-BPS check); each plane therefore carries the same
    // shape.
    let bits_per_plane_row = (width as u64) * (bps_first as u64);
    let plane_row_bytes = bits_per_plane_row.div_ceil(8) as usize;

    let spp = samples_per_pixel as usize;
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(spp);
    for plane in 0..spp {
        let mut plane_buf: Vec<u8> = Vec::with_capacity(plane_row_bytes * height as usize);
        let mut rows_done: u32 = 0;
        let plane_start = plane * strips_per_image;
        for s in 0..strips_per_image {
            let i = plane_start + s;
            let offset = strip_offsets[i];
            let byte_count = strip_byte_counts[i];
            let start = offset as usize;
            let end = start.checked_add(byte_count as usize).ok_or_else(|| {
                Error::invalid(format!("TIFF: plane-{plane} strip {s} length overflow"))
            })?;
            if end > input.len() {
                return Err(Error::invalid(format!(
                    "TIFF: plane-{plane} strip {s} extends past EOF"
                )));
            }
            let raw = &input[start..end];
            let rows_this_strip = rows_per_strip.min(height - rows_done);
            let expected = plane_row_bytes * rows_this_strip as usize;
            let ccitt = if matches!(
                compression,
                COMPRESSION_CCITT_HUFFMAN | COMPRESSION_CCITT_T4 | COMPRESSION_CCITT_T6
            ) {
                Some(CcittParams {
                    width,
                    rows: rows_this_strip,
                    fill: fill_order,
                    t4_options,
                })
            } else {
                None
            };
            let decompressed = decompress_block(raw, expected, compression, ccitt)?;
            if decompressed.len() < expected {
                return Err(Error::invalid(format!(
                    "TIFF: plane-{plane} strip {s} short after decompress: got {}, expected {expected}",
                    decompressed.len()
                )));
            }
            let mut strip = decompressed[..expected].to_vec();
            if compression == COMPRESSION_NONE
                && fill_order == FillOrder::LsbFirst
                && bps_first == 1
            {
                reverse_bits_in_place(&mut strip);
            }
            if predictor == PREDICTOR_HORIZONTAL {
                // Spec §14: "If PlanarConfiguration is 2, there is
                // no problem. Differencing works the same as it does
                // for grayscale data." So we drive the predictor
                // with `samples = 1` regardless of the file's actual
                // SPP, because *within* a plane the data is a
                // single-component stream.
                apply_horizontal_predictor(
                    &mut strip,
                    width as usize,
                    rows_this_strip as usize,
                    1,
                    bps_first as usize,
                    plane_row_bytes,
                    bo,
                )?;
            }
            plane_buf.extend_from_slice(&strip);
            rows_done += rows_this_strip;
            if rows_done >= height {
                break;
            }
        }
        if rows_done < height {
            return Err(Error::invalid(format!(
                "TIFF: plane-{plane} strips did not cover full image"
            )));
        }
        planes.push(plane_buf);
    }
    interleave_planes(&planes, width, height, samples_per_pixel, bps_first)
}

/// Decode tiles for `PlanarConfiguration = 2`, per TIFF 6.0
/// §"TileOffsets" (page 71): "TileOffsets length =
/// SamplesPerPixel * TilesPerImage for PlanarConfiguration = 2" with
/// "the offsets for the first component plane are stored first,
/// followed by all the offsets for the second component plane, and
/// so on." Same shape as the planar-strip walker; each plane decodes
/// into a single-component image, then we re-interleave.
#[allow(clippy::too_many_arguments)]
fn decode_tiles_planar(
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
    fill_order: FillOrder,
) -> Result<Vec<u8>> {
    if samples_per_pixel < 2 {
        return Err(Error::invalid(
            "TIFF: PlanarConfiguration=2 with SamplesPerPixel<2 is meaningless",
        ));
    }
    if bps_first % 8 != 0 {
        return Err(Error::invalid(
            "TIFF: planar tiled images at sub-byte bit depths not supported",
        ));
    }
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
    let tiles_per_plane = (tiles_across as usize) * (tiles_down as usize);
    let expected_tiles = tiles_per_plane * samples_per_pixel as usize;
    if tile_offsets.len() != expected_tiles {
        return Err(Error::invalid(format!(
            "TIFF: PlanarConfiguration=2 expects {expected_tiles} tile entries \
             (SamplesPerPixel={samples_per_pixel} × TilesPerImage={tiles_per_plane}), got {}",
            tile_offsets.len()
        )));
    }

    let sample_bytes = (bps_first / 8) as usize;
    let bits_per_plane_tile_row = (tile_w as u64) * (bps_first as u64);
    let tile_row_bytes = bits_per_plane_tile_row.div_ceil(8) as usize;
    let tile_size_bytes = tile_row_bytes * tile_h as usize;
    let bits_per_plane_image_row = (width as u64) * (bps_first as u64);
    let plane_image_row_bytes = bits_per_plane_image_row.div_ceil(8) as usize;

    let spp = samples_per_pixel as usize;
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(spp);
    for plane in 0..spp {
        let mut plane_buf = vec![0u8; plane_image_row_bytes * height as usize];
        let plane_start = plane * tiles_per_plane;
        for ty in 0..tiles_down {
            for tx in 0..tiles_across {
                let idx = plane_start + (ty * tiles_across + tx) as usize;
                let off = tile_offsets[idx] as usize;
                let bc = tile_byte_counts[idx] as usize;
                let end = off.checked_add(bc).ok_or_else(|| {
                    Error::invalid(format!("TIFF: plane-{plane} tile length overflow"))
                })?;
                if end > input.len() {
                    return Err(Error::invalid(format!(
                        "TIFF: plane-{plane} tile extends past EOF"
                    )));
                }
                let raw = &input[off..end];
                let ccitt = if matches!(
                    compression,
                    COMPRESSION_CCITT_HUFFMAN | COMPRESSION_CCITT_T4 | COMPRESSION_CCITT_T6
                ) {
                    Some(CcittParams {
                        width: tile_w,
                        rows: tile_h,
                        fill: fill_order,
                        t4_options,
                    })
                } else {
                    None
                };
                let mut tile = decompress_block(raw, tile_size_bytes, compression, ccitt)?;
                if tile.len() < tile_size_bytes {
                    return Err(Error::invalid(format!(
                        "TIFF: plane-{plane} tile short after decompress"
                    )));
                }
                tile.truncate(tile_size_bytes);
                if compression == COMPRESSION_NONE
                    && fill_order == FillOrder::LsbFirst
                    && bps_first == 1
                {
                    reverse_bits_in_place(&mut tile);
                }
                if predictor == PREDICTOR_HORIZONTAL {
                    apply_horizontal_predictor(
                        &mut tile,
                        tile_w as usize,
                        tile_h as usize,
                        1,
                        bps_first as usize,
                        tile_row_bytes,
                        bo,
                    )?;
                }
                let visible_w =
                    ((width as i64) - (tx as i64) * (tile_w as i64)).min(tile_w as i64) as usize;
                let visible_h =
                    ((height as i64) - (ty as i64) * (tile_h as i64)).min(tile_h as i64) as usize;
                let visible_row_bytes = visible_w * sample_bytes;
                let dst_origin_x = tx as usize * tile_w as usize * sample_bytes;
                let dst_origin_y = ty as usize * tile_h as usize;
                for r in 0..visible_h {
                    let src_off = r * tile_row_bytes;
                    let dst_off = (dst_origin_y + r) * plane_image_row_bytes + dst_origin_x;
                    plane_buf[dst_off..dst_off + visible_row_bytes]
                        .copy_from_slice(&tile[src_off..src_off + visible_row_bytes]);
                }
            }
        }
        planes.push(plane_buf);
    }
    interleave_planes(&planes, width, height, samples_per_pixel, bps_first)
}

/// Re-interleave per-component planes into chunky pixel order. The
/// caller has validated that every plane is `width * height *
/// (bps/8)` bytes and that `bps` is a multiple of 8. The output is
/// `width * height * samples_per_pixel * (bps/8)` bytes in
/// component-0, component-1, …, component-(spp-1), component-0,
/// component-1, … sequence (which is what the downstream chunky
/// converters consume).
fn interleave_planes(
    planes: &[Vec<u8>],
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bps_first: u16,
) -> Result<Vec<u8>> {
    let sample_bytes = (bps_first / 8) as usize;
    let spp = samples_per_pixel as usize;
    let pixels = (width as usize) * (height as usize);
    let plane_bytes = pixels * sample_bytes;
    for (i, p) in planes.iter().enumerate() {
        if p.len() != plane_bytes {
            return Err(Error::invalid(format!(
                "TIFF: plane {i} has {} bytes, expected {plane_bytes}",
                p.len()
            )));
        }
    }
    let mut out = vec![0u8; pixels * spp * sample_bytes];
    for px in 0..pixels {
        for (c, plane) in planes.iter().enumerate() {
            let src = &plane[px * sample_bytes..(px + 1) * sample_bytes];
            let dst_off = (px * spp + c) * sample_bytes;
            out[dst_off..dst_off + sample_bytes].copy_from_slice(src);
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
/// rounded integer coefficients (off-by-one differences from
/// floating-point reference impls are within tolerance).
fn ycbcr_to_rgb(y: i32, cb: i32, cr: i32) -> (u8, u8, u8) {
    let cb = cb - 128;
    let cr = cr - 128;
    // Coefficients × 65536 (Q16) for the BT.601 conversion. TIFF
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

// ---------------------------------------------------------------------------
// CIELab (PhotometricInterpretation = 8) decode, per TIFF 6.0 §23.
// ---------------------------------------------------------------------------

/// Decode a 3-sample CIELab strip/tile buffer into a packed Rgb24
/// [`TiffImage`] suitable for display.
///
/// On-disk layout per §23 page 110: 8-bit chunky L*, a*, b* triples
/// with L* unsigned 0..255 ↔ 0..100 and a*/b* two's-complement signed
/// 8-bit values in -128..127. The "reference white for this data type
/// is the perfect reflecting diffuser" with "D65 illumination", so we
/// convert via the Lab → XYZ formulas given on page 110 and the
/// inverse of §23's stated NTSC tristimulus matrix to recover linear
/// RGB. A standard sRGB-style gamma curve then maps the linear values
/// into a display-ready 8-bit byte. This is the same shape the
/// existing CMYK and YCbCr photometric paths take (decode produces
/// Rgb24 ready for compositing).
fn build_rgb24_from_cielab(src: &[u8], w: u32, h: u32) -> TiffImage {
    let stride = w as usize * 3;
    let pixels = (w as usize) * (h as usize);
    let mut data = Vec::with_capacity(stride * h as usize);
    for triple in src.chunks_exact(3).take(pixels) {
        // L* in 0..255 maps linearly to 0..100 per §23 ("Dividing
        // the 0-100 range of L* into 256 levels").
        let l_byte = triple[0] as f64;
        // a*, b* per §23: "signed 8 bit values having the range
        // -127 to +127". We accept the full two's-complement byte
        // range (-128..127) — values at the extremes are within
        // spec tolerance.
        let a_signed = triple[1] as i8 as f64;
        let b_signed = triple[2] as i8 as f64;
        let l = l_byte * (100.0 / 255.0);
        let (r, g, b) = cielab_to_rgb_byte(l, a_signed, b_signed);
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

/// Decode a 1-sample CIELab L*-only buffer into a Gray8 [`TiffImage`].
///
/// §23 page 110 ("Usage of other Fields"): "SamplesPerPixel -
/// ExtraSamples: 3 for L*a*b*, 1 implies L* only, for monochrome
/// data". The byte is the perceptual lightness on the 0..100 scale;
/// we re-encode it as sRGB-luminance so the gray level matches what
/// the 3-sample CIELab render produces for a chromatically-neutral
/// pixel (a* = b* = 0).
fn build_gray8_from_cielab_l(src: &[u8], w: u32, h: u32) -> TiffImage {
    let stride = w as usize;
    let pixels = (w as usize) * (h as usize);
    let mut data = Vec::with_capacity(pixels);
    for &byte in src.iter().take(pixels) {
        let l = byte as f64 * (100.0 / 255.0);
        // For a chromatically-neutral CIELab pixel (a* = b* = 0)
        // the Lab → XYZ formula collapses to Y = f(L*) (X and Z
        // are proportional with fx = fy = fz). Run the same
        // lightness curve as the 3-sample path so a* = b* = 0
        // CIELab pixels in either layout produce the same gray.
        let y_lin = lab_l_to_y_linear(l);
        data.push(linear_to_srgb_byte(y_lin));
    }
    TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Gray8,
        planes: vec![TiffPlane { stride, data }],
    }
}

/// CIELab triple to display Rgb24 byte, per TIFF 6.0 §23 + the
/// inverse of §23's stated forward NTSC matrix.
///
/// Steps:
/// 1. Lab → XYZ via the inverse of the §23 page 110 formulas. The
///    reference white is the perfect reflecting diffuser at D65
///    (Xn = 0.95047, Yn = 1.00000, Zn = 1.08883 — the CIE 1931 2°
///    Standard Observer values for D65 the spec implies on page
///    111: "Generally, D65 illumination is used and a perfect
///    reflecting diffuser is used for the reference white").
/// 2. XYZ → linear RGB via the analytic inverse of the NTSC
///    tristimulus matrix the spec prints on page 111:
///    X = 0.6070 R + 0.1740 G + 0.2000 B,
///    Y = 0.2990 R + 0.5870 G + 0.1140 B,
///    Z = 0.0000 R + 0.0660 G + 1.1110 B.
///    Inverting algebraically (cofactor expansion, det ≈
///    0.337438) gives the row-major coefficients hard-coded
///    below. They are arithmetic facts derived from the spec's
///    matrix only — no external impl was consulted.
/// 3. Linear RGB → 8-bit display RGB via the standard sRGB OETF
///    so the result is ready for a contemporary screen. The
///    spec's "Converting from CIELAB to RGB" caveat (page 111)
///    explicitly leaves the linear-to-display step to the
///    implementer; gamma encoding is the universally-applicable
///    choice and matches the render the rest of the crate
///    produces from RGB / CMYK / YCbCr.
fn cielab_to_rgb_byte(l_star: f64, a_star: f64, b_star: f64) -> (u8, u8, u8) {
    // ---- Lab -> XYZ (D65 white) ----
    // CIE 1931 2° Standard Observer D65 white in the spec's
    // normalised "1.0 = perfect reflecting diffuser" form.
    const XN: f64 = 0.95047;
    const YN: f64 = 1.00000;
    const ZN: f64 = 1.08883;

    // §23 page 110:
    //   L* = 116 (Y/Yn)^(1/3) - 16            (Y/Yn > 0.008856)
    //   L* = 903.3 (Y/Yn)                     (Y/Yn <= 0.008856)
    //   a* = 500 [(X/Xn)^(1/3) - (Y/Yn)^(1/3)]
    //   b* = 200 [(Y/Yn)^(1/3) - (Z/Zn)^(1/3)]
    // Inverting: let fy = (L*+16)/116; if fy^3 > 0.008856 then
    // Y/Yn = fy^3 else Y/Yn = L*/903.3. Likewise for X, Z via
    // fx = a*/500 + fy and fz = fy - b*/200, applying the low-light
    // branch when the cube is at or below 0.008856 (the spec's
    // threshold). The cube root inverse of the low-light leg
    // 7.787*F + 16/116 is (f - 16/116)/7.787.
    let fy = (l_star + 16.0) / 116.0;
    let fx = a_star / 500.0 + fy;
    let fz = fy - b_star / 200.0;

    let y_yn = if l_star > 7.999625 {
        // Boundary: L* = 116 * 0.008856^(1/3) - 16 ≈ 7.99963; above
        // this the cubic branch applies.
        fy.powi(3)
    } else {
        l_star / 903.3
    };
    let fx3 = fx.powi(3);
    let x_xn = if fx3 > 0.008856 {
        fx3
    } else {
        (fx - 16.0 / 116.0) / 7.787
    };
    let fz3 = fz.powi(3);
    let z_zn = if fz3 > 0.008856 {
        fz3
    } else {
        (fz - 16.0 / 116.0) / 7.787
    };

    let x = x_xn * XN;
    let y = y_yn * YN;
    let z = z_zn * ZN;

    // ---- XYZ -> linear RGB ----
    // Inverse of §23 NTSC matrix:
    //   [ 0.6070  0.1740  0.2000 ]
    //   [ 0.2990  0.5870  0.1140 ]
    //   [ 0.0000  0.0660  1.1110 ]
    // Determinant (cofactor expansion along row 0):
    //    0.6070*(0.5870*1.1110 - 0.1140*0.0660)
    //   -0.1740*(0.2990*1.1110 - 0.1140*0.0000)
    //   +0.2000*(0.2990*0.0660 - 0.5870*0.0000)
    //  = 0.6070*0.644633 - 0.1740*0.332189 + 0.2000*0.019734
    //  ≈ 0.337438145
    // Adjugate row-major / determinant gives the inverse below.
    // The constants are arithmetic facts derived from the spec
    // matrix only — no external impl was consulted.
    let r_lin = 1.9103738257 * x - 0.5337689371 * y - 0.2891315088 * z;
    let g_lin = -0.9844441268 * x + 1.9985203510 * y - 0.0278510303 * z;
    let b_lin = 0.0584818293 * x - 0.1187239812 * y + 0.9017445257 * z;

    (
        linear_to_srgb_byte(r_lin),
        linear_to_srgb_byte(g_lin),
        linear_to_srgb_byte(b_lin),
    )
}

/// Lab L* → linear Y under D65 reference white, used by the
/// L*-only render path. Same inverse-cube-root step as
/// [`cielab_to_rgb_byte`].
fn lab_l_to_y_linear(l_star: f64) -> f64 {
    if l_star > 7.999625 {
        ((l_star + 16.0) / 116.0).powi(3)
    } else {
        l_star / 903.3
    }
}

/// Linear-light value in [0, 1] → 8-bit sRGB display byte using the
/// canonical sRGB OETF (the published IEC 61966-2-1 piecewise
/// curve). Out-of-gamut values are clamped to [0, 255].
fn linear_to_srgb_byte(v: f64) -> u8 {
    let c = v.clamp(0.0, 1.0);
    // sRGB encoding: linear-to-display compander. The piecewise
    // breakpoint at 0.0031308 maps to ~0.04045 on the display side.
    let encoded = if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

// ---------------------------------------------------------------------------
// JPEG-in-TIFF (Compression = 7) decode, per TIFF Tech Note 2.
// ---------------------------------------------------------------------------

/// Decode a JPEG-in-TIFF IFD (`Compression = 7`) into a [`TiffImage`].
/// Lives behind the `registry` feature gate because it reaches into
/// `oxideav-mjpeg`'s `Decoder` trait surface.
#[cfg(feature = "registry")]
#[allow(clippy::too_many_arguments)]
fn decode_ifd_jpeg(
    input: &[u8],
    entries: &[Entry],
    bo: ByteOrder,
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bps_first: u16,
    photometric: u16,
) -> Result<TiffImage> {
    // TN2: SOFn precision must equal BitsPerSample, and 8-bit is the
    // only baseline-mandatory precision. Reject other depths up-front
    // so the JPEG decoder doesn't have to.
    if bps_first != 8 {
        return Err(Error::Unsupported(format!(
            "TIFF/JPEG: BitsPerSample={bps_first} (only 8-bit is supported in this build)"
        )));
    }
    // TN2 explicitly forbids palette (3) and transparency-mask (4)
    // photometrics with Compression=7. Reject any photometric we
    // don't have a render path for.
    let want_planes = match (photometric, samples_per_pixel) {
        (PHOTO_BLACK_IS_ZERO, 1) | (PHOTO_WHITE_IS_ZERO, 1) => 1,
        (PHOTO_RGB, 3) => 3,
        (PHOTO_YCBCR, 3) => 3,
        // CMYK JPEG-in-TIFF: mjpeg packs the 4 components into one
        // plane (stride = width × 4) and consumes any Adobe APP14
        // marker before handing the bytes back. Per TN2: "A
        // JPEG-compressed TIFF file will typically have
        // PhotometricInterpretation = YCbCr ... unless the source
        // data was grayscale or CMYK" — CMYK is an explicitly
        // permitted photometric for Compression=7.
        (PHOTO_CMYK, 4) => 1,
        (p, s) => {
            return Err(Error::invalid(format!(
                "TIFF/JPEG: photometric={p} samples_per_pixel={s} not supported"
            )));
        }
    };
    let _ = want_planes;

    // Optional JPEGTables blob (tag 347). Per TN2 it's type
    // UNDEFINED, which the IFD parser keeps as raw bytes already; we
    // only need to slice it out.
    let tables: Option<&[u8]> = find(entries, TAG_JPEG_TABLES).map(|e| e.data.as_slice());

    // Set up the destination buffer in the *final* output format the
    // crate emits for this photometric. Currently:
    //   - PHOTO_BLACK_IS_ZERO / WHITE_IS_ZERO  →  Gray8
    //   - PHOTO_RGB / PHOTO_YCBCR / PHOTO_CMYK →  Rgb24
    //   (CMYK is collapsed to Rgb24 by the same additive-RGB
    //    conversion the uncompressed CMYK path uses — see
    //    `build_rgb24_from_cmyk` / `composite_cmyk_to_rgb`.)
    let (pixel_format, dst_row_stride, dst_size) = match photometric {
        PHOTO_BLACK_IS_ZERO | PHOTO_WHITE_IS_ZERO => (
            TiffPixelFormat::Gray8,
            width as usize,
            width as usize * height as usize,
        ),
        PHOTO_RGB | PHOTO_YCBCR | PHOTO_CMYK => (
            TiffPixelFormat::Rgb24,
            width as usize * 3,
            width as usize * 3 * height as usize,
        ),
        _ => unreachable!("photometric vetted above"),
    };
    let mut dst = vec![0u8; dst_size];

    let invert = photometric == PHOTO_WHITE_IS_ZERO;
    let want_yuv = photometric == PHOTO_YCBCR;

    // Strip vs tile dispatch (mirrors the non-JPEG path).
    if find(entries, TAG_TILE_WIDTH).is_some() {
        decode_ifd_jpeg_tiles(
            input,
            entries,
            bo,
            width,
            height,
            photometric,
            tables,
            invert,
            want_yuv,
            &mut dst,
            dst_row_stride,
        )?;
    } else {
        decode_ifd_jpeg_strips(
            input,
            entries,
            bo,
            width,
            height,
            photometric,
            tables,
            invert,
            want_yuv,
            &mut dst,
            dst_row_stride,
        )?;
    }

    Ok(TiffImage {
        width,
        height,
        pixel_format,
        planes: vec![TiffPlane {
            stride: dst_row_stride,
            data: dst,
        }],
    })
}

#[cfg(feature = "registry")]
#[allow(clippy::too_many_arguments)]
fn decode_ifd_jpeg_strips(
    input: &[u8],
    entries: &[Entry],
    bo: ByteOrder,
    width: u32,
    height: u32,
    photometric: u16,
    tables: Option<&[u8]>,
    invert: bool,
    want_yuv: bool,
    dst: &mut [u8],
    dst_row_stride: usize,
) -> Result<()> {
    use crate::jpeg::decode_segment;

    let rows_per_strip = find(entries, TAG_ROWS_PER_STRIP)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(height);
    let strip_offsets = find(entries, TAG_STRIP_OFFSETS)
        .ok_or_else(|| Error::invalid("TIFF: missing StripOffsets"))?
        .as_u64_vec(bo)?;
    let strip_byte_counts = find(entries, TAG_STRIP_BYTE_COUNTS)
        .ok_or_else(|| Error::invalid("TIFF: missing StripByteCounts"))?
        .as_u64_vec(bo)?;
    if strip_offsets.len() != strip_byte_counts.len() {
        return Err(Error::invalid(
            "TIFF/JPEG: StripOffsets / StripByteCounts length mismatch",
        ));
    }

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

        let seg = decode_segment(tables, raw, width, rows_this_strip, photometric)?;
        composite_segment(
            &seg,
            width,
            rows_this_strip,
            dst,
            dst_row_stride,
            0,
            rows_done,
            invert,
            want_yuv,
            photometric,
        )?;
        rows_done += rows_this_strip;
        if rows_done >= height {
            break;
        }
    }
    if rows_done < height {
        return Err(Error::invalid("TIFF/JPEG: strips did not cover full image"));
    }
    Ok(())
}

#[cfg(feature = "registry")]
#[allow(clippy::too_many_arguments)]
fn decode_ifd_jpeg_tiles(
    input: &[u8],
    entries: &[Entry],
    bo: ByteOrder,
    width: u32,
    height: u32,
    photometric: u16,
    tables: Option<&[u8]>,
    invert: bool,
    want_yuv: bool,
    dst: &mut [u8],
    dst_row_stride: usize,
) -> Result<()> {
    use crate::jpeg::decode_segment;

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
            "TIFF/JPEG: TileOffsets / TileByteCounts length mismatch",
        ));
    }
    let tiles_across = (width as u64).div_ceil(tile_w as u64) as u32;
    let tiles_down = (height as u64).div_ceil(tile_h as u64) as u32;
    let expected_tiles = (tiles_across as usize) * (tiles_down as usize);
    if tile_offsets.len() != expected_tiles {
        return Err(Error::invalid(format!(
            "TIFF/JPEG: TileOffsets length {} != expected {expected_tiles}",
            tile_offsets.len()
        )));
    }

    for ty in 0..tiles_down {
        for tx in 0..tiles_across {
            let idx = (ty * tiles_across + tx) as usize;
            let off = tile_offsets[idx] as usize;
            let bc = tile_byte_counts[idx] as usize;
            let end = off
                .checked_add(bc)
                .ok_or_else(|| Error::invalid("TIFF/JPEG: tile length overflow"))?;
            if end > input.len() {
                return Err(Error::invalid("TIFF/JPEG: tile extends past EOF"));
            }
            let raw = &input[off..end];
            let seg = decode_segment(tables, raw, tile_w, tile_h, photometric)?;
            let visible_w = ((width as i64) - (tx as i64) * (tile_w as i64))
                .min(tile_w as i64)
                .max(0) as u32;
            let visible_h = ((height as i64) - (ty as i64) * (tile_h as i64))
                .min(tile_h as i64)
                .max(0) as u32;
            composite_segment(
                &seg,
                visible_w,
                visible_h,
                dst,
                dst_row_stride,
                tx * tile_w,
                ty * tile_h,
                invert,
                want_yuv,
                photometric,
            )?;
        }
    }
    Ok(())
}

/// Composite one decoded JPEG segment into the destination buffer
/// using the format-appropriate path.
#[cfg(feature = "registry")]
#[allow(clippy::too_many_arguments)]
fn composite_segment(
    seg: &crate::jpeg::JpegSegment,
    visible_w: u32,
    visible_h: u32,
    dst: &mut [u8],
    dst_row_stride: usize,
    dst_x: u32,
    dst_y: u32,
    invert: bool,
    want_yuv: bool,
    photometric: u16,
) -> Result<()> {
    use crate::jpeg::{
        composite_cmyk_to_rgb, composite_gray, composite_rgb_planar, composite_yuv_to_rgb,
        JpegPixelFormat,
    };
    match seg.pixel_format {
        JpegPixelFormat::Gray8 => composite_gray(
            seg,
            visible_w,
            visible_h,
            dst,
            dst_row_stride,
            dst_x,
            dst_y,
            invert,
        ),
        JpegPixelFormat::Yuv444P
        | JpegPixelFormat::Yuv422P
        | JpegPixelFormat::Yuv420P
        | JpegPixelFormat::Yuv411P => {
            if !want_yuv && photometric != PHOTO_RGB {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG: YUV-output JPEG segment but TIFF photometric={photometric}"
                )));
            }
            // YCbCr photometric → matrix to RGB. PHOTO_RGB with a
            // YUV-output JPEG segment is theoretically possible if a
            // writer mistakenly attached `Sf` factors implying chroma
            // subsampling but kept the photometric tag at 2; we treat
            // that as a writer error.
            if want_yuv {
                composite_yuv_to_rgb(seg, visible_w, visible_h, dst, dst_row_stride, dst_x, dst_y)
            } else {
                composite_rgb_planar(seg, visible_w, visible_h, dst, dst_row_stride, dst_x, dst_y)
            }
        }
        JpegPixelFormat::Rgb24 => {
            if photometric != PHOTO_RGB {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG: RGB-output JPEG but TIFF photometric={photometric}"
                )));
            }
            composite_rgb_planar(seg, visible_w, visible_h, dst, dst_row_stride, dst_x, dst_y)
        }
        JpegPixelFormat::Cmyk8 => {
            if photometric != PHOTO_CMYK {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG: CMYK-output JPEG but TIFF photometric={photometric}"
                )));
            }
            composite_cmyk_to_rgb(seg, visible_w, visible_h, dst, dst_row_stride, dst_x, dst_y)
        }
    }
}
