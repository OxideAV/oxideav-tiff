//! The image-crate contract's root vocabulary (`IMAGE_CRATE_API`) for
//! TIFF: `probe` / `info` / `decode*` / `encode*`, plus the deprecated
//! pre-contract entry points kept as thin wrappers for one release.
//!
//! Everything here is framework-free (no `oxideav-core`); the
//! `registry` feature's `Decoder` / `Encoder` adapters call these same
//! functions.

use std::io::{Read, Write};

#[allow(deprecated)]
use crate::decoder::DecodedTiff;
use crate::decoder::{
    decode_page_at_with, decode_page_with, decode_pages_with, ifd_chain, legacy_display_layout,
    resolve_color,
};
use crate::encoder::{
    encode_pages, AuxIfdEntry, EncodePage, EncodePixelFormat, ExtraSampleKind, PageExtras, RgbColor,
};
use crate::error::{Result, TiffError as Error};
use crate::exif::{parse_exif_payload, OwnedEntry};
use crate::ifd::{find, parse_header, ByteOrder, Entry, TiffVariant};
use crate::image::{Frame, ImageInfo, Page, PixelFormat, RgbImage, RgbaImage, TiffImage};
use crate::metadata::{extract_format_info, extract_metadata};
use crate::options::{DecodeOptions, EncodeOptions};
use crate::types::*;

// ---- probe / info -----------------------------------------------------------

/// `true` when `bytes` start with a TIFF or BigTIFF header: `II*\0`
/// (little-endian classic), `MM\0*` (big-endian classic), `II+\0` or
/// `MM\0+` (BigTIFF, version 43). No allocation, never panics, `false`
/// on short input.
pub fn probe(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(..4),
        Some([b'I', b'I', 0x2A, 0x00])
            | Some([b'M', b'M', 0x00, 0x2A])
            | Some([b'I', b'I', 0x2B, 0x00])
            | Some([b'M', b'M', 0x00, 0x2B])
    )
}

/// Header-only inspection: walks the header and the next-IFD chain
/// (no strip / tile is read) and reports the first image's dimensions,
/// the layout [`decode`] would return, the page count, alpha, colour
/// signalling and metadata presence, plus the TIFF structural extras.
pub fn info(bytes: &[u8]) -> Result<ImageInfo> {
    let header = parse_header(bytes)?;
    let chain = ifd_chain(bytes, &header)?;
    let entries = &chain[0];
    let bo = header.byte_order;
    let layout = native_layout(entries, bo)?;
    let md = extract_metadata(entries, bo);
    let fi = extract_format_info(entries, bo);
    let (width, height) = if layout.orientation >= 5 {
        (layout.height, layout.width)
    } else {
        (layout.width, layout.height)
    };
    Ok(ImageInfo {
        width,
        height,
        format: layout.format,
        frames: chain.len() as u32,
        has_alpha: layout.format.has_alpha(),
        color: resolve_color(entries, bo),
        has_icc: md.icc_profile.is_some(),
        has_exif: find(entries, TAG_EXIF_IFD).is_some() || find(entries, TAG_GPS_IFD).is_some(),
        has_xmp: md.xmp.is_some(),
        bits_per_sample: layout.bits_per_sample,
        samples_per_pixel: layout.samples_per_pixel,
        photometric: layout.photometric,
        compression: layout.compression,
        planar: fi.planar_config == Some(PLANAR_SEPARATE),
        tiled: fi.tiled,
        big_tiff: header.variant == TiffVariant::Big,
        orientation: layout.orientation,
    })
}

/// What the IFD tags say the decoded layout will be.
pub(crate) struct Layout {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub bits_per_sample: u16,
    pub samples_per_pixel: u16,
    pub photometric: u16,
    pub compression: u16,
    pub orientation: u16,
}

