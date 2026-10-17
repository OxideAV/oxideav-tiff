//! `oxideav-core` integration layer for `oxideav-tiff`.
//!
//! Gated behind the default-on `registry` feature so image-library
//! consumers can depend on `oxideav-tiff` with `default-features = false`
//! and skip the `oxideav-core` dependency entirely.
//!
//! The module exposes:
//! * [`register`] (fleet signature, `&mut RuntimeContext`) /
//!   [`register_codecs`] / [`register_containers`] — the registry entry
//!   points the umbrella `oxideav` crate calls during framework
//!   initialisation.
//! * [`make_decoder`] / [`make_encoder`] and the [`TiffDecoder`] /
//!   [`TiffEncoder`] trait impls — thin adapters over the standalone
//!   [`crate::decode_with`] / [`crate::encode`] (one implementation).
//! * `From<TiffImage> for VideoFrame`, [`TiffImage::from_video_frame`]
//!   and `TryFrom<(&VideoFrame, &CodecParameters)>` — the plane plus
//!   the palette / colour-signal / metadata-blobs side-channels.
//! * [`metadata_to_blobs`] / [`metadata_from_blobs`] — the image's
//!   ICC / Exif / XMP / IPTC as framework [`MetadataBlob`]s and back.
//! * `From<TiffError> for oxideav_core::Error` and the
//!   `CodecOptionsStruct` schema for [`EncodeOptions`].
//!
//! # Metadata blobs
//!
//! The four payloads TIFF carries travel on the framework's blob
//! carriage (`oxideav_core::blob`): the demuxer puts the first page of
//! each stream's blobs on `CodecParameters::blobs`, the decoder
//! attaches every page's own blobs to its frame's record (pages
//! differ), and the encoder writes the union — a frame blob overrides
//! a stream blob of the same kind. The payload conventions match the
//! kinds' definitions: a complete ICC profile (tag 34675), the Exif
//! IFD + GPS IFD re-serialised as a TIFF-structured Exif block
//! starting at the byte-order mark ([`crate::Metadata::exif`]), the XMP
//! packet (tag 700) and the IPTC-IIM record stream (tag 33723).
//!
//! # Multi-page files
//!
//! The demuxer declares one stream per distinct page geometry
//! (`width × height × layout`, first-appearance order — a uniform
//! multi-page file is one stream), each carrying the whole file as its
//! single packet; the decoder of a stream returns the pages whose
//! native layout equals the stream's declared one, one frame per page
//! (`pts` = the page's position on the IFD chain), so every frame's
//! label is truthful. A decoder built without a geometry returns every
//! page.

use std::collections::VecDeque;

use oxideav_core::{
    frame::VideoPlane, parse_options, BlobKind, CodecCapabilities, CodecId, CodecInfo,
    CodecOptionsStruct, CodecParameters, CodecRegistry, ColorPrimaries, ColorSignal,
    ContainerRegistry, Decoder, Encoder, Error, Frame, MatrixCoefficients, MediaType, MetadataBlob,
    OptionField, OptionKind, OptionValue, Packet, PixelFormat, Result, RuntimeContext, TimeBase,
    TransferCharacteristics, VideoFrame,
};

use crate::container;
use crate::decoder::{decode_page_entries, ifd_chain};
use crate::encoder::TiffCompression;
use crate::error::TiffError;
use crate::ifd::{parse_header, ByteOrder, Entry, ParsedHeader};
use crate::image::{ColorInfo, ColorRange, Metadata, Palette, Plane, TiffImage, TiffPixelFormat};
use crate::options::{DecodeOptions, EncodeOptions};
use crate::CODEC_ID_STR;

impl From<TiffError> for Error {
    fn from(e: TiffError) -> Self {
        match e {
            TiffError::InvalidData(s) => Error::InvalidData(s),
            TiffError::Unsupported(s) => Error::Unsupported(s),
            TiffError::LimitExceeded(s) => Error::InvalidData(s),
            TiffError::Io(e) => Error::Io(e),
        }
    }
}

// ---- Pixel-format and colour bridges ----------------------------------------

