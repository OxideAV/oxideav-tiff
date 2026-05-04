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
//!
//! ## Standalone vs registry-integrated
//!
//! The crate's default `registry` Cargo feature pulls in `oxideav-core`
//! and exposes the `Decoder` trait surface, the TIFF container
//! demuxer/probe, and the [`registry::register`] entry point. Disable
//! the feature (`default-features = false`) for an oxideav-core-free
//! build that still exposes the standalone [`decode_tiff`] API plus
//! [`TiffImage`] / [`TiffPixelFormat`] / [`TiffPlane`] / [`TiffError`]
//! types — none of which depend on `oxideav-core`.

pub mod compress;
pub mod decoder;
pub mod error;
pub mod ifd;
pub mod image;
pub mod types;

#[cfg(feature = "registry")]
pub mod container;

#[cfg(feature = "registry")]
pub mod registry;

/// Codec id for TIFF image frames.
pub const CODEC_ID_STR: &str = "tiff";

// Standalone, framework-free API. Available regardless of the
// `registry` feature.
pub use decoder::{decode_tiff, DecodedTiff};
pub use error::{Result, TiffError};
pub use image::{TiffImage, TiffPixelFormat, TiffPlane};

// Framework-integrated API (`oxideav-core`-dependent). Gated behind
// `registry` so image-library callers can build the crate without
// dragging in `oxideav-core`.
#[cfg(feature = "registry")]
pub use registry::{make_decoder, register, register_codecs, register_containers};