/// Resolve the native layout [`decode`] returns for an IFD from its
/// tags alone — the same photometric / depth / sample-format dispatch
/// the pixel decoder applies, without touching strip or tile data.
pub(crate) fn native_layout(entries: &[Entry], bo: ByteOrder) -> Result<Layout> {
    let width = find(entries, TAG_IMAGE_WIDTH)
        .ok_or_else(|| Error::invalid("TIFF: missing ImageWidth"))?
        .as_u32(bo)?;
    let height = find(entries, TAG_IMAGE_LENGTH)
        .ok_or_else(|| Error::invalid("TIFF: missing ImageLength"))?
        .as_u32(bo)?;
    if width == 0 || height == 0 {
        return Err(Error::invalid("TIFF: zero dimension"));
    }
    let short = |tag: u16, default: u32| -> Result<u16> {
        Ok(find(entries, tag)
            .map(|e| e.as_u32(bo))
            .transpose()?
            .unwrap_or(default) as u16)
    };
    let compression = short(TAG_COMPRESSION, COMPRESSION_NONE as u32)?;
    let photometric = find(entries, TAG_PHOTOMETRIC_INTERPRETATION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .ok_or_else(|| Error::invalid("TIFF: missing PhotometricInterpretation"))?
        as u16;
    let spp = short(TAG_SAMPLES_PER_PIXEL, 1)?;
    if spp == 0 {
        return Err(Error::invalid(
            "TIFF: SamplesPerPixel=0 (a pixel must have at least one component)",
        ));
    }
    let bps = match find(entries, TAG_BITS_PER_SAMPLE) {
        None => 1,
        Some(e) => e.as_u32_vec(bo)?.first().copied().unwrap_or(1) as u16,
    };
    let sample_format = match find(entries, TAG_SAMPLE_FORMAT) {
        None => SAMPLE_FORMAT_UINT,
        Some(e) => match e.as_u32_vec(bo)?.first().copied().unwrap_or(1) as u16 {
            SAMPLE_FORMAT_UNDEFINED => SAMPLE_FORMAT_UINT,
            f => f,
        },
    };
    let extra: Vec<u16> = match find(entries, TAG_EXTRA_SAMPLES) {
        None => Vec::new(),
        Some(e) => e.as_u32_vec(bo)?.iter().map(|&v| v as u16).collect(),
    };
    let orientation = short(TAG_ORIENTATION, 1)?;
    let is_float = sample_format == SAMPLE_FORMAT_IEEE_FP;
    let jpeg = matches!(compression, COMPRESSION_JPEG_OLD | COMPRESSION_JPEG_NEW);
    let deep_jpeg = jpeg && (9..=16).contains(&bps);
    let format = match (photometric, spp, bps) {
        (PHOTO_BLACK_IS_ZERO | PHOTO_WHITE_IS_ZERO | PHOTO_TRANSPARENCY_MASK, 1, 1 | 4 | 8) => {
            PixelFormat::Gray8
        }
        (PHOTO_BLACK_IS_ZERO | PHOTO_WHITE_IS_ZERO, 1, 16 | 32 | 64) if is_float => {
            PixelFormat::Gray8
        }
        (PHOTO_BLACK_IS_ZERO | PHOTO_WHITE_IS_ZERO, 1, 16) => PixelFormat::Gray16Le,
        (PHOTO_BLACK_IS_ZERO | PHOTO_WHITE_IS_ZERO, 1, _) if deep_jpeg => PixelFormat::Gray16Le,
        (PHOTO_RGB, 3, 16 | 32 | 64) if is_float => PixelFormat::Rgb24,
        (PHOTO_RGB, 3, 8) => PixelFormat::Rgb24,
        (PHOTO_RGB, 3, 16) => PixelFormat::Rgb48Le,
        (PHOTO_RGB | PHOTO_YCBCR, 3, _) if deep_jpeg => PixelFormat::Rgb48Le,
        (PHOTO_RGB, 4, 8) if extra == [EXTRA_SAMPLE_UNASSOCIATED_ALPHA] => PixelFormat::Rgba,
        (PHOTO_RGB, n, 8) if n >= 4 => PixelFormat::Rgb24,
        (PHOTO_PALETTE, 1, 4 | 8) => PixelFormat::Pal8,
        (PHOTO_CMYK, 4, 8) => PixelFormat::Cmyk,
        (PHOTO_YCBCR, 3, 8) => PixelFormat::Rgb24,
        (PHOTO_CIELAB, 3, 8) => PixelFormat::Rgb24,
        (PHOTO_CIELAB, 1, 8) => PixelFormat::Gray8,
        (p, s, b) => {
            return Err(Error::invalid(format!(
                "TIFF: photometric={p} samples_per_pixel={s} bits_per_sample={b} not supported"
            )))
        }
    };
    Ok(Layout {
        width,
        height,
        format,
        bits_per_sample: bps,
        samples_per_pixel: spp,
        photometric,
        compression,
        orientation,
    })
}

// ---- decode -----------------------------------------------------------------

/// Decode the first image (first IFD) in its native layout, with
/// colour signalling and metadata filled. Uses
/// [`DecodeOptions::default`].
pub fn decode(bytes: &[u8]) -> Result<TiffImage> {
    decode_with(bytes, &DecodeOptions::default())
}

/// [`decode`] with explicit limits / strictness.
pub fn decode_with(bytes: &[u8], opts: &DecodeOptions) -> Result<TiffImage> {
    Ok(decode_page_with(bytes, opts)?.image)
}

/// The first image as tightly packed 8-bit RGB ([`TiffImage::to_rgb8`]).
pub fn decode_rgb8(bytes: &[u8]) -> Result<RgbImage> {
    let img = decode(bytes)?;
    Ok(RgbImage::new(img.width, img.height, img.to_rgb8()))
}

/// The first image as tightly packed 8-bit RGBA ([`TiffImage::to_rgba8`]).
pub fn decode_rgba8(bytes: &[u8]) -> Result<RgbaImage> {
    let img = decode(bytes)?;
    Ok(RgbaImage::new(img.width, img.height, img.to_rgba8()))
}

/// Every image on the next-IFD chain (all pages of a multi-page TIFF),
/// in file order, as [`Frame`]s (`delay` is always `None`; the chain
/// index and the `PageNumber` / `NewSubfileType` tags ride along).
/// Uses [`DecodeOptions::default`].
pub fn decode_all(bytes: &[u8]) -> Result<Vec<Frame>> {
    decode_all_with(bytes, &DecodeOptions::default())
}

/// [`decode_all`] with explicit limits / strictness.
pub fn decode_all_with(bytes: &[u8], opts: &DecodeOptions) -> Result<Vec<Frame>> {
    Ok(decode_pages_with(bytes, opts)?
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            Frame::new(p.image, i as u32)
                .with_page_number(p.metadata.page_number)
                .with_new_subfile_type(p.metadata.new_subfile_type)
        })
        .collect())
}