impl From<TiffPixelFormat> for PixelFormat {
    fn from(p: TiffPixelFormat) -> Self {
        match p {
            TiffPixelFormat::Gray8 => PixelFormat::Gray8,
            TiffPixelFormat::Gray16Le => PixelFormat::Gray16Le,
            TiffPixelFormat::Rgb24 => PixelFormat::Rgb24,
            TiffPixelFormat::Rgb48Le => PixelFormat::Rgb48Le,
            TiffPixelFormat::Rgba => PixelFormat::Rgba,
            TiffPixelFormat::Pal8 => PixelFormat::Pal8,
            TiffPixelFormat::Cmyk => PixelFormat::Cmyk,
            TiffPixelFormat::GrayF32Le => PixelFormat::GrayF32Le,
            TiffPixelFormat::RgbF32Le => PixelFormat::RgbF32Le,
        }
    }
}

impl TryFrom<PixelFormat> for TiffPixelFormat {
    type Error = Error;
    fn try_from(p: PixelFormat) -> Result<Self> {
        Ok(match p {
            PixelFormat::Gray8 => TiffPixelFormat::Gray8,
            PixelFormat::Gray16Le => TiffPixelFormat::Gray16Le,
            PixelFormat::Rgb24 => TiffPixelFormat::Rgb24,
            PixelFormat::Rgb48Le => TiffPixelFormat::Rgb48Le,
            PixelFormat::Rgba => TiffPixelFormat::Rgba,
            PixelFormat::Pal8 => TiffPixelFormat::Pal8,
            PixelFormat::Cmyk => TiffPixelFormat::Cmyk,
            PixelFormat::GrayF32Le => TiffPixelFormat::GrayF32Le,
            PixelFormat::RgbF32Le => TiffPixelFormat::RgbF32Le,
            other => {
                return Err(Error::unsupported(format!(
                    "TIFF: pixel format {other:?} has no TIFF layout (Gray8 / Gray16Le / \
                     Rgb24 / Rgb48Le / Rgba / Pal8 / Cmyk / GrayF32Le / RgbF32Le)"
                )))
            }
        })
    }
}

/// [`ColorInfo`] as the framework's [`ColorSignal`] (code points map
/// 1:1; `Unspecified` range stays unspecified).
pub fn to_color_signal(c: &ColorInfo) -> ColorSignal {
    let range = match c.range {
        ColorRange::Unspecified => oxideav_core::ColorRange::Unspecified,
        ColorRange::Limited => oxideav_core::ColorRange::Limited,
        ColorRange::Full => oxideav_core::ColorRange::Full,
    };
    ColorSignal::new(
        range,
        ColorPrimaries(c.primaries),
        TransferCharacteristics(c.transfer),
        MatrixCoefficients(c.matrix),
    )
}

/// The inverse of [`to_color_signal`].
pub fn from_color_signal(s: &ColorSignal) -> ColorInfo {
    let range = match s.range {
        oxideav_core::ColorRange::Limited => ColorRange::Limited,
        oxideav_core::ColorRange::Full => ColorRange::Full,
        _ => ColorRange::Unspecified,
    };
    ColorInfo::new(range, s.primaries.0, s.transfer.0, s.matrix.0)
}

// ---- Metadata ⇄ blobs -----------------------------------------------------------

/// The image's metadata as framework blobs, in the well-known order:
/// [`BlobKind::ICC`] (tag 34675), [`BlobKind::EXIF`] (the Exif + GPS
/// IFDs as a TIFF-structured Exif block), [`BlobKind::XMP`] (tag 700)
/// and [`BlobKind::IPTC`] (tag 33723) — one blob per payload the image
/// carries, `data` verbatim, no `mime`. `gamma` has no blob (TIFF
/// carries none).
pub fn metadata_to_blobs(metadata: &Metadata) -> Vec<MetadataBlob> {
    let mut out = Vec::with_capacity(4);
    for (kind, payload) in [
        (BlobKind::ICC, &metadata.icc),
        (BlobKind::EXIF, &metadata.exif),
        (BlobKind::XMP, &metadata.xmp),
        (BlobKind::IPTC, &metadata.iptc),
    ] {
        if let Some(p) = payload {
            out.push(MetadataBlob::new(kind, p.clone()));
        }
    }
    out
}

