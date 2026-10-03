//! Decode-side limits / strictness and encode-side options — the
//! image-crate contract's [`DecodeOptions`] / [`EncodeOptions`]
//! (`IMAGE_CRATE_API`) specialised for TIFF.

use crate::encoder::{PageResolution, TiffCompression};
use crate::error::{Result, TiffError};

/// Limits and strictness for [`crate::decode_with`] /
/// [`crate::decode_all_with`].
///
/// Every `max_*` is `None` = unlimited. The defaults are the limits
/// the decoder has always enforced: 256 megapixels per image
/// (`ImageWidth × ImageLength`) and 1 GiB of assembled sample bytes
/// (`width × height × SamplesPerPixel × ceil(BitsPerSample / 8)`),
/// both checked from the IFD tags before any strip / tile buffer is
/// allocated; no width / height cap on its own.
///
/// `strict` selects how much writer shorthand the reader tolerates:
///
/// * `strict = false` (default): a `SampleFormat` (339) field with a
///   single entry is accepted for a multi-sample image as applying to
///   every sample (the spec requires `N = SamplesPerPixel`).
/// * `strict = true`: that shorthand is `Error::InvalidData`. Every
///   other check (unknown `Orientation`, `ResolutionUnit`,
///   `ExtraSamples`, `PlanarConfiguration`, `Predictor` values,
///   mismatched `BitsPerSample` counts, …) is already an error in
///   both modes — the decoder never guesses at malformed layout tags.
///   Descriptive metadata (TIFF 6.0 §8 fields, ICC, XMP) is extracted
///   totally in both modes: a malformed entry leaves that one field
///   `None` and never gates the pixel decode.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Maximum `ImageWidth` (after `Orientation`, the stored width).
    pub max_width: Option<u32>,
    /// Maximum `ImageLength`.
    pub max_height: Option<u32>,
    /// Maximum `ImageWidth × ImageLength`.
    pub max_pixels: Option<u64>,
    /// Maximum assembled sample bytes (`width × height × samples ×
    /// bytes per sample`).
    pub max_bytes: Option<u64>,
    /// Reject writer shorthands the lenient reader accepts (see the
    /// type docs).
    pub strict: bool,
}

impl DecodeOptions {
    /// Default pixel-count limit: 256 megapixels.
    pub const DEFAULT_MAX_PIXELS: u64 = 256 * 1024 * 1024;
    /// Default assembled-bytes limit: 1 GiB.
    pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;

    /// The defaults (see the type docs).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or lift with `None`) the width limit.
    pub fn with_max_width(mut self, max_width: impl Into<Option<u32>>) -> Self {
        self.max_width = max_width.into();
        self
    }

    /// Set (or lift with `None`) the height limit.
    pub fn with_max_height(mut self, max_height: impl Into<Option<u32>>) -> Self {
        self.max_height = max_height.into();
        self
    }

    /// Set (or lift with `None`) the pixel-count limit.
    pub fn with_max_pixels(mut self, max_pixels: impl Into<Option<u64>>) -> Self {
        self.max_pixels = max_pixels.into();
        self
    }

    /// Set (or lift with `None`) the assembled-bytes limit.
    pub fn with_max_bytes(mut self, max_bytes: impl Into<Option<u64>>) -> Self {
        self.max_bytes = max_bytes.into();
        self
    }

    /// Set strict mode.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Lift every limit (`max_*` all `None`).
    pub fn unlimited(mut self) -> Self {
        self.max_width = None;
        self.max_height = None;
        self.max_pixels = None;
        self.max_bytes = None;
        self
    }

    /// Check the geometry limits against an IFD's declared dimensions
    /// — called before any pixel allocation.
    pub(crate) fn check_dimensions(&self, width: u32, height: u32) -> Result<()> {
        if let Some(m) = self.max_width {
            if width > m {
                return Err(TiffError::limit(format!(
                    "TIFF: ImageWidth {width} exceeds the {m} limit"
                )));
            }
        }
        if let Some(m) = self.max_height {
            if height > m {
                return Err(TiffError::limit(format!(
                    "TIFF: ImageLength {height} exceeds the {m} limit"
                )));
            }
        }
        if let Some(m) = self.max_pixels {
            let px = (width as u64).saturating_mul(height as u64);
            if px > m {
                return Err(TiffError::limit(format!(
                    "TIFF: image too large ({width}x{height} = {px} pixels > {m})"
                )));
            }
        }
        Ok(())
    }

    /// Check the assembled-bytes limit — called before any pixel
    /// allocation.
    pub(crate) fn check_bytes(&self, total: u64, what: &str) -> Result<()> {
        if let Some(m) = self.max_bytes {
            if total > m {
                return Err(TiffError::limit(format!(
                    "TIFF: image too large ({what} = {total} bytes > {m})"
                )));
            }
        }
        Ok(())
    }
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            max_width: None,
            max_height: None,
            max_pixels: Some(Self::DEFAULT_MAX_PIXELS),
            max_bytes: Some(Self::DEFAULT_MAX_BYTES),
            strict: false,
        }
    }
}