/// Read `r` to its end and [`decode`] the bytes.
pub fn decode_from<R: Read>(mut r: R) -> Result<TiffImage> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)?;
    decode(&buf)
}

/// The first IFD as a [`Page`] (image, descriptive
/// [`crate::TiffMetadata`], structural [`crate::TiffFormatInfo`]).
/// Uses [`DecodeOptions::default`]; see [`crate::decode_page_with`].
pub fn decode_page(bytes: &[u8]) -> Result<Page> {
    decode_page_with(bytes, &DecodeOptions::default())
}

/// The IFD at `ifd_offset` (e.g. a `SubIFDs` child) as a [`Page`].
/// Uses [`DecodeOptions::default`]; see [`crate::decode_page_at_with`].
pub fn decode_page_at(bytes: &[u8], ifd_offset: u64) -> Result<Page> {
    decode_page_at_with(bytes, ifd_offset, &DecodeOptions::default())
}

/// Every IFD on the next-IFD chain as [`Page`]s. Uses
/// [`DecodeOptions::default`]; see [`crate::decode_pages_with`].
pub fn decode_pages(bytes: &[u8]) -> Result<Vec<Page>> {
    decode_pages_with(bytes, &DecodeOptions::default())
}

// ---- encode -----------------------------------------------------------------