/// The first blob of each well-known kind TIFF can carry (ICC / Exif /
/// XMP / IPTC) as the image's [`Metadata`]; blobs of any other kind
/// are left alone (TIFF has no tag for them), `gamma` stays `None`.
pub fn metadata_from_blobs(blobs: &[MetadataBlob]) -> Metadata {
    let first = |kind: BlobKind| blobs.iter().find(|b| b.is(&kind)).map(|b| b.data.clone());
    Metadata::new()
        .with_icc(first(BlobKind::ICC))
        .with_exif(first(BlobKind::EXIF))
        .with_xmp(first(BlobKind::XMP))
        .with_iptc(first(BlobKind::IPTC))
}

/// The blobs a page is written with: the frame's own first, then the
/// stream's blobs of every kind the frame does not carry (a frame blob
/// overrides a stream blob of the same kind — the gateway's
/// precedence rule).
pub fn merge_blobs(frame: &[MetadataBlob], stream: &[MetadataBlob]) -> Vec<MetadataBlob> {
    let mut out: Vec<MetadataBlob> = frame.to_vec();
    out.extend(
        stream
            .iter()
            .filter(|s| !frame.iter().any(|f| f.kind == s.kind))
            .cloned(),
    );
    out
}

// ---- TiffImage ⇄ VideoFrame ----------------------------------------------------

fn stamp_frame_side_channels(frame: &mut VideoFrame, image: &TiffImage) {
    if let (TiffPixelFormat::Pal8, Some(p)) = (image.format, &image.palette) {
        frame.set_palette(p.to_rgb_triples().concat());
        if p.has_alpha() {
            frame.set_palette_alpha(p.entries.iter().map(|e| e[3]).collect());
        }
    }
    let c = image.color;
    if c.primaries != ColorInfo::UNSPECIFIED
        || c.transfer != ColorInfo::UNSPECIFIED
        || c.range == ColorRange::Limited
    {
        frame.set_color_signal(to_color_signal(&c));
    }
    frame.set_blobs(metadata_to_blobs(&image.metadata));
}

/// [`From<TiffImage>`] with an explicit `pts`, moving the plane.
pub(crate) fn image_into_video_frame(mut image: TiffImage, pts: Option<i64>) -> VideoFrame {
    let stride = image.stride();
    let data = if image.planes.is_empty() {
        Vec::new()
    } else {
        std::mem::take(&mut image.planes[0].data)
    };
    let mut frame = VideoFrame {
        pts,
        planes: vec![VideoPlane { stride, data }],
    };
    stamp_frame_side_channels(&mut frame, &image);
    frame
}

impl From<TiffImage> for VideoFrame {
    /// The pixel plane (`pts` `None`), plus the palette side-channel
    /// for `Pal8` (and its alpha record when an entry is not opaque),
    /// the colour-signal side-channel when the image signals more than
    /// the TIFF default, and the metadata-blobs record
    /// ([`metadata_to_blobs`]) when the image carries metadata.
    fn from(image: TiffImage) -> Self {
        image_into_video_frame(image, None)
    }
}

impl From<&TiffImage> for VideoFrame {
    fn from(image: &TiffImage) -> Self {
        image_into_video_frame(image.clone(), None)
    }
}

impl From<TiffImage> for Frame {
    fn from(img: TiffImage) -> Self {
        Frame::Video(img.into())
    }
}

