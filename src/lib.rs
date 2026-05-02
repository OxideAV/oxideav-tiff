//! Pure-Rust TIFF 6.0 image decoder + container.
//!
//! Implements the *Aldus TIFF Revision 6.0 (June 1992)* baseline, plus
//! the two universally-deployed Part 2 extensions (LZW with optional
//! horizontal-differencing predictor; Adobe Deflate). Spec-only
//! clean-room: no external library source was consulted at any point.
//!
//! Decode-side coverage:
//!
//! * Byte order: `II` (little-endian) and `MM` (big-endian)
//! * Photometric: WhiteIsZero / BlackIsZero / RGB / Palette
//! * Bit depths: 1, 4, 8, 16
//! * Compression: 1 None / 32773 PackBits / 5 LZW / 8 Deflate (zlib)
//! * Predictor: 1 (none) and 2 (horizontal differencing,
//!   per-component for SamplesPerPixel > 1)
//! * Strip-based decode (any number of strips)
//!
//! Out of scope for round 1 (round 2 backlog): BigTIFF, tiles, CCITT
//! G3/G4 fax, JPEG-in-TIFF, YCbCr / CIELab / CMYK, multi-page IFD
//! chain, encoder.

pub mod compress;
pub mod container;
pub mod decoder;
pub mod ifd;
pub mod types;

use oxideav_core::ContainerRegistry;
use oxideav_core::{CodecCapabilities, CodecId, PixelFormat};
use oxideav_core::{CodecInfo, CodecRegistry};

/// Codec id for TIFF image frames.
pub const CODEC_ID_STR: &str = "tiff";

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
            .decoder(decoder::make_decoder),
    );
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}

pub fn register(codecs: &mut CodecRegistry, containers: &mut ContainerRegistry) {
    register_codecs(codecs);
    register_containers(containers);
}

pub use decoder::{decode_tiff, DecodedTiff};
