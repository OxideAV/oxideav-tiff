//! High-level TIFF 6.0 decode: parse the header + first IFD,
//! decompress every strip, assemble the image, apply the predictor
//! if any, expand palette / bilevel / 16-bit pixels into one of our
//! standard `TiffPixelFormat`s.

use crate::ccitt::{decode_ccitt, reverse_bits_in_place, CcittVariant, FillOrder};
use crate::compress::{unpack_deflate, unpack_lzw, unpack_packbits, unpack_zstd};
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

    // SampleFormat (TIFF 6.0 §SampleFormat tag 339, page 80). When
    // present the field has one entry per sample, all of which must
    // share an interpretation for this decoder's uniform-bit-depth
    // assembly path to apply consistently. The spec lists four values:
    // 1 (unsigned integer, the default), 2 (two's-complement signed
    // integer), 3 (IEEE floating point) and 4 (undefined). The reader
    // rule is: "If the SampleFormat field is present and the value is
    // not 1, a Baseline TIFF reader that cannot handle the SampleFormat
    // value must terminate the import process gracefully."
    //
    // This decoder routes value 1, value 4 (folded back to unsigned per
    // the §SampleFormat note "A reader would typically treat an image
    // with 'undefined' data as if the field were not present (i.e. as
    // unsigned integer data)") and value 2 (two's-complement signed
    // integer grayscale — see the grayscale build arms) through the
    // integer assembly path; value 3 (IEEE floating point) decodes for
    // single-channel grayscale at the IEEE widths (16-bit half / 32-bit
    // single / 64-bit double) by mapping the floating sample extent onto
    // the unsigned display plane — see the grayscale float build arm. An
    // absent field defaults to unsigned per the §SampleFormat default-1
    // paragraph.
    let sample_format = if let Some(sf_entry) = find(entries, TAG_SAMPLE_FORMAT) {
        // The §SampleFormat header line "N = SamplesPerPixel" sets the
        // expected count. Some writers under-state the count (one
        // entry meant to apply to every sample); accept either the
        // spec-required SamplesPerPixel-long array or a single-entry
        // shorthand, but never a mismatched non-1 count.
        let sf = sf_entry.as_u32_vec(bo)?;
        if sf.len() != samples_per_pixel as usize && sf.len() != 1 {
            return Err(Error::invalid(format!(
                "TIFF: SampleFormat count {} != SamplesPerPixel {}",
                sf.len(),
                samples_per_pixel
            )));
        }
        if !sf.iter().all(|&v| v == sf[0]) {
            return Err(Error::Unsupported(
                "TIFF: per-component SampleFormat values must be uniform".into(),
            ));
        }
        let fmt = sf[0] as u16;
        match fmt {
            SAMPLE_FORMAT_UINT | SAMPLE_FORMAT_UNDEFINED => SAMPLE_FORMAT_UINT,
            SAMPLE_FORMAT_SINT => SAMPLE_FORMAT_SINT,
            SAMPLE_FORMAT_IEEE_FP => SAMPLE_FORMAT_IEEE_FP,
            other => {
                return Err(Error::invalid(format!(
                    "TIFF: SampleFormat={other} unknown (spec defines 1..=4)"
                )));
            }
        }
    } else {
        SAMPLE_FORMAT_UINT
    };

    // Orientation (TIFF 6.0 §Orientation tag 274, page 36). SHORT,
    // N = 1, default 1. The spec defines eight values describing how
    // the stored 0th row / 0th column map onto the displayed image:
    //
    //   1 = 0th row → visual top,    0th column → visual left
    //   2 = 0th row → visual top,    0th column → visual right
    //   3 = 0th row → visual bottom, 0th column → visual right
    //   4 = 0th row → visual bottom, 0th column → visual left
    //   5 = 0th row → visual left,   0th column → visual top
    //   6 = 0th row → visual right,  0th column → visual top
    //   7 = 0th row → visual right,  0th column → visual bottom
    //   8 = 0th row → visual left,   0th column → visual bottom
    //
    // Value 1 is the canonical "as-stored == as-displayed" layout
    // (nothing to do; the decoder writes pixels in storage order,
    // which is also display order). Values 2..=4 are in-place mirror /
    // 180° permutations that keep the (width, height) shape; values
    // 5..=8 are transpose-family permutations that swap width and
    // height. We capture the value here and apply the corresponding
    // geometric remap to the fully-assembled image at the end of the
    // decode (see `apply_orientation`), so every photometric / bit
    // depth / compression path picks it up uniformly. Value 0 and
    // values ≥ 9 are surfaced as invalid-data errors because the spec
    // lists 1..=8 only. An absent field defaults to 1 per the
    // §Orientation "Default is 1" line.
    let orientation = if let Some(orient_entry) = find(entries, TAG_ORIENTATION) {
        let orient = orient_entry.as_u32(bo)? as u16;
        if !(1..=8).contains(&orient) {
            return Err(Error::invalid(format!(
                "TIFF: Orientation={orient} unknown (spec defines 1..=8)"
            )));
        }
        orient
    } else {
        1
    };

    // ResolutionUnit (TIFF 6.0 §"Physical Dimensions" / ResolutionUnit
    // tag 296, page 18). SHORT, N = 1, "Default = 2 (inch)." Spec defines
    // three values:
    //
    //   1 = No absolute unit of measurement (non-square aspect ratio
    //       with no meaningful absolute dimensions).
    //   2 = Inch.
    //   3 = Centimeter.
    //
    // The on-disk pixel bytes are independent of the resolution
    // metadata, so the decoder does not dispatch on this field — but a
    // spec-conformant reader still benefits from rejecting malformed
    // writers up front (rather than silently dropping the field).
    // Absent → default 2 (inch) per spec; explicit 1 / 2 / 3 → accept;
    // 0 / ≥ 4 → InvalidData because the spec lists 1..=3 only.
    if let Some(unit_entry) = find(entries, TAG_RESOLUTION_UNIT) {
        let unit = unit_entry.as_u32(bo)? as u16;
        match unit {
            RESOLUTION_UNIT_NONE | RESOLUTION_UNIT_INCH | RESOLUTION_UNIT_CENTIMETER => {
                // Spec-defined; pixel decode does not depend on the
                // resolution unit so nothing further to do.
            }
            other => {
                return Err(Error::invalid(format!(
                    "TIFF: ResolutionUnit={other} unknown (spec defines 1..=3)"
                )));
            }
        }
    }

    // ExtraSamples (TIFF 6.0 §ExtraSamples tag 338, pages 31-32).
    // SHORT, N = m. "Specifies that each pixel has m extra components
    // whose interpretation is defined by one of the values listed
    // below. When this field is used, the SamplesPerPixel field has a
    // value greater than the PhotometricInterpretation field
    // suggests." The spec example fixes the count arithmetic: "For
    // example, full-color RGB data normally has SamplesPerPixel=3. If
    // SamplesPerPixel is greater than 3, then the ExtraSamples field
    // describes the meaning of the extra samples. If SamplesPerPixel
    // is, say, 5 then ExtraSamples will contain 2 values, one for
    // each extra sample." Per-value reader policy here:
    //
    //   0 (unspecified data) — the extra components carry no defined
    //     meaning, so the color components render correctly without
    //     them; the pixel paths skip the trailing extras.
    //   1 (associated alpha, pre-multiplied color) — the color
    //     components are stored pre-multiplied by the alpha component.
    //     §18 "Associated Alpha Handling" (page 78) states that "naive
    //     applications that want to display an RGBA image on a display
    //     can do so simply by displaying the RGB component values. This
    //     works because it is effectively the same as merging the image
    //     with a black background. … Cr = Cover * Aover", which "is
    //     exactly the pre-multiplied color; i.e. what is stored in the
    //     image." So for a display-target render the stored
    //     pre-multiplied color components ARE the composite-over-black
    //     pixel values, and the decoder displays them directly (the
    //     trailing alpha is dropped from the rendered plane). This is
    //     the spec-endorsed display path, not a mis-render.
    //   2 (unassociated alpha) — "transparency information that
    //     logically exists independent of an image; it is commonly
    //     called a soft matte." The color components are stored
    //     straight (not pre-multiplied), so skipping the matte yields
    //     the correctly-colored fully-opaque render.
    //
    // Both alpha kinds therefore render the leading color components
    // verbatim onto the display plane and drop the trailing extra(s):
    // for unassociated alpha the stored color is already straight, and
    // for associated alpha the stored pre-multiplied color is the
    // composite-over-black value §18 says a display reader should show.
    //
    // "The default is no extra samples" — an absent field changes
    // nothing. Values ≥ 3 are surfaced as `InvalidData` because the
    // spec lists 0..=2 only.
    if let Some(es_entry) = find(entries, TAG_EXTRA_SAMPLES) {
        let es = es_entry.as_u32_vec(bo)?;
        // "By convention, extra components that are present must be
        // stored as the 'last components' in each pixel" — so the
        // leading `SamplesPerPixel − m` components must be exactly
        // the color components the photometric calls for. §23 CIELab
        // admits two layouts ("SamplesPerPixel - ExtraSamples: 3 for
        // L*a*b*, 1 implies L* only, for monochrome data"), hence the
        // slice of admissible color-component counts.
        let color_counts: &[u16] = match photometric {
            PHOTO_WHITE_IS_ZERO | PHOTO_BLACK_IS_ZERO | PHOTO_PALETTE | PHOTO_TRANSPARENCY_MASK => {
                &[1]
            }
            PHOTO_RGB | PHOTO_YCBCR => &[3],
            PHOTO_CMYK => &[4],
            PHOTO_CIELAB => &[3, 1],
            // Unknown photometric: leave the rejection to the
            // photometric dispatch below, which names the value.
            _ => &[],
        };
        if !color_counts.is_empty() {
            let m = es.len() as u64;
            let ok = color_counts
                .iter()
                .any(|&c| samples_per_pixel as u64 == c as u64 + m);
            if !ok {
                return Err(Error::invalid(format!(
                    "TIFF: ExtraSamples count {m} does not leave a color-component \
                     count photometric={photometric} defines \
                     (SamplesPerPixel={samples_per_pixel})"
                )));
            }
        }
        for &v in &es {
            if v == EXTRA_SAMPLE_UNSPECIFIED as u32
                || v == EXTRA_SAMPLE_UNASSOCIATED_ALPHA as u32
                || v == EXTRA_SAMPLE_ASSOCIATED_ALPHA as u32
            {
                // All three defined kinds render the leading color
                // components and drop the trailing extra(s): unspecified
                // and unassociated-alpha color is stored straight, and
                // associated-alpha (pre-multiplied) color is the §18
                // composite-over-black display value a display reader
                // shows directly. The pixel-assembly arms below copy the
                // leading color components verbatim in every case.
            } else {
                return Err(Error::invalid(format!(
                    "TIFF: ExtraSamples={v} unknown (spec defines 0..=2)"
                )));
            }
        }
    }

    // ---- Tiles vs. strips ----
    let bps_first = bits_per_sample[0];
    if !bits_per_sample.iter().all(|&b| b == bps_first) {
        return Err(Error::invalid(
            "TIFF: per-channel BitsPerSample must be uniform in this build",
        ));
    }
    // The integer assembly paths handle 1/4/8/16-bit samples. A
    // SampleFormat = 3 (IEEE float) image additionally admits the 32-bit
    // (single) and 64-bit (double) widths the float photometric uses —
    // their decode runs through `build_gray8_from_float`, validated
    // against the float grayscale arms below.
    let float_width_ok =
        sample_format == SAMPLE_FORMAT_IEEE_FP && (bps_first == 32 || bps_first == 64);
    if bps_first != 1 && bps_first != 4 && bps_first != 8 && bps_first != 16 && !float_width_ok {
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
    let t6_options = find(entries, TAG_T6_OPTIONS)
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
                t6_options,
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
                t6_options,
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
            t6_options,
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
            t6_options,
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

    // SampleFormat = 2 (two's-complement signed integer, TIFF 6.0
    // §SampleFormat page 80) is honoured for the single-channel
    // grayscale photometrics at the integer widths this decoder
    // assembles (8- and 16-bit BlackIsZero / WhiteIsZero) — the layout
    // scientific and elevation TIFFs use. The on-disk samples span the
    // signed range whose default extent §SampleFormat fixes at "the
    // full range of the data type" (SMinSampleValue / SMaxSampleValue
    // tags 340 / 341 default to the type bounds). Rendering signed
    // samples on the codec's unsigned display planes uses the
    // order-preserving offset-binary map: the signed minimum lands at
    // 0 and the signed maximum at the unsigned ceiling (a sign-bit
    // flip: 8-bit XOR 0x80, 16-bit XOR 0x8000), the bijective,
    // monotone mapping that keeps relative brightness intact — the
    // same "some conversion to a display range is required" latitude
    // §23 grants the CIELab path. For any other photometric / width
    // the integer-signed bytes have no single defensible display
    // mapping in this build, so SampleFormat = 2 is refused there per
    // the §SampleFormat "terminate gracefully" reader rule rather than
    // silently mis-rendered.
    if sample_format == SAMPLE_FORMAT_SINT {
        let grayscale = matches!(photometric, PHOTO_BLACK_IS_ZERO | PHOTO_WHITE_IS_ZERO);
        if !grayscale || samples_per_pixel != 1 || !(bps_first == 8 || bps_first == 16) {
            return Err(Error::Unsupported(format!(
                "TIFF: SampleFormat=2 (signed integer) supported only for 8-/16-bit \
                 grayscale (photometric 0/1, SamplesPerPixel=1); got photometric={photometric} \
                 samples_per_pixel={samples_per_pixel} bits_per_sample={bps_first}"
            )));
        }
    }

    // SampleFormat = 3 (IEEE floating-point, TIFF 6.0 §SampleFormat
    // page 80) decodes for the single-channel grayscale photometrics and
    // for 3-channel RGB at the three IEEE 754 widths a float TIFF stores:
    // 16-bit (half), 32-bit (single), 64-bit (double). §SampleFormat
    // fixes the size in BitsPerSample, not in this field, so the width is
    // read from BitsPerSample exactly as for the integer paths. A float
    // sample carries no intrinsic display range, so — paralleling the
    // §23 CIELab "some conversion to a display range will be required"
    // latitude and the signed-integer offset-binary map — the decoder
    // maps the finite sample extent linearly onto the 8-bit display
    // plane: the extent is the §SampleFormat SMinSampleValue /
    // SMaxSampleValue pair (tags 340 / 341) when both are present
    // ("makes it possible for readers to assume that data samples are
    // bound to the range [SMinSampleValue, SMaxSampleValue] without
    // scanning the image data"), else the actual finite min/max scanned
    // from the decoded samples (the spec's stated fallback when the
    // bound tags are absent). For RGB the extent is a single shared
    // bound across all three colour channels so the relative R / G / B
    // magnitudes — the pixel's chromaticity — are preserved exactly; a
    // per-channel extent would re-balance the colour. Non-finite samples
    // (NaN / ±Inf) are excluded from the extent and rendered at the
    // display floor. Only Predictor = 1 is meaningful here — the §14
    // horizontal-differencing predictor is defined over integer samples,
    // and the floating-point predictor (Predictor = 3) is already
    // rejected at the predictor gate above. The float byte stream has no
    // other defensible display mapping for the remaining photometrics, so
    // SampleFormat = 3 is refused there per the §SampleFormat "terminate
    // gracefully" reader rule rather than mis-rendered.
    if sample_format == SAMPLE_FORMAT_IEEE_FP {
        let grayscale = matches!(photometric, PHOTO_BLACK_IS_ZERO | PHOTO_WHITE_IS_ZERO);
        let rgb = photometric == PHOTO_RGB && samples_per_pixel == 3;
        let width_ok = matches!(bps_first, 16 | 32 | 64);
        let shape_ok = (grayscale && samples_per_pixel == 1) || rgb;
        if !shape_ok || !width_ok {
            return Err(Error::Unsupported(format!(
                "TIFF: SampleFormat=3 (IEEE floating-point) supported only for \
                 16-/32-/64-bit grayscale (photometric 0/1, SamplesPerPixel=1) or \
                 RGB (photometric 2, SamplesPerPixel=3); got \
                 photometric={photometric} samples_per_pixel={samples_per_pixel} \
                 bits_per_sample={bps_first}"
            )));
        }
        if predictor != PREDICTOR_NONE {
            return Err(Error::Unsupported(format!(
                "TIFF: SampleFormat=3 (IEEE floating-point) with Predictor={predictor} \
                 not supported (§14 differencing is integer-only)"
            )));
        }
    }

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
            let signed = sample_format == SAMPLE_FORMAT_SINT;
            (
                build_gray8(&pixel_buf, width, height, inv, signed),
                TiffPixelFormat::Gray8,
            )
        }
        // SampleFormat = 3 (IEEE floating-point) grayscale: 16-bit half,
        // 32-bit single, 64-bit double. The guards take precedence over
        // the integer 16-bit arm below, so the unsigned/signed-integer
        // path only runs when the field is not float. Output is a Gray8
        // display plane scaled from the float extent (see the
        // SampleFormat = 3 note above and `build_gray8_from_float`).
        (PHOTO_BLACK_IS_ZERO, 1, 16) | (PHOTO_WHITE_IS_ZERO, 1, 16)
            if sample_format == SAMPLE_FORMAT_IEEE_FP =>
        {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            let (dmin, dmax) = float_display_extent(entries, bo);
            (
                build_gray8_from_float(&pixel_buf, width, height, bo, 16, inv, dmin, dmax)?,
                TiffPixelFormat::Gray8,
            )
        }
        (PHOTO_BLACK_IS_ZERO, 1, 32) | (PHOTO_WHITE_IS_ZERO, 1, 32)
            if sample_format == SAMPLE_FORMAT_IEEE_FP =>
        {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            let (dmin, dmax) = float_display_extent(entries, bo);
            (
                build_gray8_from_float(&pixel_buf, width, height, bo, 32, inv, dmin, dmax)?,
                TiffPixelFormat::Gray8,
            )
        }
        (PHOTO_BLACK_IS_ZERO, 1, 64) | (PHOTO_WHITE_IS_ZERO, 1, 64)
            if sample_format == SAMPLE_FORMAT_IEEE_FP =>
        {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            let (dmin, dmax) = float_display_extent(entries, bo);
            (
                build_gray8_from_float(&pixel_buf, width, height, bo, 64, inv, dmin, dmax)?,
                TiffPixelFormat::Gray8,
            )
        }
        (PHOTO_BLACK_IS_ZERO, 1, 16) | (PHOTO_WHITE_IS_ZERO, 1, 16) => {
            let inv = photometric == PHOTO_WHITE_IS_ZERO;
            let signed = sample_format == SAMPLE_FORMAT_SINT;
            (
                build_gray16le(&pixel_buf, width, height, bo, inv, signed),
                TiffPixelFormat::Gray16Le,
            )
        }
        // SampleFormat = 3 (IEEE floating-point) RGB: 16-bit half,
        // 32-bit single, 64-bit double, three interleaved colour
        // channels per pixel. These guarded arms take precedence over
        // the integer 16-bit RGB arm below, so the integer path only
        // runs when the field is not float. Output is an Rgb24 display
        // plane scaled from a single shared float extent across the
        // three channels (see the SampleFormat = 3 note above and
        // `build_rgb24_from_float`).
        (PHOTO_RGB, 3, bpsw @ (16 | 32 | 64)) if sample_format == SAMPLE_FORMAT_IEEE_FP => {
            let (dmin, dmax) = float_display_extent(entries, bo);
            (
                build_rgb24_from_float(&pixel_buf, width, height, bo, bpsw, dmin, dmax)?,
                TiffPixelFormat::Rgb24,
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
            // §ExtraSamples (pages 31-32): "By convention, extra
            // components that are present must be stored as the
            // 'last components' in each pixel" — the leading three
            // components are the R, G, B triple and the trailing
            // n − 3 extras are dropped. For an associated-alpha (tag
            // 338 = 1) RGBA page the leading triple is the §18
            // pre-multiplied color, which §18 page 78 states is the
            // composite-over-black value a display reader shows
            // directly — so the same verbatim copy renders it.
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

    // Re-orient the storage-order image into display order per the
    // §Orientation tag captured above. Value 1 is a no-op; 2..=8 remap
    // pixel cells (and, for 5..=8, swap width and height).
    let image = apply_orientation(image, orientation);

    Ok(image)
}

/// Remap a storage-order [`TiffImage`] into display order per the TIFF
/// 6.0 §Orientation (tag 274) value `orientation` (1..=8). Value 1 is
/// returned unchanged. The crate's pixel formats are all single-plane
/// with `stride == width × bytes_per_pixel` (no row padding), so the
/// remap operates on fixed-size pixel cells of `bpp` bytes.
///
/// Geometry (derived from the §Orientation page-36 table; `s` = stored
/// `height`×`width`, `d` = display, indices row `r` / column `c`):
///
/// ```text
///   2: d[r][c] = s[r][w-1-c]            (mirror horizontal)
///   3: d[r][c] = s[h-1-r][w-1-c]        (rotate 180°)
///   4: d[r][c] = s[h-1-r][c]            (mirror vertical)
///   5: d[r][c] = s[c][r]                (transpose;        dims → h×w)
///   6: d[r][c] = s[h-1-c][r]            (rotate 90° CW;    dims → h×w)
///   7: d[r][c] = s[h-1-c][w-1-r]        (transpose+180°;   dims → h×w)
///   8: d[r][c] = s[c][w-1-r]            (rotate 90° CCW;   dims → h×w)
/// ```
fn apply_orientation(image: TiffImage, orientation: u16) -> TiffImage {
    if orientation == 1 {
        return image;
    }
    let w = image.width as usize;
    let h = image.height as usize;
    let plane = &image.planes[0];
    let bpp = plane.stride / w;
    let src = &plane.data;

    // 5..=8 transpose the axes: display width = stored height and vice
    // versa. 2..=4 keep the (w, h) shape.
    let transpose = orientation >= 5;
    let (dw, dh) = if transpose { (h, w) } else { (w, h) };
    let dstride = dw * bpp;
    let mut data = vec![0u8; dstride * dh];

    for r in 0..dh {
        for c in 0..dw {
            // Map the display cell (r, c) back to its stored (si, sj).
            let (si, sj) = match orientation {
                2 => (r, w - 1 - c),
                3 => (h - 1 - r, w - 1 - c),
                4 => (h - 1 - r, c),
                5 => (c, r),
                6 => (h - 1 - c, r),
                7 => (h - 1 - c, w - 1 - r),
                8 => (c, w - 1 - r),
                _ => (r, c),
            };
            let s_off = si * plane.stride + sj * bpp;
            let d_off = r * dstride + c * bpp;
            data[d_off..d_off + bpp].copy_from_slice(&src[s_off..s_off + bpp]);
        }
    }

    TiffImage {
        width: dw as u32,
        height: dh as u32,
        pixel_format: image.pixel_format,
        planes: vec![TiffPlane {
            stride: dstride,
            data,
        }],
    }
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
    t6_options: u32,
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

    // Chroma-subsampled YCbCr (TIFF 6.0 §21, PlanarConfiguration = 1)
    // stores data units rather than full-resolution per-pixel triples,
    // so its on-disk byte budget per luma row is smaller than
    // `width * SamplesPerPixel * bps`. Detect that layout up front and
    // size the strips by the §21 data-unit geometry; everything else
    // keeps the standard full-resolution row accounting. The
    // JPEG-compressed YCbCr path never reaches here (each strip is a
    // self-contained JPEG datastream decoded separately), and §14
    // predictor / §"PlanarConfiguration"=2 are out of scope for the
    // subsampled byte-aligned path.
    let photometric = find(entries, TAG_PHOTOMETRIC_INTERPRETATION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(0);
    let ycbcr_subsampling: Option<(usize, usize)> = if photometric == PHOTO_YCBCR as u32
        && samples_per_pixel == 3
        && bps_first == 8
        && !matches!(compression, COMPRESSION_JPEG_OLD | COMPRESSION_JPEG_NEW)
    {
        let (sh, sv) = match find(entries, TAG_YCBCR_SUBSAMPLING) {
            Some(e) => {
                let v = e.as_u32_vec(bo)?;
                if v.len() < 2 {
                    return Err(Error::invalid("TIFF: YCbCrSubSampling too short"));
                }
                (v[0] as usize, v[1] as usize)
            }
            // §21 page 90 default is [2, 2].
            None => (2, 2),
        };
        if sh == 0 || sv == 0 {
            return Err(Error::invalid("TIFF: YCbCrSubSampling must be > 0"));
        }
        if (sh, sv) != (1, 1) {
            Some((sh, sv))
        } else {
            None
        }
    } else {
        None
    };

    // For sub-byte bit depths, rows are padded to byte boundaries
    // per TIFF 6.0 spec.
    let bits_per_row = (width as u64) * (samples_per_pixel as u64) * (bps_first as u64);
    let row_bytes = bits_per_row.div_ceil(8) as usize;

    // For subsampled YCbCr the per-strip byte budget is computed from
    // data units (§21 page 93): each strip holds `rows / sv` data-unit
    // rows of `width / sh` units, every unit `sh*sv + 2` bytes. The
    // strip's RowsPerStrip is constrained by §21 page 90 to be an
    // integer multiple of `sv`.
    let ycbcr_strip_bytes = |rows: u32| -> usize {
        if let Some((sh, sv)) = ycbcr_subsampling {
            let block_w = (width as usize).div_ceil(sh);
            let block_rows = (rows as usize).div_ceil(sv);
            block_w * block_rows * (sh * sv + 2)
        } else {
            row_bytes * rows as usize
        }
    };

    let buf_cap = if ycbcr_subsampling.is_some() {
        ycbcr_strip_bytes(height)
    } else {
        row_bytes * height as usize
    };
    let mut pixel_buf: Vec<u8> = Vec::with_capacity(buf_cap);
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
        let expected = ycbcr_strip_bytes(rows_this_strip);
        let ccitt = if matches!(
            compression,
            COMPRESSION_CCITT_HUFFMAN | COMPRESSION_CCITT_T4 | COMPRESSION_CCITT_T6
        ) {
            Some(CcittParams {
                width,
                rows: rows_this_strip,
                fill: fill_order,
                t4_options,
                t6_options,
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
            // §14 horizontal differencing operates on full-resolution
            // sample rows; it is undefined over the §21 subsampled
            // data-unit layout (where one packed row interleaves a
            // 2-D Y block with single Cb/Cr samples). Reject rather
            // than mis-apply it across the data-unit boundaries.
            if ycbcr_subsampling.is_some() {
                return Err(Error::invalid(
                    "TIFF: Predictor=2 with chroma-subsampled YCbCr (TIFF 6.0 §21 data units) \
                     is unsupported — §14 differencing has no defined meaning over the \
                     packed data-unit layout",
                ));
            }
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
    t6_options: u32,
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
                    t6_options,
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
            //
            // TIFF 6.0 §15 (page 67) requires `TileWidth` to be "a
            // multiple of 16", and §15 (page 66) states that within a
            // tile "each row of data in a tile is treated as a separate
            // 'scanline'", i.e. every tile row is independently padded
            // to a byte boundary (`tile_row_bytes` above). Together
            // these make every tile-column boundary in the destination
            // image row land on a byte boundary even at sub-byte bit
            // depths: column `tx` starts at bit
            // `tx · TileWidth · SamplesPerPixel · BitsPerSample`, and
            // `TileWidth` being a multiple of 16 makes that product a
            // multiple of 8 for any `BitsPerSample ∈ {1, 4}`. The
            // visible region of a tile row therefore copies as a whole
            // number of bytes starting at a byte-aligned destination
            // offset, exactly as for byte-aligned depths — the only
            // difference is that the visible width is measured in bits
            // and rounded up to bytes (the high bits of a partial-tile
            // trailing byte are padding the downstream expander, which
            // reads only `width` pixels per row, ignores).
            let bits_per_dst_origin =
                (tx as u64) * (tile_w as u64) * (samples_per_pixel as u64) * (bps_first as u64);
            if bits_per_dst_origin % 8 != 0 {
                // Cannot occur for `TileWidth` a multiple of 16, but
                // guard against a non-conformant writer rather than
                // panic on a misaligned slice.
                return Err(Error::invalid(
                    "TIFF: tile column boundary is not byte-aligned (TileWidth must be a \
                     multiple of 16 per TIFF 6.0 §15)",
                ));
            }
            let dst_origin_x = (bits_per_dst_origin / 8) as usize;
            let visible_w =
                ((width as i64) - (tx as i64) * (tile_w as i64)).min(tile_w as i64) as usize;
            let visible_h =
                ((height as i64) - (ty as i64) * (tile_h as i64)).min(tile_h as i64) as usize;
            let visible_row_bytes =
                ((visible_w as u64) * (samples_per_pixel as u64) * (bps_first as u64)).div_ceil(8)
                    as usize;
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
    t6_options: u32,
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
                    t6_options,
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
    t6_options: u32,
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
                        t6_options,
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

/// Parameters needed for CCITT compression schemes (2 / 3 / 4).
/// Carries the per-block geometry plus the codec-shape options
/// decoded out of the IFD (FillOrder + T4Options + T6Options).
#[derive(Debug, Clone, Copy)]
struct CcittParams {
    width: u32,
    rows: u32,
    fill: FillOrder,
    t4_options: u32,
    t6_options: u32,
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
        // Compression=50000 (Zstandard, de-facto registry extension —
        // trace doc `docs/image/tiff/tiff-zstd-compression-50000.md`).
        // Structurally identical to Deflate: one self-contained codec
        // stream per strip / tile over the post-predictor bytes, so
        // the predictor reversal and photometric assembly downstream
        // of this dispatch apply unchanged.
        COMPRESSION_ZSTD => unpack_zstd(raw, expected),
        COMPRESSION_CCITT_HUFFMAN => {
            let p = ccitt
                .ok_or_else(|| Error::invalid("TIFF: CCITT compression requires CcittParams"))?;
            decode_ccitt(raw, p.width, p.rows, CcittVariant::ModifiedHuffman, p.fill)
        }
        COMPRESSION_CCITT_T4 => {
            let p = ccitt
                .ok_or_else(|| Error::invalid("TIFF: CCITT-T4 compression requires CcittParams"))?;
            // T4Options bit 1 ("uncompressed mode allowed") is a writer
            // capability flag, not a per-stream requirement: the
            // uncompressed-mode segments are self-delimiting (entrance
            // code `0000001111`, exit code `0000001T`…) and the 2-D
            // READ decoder handles them inline whether or not the bit
            // is set, so we no longer reject the flag. The decode of an
            // actual uncompressed segment lives in
            // `ccitt::decode_uncompressed_segment` (Table 5/T.4).
            let eol_byte_aligned = p.t4_options & T4OPT_EOL_BYTE_ALIGNED != 0;
            let variant = if p.t4_options & T4OPT_2D_CODING != 0 {
                CcittVariant::T4TwoD { eol_byte_aligned }
            } else {
                CcittVariant::T4OneD { eol_byte_aligned }
            };
            decode_ccitt(raw, p.width, p.rows, variant, p.fill)
        }
        COMPRESSION_CCITT_T6 => {
            let p = ccitt
                .ok_or_else(|| Error::invalid("TIFF: CCITT-T6 compression requires CcittParams"))?;
            // T6Options (tag 293): only bit 1 ("uncompressed mode
            // allowed") is defined per TIFF 6.0 §11; bit 0 is reserved
            // and all higher bits are undefined. Bit 1 is a
            // writer-capability hint, not a per-stream requirement —
            // uncompressed-mode segments are self-delimiting and the
            // 2-D READ decoder (`ccitt::decode_uncompressed_segment`,
            // Table 4/T.6 §2.3.1) handles them inline regardless of the
            // bit, so we accept the flag. We still reject any *other*
            // bit being set rather than silently ignore a tag the spec
            // doesn't define.
            if p.t6_options & !T6OPT_UNCOMPRESSED != 0 {
                return Err(Error::invalid(format!(
                    "TIFF: T6Options has undefined bits set (0x{:x}); only bit 1 is defined",
                    p.t6_options
                )));
            }
            decode_ccitt(raw, p.width, p.rows, CcittVariant::T6, p.fill)
        }
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

fn build_gray8(src: &[u8], w: u32, h: u32, invert: bool, signed: bool) -> TiffImage {
    let stride = w as usize;
    let mut data = src[..stride * h as usize].to_vec();
    // SampleFormat = 2: map two's-complement signed bytes onto the
    // unsigned display range with the order-preserving offset-binary
    // transform (signed min -> 0, signed max -> 255). For 8-bit that
    // is exactly a sign-bit flip (XOR 0x80). This runs before the
    // WhiteIsZero polarity inversion, which operates on the unsigned
    // display value.
    if signed {
        for b in data.iter_mut() {
            *b ^= 0x80;
        }
    }
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

fn build_gray16le(
    src: &[u8],
    w: u32,
    h: u32,
    bo: ByteOrder,
    invert: bool,
    signed: bool,
) -> TiffImage {
    let stride = w as usize * 2;
    let n = (w * h) as usize;
    let mut data = Vec::with_capacity(stride * h as usize);
    for i in 0..n {
        let mut v = bo.read_u16(&src[i * 2..i * 2 + 2]);
        // SampleFormat = 2: offset-binary map (signed min -> 0, signed
        // max -> 0xFFFF) is a 16-bit sign-bit flip (XOR 0x8000),
        // applied before the WhiteIsZero polarity inversion.
        if signed {
            v ^= 0x8000;
        }
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

/// Decode one IEEE 754 binary16 (half-precision) value from a 16-bit
/// big-/little-endian field. TIFF 6.0 §SampleFormat states a
/// `SampleFormat = 3` sample is IEEE 754 and leaves the width to
/// BitsPerSample; the 16-bit width is IEEE 754 binary16: one sign bit,
/// five exponent bits (bias 15), ten mantissa bits. The value is widened
/// to `f32` exactly (binary16 → binary32 is lossless).
fn half_to_f32(bits: u16) -> f32 {
    let sign = (bits >> 15) & 0x1;
    let exp = (bits >> 10) & 0x1f;
    let mant = bits & 0x3ff;
    let sign_f = if sign == 1 { -1.0f32 } else { 1.0f32 };
    if exp == 0 {
        // Subnormal (or zero): value = sign × 2^-14 × (mant / 2^10).
        sign_f * 2.0f32.powi(-14) * (mant as f32 / 1024.0)
    } else if exp == 0x1f {
        // Inf / NaN.
        if mant == 0 {
            sign_f * f32::INFINITY
        } else {
            f32::NAN
        }
    } else {
        // Normal: value = sign × 2^(exp-15) × (1 + mant / 2^10).
        sign_f * 2.0f32.powi(exp as i32 - 15) * (1.0 + mant as f32 / 1024.0)
    }
}

/// Read every floating-point sample (16-/32-/64-bit IEEE 754) from the
/// assembled byte buffer into `f64`s, in storage order. `bps` selects
/// the width.
fn read_float_samples(src: &[u8], n: usize, bo: ByteOrder, bps: u16) -> Vec<f64> {
    let mut out = Vec::with_capacity(n);
    match bps {
        16 => {
            for i in 0..n {
                out.push(half_to_f32(bo.read_u16(&src[i * 2..i * 2 + 2])) as f64);
            }
        }
        32 => {
            for i in 0..n {
                out.push(bo.read_f32(&src[i * 4..i * 4 + 4]) as f64);
            }
        }
        // 64-bit double.
        _ => {
            for i in 0..n {
                out.push(bo.read_f64(&src[i * 8..i * 8 + 8]));
            }
        }
    }
    out
}

/// Determine the [min, max] display extent for a `SampleFormat = 3`
/// image. TIFF 6.0 §SampleFormat (page 80): SMinSampleValue (340) and
/// SMaxSampleValue (341) bound the samples "without scanning the image
/// data". When both are present (and SMin < SMax) they define the
/// extent; otherwise the caller scans the actual finite sample extent.
/// Returns `None` for each missing/unusable bound.
fn float_display_extent(entries: &[Entry], bo: ByteOrder) -> (Option<f64>, Option<f64>) {
    let smin = find(entries, TAG_S_MIN_SAMPLE_VALUE)
        .and_then(|e| e.as_f64_vec(bo).ok())
        .and_then(|v| v.into_iter().next())
        .filter(|x| x.is_finite());
    let smax = find(entries, TAG_S_MAX_SAMPLE_VALUE)
        .and_then(|e| e.as_f64_vec(bo).ok())
        .and_then(|v| v.into_iter().next())
        .filter(|x| x.is_finite());
    (smin, smax)
}

/// Render `SampleFormat = 3` IEEE-float grayscale to a Gray8 display
/// plane. The float extent [`dmin`, `dmax`] (from SMin/SMaxSampleValue
/// when present, else scanned from the finite samples) maps linearly to
/// 0..=255: a sample at `dmin` renders 0, a sample at `dmax` renders
/// 255, in between scaled and rounded. Non-finite samples (NaN / ±Inf)
/// render at the display floor (0). The WhiteIsZero polarity inversion
/// runs on the resulting unsigned display value, as for the integer
/// paths. A degenerate extent (all samples equal) renders a flat 0
/// plane. `bps` selects the IEEE width (16 / 32 / 64).
#[allow(clippy::too_many_arguments)]
fn build_gray8_from_float(
    src: &[u8],
    w: u32,
    h: u32,
    bo: ByteOrder,
    bps: u16,
    invert: bool,
    dmin: Option<f64>,
    dmax: Option<f64>,
) -> Result<TiffImage> {
    let n = (w as usize) * (h as usize);
    let bytes_per = (bps / 8) as usize;
    if src.len() < n * bytes_per {
        return Err(Error::invalid(
            "TIFF: float grayscale strip data shorter than image",
        ));
    }
    let samples = read_float_samples(src, n, bo, bps);

    // Resolve the display extent: prefer the SMin/SMax tag pair, else
    // scan the finite samples for their actual min/max.
    let (mut lo, mut hi) = (dmin, dmax);
    if lo.is_none() || hi.is_none() {
        let mut smin = f64::INFINITY;
        let mut smax = f64::NEG_INFINITY;
        for &s in &samples {
            if s.is_finite() {
                if s < smin {
                    smin = s;
                }
                if s > smax {
                    smax = s;
                }
            }
        }
        if smin.is_finite() && smax.is_finite() {
            lo.get_or_insert(smin);
            hi.get_or_insert(smax);
        }
    }
    let lo = lo.unwrap_or(0.0);
    let hi = hi.unwrap_or(0.0);
    let span = hi - lo;

    let stride = w as usize;
    let mut data = Vec::with_capacity(stride * h as usize);
    for &s in &samples {
        let mut v = if !s.is_finite() || span <= 0.0 {
            0u8
        } else {
            let t = ((s - lo) / span).clamp(0.0, 1.0);
            (t * 255.0 + 0.5) as u8
        };
        if invert {
            v = 255 - v;
        }
        data.push(v);
    }
    Ok(TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Gray8,
        planes: vec![TiffPlane { stride, data }],
    })
}

/// Render `SampleFormat = 3` IEEE-float RGB to an Rgb24 display plane.
/// Three interleaved colour channels per pixel (`R G B R G B …`) are read
/// as floats and mapped through a *single shared* extent [`dmin`, `dmax`]:
/// a sample at `dmin` renders 0, one at `dmax` renders 255, in between
/// scaled and rounded. The extent comes from SMin/SMaxSampleValue when
/// both are present, else is scanned from the finite samples of all three
/// channels together — so the relative R / G / B magnitudes (the pixel's
/// chromaticity) survive the conversion; a per-channel extent would
/// re-balance the colour. Non-finite samples (NaN / ±Inf) render at the
/// display floor (0). A degenerate extent (all samples equal) renders a
/// flat 0 plane. `bps` selects the IEEE width (16 / 32 / 64).
#[allow(clippy::too_many_arguments)]
fn build_rgb24_from_float(
    src: &[u8],
    w: u32,
    h: u32,
    bo: ByteOrder,
    bps: u16,
    dmin: Option<f64>,
    dmax: Option<f64>,
) -> Result<TiffImage> {
    let pixels = (w as usize) * (h as usize);
    let n = pixels * 3;
    let bytes_per = (bps / 8) as usize;
    if src.len() < n * bytes_per {
        return Err(Error::invalid(
            "TIFF: float RGB strip data shorter than image",
        ));
    }
    let samples = read_float_samples(src, n, bo, bps);

    // Resolve the shared display extent: prefer the SMin/SMax tag pair,
    // else scan the finite samples across all three channels for their
    // actual min/max (the spec's stated fallback when the bound tags are
    // absent).
    let (mut lo, mut hi) = (dmin, dmax);
    if lo.is_none() || hi.is_none() {
        let mut smin = f64::INFINITY;
        let mut smax = f64::NEG_INFINITY;
        for &s in &samples {
            if s.is_finite() {
                if s < smin {
                    smin = s;
                }
                if s > smax {
                    smax = s;
                }
            }
        }
        if smin.is_finite() && smax.is_finite() {
            lo.get_or_insert(smin);
            hi.get_or_insert(smax);
        }
    }
    let lo = lo.unwrap_or(0.0);
    let hi = hi.unwrap_or(0.0);
    let span = hi - lo;

    let stride = w as usize * 3;
    let mut data = Vec::with_capacity(stride * h as usize);
    for &s in &samples {
        let v = if !s.is_finite() || span <= 0.0 {
            0u8
        } else {
            let t = ((s - lo) / span).clamp(0.0, 1.0);
            (t * 255.0 + 0.5) as u8
        };
        data.push(v);
    }
    Ok(TiffImage {
        width: w,
        height: h,
        pixel_format: TiffPixelFormat::Rgb24,
        planes: vec![TiffPlane { stride, data }],
    })
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
        composite_cmyk_to_rgb, composite_gray, composite_rgb_packed, composite_rgb_planar,
        composite_yuv_to_rgb, JpegPixelFormat,
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
        JpegPixelFormat::Rgb24Packed => {
            if photometric != PHOTO_RGB {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG: packed-RGB-output JPEG but TIFF photometric={photometric}"
                )));
            }
            composite_rgb_packed(seg, visible_w, visible_h, dst, dst_row_stride, dst_x, dst_y)
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