impl TiffImage {
    /// Rebuild an image from a framework frame and the stream
    /// parameters that describe it (`width`, `height` and
    /// `pixel_format` are required). The frame's palette side-channel
    /// becomes [`TiffImage::palette`] for `Pal8` (falling back to the
    /// `extradata` RGB triples; the palette-alpha record, when
    /// attached, fills the entries' alpha); its colour-signal
    /// side-channel becomes [`TiffImage::color`]; its metadata blobs,
    /// then the stream's `params.blobs` of every kind the frame does
    /// not carry ([`merge_blobs`]), become [`TiffImage::metadata`]
    /// ([`metadata_from_blobs`]).
    pub fn from_video_frame(frame: &VideoFrame, params: &CodecParameters) -> crate::Result<Self> {
        let width = params
            .width
            .ok_or_else(|| TiffError::invalid("TIFF: missing width"))?;
        let height = params
            .height
            .ok_or_else(|| TiffError::invalid("TIFF: missing height"))?;
        let pix = params
            .pixel_format
            .ok_or_else(|| TiffError::invalid("TIFF: missing pixel_format"))?;
        let pix = TiffPixelFormat::try_from(pix).map_err(|_| {
            TiffError::unsupported(format!(
                "TIFF: pixel format {pix:?} has no TIFF layout (Gray8 / Gray16Le / Rgb24 / \
                 Rgb48Le / Rgba / Pal8 / Cmyk / GrayF32Le / RgbF32Le)"
            ))
        })?;
        let plane = frame
            .image_planes()
            .first()
            .ok_or_else(|| TiffError::invalid("TIFF: frame has no planes"))?;
        let mut img = TiffImage::new(
            width,
            height,
            pix,
            vec![Plane::new(plane.stride, plane.data.clone())],
        )?;
        if pix == TiffPixelFormat::Pal8 {
            let rgb: Option<&[u8]> = frame
                .palette()
                .or((!params.extradata.is_empty()).then_some(params.extradata.as_slice()));
            let alpha = frame.palette_alpha().unwrap_or(&[]);
            img.palette = rgb.map(|rgb| {
                Palette::new(
                    rgb.chunks_exact(3)
                        .enumerate()
                        .map(|(i, c)| [c[0], c[1], c[2], alpha.get(i).copied().unwrap_or(255)])
                        .collect(),
                )
            });
            if img.palette.is_none() {
                return Err(TiffError::invalid(
                    "TIFF: Pal8 frame without a palette side-channel or extradata",
                ));
            }
        }
        if let Some(sig) = frame.color_signal() {
            img.color = from_color_signal(&sig);
        }
        img.metadata = metadata_from_blobs(&merge_blobs(&frame.blobs(), &params.blobs));
        Ok(img)
    }
}

impl TryFrom<(&VideoFrame, &CodecParameters)> for TiffImage {
    type Error = TiffError;
    fn try_from((frame, params): (&VideoFrame, &CodecParameters)) -> crate::Result<Self> {
        TiffImage::from_video_frame(frame, params)
    }
}

// ---- CodecOptionsStruct (registry-only schema for EncodeOptions) ------------

/// The framework's advisory `quality` (0..=100) onto a JPEG-in-TIFF
/// scheme: `1..=99` is the DCT quantiser scale, `100` the lossless
/// `SOF3` process (predictor 1), `0` the lowest DCT quality. Any other
/// compression scheme is lossless already and ignores it — the
/// option never switches the scheme.
fn apply_quality(opts: &mut EncodeOptions, quality: u8) {
    if let TiffCompression::Jpeg(ref mut j) = opts.compression {
        if quality >= 100 {
            j.process = crate::JpegProcess::Lossless { predictor: 1 };
        } else {
            j.quality = quality.max(1);
            j.process = crate::JpegProcess::Dct;
        }
    }
}

/// The registry encoder's option bag: [`EncodeOptions`] plus the
/// framework's advisory `quality`, applied after every other option so
/// `{"quality": "90", "compression": "jpeg"}` and the reverse order
/// resolve alike ([`resolve`](Self::resolve)). Same schema as
/// [`EncodeOptions`].
// internal — the factory's parse target; not part of the stable API
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct RegistryEncodeOptions {
    pub inner: EncodeOptions,
    pub quality: Option<u8>,
}

impl RegistryEncodeOptions {
    /// The resolved [`EncodeOptions`].
    pub fn resolve(mut self) -> EncodeOptions {
        if let Some(q) = self.quality {
            apply_quality(&mut self.inner, q);
        }
        self.inner
    }
}

impl CodecOptionsStruct for RegistryEncodeOptions {
    const SCHEMA: &'static [OptionField] = <EncodeOptions as CodecOptionsStruct>::SCHEMA;

    fn apply(&mut self, key: &str, value: &OptionValue) -> Result<()> {
        if key == "quality" {
            let q = value.as_u32()?;
            if q > 100 {
                return Err(Error::invalid(format!(
                    "TIFF encoder: quality {q} out of 0..=100"
                )));
            }
            self.quality = Some(q as u8);
            return Ok(());
        }
        self.inner.apply(key, value)
    }
}

