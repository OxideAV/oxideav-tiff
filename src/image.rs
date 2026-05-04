//! Crate-local uncompressed image representation returned by
//! `oxideav-tiff`'s standalone (no `oxideav-core`) decode API.
//!
//! Defined here (rather than reusing `oxideav_core::VideoFrame`) so the
//! crate can be built with the default `registry` feature off — i.e.
//! without depending on `oxideav-core` at all. When the `registry`
//! feature is on the [`crate::registry`] module provides
//! `From<TiffImage> for oxideav_core::Frame` so the `Decoder` trait
//! surface still interoperates cleanly.

/// Pixel layout produced by [`crate::decode_tiff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiffPixelFormat {
    /// 8-bit single-channel grayscale, one plane.
    Gray8,
    /// 16-bit single-channel grayscale, little-endian, one plane.
    Gray16Le,
    /// 8-bit packed RGB, one plane (3 bytes per pixel).
    Rgb24,
    /// 16-bit packed RGB, little-endian, one plane (6 bytes per pixel).
    Rgb48Le,
}

/// One image plane: row-major bytes plus the row stride in bytes.
#[derive(Debug, Clone)]
pub struct TiffPlane {
    /// Bytes per row in `data` (may be larger than the logical row width).
    pub stride: usize,
    /// Raw plane bytes, packed `stride` × number of rows.
    pub data: Vec<u8>,
}

/// One decoded TIFF image.
///
/// All-`std`, no `oxideav-core` types — the crate's standalone path
/// hands these out directly. The gated [`crate::registry`] module
/// provides a `From<TiffImage> for oxideav_core::Frame` conversion.
#[derive(Debug, Clone)]
pub struct TiffImage {
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// Pixel layout. Determines how many planes are expected and how
    /// to interpret each plane's bytes.
    pub pixel_format: TiffPixelFormat,
    /// One entry per plane (always 1 for the formats this crate emits).
    pub planes: Vec<TiffPlane>,
}
