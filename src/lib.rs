//! Pure-Rust TIFF 6.0 image decoder + encoder + container.
//!
//! Implements the *Aldus TIFF Revision 6.0 (June 1992)* baseline plus
//! the universally-deployed Part 2 extensions (LZW with optional
//! horizontal-differencing predictor; Adobe Deflate; YCbCr / CMYK
//! photometrics; tiles), the multi-IFD chain (multi-page), the
//! Adobe Pagemaker 6.0 *BigTIFF* design (8-byte offsets, magic 43),
//! and the de-facto registry extensions Compression = 50000
//! (Zstandard) and Compression = 50001 (WebP-in-TIFF — each strip /
//! tile payload is one complete WebP file, decoded / encoded through
//! the `oxideav-webp` sibling crate's public API; see the README's
//! "WebP (Compression = 50001)" section).
//! Spec-only clean-room: no external library source was consulted at
//! any point.
//!
//! Decode-side coverage:
//!
//! * Byte order: `II` (little-endian) and `MM` (big-endian)
//! * Variants: classic TIFF (32-bit offsets) + BigTIFF (64-bit offsets)
//! * Photometric: WhiteIsZero / BlackIsZero / RGB / Palette / CMYK /
//!   YCbCr / TransparencyMask / CIELab (3-sample L*a*b* and 1-sample
//!   L*-only, TIFF 6.0 §23, decoded to display Rgb24 / Gray8 via
//!   Lab → XYZ@D65 → linear NTSC RGB → sRGB)
//! * Bit depths: 1, 4, 8, 16 (per-strip and per-tile)
//! * Compression: 1 None / 2 CCITT Modified Huffman / 3 CCITT T.4
//!   (1-D and 2-D) / 4 CCITT T.6 / 32773 PackBits / 5 LZW /
//!   8 Deflate (zlib) / 50000 Zstandard (de-facto registry
//!   extension; one RFC 8478 frame per strip or tile) /
//!   50001 WebP-in-TIFF (one complete WebP file per strip or tile,
//!   VP8L lossless or VP8 lossy, 8-bit chunky RGB / RGBA; routed
//!   through `oxideav-webp`) /
//!   7 JPEG-in-TIFF (TIFF Tech Note 2; routes each strip/tile through
//!   `oxideav-mjpeg`)
//! * Predictor: 1 (none) and 2 (horizontal differencing,
//!   per-component for SamplesPerPixel > 1)
//! * Strip OR tile layout
//! * Multi-page (full next-IFD chain walk via [`decode_tiff_all`])
//!
//! Encode-side coverage (classic II / single or multi page):
//!
//! * Photometric: WhiteIsZero (1-bit bilevel) / BlackIsZero (4/8/16-bit
//!   greyscale, plus 8/16-bit **signed** two's-complement greyscale
//!   with `SampleFormat = 2` per TIFF 6.0 §SampleFormat) / RGB (8-bit
//!   and 16-bit `Rgb48`, plus 8-bit RGBA with the §ExtraSamples tag
//!   via [`EncodePixelFormat::Rgba32`]) / Palette (4- and 8-bit
//!   indexed) /
//!   TransparencyMask (1-bit, sets PhotometricInterpretation = 4 and
//!   NewSubfileType bit 2 per TIFF 6.0 §"PhotometricInterpretation"
//!   value 4 + §"NewSubfileType" bit 2) / CIELab (8-bit chunky
//!   `(L*, a*, b*)` and 1-sample L*-only, PhotometricInterpretation = 8
//!   per TIFF 6.0 §23 "CIE L*a*b* Images") / CMYK (8-bit chunky
//!   `(C, M, Y, K)`, PhotometricInterpretation = 5 per TIFF 6.0 §16
//!   "CMYK Images") / YCbCr (8-bit chunky `(Y, Cb, Cr)` at
//!   `YCbCrSubSampling = [1, 1]` chunky 4:4:4,
//!   PhotometricInterpretation = 6 per TIFF 6.0 §21 "YCbCr Images")
//! * Compression: None / PackBits / LZW / Deflate / Zstandard
//!   (Compression=50000) / WebP-in-TIFF (Compression=50001 — one
//!   lossless VP8L file per strip / tile, Rgb24 / Rgba32 input only) /
//!   CCITT Modified Huffman (Compression=2) /
//!   CCITT T.4 1-D and 2-D (Compression=3, with optional T4Options
//!   bit 2 byte-aligned EOLs) / CCITT T.6 (Compression=4)
//! * Layout: strips (single or multi via
//!   [`encoder::PageExtras::rows_per_strip`], TIFF 6.0 §"RowsPerStrip"),
//!   `PlanarConfiguration = 2` (separate planes, chunky-source), or
//!   tiled (TIFF 6.0 §15, chunky) — see [`EncodePage::planar`] /
//!   [`EncodePage::tiling`]
//! * Predictor: 2 (horizontal differencing) and 3 (floating-point)
//!   via [`EncodePage::predictor`]
//! * Multi-page chain via [`encode_tiff_multi`]; SubIFDs (330) child
//!   image trees, Exif (34665) / GPS (34853) verbatim child IFDs,
//!   PageNumber / NewSubfileType bits, resolution + §8 ASCII metadata
//!   via [`encoder::PageExtras`] (children decode through
//!   [`decode_tiff_at`])
//!
//! The deprecated TIFF 6.0 §22 old-style JPEG (Compression=6) decodes
//! in its interchange-format layout (`JPEGInterchangeFormat`, tag 513,
//! points at a complete SOI..EOI bitstream); the §22 tables-form
//! layout (raw JPEGQTables/JPEGDCTables/JPEGACTables + entropy-coded
//! strips) is recognised and rejected with a precise error — see
//! [`jpeg_old`]. Encode-side JPEG-in-TIFF (Compression=7) remains out
//! of scope. YCbCr encodes chunky 4:4:4, chroma-subsampled chunky
//! (§21 data-unit packing, strip + tiled) and chroma-subsampled
//! `PlanarConfiguration = 2` strips (full-resolution Y plane +
//! reduced §21 "chroma image" Cb / Cr planes, per-plane §14
//! predictor); still deferred are the §14 predictor over the packed
//! *chunky* data-unit stream and *tiled* planar subsampled layout
//! (no consistent per-plane tile geometry under §15's fixed
//! TileOffsets count). Decode-side Compression=7 (new-style
//! JPEG-in-TIFF, per TIFF Tech Note 2) is implemented as of round 92.
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