impl CodecOptionsStruct for EncodeOptions {
    const SCHEMA: &'static [OptionField] = &[
        OptionField {
            name: "compression",
            kind: OptionKind::String,
            default: OptionValue::String(String::new()),
            help: "Compression scheme (tag 259): `none` (default), `packbits`, `lzw`, \
                   `deflate`, `zstd`, `webp` (Rgb24 / Rgba only), `jpeg` (quality from \
                   `quality`).",
        },
        OptionField {
            name: "quality",
            kind: OptionKind::U32,
            default: OptionValue::U32(75),
            help: "Advisory quality 0..=100: governs JPEG-in-TIFF only (compression = jpeg; \
                   1..=99 = the DCT quantiser scale, 100 = the lossless SOF3 process); \
                   ignored by every other scheme, in any option order.",
        },
        OptionField {
            name: "predictor",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "Apply the TIFF 6.0 §14 horizontal-differencing predictor (Predictor = 2).",
        },
        OptionField {
            name: "planar",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "Write PlanarConfiguration = 2 (one plane per component).",
        },
        OptionField {
            name: "tile",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "Square §15 tile size (multiple of 16); 0 writes strips.",
        },
        OptionField {
            name: "rows_per_strip",
            kind: OptionKind::U32,
            default: OptionValue::U32(0),
            help: "RowsPerStrip for a stripped page; 0 writes one strip.",
        },
        OptionField {
            name: "bigtiff",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "Write a BigTIFF (version 43, 8-byte offsets).",
        },
    ];

    fn apply(&mut self, key: &str, value: &OptionValue) -> Result<()> {
        match key {
            "compression" => {
                let s = value.as_str()?;
                let quality = match self.compression {
                    TiffCompression::Jpeg(j) => j.quality,
                    _ => 75,
                };
                self.compression = match s.to_ascii_lowercase().as_str() {
                    "" | "none" => TiffCompression::None,
                    "packbits" => TiffCompression::PackBits,
                    "lzw" => TiffCompression::Lzw,
                    "deflate" | "zip" => TiffCompression::Deflate,
                    "zstd" => TiffCompression::Zstd,
                    "webp" => TiffCompression::Webp,
                    "jpeg" => TiffCompression::Jpeg(crate::JpegOptions {
                        quality,
                        ..Default::default()
                    }),
                    other => {
                        return Err(Error::invalid(format!(
                            "TIFF encoder: unknown compression '{other}'"
                        )))
                    }
                };
            }
            "quality" => {
                let q = value.as_u32()?;
                if q > 100 {
                    return Err(Error::invalid(format!(
                        "TIFF encoder: quality {q} out of 0..=100"
                    )));
                }
                // Advisory: governs JPEG-in-TIFF only, never switches
                // the compression scheme (see `apply_quality`).
                apply_quality(self, q as u8);
            }
            "predictor" => self.predictor = value.as_bool()?,
            "planar" => self.planar = value.as_bool()?,
            "tile" => {
                let t = value.as_u32()?;
                self.tiling = if t == 0 { None } else { Some((t, t)) };
            }
            "rows_per_strip" => {
                let r = value.as_u32()?;
                self.rows_per_strip = if r == 0 { None } else { Some(r) };
            }
            "bigtiff" => self.bigtiff = value.as_bool()?,
            _ => unreachable!("guarded by SCHEMA"),
        }
        Ok(())
    }
}

// ---- Registration -------------------------------------------------------------

/// The input layouts the registry encoder codes, in the ladder's
/// order (the closest colour rung first, then the other colour
/// families, grey last): every one is written as itself — see
/// [`TiffPixelFormat`] — and anything else is a typed `Unsupported`
/// refusal from [`make_encoder`], never a silent conversion.
pub const ACCEPTED_PIXEL_FORMATS: [PixelFormat; 9] = [
    PixelFormat::Rgb24,
    PixelFormat::Rgba,
    PixelFormat::Rgb48Le,
    PixelFormat::RgbF32Le,
    PixelFormat::Cmyk,
    PixelFormat::Pal8,
    PixelFormat::Gray8,
    PixelFormat::Gray16Le,
    PixelFormat::GrayF32Le,
];