/// Encode one image as a single-page TIFF.
///
/// Every native layout is written as itself (`Gray8` / `Gray16Le` /
/// `Rgb24` / `Rgb48Le` as BlackIsZero / RGB, `Rgba` as RGB +
/// `ExtraSamples = [2]`, `Pal8` as a palette-colour page with
/// `ColorMap`, `Cmyk` as `PhotometricInterpretation = 5`), so there is
/// no silent conversion. `Error::Unsupported` is returned for inputs
/// TIFF cannot carry — a `Pal8` palette with a non-opaque entry (TIFF
/// palettes have no alpha) — and for layout / compression pairs the
/// writer refuses (e.g. WebP with anything but `Rgb24` / `Rgba`, JPEG
/// with `Pal8`). `Error::InvalidData` is returned for an image whose
/// plane does not match its geometry.
pub fn encode(image: &TiffImage, opts: &EncodeOptions) -> Result<Vec<u8>> {
    let prep = PreparedPage::prepare(image, opts)?;
    let aux = prep.aux_entries();
    let page = prep.page(&aux, opts, None);
    encode_pages(std::slice::from_ref(&page))
}

/// Encode several images as one multi-page TIFF (the mirror of
/// [`decode_all`]): one IFD per frame on the next-IFD chain, in order,
/// each written exactly as [`encode`] writes a single image (its own
/// native layout, compression and metadata per `opts`). Every page
/// carries `PageNumber = (i, n)` and `NewSubfileType` bit 1 (one page
/// of a multi-page image); a frame's own `page_number` /
/// `new_subfile_type`, when set, are written instead. `delay` is
/// ignored (TIFF pages are not timed). At least one frame is required
/// (`Error::InvalidData` otherwise). The depth form with per-page
/// control over everything is [`encode_pages`] / [`EncodePage`].
pub fn encode_all(frames: &[Frame], opts: &EncodeOptions) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(Error::invalid("encode_all: at least one frame is required"));
    }
    let total = u16::try_from(frames.len()).map_err(|_| {
        Error::unsupported(format!(
            "encode_all: {} pages exceed the 16-bit PageNumber total",
            frames.len()
        ))
    })?;
    let prepared = frames
        .iter()
        .map(|f| PreparedPage::prepare(&f.image, opts))
        .collect::<Result<Vec<_>>>()?;
    let aux: Vec<_> = prepared.iter().map(PreparedPage::aux_entries).collect();
    let pages: Vec<EncodePage<'_>> = prepared
        .iter()
        .zip(&aux)
        .zip(frames)
        .enumerate()
        .map(|(i, ((p, a), f))| {
            let page_number = f.page_number.unwrap_or((i as u16, total));
            let subfile = f.new_subfile_type.unwrap_or(0b10);
            p.page(a, opts, Some((page_number, subfile)))
        })
        .collect();
    encode_pages(&pages)
}

/// The owned per-page material [`encode`] / [`encode_all`] hand to the
/// borrowing [`EncodePage`] description.
struct PreparedPage<'i> {
    image: &'i TiffImage,
    pixels: std::borrow::Cow<'i, [u8]>,
    palette: Vec<RgbColor>,
    exif_owned: Vec<OwnedEntry>,
    gps_owned: Vec<OwnedEntry>,
}

impl<'i> PreparedPage<'i> {
    fn prepare(image: &'i TiffImage, opts: &EncodeOptions) -> Result<Self> {
        let pixels = packed_pixels(image)?;
        let palette: Vec<RgbColor> = match (image.format, &image.palette) {
            (PixelFormat::Pal8, Some(p)) => {
                if p.has_alpha() {
                    return Err(Error::unsupported(
                        "TIFF encode: palette entries carry alpha, which a TIFF ColorMap cannot \
                         hold (flatten with to_rgba8 / encode_rgba8 to keep the transparency)",
                    ));
                }
                if p.is_empty() || p.len() > 256 {
                    return Err(Error::invalid(format!(
                        "TIFF encode: Pal8 palette must have 1..=256 entries, got {}",
                        p.len()
                    )));
                }
                p.to_rgb_triples()
            }
            (PixelFormat::Pal8, None) => {
                return Err(Error::invalid("TIFF encode: Pal8 image without a palette"));
            }
            _ => Vec::new(),
        };
        // Metadata blobs → page extras. The Exif payload is parsed into
        // owned entry lists first; the borrowing `AuxIfdEntry` views
        // must outlive the page description.
        let (exif_owned, gps_owned): (Vec<OwnedEntry>, Vec<OwnedEntry>) =
            match (opts.embed_exif, &image.metadata.exif) {
                (true, Some(blob)) => parse_exif_payload(blob)?,
                _ => (Vec::new(), Vec::new()),
            };
        Ok(Self {
            image,
            pixels,
            palette,
            exif_owned,
            gps_owned,
        })
    }

