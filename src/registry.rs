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
//!   the palette / colour-signal side-channels.
//! * `From<TiffError> for oxideav_core::Error` and the
//!   `CodecOptionsStruct` schema for [`EncodeOptions`].

use oxideav_core::{
    frame::VideoPlane, parse_options, CodecCapabilities, CodecId, CodecInfo, CodecOptionsStruct,
    CodecParameters, CodecRegistry, ColorPrimaries, ColorSignal, ContainerRegistry, Decoder,
    Encoder, Error, Frame, MatrixCoefficients, MediaType, OptionField, OptionKind, OptionValue,
    Packet, PixelFormat, Result, RuntimeContext, TimeBase, TransferCharacteristics, VideoFrame,
};

use crate::container;
use crate::encoder::TiffCompression;
use crate::error::TiffError;
use crate::image::{ColorInfo, ColorRange, Palette, Plane, TiffImage, TiffPixelFormat};
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
            other => {
                return Err(Error::unsupported(format!(
                    "TIFF: pixel format {other:?} has no TIFF layout (Gray8 / Gray16Le / \
                     Rgb24 / Rgb48Le / Rgba / Pal8 / Cmyk)"
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

// ---- TiffImage ⇄ VideoFrame ----------------------------------------------------

fn stamp_frame_side_channels(frame: &mut VideoFrame, image: &TiffImage) {
    if let (TiffPixelFormat::Pal8, Some(p)) = (image.format, &image.palette) {
        frame.set_palette(p.to_rgb_triples().concat());
    }
    let c = image.color;
    if c.primaries != ColorInfo::UNSPECIFIED
        || c.transfer != ColorInfo::UNSPECIFIED
        || c.range == ColorRange::Limited
    {
        frame.set_color_signal(to_color_signal(&c));
    }
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
    /// for `Pal8` and the colour-signal side-channel when the image
    /// signals more than the TIFF default.
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
    /// `extradata` RGB triples); its colour-signal side-channel becomes
    /// [`TiffImage::color`].
    pub fn from_video_frame(frame: &VideoFrame, params: &CodecParameters) -> Result<Self> {
        let width = params
            .width
            .ok_or_else(|| Error::invalid("TIFF: missing width"))?;
        let height = params
            .height
            .ok_or_else(|| Error::invalid("TIFF: missing height"))?;
        let pix = params
            .pixel_format
            .ok_or_else(|| Error::invalid("TIFF: missing pixel_format"))?;
        let pix = TiffPixelFormat::try_from(pix)?;
        let plane = frame
            .image_planes()
            .first()
            .ok_or_else(|| Error::invalid("TIFF: frame has no planes"))?;
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
            img.palette = rgb.map(|rgb| {
                Palette::new(
                    rgb.chunks_exact(3)
                        .map(|c| [c[0], c[1], c[2], 255])
                        .collect(),
                )
            });
            if img.palette.is_none() {
                return Err(Error::invalid(
                    "TIFF: Pal8 frame without a palette side-channel or extradata",
                ));
            }
        }
        if let Some(sig) = frame.color_signal() {
            img.color = from_color_signal(&sig);
        }
        Ok(img)
    }
}

impl TryFrom<(&VideoFrame, &CodecParameters)> for TiffImage {
    type Error = Error;
    fn try_from((frame, params): (&VideoFrame, &CodecParameters)) -> Result<Self> {
        TiffImage::from_video_frame(frame, params)
    }
}

// ---- CodecOptionsStruct (registry-only schema for EncodeOptions) ------------

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
            help: "JPEG-in-TIFF quality 1..=100 (only with compression = jpeg).",
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
                if !(1..=100).contains(&q) {
                    return Err(Error::invalid(format!(
                        "TIFF encoder: quality {q} out of 1..=100"
                    )));
                }
                if let TiffCompression::Jpeg(ref mut j) = self.compression {
                    j.quality = q as u8;
                } else {
                    self.compression = TiffCompression::Jpeg(crate::JpegOptions {
                        quality: q as u8,
                        ..Default::default()
                    });
                }
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

/// Register the TIFF codec (decoder + encoder) into the supplied
/// [`CodecRegistry`].
pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("tiff_sw")
        .with_intra_only(true)
        .with_lossless(true)
        .with_max_size(65535, 65535)
        .with_pixel_formats(vec![
            PixelFormat::Rgb24,
            PixelFormat::Rgba,
            PixelFormat::Rgb48Le,
            PixelFormat::Gray8,
            PixelFormat::Gray16Le,
            PixelFormat::Pal8,
            PixelFormat::Cmyk,
        ]);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder)
            .encoder_options::<EncodeOptions>(),
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
/// [`DecodeOptions`] (never loosen them).
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
    Ok(Box::new(TiffDecoder {
        codec_id: CodecId::new(CODEC_ID_STR),
        opts,
        pending: None,
        eof: false,
    }))
}

/// TIFF `Decoder` trait impl: each `send_packet` carries one complete
/// TIFF file and the matching `receive_frame` returns its first page
/// decoded through [`crate::decode_with`].
pub struct TiffDecoder {
    codec_id: CodecId,
    opts: DecodeOptions,
    pending: Option<VideoFrame>,
    eof: bool,
}

impl Decoder for TiffDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let img = crate::decode_with(&packet.data, &self.opts)?;
        self.pending = Some(image_into_video_frame(img, packet.pts));
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> {
        match self.pending.take() {
            Some(f) => Ok(Frame::Video(f)),
            None => {
                if self.eof {
                    Err(Error::Eof)
                } else {
                    Err(Error::NeedMore)
                }
            }
        }
    }
    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
}

// ---- Encoder --------------------------------------------------------------------

/// Factory registered with the codec registry: one packet (a complete
/// single-page TIFF file) per frame, through [`crate::encode`].
pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    let opts = parse_options::<EncodeOptions>(&params.options)?;
    let width = params
        .width
        .ok_or_else(|| Error::invalid("TIFF encoder: missing width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("TIFF encoder: missing height"))?;
    let pix_core = params.pixel_format.unwrap_or(PixelFormat::Rgb24);
    TiffPixelFormat::try_from(pix_core)?;

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

/// TIFF `Encoder` trait impl: every video frame becomes one packet
/// holding a complete single-page TIFF file.
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