/// Register the TIFF codec (decoder + encoder) into the supplied
/// [`CodecRegistry`]. The capabilities declare
/// [`ACCEPTED_PIXEL_FORMATS`] as the encoder's accepted set.
pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("tiff_sw")
        .with_intra_only(true)
        .with_lossless(true)
        .with_max_size(65535, 65535)
        .with_pixel_formats(ACCEPTED_PIXEL_FORMATS.to_vec());
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder)
            .encoder_options::<RegistryEncodeOptions>(),
    );
}

/// Register the TIFF container demuxer + muxer + extension + probe.
pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}

/// Unified registration entry point — installs the TIFF codec into
/// the codec sub-registry and the TIFF container into the container
/// sub-registry of the supplied [`RuntimeContext`].
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

/// Pre-contract two-registry form of [`register`].
#[deprecated(note = "use oxideav_tiff::register(&mut RuntimeContext) (IMAGE_CRATE_API)")]
pub fn register_into(codecs: &mut CodecRegistry, containers: &mut ContainerRegistry) {
    register_codecs(codecs);
    register_containers(containers);
}

oxideav_core::register!("tiff", register);

// ---- Decoder --------------------------------------------------------------------

/// Factory registered with the codec registry. The framework's
/// [`oxideav_core::DecoderLimits`] tighten the standalone
/// [`DecodeOptions`] (never loosen them). When `params` carry a
/// geometry (`width`, `height`, `pixel_format`) the decoder returns
/// the pages of each packet whose native layout equals it (the
/// demuxer's one-stream-per-geometry contract); without one — or when
/// no page matches — it returns every page.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let limits = params.limits();
    let mut opts = DecodeOptions::default();
    opts.max_pixels = Some(opts.max_pixels.map_or(limits.max_pixels_per_frame, |m| {
        m.min(limits.max_pixels_per_frame)
    }));
    opts.max_bytes = Some(
        opts.max_bytes
            .map_or(limits.max_alloc_bytes_per_frame, |m| {
                m.min(limits.max_alloc_bytes_per_frame)
            }),
    );
    let want = match (params.width, params.height, params.pixel_format) {
        (Some(w), Some(h), Some(pf)) => Some((w, h, pf)),
        _ => None,
    };
    Ok(Box::new(TiffDecoder {
        codec_id: CodecId::new(CODEC_ID_STR),
        opts,
        want,
        pending: None,
        eof: false,
    }))
}

/// A page's geometry as the stream declares it: the layout
/// [`crate::decode`] returns, after `Orientation`.
pub(crate) fn page_geometry(
    entries: &[Entry],
    bo: ByteOrder,
) -> crate::Result<(u32, u32, PixelFormat)> {
    let l = crate::api::native_layout(entries, bo)?;
    let (w, h) = if l.orientation >= 5 {
        (l.height, l.width)
    } else {
        (l.width, l.height)
    };
    Ok((w, h, l.format.into()))
}

/// One packet's file, parsed, with the pages still to be returned.
struct PendingFile {
    data: Vec<u8>,
    header: ParsedHeader,
    chain: Vec<Vec<Entry>>,
    pages: VecDeque<usize>,
    base_pts: Option<i64>,
}

/// TIFF `Decoder` trait impl: each `send_packet` carries one complete
/// TIFF file; the following `receive_frame`s return its pages (those
/// of the stream's geometry, or all of them — see [`make_decoder`]),
/// one per call, decoded through the standalone page decoder with the
/// page's own metadata on the frame's blobs record. A frame's `pts`
/// is the packet's `pts` plus the page's position on the IFD chain.
pub struct TiffDecoder {
    codec_id: CodecId,
    opts: DecodeOptions,
    want: Option<(u32, u32, PixelFormat)>,
    pending: Option<PendingFile>,
    eof: bool,
}