    fn aux_entries(&self) -> (Vec<AuxIfdEntry<'_>>, Vec<AuxIfdEntry<'_>>) {
        (
            self.exif_owned.iter().map(aux_view).collect(),
            self.gps_owned.iter().map(aux_view).collect(),
        )
    }

    /// The borrowing page description; `paging` = `(PageNumber,
    /// NewSubfileType)` for a multi-page chain.
    fn page<'p>(
        &'p self,
        aux: &'p (Vec<AuxIfdEntry<'p>>, Vec<AuxIfdEntry<'p>>),
        opts: &'p EncodeOptions,
        paging: Option<((u16, u16), u32)>,
    ) -> EncodePage<'p> {
        let image = self.image;
        let pixels: &[u8] = &self.pixels;
        let kind = match image.format {
            PixelFormat::Gray8 => EncodePixelFormat::Gray8 { pixels },
            PixelFormat::Gray16Le => EncodePixelFormat::Gray16Le { pixels },
            PixelFormat::Rgb24 => EncodePixelFormat::Rgb24 { pixels },
            PixelFormat::Rgb48Le => EncodePixelFormat::Rgb48 { pixels },
            PixelFormat::Rgba => EncodePixelFormat::Rgba32 {
                pixels,
                kind: ExtraSampleKind::UnassociatedAlpha,
            },
            PixelFormat::Pal8 => EncodePixelFormat::Palette8 {
                indices: pixels,
                palette: &self.palette,
            },
            PixelFormat::Cmyk => EncodePixelFormat::Cmyk32 { pixels },
        };
        let (exif_entries, gps_entries) = aux;
        let extras = PageExtras {
            resolution: opts.resolution,
            software: opts.software.as_deref(),
            xmp: if opts.embed_xmp {
                image.metadata.xmp.as_deref()
            } else {
                None
            },
            icc_profile: if opts.embed_icc {
                image.metadata.icc.as_deref()
            } else {
                None
            },
            exif_ifd: if exif_entries.is_empty() {
                None
            } else {
                Some(exif_entries)
            },
            gps_ifd: if gps_entries.is_empty() {
                None
            } else {
                Some(gps_entries)
            },
            rows_per_strip: opts.rows_per_strip,
            page_number: paging.map(|(n, _)| n),
            reduced_resolution: paging.is_some_and(|(_, t)| t & 0b1 != 0),
            multi_page: paging.is_some_and(|(_, t)| t & 0b10 != 0),
            ..PageExtras::default()
        };
        EncodePage {
            width: image.width,
            height: image.height,
            kind,
            compression: opts.compression,
            predictor: opts.predictor,
            planar: opts.planar,
            tiling: opts.tiling,
            bigtiff: opts.bigtiff,
            extras,
        }
    }
}

fn aux_view(e: &OwnedEntry) -> AuxIfdEntry<'_> {
    AuxIfdEntry {
        tag: e.tag,
        field_type: e.field_type,
        count: e.count,
        value: &e.value,
    }
}

