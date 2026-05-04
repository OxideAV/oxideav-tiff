//! `oxideav-core` integration layer for `oxideav-tiff`.
//!
//! Gated behind the default-on `registry` feature so image-library
//! consumers can depend on `oxideav-tiff` with `default-features = false`
//! and skip the `oxideav-core` dependency entirely.
//!
//! The module exposes:
//! * [`register`] / [`register_codecs`] / [`register_containers`] — the
//!   `CodecRegistry` / `ContainerRegistry` entry points the umbrella
//!   `oxideav` crate calls during framework initialisation.
//! * The `From<TiffError> for oxideav_core::Error` and
//!   `From<TiffImage> for oxideav_core::Frame` conversions used by the
//!   [`TiffDecoder`] trait impl below.

use oxideav_core::{
    frame::VideoPlane, CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry,
    ContainerRegistry, Decoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame,
};

use crate::container;
use crate::decoder::decode_tiff;
use crate::error::TiffError;
use crate::image::{TiffImage, TiffPixelFormat};
use crate::CODEC_ID_STR;

impl From<TiffError> for Error {
    fn from(e: TiffError) -> Self {
        match e {
            TiffError::InvalidData(s) => Error::InvalidData(s),
            TiffError::Unsupported(s) => Error::Unsupported(s),
        }
    }
}

impl From<TiffPixelFormat> for PixelFormat {
    fn from(p: TiffPixelFormat) -> Self {
        match p {
            TiffPixelFormat::Gray8 => PixelFormat::Gray8,
            TiffPixelFormat::Gray16Le => PixelFormat::Gray16Le,
            TiffPixelFormat::Rgb24 => PixelFormat::Rgb24,
            TiffPixelFormat::Rgb48Le => PixelFormat::Rgb48Le,
        }
    }
}

impl From<TiffImage> for VideoFrame {
    fn from(img: TiffImage) -> Self {
        let planes = img
            .planes
            .into_iter()
            .map(|p| VideoPlane {
                stride: p.stride,
                data: p.data,
            })
            .collect();
        VideoFrame { pts: None, planes }
    }
}

impl From<TiffImage> for Frame {
    fn from(img: TiffImage) -> Self {
        Frame::Video(img.into())
    }
}

/// Register the TIFF codec (decoder) into the supplied [`CodecRegistry`].
pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("tiff_sw")
        .with_intra_only(true)
        .with_lossless(true)
        .with_max_size(65535, 65535)
        .with_pixel_formats(vec![
            PixelFormat::Rgb24,
            PixelFormat::Rgb48Le,
            PixelFormat::Gray8,
            PixelFormat::Gray16Le,
        ]);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder),
    );
}

/// Register the TIFF container demuxer + extension + probe.
pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}

/// Combined registration for callers that just want everything wired
/// up in one call.
pub fn register(codecs: &mut CodecRegistry, containers: &mut ContainerRegistry) {
    register_codecs(codecs);
    register_containers(containers);
}

/// Factory registered with the codec registry.
pub fn make_decoder(_params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(TiffDecoder {
        codec_id: CodecId::new(CODEC_ID_STR),
        pending: None,
        eof: false,
    }))
}

struct TiffDecoder {
    codec_id: CodecId,
    pending: Option<VideoFrame>,
    eof: bool,
}

impl Decoder for TiffDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let d = decode_tiff(&packet.data)?;
        self.pending = Some(d.frame.into());
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