// Compression = 50001 (WebP-in-TIFF) codec-in-container carriage.
// Not registry-gated: the `oxideav-webp` sibling is consumed through
// its framework-free public surface (`default-features = false`), so
// Compression=50001 decodes and encodes in every build configuration,
// exactly like Compression=50000 (Zstandard).
mod webp;

#[cfg(feature = "registry")]
pub mod container;

#[cfg(feature = "registry")]
pub mod jpeg;

// TIFF 6.0 §22 old-style JPEG (Compression = 6) field parsing. Not
// registry-gated: the §22 field validation and the precise
// recognition / rejection semantics have no `oxideav-mjpeg`
// dependency; only the actual interchange-stream decode (in
// `decoder`) does.
pub mod jpeg_old;

#[cfg(feature = "registry")]
pub mod registry;

/// Codec id for TIFF image frames.
pub const CODEC_ID_STR: &str = "tiff";

// Standalone, framework-free API. Available regardless of the
// `registry` feature.
pub use decoder::{decode_tiff, decode_tiff_all, decode_tiff_at, DecodedTiff};
pub use encoder::{
    encode_tiff, encode_tiff_multi, f16_bits_to_f32, f32_to_f16_bits, AuxIfdEntry, EncodePage,
    EncodePixelFormat, ExtraSampleKind, PageExtras, PageResolution, RgbColor, TiffCompression,
};
pub use error::{Result, TiffError};
pub use image::{TiffImage, TiffPixelFormat, TiffPlane};

// Framework-integrated API (`oxideav-core`-dependent). Gated behind
// `registry` so image-library callers can build the crate without
// dragging in `oxideav-core`.
#[cfg(feature = "registry")]
pub use registry::{make_decoder, register, register_codecs, register_containers};