/// The image's pixels as the tightly packed row-major buffer the
/// writer consumes (a copy only when the plane carries row padding).
fn packed_pixels(image: &TiffImage) -> Result<std::borrow::Cow<'_, [u8]>> {
    if image.width == 0 || image.height == 0 {
        return Err(Error::invalid("TIFF encode: zero dimension"));
    }
    let plane = image
        .planes
        .first()
        .ok_or_else(|| Error::invalid("TIFF encode: image has no plane"))?;
    if image.planes.len() != 1 {
        return Err(Error::invalid(format!(
            "TIFF encode: packed layouts take exactly one plane, got {}",
            image.planes.len()
        )));
    }
    let w = image.width as usize;
    let h = image.height as usize;
    let row = w * image.format.bytes_per_pixel();
    if plane.stride < row {
        return Err(Error::invalid(format!(
            "TIFF encode: stride {} < {row} bytes per row",
            plane.stride
        )));
    }
    let need = plane
        .stride
        .checked_mul(h)
        .ok_or_else(|| Error::invalid("TIFF encode: plane size overflows"))?;
    if plane.data.len() < need {
        return Err(Error::invalid(format!(
            "TIFF encode: plane holds {} bytes, {need} needed",
            plane.data.len()
        )));
    }
    if plane.stride == row {
        return Ok(std::borrow::Cow::Borrowed(&plane.data[..need]));
    }
    let mut out = Vec::with_capacity(row * h);
    for y in 0..h {
        out.extend_from_slice(&plane.data[y * plane.stride..y * plane.stride + row]);
    }
    Ok(std::borrow::Cow::Owned(out))
}

/// Encode tightly packed 8-bit RGB (`3 × width × height` bytes) as an
/// `Rgb24` page (`PhotometricInterpretation = 2`).
pub fn encode_rgb8(width: u32, height: u32, rgb: &[u8], opts: &EncodeOptions) -> Result<Vec<u8>> {
    encode(&TiffImage::from_rgb8(width, height, rgb.to_vec())?, opts)
}

/// Encode tightly packed 8-bit RGBA (`4 × width × height` bytes) as an
/// RGB page with one unassociated-alpha extra sample
/// (`ExtraSamples = [2]`); TIFF carries alpha, so nothing is dropped.
pub fn encode_rgba8(width: u32, height: u32, rgba: &[u8], opts: &EncodeOptions) -> Result<Vec<u8>> {
    encode(&TiffImage::from_rgba8(width, height, rgba.to_vec())?, opts)
}

/// [`encode`] straight into a writer.
pub fn encode_to<W: Write>(image: &TiffImage, opts: &EncodeOptions, mut w: W) -> Result<()> {
    let bytes = encode(image, opts)?;
    w.write_all(&bytes)?;
    Ok(())
}

// ---- deprecated pre-contract entry points ----------------------------------

/// Pre-contract decode of the first IFD.
#[deprecated(note = "use oxideav_tiff::decode or decode_page (IMAGE_CRATE_API)")]
#[allow(deprecated)]
pub fn decode_tiff(input: &[u8]) -> Result<DecodedTiff> {
    Ok(decode_page(input)?.into())
}

/// Pre-contract decode of the IFD at `ifd_offset`.
#[deprecated(note = "use oxideav_tiff::decode_page_at (IMAGE_CRATE_API)")]
#[allow(deprecated)]
pub fn decode_tiff_at(input: &[u8], ifd_offset: u64) -> Result<DecodedTiff> {
    Ok(decode_page_at(input, ifd_offset)?.into())
}

/// Pre-contract decode of every page as bare images (flattened to the
/// historical display layouts).
#[deprecated(note = "use oxideav_tiff::decode_all (IMAGE_CRATE_API)")]
pub fn decode_tiff_all(input: &[u8]) -> Result<Vec<TiffImage>> {
    Ok(decode_pages(input)?
        .into_iter()
        .map(|p| legacy_display_layout(p.image))
        .collect())
}

/// Pre-contract decode of every page with its metadata.
#[deprecated(note = "use oxideav_tiff::decode_pages (IMAGE_CRATE_API)")]
#[allow(deprecated)]
pub fn decode_tiff_all_pages(input: &[u8]) -> Result<Vec<DecodedTiff>> {
    Ok(decode_pages(input)?.into_iter().map(Into::into).collect())
}

/// Pre-contract single-page encode.
#[deprecated(note = "use oxideav_tiff::encode or encode_page (IMAGE_CRATE_API)")]
pub fn encode_tiff(page: &EncodePage<'_>) -> Result<Vec<u8>> {
    encode_pages(std::slice::from_ref(page))
}

/// Pre-contract multi-page encode.
#[deprecated(note = "use oxideav_tiff::encode_pages (IMAGE_CRATE_API)")]
pub fn encode_tiff_multi(pages: &[EncodePage<'_>]) -> Result<Vec<u8>> {
    encode_pages(pages)
}
