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
//! * Photometric: WhiteIsZero / BlackIsZero / RGB / Palette / CMYK /
//!   YCbCr / TransparencyMask / CIELab (3-sample L*a*b* and 1-sample
//!   L*-only, TIFF 6.0 §23, decoded to display Rgb24 / Gray8 via
//!   Lab → XYZ@D65 → linear NTSC RGB → sRGB)
//! * Bit depths: 1, 4, 8, 16 (per-strip and per-tile)
//! * Compression: 1 None / 2 CCITT Modified Huffman / 3 CCITT T.4 1-D /
//!   3 CCITT T.4 2-D (MR) / 4 CCITT T.6 (MMR / Group 4) /
//!   32773 PackBits / 5 LZW / 8 Deflate (zlib) /
//!   7 JPEG-in-TIFF (TIFF Tech Note 2; routes each strip/tile through
//!   `oxideav-mjpeg`) /
//!   50000 Zstandard (libtiff self-assignment; one zstd frame per
//!   strip/tile, identical structural template to Compression=8 — see
//!   `docs/image/tiff/tiff-zstd-compression-50000.md`)
//! * Predictor: 1 (none) and 2 (horizontal differencing,
//!   per-component for SamplesPerPixel > 1)
//! * Strip OR tile layout
//! * Multi-page (full next-IFD chain walk via [`decode_tiff_all`])
//!
//! Encode-side coverage (classic II / single or multi page):
//!
//! * Photometric: WhiteIsZero (1-bit bilevel) / BlackIsZero (8/16-bit
//!   greyscale) / RGB (8-bit) / Palette (8-bit indexed) /
//!   TransparencyMask (1-bit, sets PhotometricInterpretation = 4 and
//!   NewSubfileType bit 2 per TIFF 6.0 §"PhotometricInterpretation"
//!   value 4 + §"NewSubfileType" bit 2)
//! * Compression: None / PackBits / LZW / Deflate /
//!   CCITT Modified Huffman (Compression=2) /
//!   CCITT T.4 1-D (Compression=3, with optional T4Options bit 2
//!   byte-aligned EOLs)
//! * Layout: single strip, `PlanarConfiguration = 2` (separate planes,
//!   chunky-source), or tiled (TIFF 6.0 §15, chunky) — see
//!   [`EncodePage::planar`] / [`EncodePage::tiling`]
//! * Predictor: 2 (horizontal differencing) via [`EncodePage::predictor`]
//! * Multi-page chain via [`encode_tiff_multi`]
//!
//! Out of scope for this round (next-round backlog): the deprecated
//! TIFF 6.0 §22 old-style JPEG (Compression=6), encode-side CIELab
//! (decode-side CIELab is implemented; the `EncodePixelFormat` enum
//! has no Lab variant yet), encode-side `Compression = 50000`
//! (Zstandard; the `TiffCompression` enum has no `Zstd` variant
//! yet), and `Compression = 50001` (WebP — same libtiff
//! self-assignment as ZSTD; needs its own per-strip-WebP fixture
//! pass). Decode-side CCITT T.4 2-D and T.6 / Group 4 are implemented
//! per CCITT Rec. T.4 §4.2 / Rec. T.6 staged at
//! `docs/image/tiff/{T-REC-T.4.pdf,T-REC-T.6.pdf}`; decode-side
//! Compression=7 (new-style JPEG-in-TIFF) is implemented per
//! TIFF Tech Note 2; decode-side Compression=50000 (Zstandard) is
//! implemented per `docs/image/tiff/tiff-zstd-compression-50000.md`.
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
pub mod jpeg;

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
