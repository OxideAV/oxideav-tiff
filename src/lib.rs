//! Pure-Rust TIFF 6.0 image decoder + encoder + container.
//!
//! Implements the *Aldus TIFF Revision 6.0 (June 1992)* baseline plus
//! the universally-deployed Part 2 extensions (LZW with optional
//! horizontal-differencing predictor; Adobe Deflate; YCbCr / CMYK
//! photometrics; tiles), the multi-IFD chain (multi-page) and the
//! Adobe Pagemaker 6.0 *BigTIFF* design (8-byte offsets, magic 43).
//! Spec-only clean-room: no external library source was consulted at
//! any point.
//!
//! Decode-side coverage:
//!
//! * Byte order: `II` (little-endian) and `MM` (big-endian)
//! * Variants: classic TIFF (32-bit offsets) + BigTIFF (64-bit offsets)
//! * Photometric: WhiteIsZero / BlackIsZero / RGB / Palette / CMYK / YCbCr
//! * Bit depths: 1, 4, 8, 16 (per-strip and per-tile)
//! * Compression: 1 None / 2 CCITT Modified Huffman / 3 CCITT T.4 1-D /
//!   32773 PackBits / 5 LZW / 8 Deflate (zlib)
//! * Predictor: 1 (none) and 2 (horizontal differencing,
//!   per-component for SamplesPerPixel > 1)
//! * Strip OR tile layout
//! * Multi-page (full next-IFD chain walk via [`decode_tiff_all`])
//!
//! Encode-side coverage (classic II / single or multi page):
//!
//! * Photometric: BlackIsZero (8/16-bit greyscale) / RGB (8-bit) /
//!   Palette (8-bit indexed)
//! * Compression: None / PackBits / LZW / Deflate
//! * Multi-page chain via [`encode_tiff_multi`]
//!
//! Out of scope for this round (next-round backlog): CCITT T.4 2-D
//! and T.6 / Group 4 (Compression=3 with T4Options bit 0 set, and
//! Compression=4 — the 2-D mode codes are not in the TIFF 6.0
//! spec, which defers to CCITT Rec. T.4 / T.6), JPEG-in-TIFF
//! (Compression=6/7), CIELab photometric, BigTIFF write, tile
//! write, predictor encoding, planar (non-chunky) layout.
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

pub mod ccitt;
pub mod compress;
pub mod decoder;
pub mod encoder;
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
pub use decoder::{decode_tiff, decode_tiff_all, DecodedTiff};
pub use encoder::{
    encode_tiff, encode_tiff_multi, EncodePage, EncodePixelFormat, RgbColor, TiffCompression,
};
pub use error::{Result, TiffError};
pub use image::{TiffImage, TiffPixelFormat, TiffPlane};

// Framework-integrated API (`oxideav-core`-dependent). Gated behind
// `registry` so image-library callers can build the crate without
// dragging in `oxideav-core`.
#[cfg(feature = "registry")]
pub use registry::{make_decoder, register, register_codecs, register_containers};