impl Decoder for TiffDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let data = packet.data.clone();
        let header = parse_header(&data)?;
        let chain = ifd_chain(&data, &header)?;
        let bo = header.byte_order;
        let mut resolvable: VecDeque<usize> = VecDeque::new();
        let mut matching: VecDeque<usize> = VecDeque::new();
        let mut first_err: Option<TiffError> = None;
        for (i, entries) in chain.iter().enumerate() {
            match page_geometry(entries, bo) {
                Ok(g) => {
                    resolvable.push_back(i);
                    if self.want == Some(g) {
                        matching.push_back(i);
                    }
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        if resolvable.is_empty() {
            return Err(first_err
                .unwrap_or_else(|| TiffError::invalid("TIFF: no IFDs in file"))
                .into());
        }
        let pages = if matching.is_empty() {
            resolvable
        } else {
            matching
        };
        self.pending = Some(PendingFile {
            data,
            header,
            chain,
            pages,
            base_pts: packet.pts,
        });
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> {
        let Some(file) = self.pending.as_mut() else {
            return Err(if self.eof {
                Error::Eof
            } else {
                Error::NeedMore
            });
        };
        let Some(i) = file.pages.pop_front() else {
            self.pending = None;
            return Err(if self.eof {
                Error::Eof
            } else {
                Error::NeedMore
            });
        };
        let page = decode_page_entries(&file.data, &file.header, &file.chain[i], &self.opts);
        let page = match page {
            Ok(p) => p,
            Err(e) => {
                self.pending = None;
                return Err(e.into());
            }
        };
        let pts = match file.base_pts {
            Some(p) => Some(p + i as i64),
            None => (i > 0).then_some(i as i64),
        };
        if file.pages.is_empty() {
            self.pending = None;
        }
        Ok(Frame::Video(image_into_video_frame(page.image, pts)))
    }
    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
}

// ---- Encoder --------------------------------------------------------------------

/// Factory registered with the codec registry: one packet (a complete
/// single-page TIFF file) per frame, through [`crate::encode`]. The
/// stream's `params.blobs` are written into every page, overridden
/// per kind by the blobs on a frame's own record ([`merge_blobs`]).
pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    let opts = parse_options::<RegistryEncodeOptions>(&params.options)?.resolve();
    let width = params
        .width
        .ok_or_else(|| Error::invalid("TIFF encoder: missing width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("TIFF encoder: missing height"))?;
    let pix_core = params.pixel_format.unwrap_or(PixelFormat::Rgb24);
    let pix = TiffPixelFormat::try_from(pix_core)?;
    check_layout_scheme(pix, &opts.compression)?;

    let mut output_params = params.clone();
    output_params.media_type = MediaType::Video;
    output_params.codec_id = CodecId::new(CODEC_ID_STR);
    output_params.width = Some(width);
    output_params.height = Some(height);
    output_params.pixel_format = Some(pix_core);

    Ok(Box::new(TiffEncoder {
        output_params,
        opts,
        pending: std::collections::VecDeque::new(),
        eof: false,
    }))
}

/// The layout × scheme pairs the writer refuses, refused up front with
/// a typed `Unsupported` (the writer's own message would only come
/// with the first frame): WebP (50001) codes 8-bit chunky RGB / RGBA
/// only; JPEG-in-TIFF (7) codes greyscale / RGB / CMYK — 8-bit under
/// either process, 16-bit under the lossless process only — never
/// palette, alpha or float pages.
fn check_layout_scheme(pix: TiffPixelFormat, scheme: &TiffCompression) -> Result<()> {
    let refuse = |why: &str| {
        Err(Error::unsupported(format!(
            "TIFF encoder: {pix:?} cannot be written with {why}; accepted: {}",
            match scheme {
                TiffCompression::Webp => "Rgb24 / Rgba",
                _ => "Gray8 / Rgb24 / Cmyk (DCT or lossless), Gray16Le / Rgb48Le (lossless)",
            }
        )))
    };
    match scheme {
        TiffCompression::Webp if !matches!(pix, TiffPixelFormat::Rgb24 | TiffPixelFormat::Rgba) => {
            refuse("WebP-in-TIFF (Compression = 50001)")
        }
        TiffCompression::Jpeg(j) => match pix {
            TiffPixelFormat::Gray8 | TiffPixelFormat::Rgb24 | TiffPixelFormat::Cmyk => Ok(()),
            TiffPixelFormat::Gray16Le | TiffPixelFormat::Rgb48Le
                if matches!(j.process, crate::JpegProcess::Lossless { .. }) =>
            {
                Ok(())
            }
            TiffPixelFormat::Gray16Le | TiffPixelFormat::Rgb48Le => {
                refuse("the JPEG-in-TIFF DCT process (16-bit input needs quality 100 / the lossless process)")
            }
            _ => refuse("JPEG-in-TIFF (Compression = 7)"),
        },
        _ => Ok(()),
    }
}

/// TIFF `Encoder` trait impl: every video frame becomes one packet
/// holding a complete single-page TIFF file carrying the merged
/// stream + frame metadata blobs.
pub struct TiffEncoder {
    output_params: CodecParameters,
    opts: EncodeOptions,
    pending: std::collections::VecDeque<Packet>,
    eof: bool,
}

impl Encoder for TiffEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.output_params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.output_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let v = match frame {
            Frame::Video(v) => v,
            _ => return Err(Error::invalid("TIFF encoder: video frames only")),
        };
        let img = TiffImage::from_video_frame(v, &self.output_params)?;
        let bytes = crate::encode(&img, &self.opts)?;
        let mut pkt = Packet::new(0, TimeBase::new(1, 1), bytes);
        pkt.pts = v.pts;
        pkt.dts = v.pts;
        pkt.flags.keyframe = true;
        self.pending.push_back(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        match self.pending.pop_front() {
            Some(p) => Ok(p),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_installs_codec_and_container() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let id = CodecId::new("tiff");
        assert!(ctx.codecs.has_decoder(&id));
        assert!(ctx.codecs.has_encoder(&id));
        assert_eq!(ctx.containers.container_for_extension("tif"), Some("tiff"));
        assert!(ctx.containers.muxer_names().any(|n| n == "tiff"));
    }

    #[test]
    fn pixel_format_bridge_round_trips() {
        for f in [
            TiffPixelFormat::Gray8,
            TiffPixelFormat::Gray16Le,
            TiffPixelFormat::Rgb24,
            TiffPixelFormat::Rgb48Le,
            TiffPixelFormat::Rgba,
            TiffPixelFormat::Pal8,
            TiffPixelFormat::Cmyk,
            TiffPixelFormat::GrayF32Le,
            TiffPixelFormat::RgbF32Le,
        ] {
            let core: PixelFormat = f.into();
            assert_eq!(TiffPixelFormat::try_from(core).unwrap(), f);
        }
        assert!(TiffPixelFormat::try_from(PixelFormat::Yuv420P).is_err());
    }

    #[test]
    fn frame_bridge_keeps_palette_and_color() {
        let img = TiffImage::new(2, 1, TiffPixelFormat::Pal8, vec![Plane::new(2, vec![0, 1])])
            .unwrap()
            .with_palette(Palette::new(vec![[1, 2, 3, 255], [4, 5, 6, 255]]))
            .with_color(ColorInfo::srgb());
        let frame: VideoFrame = img.clone().into();
        assert_eq!(frame.palette(), Some(&[1u8, 2, 3, 4, 5, 6][..]));
        let mut params = CodecParameters::video(CodecId::new("tiff"));
        params.width = Some(2);
        params.height = Some(1);
        params.pixel_format = Some(PixelFormat::Pal8);
        let back = TiffImage::try_from((&frame, &params)).unwrap();
        assert_eq!(back, img);
    }

    #[test]
    fn encoder_options_schema_parses() {
        let opts: EncodeOptions = oxideav_core::options::parse_options_json::<EncodeOptions>(
            r#"{"compression":"lzw","predictor":"true","tile":"64"}"#,
        )
        .unwrap();
        assert!(matches!(opts.compression, TiffCompression::Lzw));
        assert!(opts.predictor);
        assert_eq!(opts.tiling, Some((64, 64)));
        let jpeg: EncodeOptions = oxideav_core::options::parse_options_json::<EncodeOptions>(
            r#"{"compression":"jpeg","quality":"90"}"#,
        )
        .unwrap();
        assert!(matches!(jpeg.compression, TiffCompression::Jpeg(j) if j.quality == 90));
    }
}