/// Options for [`crate::encode`] / [`crate::encode_rgb8`] /
/// [`crate::encode_rgba8`] / [`crate::encode_to`].
///
/// The defaults write a classic (32-bit offset) little-endian TIFF with
/// `Compression = 1` (none), a single strip, chunky
/// `PlanarConfiguration = 1`, no predictor, and every metadata blob the
/// image carries (ICC profile, XMP packet, Exif IFD) embedded. Every
/// behaviour variant is a field; the full per-page surface (sub-IFDs,
/// page numbers, the TIFF 6.0 §8 descriptive strings, multi-page
/// files) stays on [`crate::EncodePage`] / [`crate::encode_pages`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EncodeOptions {
    /// `Compression` scheme (tag 259). Default
    /// [`TiffCompression::None`]. JPEG-in-TIFF (`Compression = 7`)
    /// carries its quality / process in [`crate::JpegOptions`].
    pub compression: TiffCompression,
    /// Apply the TIFF 6.0 §14 horizontal-differencing predictor
    /// (`Predictor = 2`) before compression. Composes with LZW /
    /// Deflate / Zstandard / PackBits / none; rejected with WebP and
    /// JPEG. Default `false`.
    pub predictor: bool,
    /// Write `PlanarConfiguration = 2` (one plane per component on
    /// disk). Default `false` (chunky).
    pub planar: bool,
    /// Write §15 tiles of this `(TileWidth, TileLength)` (both multiples
    /// of 16) instead of strips. Default `None` (strips).
    pub tiling: Option<(u32, u32)>,
    /// `RowsPerStrip` for a stripped page. Default `None` (one strip).
    pub rows_per_strip: Option<u32>,
    /// Write a BigTIFF (version 43, 8-byte offsets). Default `false`.
    pub bigtiff: bool,
    /// Embed `metadata.icc` as tag 34675. Default `true`.
    pub embed_icc: bool,
    /// Embed `metadata.xmp` as tag 700. Default `true`.
    pub embed_xmp: bool,
    /// Embed `metadata.exif` (an Exif TIFF payload) as the Exif IFD
    /// (34665) and GPS IFD (34853). Default `true`.
    pub embed_exif: bool,
    /// `Software` (tag 305) string. Default `None` (tag omitted).
    pub software: Option<String>,
    /// `XResolution` / `YResolution` / `ResolutionUnit` (282 / 283 /
    /// 296). Default `None` (tags omitted).
    pub resolution: Option<PageResolution>,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            rows_per_strip: None,
            bigtiff: false,
            embed_icc: true,
            embed_xmp: true,
            embed_exif: true,
            software: None,
            resolution: None,
        }
    }
}

impl EncodeOptions {
    /// The defaults (see the type docs).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the compression scheme.
    pub fn with_compression(mut self, compression: TiffCompression) -> Self {
        self.compression = compression;
        self
    }

    /// Enable / disable the §14 horizontal predictor.
    pub fn with_predictor(mut self, predictor: bool) -> Self {
        self.predictor = predictor;
        self
    }

    /// Write planar (`PlanarConfiguration = 2`) or chunky.
    pub fn with_planar(mut self, planar: bool) -> Self {
        self.planar = planar;
        self
    }

    /// Set (or clear) §15 tiling.
    pub fn with_tiling(mut self, tiling: impl Into<Option<(u32, u32)>>) -> Self {
        self.tiling = tiling.into();
        self
    }

    /// Set (or clear) `RowsPerStrip`.
    pub fn with_rows_per_strip(mut self, rows: impl Into<Option<u32>>) -> Self {
        self.rows_per_strip = rows.into();
        self
    }

    /// Write a BigTIFF.
    pub fn with_bigtiff(mut self, bigtiff: bool) -> Self {
        self.bigtiff = bigtiff;
        self
    }

    /// Embed (or skip) the ICC profile.
    pub fn with_embed_icc(mut self, embed: bool) -> Self {
        self.embed_icc = embed;
        self
    }

    /// Embed (or skip) the XMP packet.
    pub fn with_embed_xmp(mut self, embed: bool) -> Self {
        self.embed_xmp = embed;
        self
    }

    /// Embed (or skip) the Exif / GPS IFDs.
    pub fn with_embed_exif(mut self, embed: bool) -> Self {
        self.embed_exif = embed;
        self
    }

    /// Set (or clear) the `Software` string.
    pub fn with_software(mut self, software: impl Into<Option<String>>) -> Self {
        self.software = software.into();
        self
    }

    /// Set (or clear) the resolution triple.
    pub fn with_resolution(mut self, resolution: impl Into<Option<PageResolution>>) -> Self {
        self.resolution = resolution.into();
        self
    }
}
