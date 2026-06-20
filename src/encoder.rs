//! TIFF 6.0 encoder — single-IFD or multi-page (multi-IFD chain),
//! little-endian on-disk byte order ("II"), classic 32-bit offsets or
//! BigTIFF 64-bit offsets.
//!
//! The encoder targets the same baseline our decoder reads:
//!
//! * Photometric: WhiteIsZero (1-bit bilevel), BlackIsZero (greyscale
//!   8/16-bit), RGB (8-bit), Palette (8-bit indexed)
//! * Compression: 1 None, 2 CCITT Modified Huffman, 3 CCITT T.4 1-D
//!   and 2-D (with optional T4Options bit 2 byte-aligned EOLs),
//!   4 CCITT T.6, 5 LZW, 8 Deflate, 32773 PackBits, 50000 Zstandard
//!   (de-facto registry extension; trace doc
//!   `docs/image/tiff/tiff-zstd-compression-50000.md`)
//! * Strip layout — a single strip for chunky pages, or one strip per
//!   component plane for `PlanarConfiguration = 2` pages (see
//!   [`EncodePage::planar`])
//! * Single-IFD or multi-IFD chain via [`encode_tiff_multi`]
//! * Variant: classic TIFF (8-byte header, magic 42, 32-bit offsets,
//!   12-byte IFD entries) or BigTIFF (16-byte header, magic 43,
//!   8-byte offset-bytesize + reserved, 64-bit offsets, 20-byte IFD
//!   entries, 8-byte inline value/offset slot, LONG8/IFD8 types per
//!   the Adobe Pagemaker 6.0 BigTIFF design), selectable via
//!   [`EncodePage::bigtiff`].
//!
//! JPEG-in-TIFF encode is intentionally out of scope for this round.
//! CIELab output (3-sample `(L*, a*, b*)` chunky and 1-sample `L*`-only,
//! `PhotometricInterpretation = 8` per TIFF 6.0 §23) is available via
//! [`EncodePixelFormat::CieLab8`] and [`EncodePixelFormat::CieLabL8`].
//! CMYK output (4-sample chunky `(C, M, Y, K)`,
//! `PhotometricInterpretation = 5` per TIFF 6.0 §16) is available via
//! [`EncodePixelFormat::Cmyk32`]. YCbCr output (3-sample chunky
//! `(Y, Cb, Cr)` at 4:4:4 chroma sampling, `PhotometricInterpretation = 6`
//! per TIFF 6.0 §21 "YCbCr Images") is available via
//! [`EncodePixelFormat::YCbCr24`]. The horizontal-differencing
//! predictor (`Predictor = 2`, TIFF 6.0 §14) is supported on encode via
//! the [`EncodePage::predictor`] flag, and `PlanarConfiguration = 2`
//! (separate component planes, §"PlanarConfiguration") via
//! [`EncodePage::planar`].

use crate::ccitt::{encode_ccitt, CcittVariant, FillOrder};
use crate::compress::{pack_deflate, pack_lzw, pack_packbits, pack_zstd};
use crate::error::{Result, TiffError as Error};
use crate::types::*;

/// One palette entry as stored in the on-disk `ColorMap` tag (each
/// component is a 16-bit value, top-byte is the 8-bit colour).
pub type RgbColor = [u8; 3];

/// Description of one image page being written.
///
/// `pixels` is row-major, packed (no padding between rows). For
/// 16-bit grayscale pages the bytes are interpreted little-endian
/// regardless of the stored byte order — the encoder writes II
/// files and the input bytes are consumed verbatim.
#[derive(Debug, Clone)]
pub struct EncodePage<'a> {
    pub width: u32,
    pub height: u32,
    pub kind: EncodePixelFormat<'a>,
    pub compression: TiffCompression,
    /// Apply the TIFF 6.0 §14 horizontal-differencing predictor
    /// (`Predictor = 2`) to the sample data before compression. The
    /// encoder replaces each component with the difference from the
    /// previous pixel of the same component (offset `SamplesPerPixel`,
    /// per §14's "subtract red from red, green from green") and writes
    /// the `Predictor` tag (317) so the decoder reverses the step. Only
    /// meaningful for the lossless byte-aligned photometrics whose
    /// decode path supports it — `Gray8` (8-bit), `Gray16Le` (16-bit),
    /// `Rgb24` (8-bit × 3), and `Palette8` (8-bit indices). §14 ties
    /// the predictor to the byte-stream lossless coders (LZW / Deflate,
    /// and the Compression=50000 Zstandard extension per its trace doc
    /// §4); combining it with the bilevel CCITT schemes
    /// (`Compression = 2 / 3`) or with `Bilevel` input is rejected
    /// with a precise error.
    pub predictor: bool,
    /// Write the image in `PlanarConfiguration = 2` (separate component
    /// planes) layout per TIFF 6.0 §"PlanarConfiguration" (page 38).
    /// When set, each sample component is stored in its own
    /// full-resolution strip (one strip per plane), and `StripOffsets`
    /// / `StripByteCounts` carry `SamplesPerPixel` entries ordered
    /// component-0, component-1, … — the spec's "SamplesPerPixel rows
    /// and StripsPerImage columns" array with StripsPerImage = 1. Only
    /// meaningful for multi-sample formats (`Rgb24`); §"PlanarConfiguration"
    /// notes the field "is irrelevant" when SamplesPerPixel is 1, so
    /// `planar` combined with a single-sample format (`Gray8` /
    /// `Gray16Le` / `Palette8` / `Bilevel`) is rejected with a precise
    /// error. The §14 predictor still applies when both flags are set:
    /// §14 says "If PlanarConfiguration is 2 … Differencing works the
    /// same as it does for grayscale data," so each plane is differenced
    /// independently with an offset of 1 sample.
    pub planar: bool,
    /// Write the image in tiled layout (TIFF 6.0 §15) instead of a
    /// single strip. `Some((tile_width, tile_height))` divides the image
    /// into a grid of fixed-size tiles, each compressed independently,
    /// and writes the `TileWidth` / `TileLength` / `TileOffsets` /
    /// `TileByteCounts` fields (tags 322 / 323 / 324 / 325) in place of
    /// the strip fields (§15: "When the tiling fields ... are used, they
    /// replace the StripOffsets, StripByteCounts, and RowsPerStrip
    /// fields ... Do not use both strip-oriented and tile-oriented
    /// fields in the same TIFF file"). Both dimensions must be a multiple
    /// of 16 per §15's `TileWidth` / `TileLength` requirement. Boundary
    /// tiles are padded out to the tile geometry (§15 "Padding":
    /// replicating the last column / row so the padded areas compress
    /// well); the decoder displays only the `ImageWidth x ImageLength`
    /// region and ignores the padding. Tiles are laid out left-to-right
    /// then top-to-bottom (§15 `TileOffsets`). Supported on the
    /// byte-aligned chunky formats (`Gray8` / `Gray16Le` / `Rgb24` /
    /// `Palette8`) under None / PackBits / LZW / Deflate / Zstd, with or
    /// without the §14 predictor (applied per-tile, matching the
    /// decoder), and on the 1-bit `Bilevel` / `TransparencyMask` formats
    /// under the same byte-aligned compressors (sub-byte tile-row bit
    /// packing with §15 edge replication, no predictor — see
    /// `build_tiles_bilevel`). Tiling is rejected on CCITT compression.
    /// It composes with
    /// `planar = true` on `Rgb24`: one row-major tile grid per component
    /// plane, emitted plane-0 first then plane-1, etc., per §15
    /// TileOffsets ("For PlanarConfiguration = 2, the offsets for the
    /// first component plane are stored first, followed by all the offsets
    /// for the second component plane").
    pub tiling: Option<(u32, u32)>,
    /// Emit BigTIFF instead of classic TIFF. Classic TIFF is the default
    /// (`false`); when set, the encoder writes the 16-byte BigTIFF header
    /// (II/MM + magic 43 + offset-bytesize 8 + reserved 0 + 8-byte
    /// first-IFD offset) per the Adobe Pagemaker 6.0 BigTIFF design that
    /// the decoder's [`crate::ifd::parse_header`] / [`crate::ifd::parse_ifd`]
    /// already read. Each IFD then uses 20-byte entries (tag:u16 +
    /// type:u16 + count:u64 + value-or-offset:u64) with an 8-byte
    /// next-IFD pointer, the inline-value threshold widens from 4 to 8
    /// bytes, and the LONG offset/byte-count fields (`StripOffsets`,
    /// `StripByteCounts`, `TileOffsets`, `TileByteCounts`) are written
    /// as LONG8 (type 16) so the on-disk layout is no longer pinned to
    /// the 32-bit u32 ceiling that classic TIFF enforces (the encoder
    /// returns precise `Error::Unsupported` if the final byte address
    /// exceeds `u32::MAX` on a classic page; BigTIFF lifts that limit
    /// to the full u64 file-offset range). The pixel / IFD-entry
    /// semantics are otherwise identical to classic TIFF — all the
    /// pixel formats, compressors, predictor / planar / tiling flags
    /// compose with `bigtiff = true` unchanged.
    ///
    /// For [`encode_tiff_multi`], every page must agree on the variant
    /// (all classic or all BigTIFF); mixing is rejected with a precise
    /// error.
    pub bigtiff: bool,
}

/// Pixel layouts the encoder knows how to write.
#[derive(Debug, Clone)]
pub enum EncodePixelFormat<'a> {
    /// 1-bit bilevel (1 sample per pixel, MSB-first byte packing).
    /// `pixels` must contain `ceil(width / 8) * height` bytes. The
    /// bit convention follows §10 / §11: bit value 0 = white,
    /// 1 = black. The `WhiteIsZero` (default) PhotometricInterpretation
    /// reads those bits straight through; combine with
    /// `BlackIsZero` by inverting the input first. Required for
    /// CCITT compression schemes ([`TiffCompression::CcittRle`],
    /// [`TiffCompression::CcittT4OneD`]).
    Bilevel { pixels: &'a [u8] },
    /// 1-bit transparency mask (TIFF 6.0 §"PhotometricInterpretation"
    /// value 4, page 37). 1 sample per pixel, MSB-first byte packing —
    /// the on-disk layout is identical to [`Self::Bilevel`], but the
    /// encoder writes `PhotometricInterpretation = 4` (Transparency
    /// Mask) and sets bit 2 of `NewSubfileType` (tag 254), which the
    /// spec defines as "1 if the image defines a transparency mask
    /// for another image in this TIFF file. The PhotometricInterpretation
    /// value must be 4". `pixels` must contain `ceil(width / 8) * height`
    /// bytes; the bit convention is fixed by spec (§Photometric-
    /// Interpretation page 37: "The 1-bits define the interior of the
    /// region; the 0-bits define the exterior of the region"). The
    /// spec recommends PackBits but does not forbid the other
    /// compressions; this encoder accepts None / PackBits / LZW /
    /// Deflate / Zstd / CCITT-MH / CCITT-T.4-1D, the same compressor
    /// set [`Self::Bilevel`] accepts.
    TransparencyMask { pixels: &'a [u8] },
    /// 8-bit greyscale (BlackIsZero, 1 sample per pixel).
    /// `pixels.len() == width * height`.
    Gray8 { pixels: &'a [u8] },
    /// 16-bit greyscale (BlackIsZero, 1 sample per pixel,
    /// little-endian on disk). `pixels.len() == width * height * 2`.
    Gray16Le { pixels: &'a [u8] },
    /// 8-bit packed RGB. `pixels.len() == width * height * 3`.
    Rgb24 { pixels: &'a [u8] },
    /// 8-bit indexed palette. `indices.len() == width * height`,
    /// `palette.len() <= 256` (any extras are ignored).
    Palette8 {
        indices: &'a [u8],
        palette: &'a [RgbColor],
    },
    /// 8-bit chunky 1976 CIE L\*a\*b\* (`PhotometricInterpretation = 8`,
    /// `SamplesPerPixel = 3`, `BitsPerSample = 8 / 8 / 8`) per TIFF 6.0
    /// §23 "CIE L\*a\*b\* Images" (page 110). `pixels` is row-major
    /// interleaved `(L*, a*, b*)` triples — `pixels.len() == width *
    /// height * 3`. The on-disk bit interpretation is fixed by §23: L\*
    /// is unsigned 0..255 mapping linearly to the perceptual 0..100
    /// lightness scale, and a\* / b\* are two's-complement signed bytes
    /// in -128..127 representing the red/green and yellow/blue chrominance
    /// channels (§23: "The a\* and b\* ranges will be represented as
    /// signed 8 bit values"). The encoder writes these bytes through to
    /// the strip / tile / plane payload verbatim — the caller owns the
    /// colourimetric encoding, exactly as the decoder takes them
    /// verbatim back off disk. Compressors accepted: None / PackBits /
    /// LZW / Deflate / Zstd (the byte-aligned, photometric-agnostic set
    /// the other multi-bit photometric paths use); CCITT is bilevel-only
    /// per §10 / §11 and rejected here. `Predictor = 2` (TIFF 6.0 §14
    /// horizontal differencing, per-component on chunky multi-sample
    /// data) composes; `PlanarConfiguration = 2` (separate L\* / a\* /
    /// b\* component planes) composes (§14 says differencing in planar
    /// "works the same as it does for grayscale data" — each plane is
    /// differenced independently with an offset of one sample); tiled
    /// layout (§15) composes for both chunky and planar.
    CieLab8 { pixels: &'a [u8] },
    /// 8-bit 1-sample CIE L\* monochrome (`PhotometricInterpretation =
    /// 8`, `SamplesPerPixel = 1`, `BitsPerSample = 8`) per TIFF 6.0 §23
    /// page 110 "Usage of other Fields": "SamplesPerPixel - ExtraSamples:
    /// 3 for L\*a\*b\*, 1 implies L\* only, for monochrome data".
    /// `pixels.len() == width * height`. Each byte is L\* on the
    /// 0..255-maps-to-0..100 scale. As with [`Self::CieLab8`], the bytes
    /// are written through verbatim. Compressors accepted: None /
    /// PackBits / LZW / Deflate / Zstd. `Predictor = 2` composes (single-sample
    /// chunky path with offset = 1); `PlanarConfiguration = 2` is
    /// rejected per §"PlanarConfiguration" "irrelevant" for
    /// `SamplesPerPixel = 1`; tiled layout composes.
    CieLabL8 { pixels: &'a [u8] },
    /// 8-bit chunky CMYK per TIFF 6.0 §16 "CMYK Images" (page 69):
    /// `PhotometricInterpretation = 5`, `SamplesPerPixel = 4`,
    /// `BitsPerSample = 8, 8, 8, 8`. `pixels` is row-major interleaved
    /// cyan, magenta, yellow, black quadruples — its length must equal
    /// `width * height * 4`. The on-disk bit interpretation is fixed
    /// by §16: each component is the amount of that ink at the pixel
    /// on the canonical `InkSet = 1` CMYK ordering, where 0 means no
    /// ink and 255 means full coverage (§16 `InkSet` page 70: "Usually,
    /// a value of 0 represents 0 % ink coverage and a value of 255
    /// represents 100 % ink coverage for that component"). The encoder
    /// writes the caller-supplied bytes through to the strip, tile, or
    /// plane payload verbatim — the caller owns the colourimetric
    /// encoding, exactly as the decoder takes them verbatim off disk
    /// and collapses them into additive RGB by the §16 "amount of dye"
    /// convention. Alongside the §16-required Baseline tags
    /// `SamplesPerPixel`, `BitsPerSample`, and `PhotometricInterpretation`,
    /// the encoder also writes the two optional §16 separated-image
    /// tags `InkSet = 1` (tag 332, the "CMYK" InkSet value) and
    /// `NumberOfInks = 4` (tag 334). Both match their §16 defaults,
    /// but emitting them explicitly makes the written file
    /// self-describing to readers that key on those fields. Compressors
    /// accepted: the byte-aligned, photometric-agnostic set the other
    /// multi-bit photometric paths use (None, PackBits, LZW, Deflate,
    /// Zstd).
    /// CCITT is bilevel-only per §10 and §11 and rejected here.
    /// `Predictor = 2` (TIFF 6.0 §14 horizontal differencing,
    /// per-component on chunky multi-sample data with offset
    /// `SamplesPerPixel = 4`) composes. `PlanarConfiguration = 2`
    /// composes too: each of the four component planes is written as
    /// its own strip per §"PlanarConfiguration", and the §14 predictor
    /// differences each plane independently with an offset of one
    /// sample (§14: "Differencing works the same as it does for
    /// grayscale data" when PlanarConfiguration is 2). Tiled layout
    /// (§15) composes for both chunky and planar.
    Cmyk32 { pixels: &'a [u8] },
    /// 8-bit chunky `(Y, Cb, Cr)` per TIFF 6.0 §21 "YCbCr Images"
    /// (page 89), `PhotometricInterpretation = 6`, `SamplesPerPixel = 3`,
    /// `BitsPerSample = 8, 8, 8` at the §21 chroma-subsampling-1:1
    /// (`YCbCrSubSampling = [1, 1]`) layout. `pixels` is row-major,
    /// interleaved `(Y, Cb, Cr)` triples — `pixels.len() == width *
    /// height * 3`. At `YCbCrSubSampling = [1, 1]` the §21 "Ordering
    /// of Component Samples" rule for `PlanarConfiguration = 1`
    /// collapses to one Y sample per data unit (1×1 luminance grid)
    /// followed by one Cb sample and one Cr sample — i.e. the bytes
    /// are written through to the strip / tile payload exactly as the
    /// caller supplied them, with no re-tiling. The caller owns the
    /// RGB→YCbCr conversion; the encoder transports the supplied bytes
    /// verbatim, matching the decoder's `build_rgb24_from_ycbcr` path
    /// which treats §21's stated chunky data-unit layout as fact.
    /// Alongside the §21 / Baseline tags (`SamplesPerPixel`,
    /// `BitsPerSample`, `PhotometricInterpretation`), the encoder
    /// emits the three §21 fields with their full-range
    /// no-headroom/no-footroom values fixed by §20
    /// "ReferenceBlackWhite" page 87 ("Useful ReferenceBlackWhite
    /// values for YCbCr images are: `[0, 255, 128, 255, 128, 255]`
    /// no headroom/footroom"):
    ///
    /// * `YCbCrSubSampling = [1, 1]` (tag 530), the chunky-444 layout
    ///   the encoder writes;
    /// * `YCbCrPositioning = 1` (tag 531), §21 "centered" — the
    ///   tag's documented default and the only positioning that
    ///   matters when both subsampling factors are 1;
    /// * `ReferenceBlackWhite = [0/1, 255/1, 128/1, 255/1, 128/1,
    ///   255/1]` (tag 532), the §20 page 87 "no headroom/footroom"
    ///   reference coding range — §21 says this field "must be used
    ///   explicitly" for Class Y images.
    ///
    /// The §21 default `YCbCrCoefficients` (tag 529, CCIR
    /// Recommendation 601-1's `{299/1000, 587/1000, 114/1000}`
    /// luma weights) are omitted: §21 explicitly defaults to those
    /// values and the decoder's `ycbcr_to_rgb` matrix is the Q16
    /// inverse of the same `{0.299, 0.587, 0.114}` coefficients, so
    /// emitting the tag would just restate the spec default. Future
    /// rounds add chroma-subsampled (`YCbCrSubSampling = [2, 1]`,
    /// `[2, 2]`, etc.) and `PlanarConfiguration = 2` writers; the
    /// 1:1 chunky surface here is the minimal §21-conformant slice.
    /// Compressors accepted: None / PackBits / LZW / Deflate / Zstd
    /// (the byte-aligned photometric-agnostic set the other multi-bit
    /// photometric paths use). CCITT is bilevel-only per §10 / §11
    /// and rejected here. `Predictor = 2`,
    /// `PlanarConfiguration = 2`, and tiled layout are deferred —
    /// the §21 sample-ordering rule changes shape under non-1:1
    /// subsampling, so the encoder pins those flags off in this
    /// initial YCbCr round and rejects the combinations with a
    /// precise error rather than emitting something the decoder
    /// might mis-tile.
    YCbCr24 { pixels: &'a [u8] },
    /// 8-bit chroma-subsampled `(Y, Cb, Cr)` per TIFF 6.0 §21 "YCbCr
    /// Images" (pages 90–94), `PhotometricInterpretation = 6`,
    /// `SamplesPerPixel = 3`, `BitsPerSample = [8, 8, 8]`,
    /// `PlanarConfiguration = 1` (chunky). `pixels` is the
    /// **full-resolution** row-major interleaved `(Y, Cb, Cr)` raster
    /// (`pixels.len() == width * height * 3`); the encoder performs the
    /// chroma decimation and the §21 "Ordering of Component Samples"
    /// data-unit packing.
    ///
    /// `subsampling` is `(ChromaSubsampleHoriz, ChromaSubsampleVert)`
    /// from the §21 `YCbCrSubSampling` field (tag 530). §21 page 90
    /// restricts each factor to `1`, `2`, or `4` and requires
    /// `YCbCrSubsampleVert <= YCbCrSubsampleHoriz`, so the supported
    /// set is `(1,1)`, `(2,1)`, `(2,2)`, `(4,1)`, `(4,2)` — the same
    /// configurations the decoder's full-resolution chroma-splat path
    /// reverses. `(1,1)` is accepted here too (it is the trivial
    /// no-subsampling case) and behaves identically to
    /// [`EncodePixelFormat::YCbCr24`].
    ///
    /// §21 page 90: "ImageWidth and ImageLength are constrained to be
    /// integer multiples of YCbCrSubsampleHoriz and
    /// YCbCrSubsampleVert respectively." The encoder rejects a
    /// `width`/`height` that is not a multiple of the corresponding
    /// factor with a precise error.
    ///
    /// Data-unit layout (§21 page 93): a data unit is
    /// `ChromaSubsampleVert` rows of `ChromaSubsampleHoriz` Y samples
    /// (row-major), then one Cb sample, then one Cr sample. For
    /// `(sh, sv) = (4, 2)` the worked example on page 94 is
    /// `Y00, Y01, Y02, Y03, Y10, Y11, Y12, Y13, Cb00, Cr00, Y04, …`.
    /// The encoder visits data units left-to-right then top-to-bottom
    /// and emits exactly this byte order, which is what the decoder's
    /// `build_rgb24_from_ycbcr` block walker consumes.
    ///
    /// Chroma decimation: each `sh × sv` block's Cb / Cr is the rounded
    /// arithmetic mean of the block's full-resolution Cb / Cr samples
    /// (a box filter — the symmetric even-tap filter §21 page 92 calls
    /// out for `YCbCrPositioning = 1` centered subsampling). The Y
    /// samples are transported full-resolution and unchanged, so the
    /// luminance round-trips bit-exact; chroma round-trips exactly only
    /// when it is already constant across each block (decode splats one
    /// Cb / Cr back over the whole block).
    ///
    /// The same §21 / §20 fields the 1:1 path emits are written, with
    /// tag 530 carrying the actual `[sh, sv]`. `YCbCrPositioning = 1`
    /// (centered) is emitted to match the box-filter decimation.
    /// Compressors accepted: None / PackBits / LZW / Deflate / Zstd.
    /// CCITT is bilevel-only and rejected. `Predictor = 2`,
    /// `PlanarConfiguration = 2`, and tiled layout are rejected with a
    /// precise error (the data-unit packing is single-strip chunky
    /// only in this round).
    YCbCrSubsampled24 {
        pixels: &'a [u8],
        subsampling: (u16, u16),
    },
}

/// Compression scheme for an [`EncodePage`].
#[derive(Debug, Clone, Copy)]
pub enum TiffCompression {
    None,
    PackBits,
    Lzw,
    Deflate,
    /// Compression=50000 — Zstandard (RFC 8478), the de-facto
    /// registry extension documented in the OxideAV trace doc
    /// `docs/image/tiff/tiff-zstd-compression-50000.md`. Follows the
    /// Compression=8 Deflate template exactly: each strip / tile
    /// becomes one self-contained Zstandard frame over the
    /// post-predictor sample bytes, so it composes with every
    /// byte-aligned pixel format, `Predictor = 2`, planar writing,
    /// tiling, and BigTIFF just like [`TiffCompression::Deflate`].
    /// The encoder compression level is an out-of-band runtime
    /// parameter (never stored in the file); the writer uses the
    /// compression backend's default.
    Zstd,
    /// Compression=2 — CCITT Modified Huffman (TIFF 6.0 §10). Bilevel
    /// only. Encoded as a sequence of white/black run-length codes
    /// from Tables 1/T.4 and 2/T.4. No EOL codes; rows align to byte
    /// boundaries.
    CcittRle,
    /// Compression=3 — CCITT T.4 1-D (TIFF 6.0 §11). Bilevel only.
    /// Each row is preceded by a 12-bit EOL prefix. With
    /// `eol_byte_aligned`, the EOL is byte-aligned (T4Options bit 2).
    CcittT4OneD {
        eol_byte_aligned: bool,
    },
    /// Compression=3 — CCITT T.4 2-D / Modified READ (TIFF 6.0 §11
    /// with T4Options bit 0 set). Bilevel only. Row 0 is coded 1-D
    /// (tag bit 1) and seeds the reference line for row 1; rows
    /// 1.. are coded 2-D (tag bit 0) against the previously coded
    /// row using the Pass / Horizontal / Vertical mode codes from
    /// Table 4/T.4 (docs §1). `eol_byte_aligned` mirrors T4Options
    /// bit 2 just as in [`TiffCompression::CcittT4OneD`].
    CcittT4TwoD {
        eol_byte_aligned: bool,
    },
    /// Compression=4 — CCITT T.6 / Modified Modified READ (MMR)
    /// (TIFF 6.0 §11). Bilevel only. Every row is 2-D against the
    /// previously coded row; the first row's reference is an
    /// imaginary all-white line per T.6 §2.2.1. No EOL framing
    /// between rows. The decoder stops at `rows` rows so no EOFB
    /// sentinel is written.
    CcittT6,
}

impl TiffCompression {
    fn tag_value(self) -> u16 {
        match self {
            TiffCompression::None => COMPRESSION_NONE,
            TiffCompression::PackBits => COMPRESSION_PACKBITS,
            TiffCompression::Lzw => COMPRESSION_LZW,
            TiffCompression::Deflate => COMPRESSION_DEFLATE_ADOBE,
            TiffCompression::Zstd => COMPRESSION_ZSTD,
            TiffCompression::CcittRle => COMPRESSION_CCITT_HUFFMAN,
            // Compression=3 covers both T.4 1-D and T.4 2-D; the
            // T4Options tag (292) distinguishes them on the wire.
            TiffCompression::CcittT4OneD { .. } | TiffCompression::CcittT4TwoD { .. } => {
                COMPRESSION_CCITT_T4
            }
            TiffCompression::CcittT6 => COMPRESSION_CCITT_T6,
        }
    }

    /// Compress `raw` per this scheme. For bilevel CCITT schemes the
    /// caller supplies the geometry; non-CCITT schemes ignore those
    /// arguments.
    fn pack(self, raw: &[u8], width: u32, rows: u32) -> Result<Vec<u8>> {
        match self {
            TiffCompression::None => Ok(raw.to_vec()),
            TiffCompression::PackBits => Ok(pack_packbits(raw)),
            TiffCompression::Lzw => Ok(pack_lzw(raw)),
            TiffCompression::Deflate => pack_deflate(raw),
            TiffCompression::Zstd => pack_zstd(raw),
            TiffCompression::CcittRle => encode_ccitt(
                raw,
                width,
                rows,
                CcittVariant::ModifiedHuffman,
                FillOrder::MsbFirst,
            ),
            TiffCompression::CcittT4OneD { eol_byte_aligned } => encode_ccitt(
                raw,
                width,
                rows,
                CcittVariant::T4OneD { eol_byte_aligned },
                FillOrder::MsbFirst,
            ),
            TiffCompression::CcittT4TwoD { eol_byte_aligned } => encode_ccitt(
                raw,
                width,
                rows,
                CcittVariant::T4TwoD { eol_byte_aligned },
                FillOrder::MsbFirst,
            ),
            TiffCompression::CcittT6 => {
                encode_ccitt(raw, width, rows, CcittVariant::T6, FillOrder::MsbFirst)
            }
        }
    }

    /// Bilevel CCITT schemes accept only [`EncodePixelFormat::Bilevel`].
    fn is_ccitt(self) -> bool {
        matches!(
            self,
            TiffCompression::CcittRle
                | TiffCompression::CcittT4OneD { .. }
                | TiffCompression::CcittT4TwoD { .. }
                | TiffCompression::CcittT6
        )
    }
}

/// One planned page's compressed image segments plus its IFD and any
/// out-of-line value blobs. `strips` is the segment payload list — one
/// entry for a chunky single-strip page, `SamplesPerPixel` entries for
/// `PlanarConfiguration = 2`, or one entry per tile (row-major) for a
/// tiled page (TIFF 6.0 §15). The on-disk offset / byte-count arrays
/// (`StripOffsets` / `StripByteCounts` or `TileOffsets` /
/// `TileByteCounts`, depending on which tags the IFD carries) index
/// into this list in storage order, so the address-assignment pass is
/// identical for strips and tiles.
struct PlannedPage {
    strips: Vec<Vec<u8>>,
    ifd: PageIfd,
    externals: Vec<(BlobId, Vec<u8>)>,
}

/// Encode a single-page TIFF file. Produces the complete byte
/// sequence. Convenience wrapper around [`encode_tiff_multi`].
pub fn encode_tiff(page: &EncodePage<'_>) -> Result<Vec<u8>> {
    encode_tiff_multi(std::slice::from_ref(page))
}

/// Encode a multi-page TIFF file (one IFD per page, chained via
/// the next-IFD pointer in file order). Produces the complete byte
/// sequence.
pub fn encode_tiff_multi(pages: &[EncodePage<'_>]) -> Result<Vec<u8>> {
    if pages.is_empty() {
        return Err(Error::invalid("TIFF encode: must supply at least one page"));
    }

    // All pages must agree on the on-disk variant — a classic-TIFF file
    // and a BigTIFF file have incompatible IFD layouts, so we cannot
    // mix them in one chain.
    let bigtiff = pages[0].bigtiff;
    if pages.iter().any(|p| p.bigtiff != bigtiff) {
        return Err(Error::invalid(
            "TIFF encode: encode_tiff_multi pages must all agree on `bigtiff` (cannot mix \
             classic-TIFF and BigTIFF IFDs in one file)",
        ));
    }

    // Layout strategy:
    //
    // 1. Header. Classic: 8 bytes (II + 42 + 4-byte first-IFD offset).
    //    BigTIFF: 16 bytes (II + 43 + 2-byte off-size=8 + 2-byte
    //    reserved=0 + 8-byte first-IFD offset), per Adobe Pagemaker 6.0
    //    BigTIFF.
    // 2. For each page in order:
    //    a. Compressed strip / tile data.
    //    b. Out-of-line value blobs that don't fit inline (BitsPerSample
    //       array for RGB, ColorMap for palette, StripOffsets /
    //       StripByteCounts arrays for >1 strip/tile).
    //    c. The IFD itself (count + N × entry + next-IFD; size depends
    //       on the variant).
    //
    // We compute layout in two passes: first sizing pass, then write
    // pass. The IFD offset of page N+1 is the next-IFD field of
    // page N's IFD.

    // Variant-dependent constants. All addresses go through these so the
    // classic / BigTIFF write paths share one set of loops.
    let header_size: u64 = if bigtiff { 16 } else { 8 };
    let entry_size: u64 = if bigtiff { 20 } else { 12 }; // u16+u16+count+value-or-offset
    let count_size: u64 = if bigtiff { 8 } else { 2 }; // IFD entry-count field
    let next_ifd_size: u64 = if bigtiff { 8 } else { 4 }; // next-IFD slot
    let inline_threshold: usize = if bigtiff { 8 } else { 4 };
    let offset_bytes: usize = if bigtiff { 8 } else { 4 }; // value-or-offset slot width
    let array_align: u64 = if bigtiff { 8 } else { 4 }; // LONG8 vs LONG alignment

    // ---- Sizing pass: per-page, derive the on-disk layout ----
    let mut planned: Vec<PlannedPage> = Vec::with_capacity(pages.len());
    for p in pages {
        let plan = plan_page_full(p, bigtiff)?;
        planned.push(plan);
    }

    // ---- Address assignment ----
    //
    // Start at byte `header_size` (after the variant-specific header).
    // For each page we lay out:
    // [compressed strip(s) / tile(s)][external blobs][StripOffsets /
    // ByteCounts LONG (classic) or LONG8 (BigTIFF) arrays, only when
    // count > 1][IFD].
    //
    // We need to know the IFD offset *before* writing it (it goes in
    // the previous IFD's next-IFD slot or in the header for the
    // first page), so we walk pages once to assign byte ranges. The
    // StripOffsets / StripByteCounts arrays are written out-of-line
    // only when there are multiple strips/tiles; a single-strip chunky
    // page keeps both inline in the IFD value slot.
    let mut cursor: u64 = header_size;
    struct PlannedPageAddr {
        // One (offset, size) per strip / tile, in storage order.
        strips: Vec<(u64, u64)>,
        externals: Vec<(BlobId, u64, u64)>, // (id, offset, size)
        // File offsets of the out-of-line StripOffsets / StripByteCounts
        // arrays (only Some when there is more than one strip/tile).
        strip_offsets_array: Option<u64>,
        strip_byte_counts_array: Option<u64>,
        ifd_offset: u64,
    }
    let mut addrs: Vec<PlannedPageAddr> = Vec::with_capacity(planned.len());
    for plan in &planned {
        // Strip / tile payloads, laid out contiguously in storage order.
        let mut strip_addrs = Vec::with_capacity(plan.strips.len());
        for strip in &plan.strips {
            strip_addrs.push((cursor, strip.len() as u64));
            cursor += strip.len() as u64;
        }
        let mut ext_addrs = Vec::with_capacity(plan.externals.len());
        for (id, blob) in &plan.externals {
            // SHORT arrays are 2-byte aligned; RATIONAL arrays are
            // 4-byte aligned (each value is a 4-byte LONG numerator
            // followed by a 4-byte LONG denominator, so a 4-byte
            // alignment is the natural one). The 4-byte choice is
            // a safe superset for the SHORT blobs too.
            let align: u64 = match id {
                BlobId::ReferenceBlackWhite => 4,
                BlobId::BitsPerSample | BlobId::ColorMapWords => 2,
            };
            if cursor % align != 0 {
                cursor += align - (cursor % align);
            }
            ext_addrs.push((*id, cursor, blob.len() as u64));
            cursor += blob.len() as u64;
        }
        // Out-of-line StripOffsets / StripByteCounts arrays for
        // multi-strip / multi-tile pages. Classic TIFF uses LONG
        // (4 bytes per value); BigTIFF uses LONG8 (8 bytes per value),
        // so the alignment + per-entry stride both follow the variant.
        let (strip_offsets_array, strip_byte_counts_array) = if plan.strips.len() > 1 {
            if cursor % array_align != 0 {
                cursor += array_align - (cursor % array_align);
            }
            let so = cursor;
            cursor += array_align * plan.strips.len() as u64;
            let sbc = cursor;
            cursor += array_align * plan.strips.len() as u64;
            (Some(so), Some(sbc))
        } else {
            (None, None)
        };
        // IFD must be 2-byte aligned (entries start with u16 fields).
        if cursor % 2 != 0 {
            cursor += 1;
        }
        let ifd_offset = cursor;
        // count(2 or 8) + entries × (12 or 20) + next_ifd(4 or 8)
        let ifd_size = count_size + (plan.ifd.entries.len() as u64) * entry_size + next_ifd_size;
        cursor += ifd_size;
        addrs.push(PlannedPageAddr {
            strips: strip_addrs,
            externals: ext_addrs,
            strip_offsets_array,
            strip_byte_counts_array,
            ifd_offset,
        });
        // Classic TIFF caps every offset (and therefore the total file
        // size if the IFD is at the end of the file) at u32::MAX.
        // BigTIFF lifts the cap to the full u64 range — there is no
        // documented BigTIFF size ceiling in the Adobe Pagemaker 6.0
        // design beyond what u64 can express.
        if !bigtiff && (ifd_offset > u32::MAX as u64 || cursor > u32::MAX as u64) {
            return Err(Error::invalid(
                "TIFF encode: classic-TIFF 32-bit offset overflow (would need BigTIFF — set \
                 EncodePage::bigtiff = true)",
            ));
        }
    }

    // ---- Write pass ----
    let total = cursor as usize;
    let mut out = vec![0u8; total];
    // Header — classic 8-byte or BigTIFF 16-byte.
    if bigtiff {
        // II + 43 + offset-bytesize 8 + reserved 0 + 8-byte first-IFD
        // offset. Per the BigTIFF design (`docs/image/tiff/tiff6.pdf`
        // BigTIFF / Adobe Pagemaker 6.0 sections, reproduced in
        // `src/ifd.rs`).
        out[0] = b'I';
        out[1] = b'I';
        out[2..4].copy_from_slice(&BIGTIFF_MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&8u16.to_le_bytes()); // offset bytesize
        out[6..8].copy_from_slice(&0u16.to_le_bytes()); // reserved
        out[8..16].copy_from_slice(&addrs[0].ifd_offset.to_le_bytes());
    } else {
        out[0] = b'I';
        out[1] = b'I';
        out[2..4].copy_from_slice(&TIFF_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&(addrs[0].ifd_offset as u32).to_le_bytes());
    }

    for (i, (plan, addr)) in planned.iter().zip(addrs.iter()).enumerate() {
        // Strip / tile payload(s).
        for (strip, (off, size)) in plan.strips.iter().zip(addr.strips.iter()) {
            assert_eq!(strip.len() as u64, *size);
            out[*off as usize..(*off + *size) as usize].copy_from_slice(strip);
        }
        // External blobs.
        for (j, (id, off, size)) in addr.externals.iter().enumerate() {
            assert_eq!(*id, plan.externals[j].0);
            let blob = &plan.externals[j].1;
            assert_eq!(blob.len() as u64, *size);
            out[*off as usize..(*off + *size) as usize].copy_from_slice(blob);
        }
        // Out-of-line StripOffsets / StripByteCounts arrays (only present
        // for multi-strip / multi-tile pages). Classic TIFF writes
        // 4 bytes per entry (LONG); BigTIFF writes 8 bytes (LONG8).
        if let Some(so) = addr.strip_offsets_array {
            for (k, (off, _)) in addr.strips.iter().enumerate() {
                let slot = so as usize + k * (array_align as usize);
                if bigtiff {
                    out[slot..slot + 8].copy_from_slice(&off.to_le_bytes());
                } else {
                    out[slot..slot + 4].copy_from_slice(&(*off as u32).to_le_bytes());
                }
            }
        }
        if let Some(sbc) = addr.strip_byte_counts_array {
            for (k, (_, size)) in addr.strips.iter().enumerate() {
                let slot = sbc as usize + k * (array_align as usize);
                if bigtiff {
                    out[slot..slot + 8].copy_from_slice(&size.to_le_bytes());
                } else {
                    out[slot..slot + 4].copy_from_slice(&(*size as u32).to_le_bytes());
                }
            }
        }
        // IFD.
        let ifd_off = addr.ifd_offset as usize;
        // Entry-count field — 2 bytes in classic TIFF, 8 bytes in BigTIFF.
        if bigtiff {
            out[ifd_off..ifd_off + 8]
                .copy_from_slice(&(plan.ifd.entries.len() as u64).to_le_bytes());
        } else {
            out[ifd_off..ifd_off + 2]
                .copy_from_slice(&(plan.ifd.entries.len() as u16).to_le_bytes());
        }
        let entries_start = ifd_off + count_size as usize;
        let next_ifd_off = entries_start + plan.ifd.entries.len() * (entry_size as usize);
        // Resolve each entry's value-or-offset slot. The entry layout
        // differs between variants:
        //   classic: tag(2) + type(2) + count(4)  + value/offset(4)   = 12 bytes
        //   BigTIFF: tag(2) + type(2) + count(8)  + value/offset(8)   = 20 bytes
        for (k, e) in plan.ifd.entries.iter().enumerate() {
            let entry_off = entries_start + k * (entry_size as usize);
            out[entry_off..entry_off + 2].copy_from_slice(&e.tag.to_le_bytes());
            out[entry_off + 2..entry_off + 4].copy_from_slice(&e.field_type.to_le_bytes());
            if bigtiff {
                out[entry_off + 4..entry_off + 12].copy_from_slice(&e.count.to_le_bytes());
            } else {
                // Classic TIFF stores count as u32; if a caller-side count
                // somehow exceeds u32::MAX in classic mode the address
                // assignment above would have already errored, but we
                // guard defensively.
                if e.count > u32::MAX as u64 {
                    return Err(Error::invalid(
                        "TIFF encode: classic-TIFF entry count exceeds u32::MAX",
                    ));
                }
                out[entry_off + 4..entry_off + 8].copy_from_slice(&(e.count as u32).to_le_bytes());
            }
            // Value-or-offset slot:
            let slot_off = entry_off + if bigtiff { 12 } else { 8 };
            let slot = &mut out[slot_off..slot_off + offset_bytes];
            match &e.value {
                IfdValue::Inline(bytes) => {
                    let n = bytes.len();
                    debug_assert!(n <= inline_threshold);
                    slot[..n].copy_from_slice(bytes);
                    for b in &mut slot[n..] {
                        *b = 0;
                    }
                }
                IfdValue::StripOffsets => {
                    let val: u64 = if let Some(so) = addr.strip_offsets_array {
                        // >1 strip / tile: value slot holds the file
                        // offset of the out-of-line LONG / LONG8 array.
                        so
                    } else {
                        // Single strip / tile: the offset fits inline
                        // (LONG for classic — 4 bytes; LONG8 for BigTIFF
                        // — 8 bytes, exactly filling the value slot).
                        addr.strips[0].0
                    };
                    if bigtiff {
                        slot.copy_from_slice(&val.to_le_bytes());
                    } else {
                        slot.copy_from_slice(&(val as u32).to_le_bytes());
                    }
                }
                IfdValue::StripByteCounts => {
                    let val: u64 = if let Some(sbc) = addr.strip_byte_counts_array {
                        sbc
                    } else {
                        addr.strips[0].1
                    };
                    if bigtiff {
                        slot.copy_from_slice(&val.to_le_bytes());
                    } else {
                        slot.copy_from_slice(&(val as u32).to_le_bytes());
                    }
                }
                IfdValue::ExternalBlob(id) => {
                    let (_, off, _) = addr
                        .externals
                        .iter()
                        .find(|(x, _, _)| *x == *id)
                        .ok_or_else(|| {
                            Error::invalid("TIFF encode: missing planned blob for entry")
                        })?;
                    if bigtiff {
                        slot.copy_from_slice(&off.to_le_bytes());
                    } else {
                        slot.copy_from_slice(&(*off as u32).to_le_bytes());
                    }
                }
            }
        }
        // Next-IFD pointer.
        let next_offset: u64 = if i + 1 < addrs.len() {
            addrs[i + 1].ifd_offset
        } else {
            0
        };
        if bigtiff {
            out[next_ifd_off..next_ifd_off + 8].copy_from_slice(&next_offset.to_le_bytes());
        } else {
            out[next_ifd_off..next_ifd_off + 4]
                .copy_from_slice(&(next_offset as u32).to_le_bytes());
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobId {
    BitsPerSample,
    ColorMapWords,
    /// 6-RATIONAL `ReferenceBlackWhite` (tag 532) blob — six
    /// 8-byte RATIONAL values = 48 bytes, always out of line.
    /// Carries the §20 "no headroom/footroom" full-range YCbCr
    /// reference coding ([0/1, 255/1, 128/1, 255/1, 128/1, 255/1]).
    ReferenceBlackWhite,
}

#[derive(Debug, Clone)]
enum IfdValue {
    /// Up to 4 bytes packed inline into the value/offset slot.
    Inline(Vec<u8>),
    /// Resolved at write time: the page's `StripOffsets` array. For a
    /// single strip (chunky) the LONG fits inline; for
    /// `PlanarConfiguration = 2` the per-plane offsets are written as a
    /// LONG array out-of-line and this slot holds its file offset.
    StripOffsets,
    /// Resolved at write time: the page's `StripByteCounts` array,
    /// inline for a single strip or an out-of-line LONG array for
    /// multiple strips.
    StripByteCounts,
    /// Reference to an external blob attached to this page.
    ExternalBlob(BlobId),
}

#[derive(Debug, Clone)]
struct PageIfdEntry {
    tag: u16,
    field_type: u16,
    /// Field-value count (`count` in IFD parlance). Classic TIFF stores
    /// this as a u32; BigTIFF as a u64. We keep it u64 here and narrow
    /// at write time, with a range check for the classic path.
    count: u64,
    value: IfdValue,
}

#[derive(Debug)]
struct PageIfd {
    entries: Vec<PageIfdEntry>,
}

fn plan_page_full(p: &EncodePage<'_>, bigtiff: bool) -> Result<PlannedPage> {
    // CCITT schemes are bilevel-only per TIFF 6.0 §10 / §11. Both the
    // generic Bilevel input and the TransparencyMask variant carry 1-bit
    // data with identical on-disk packing and so satisfy that gate.
    if p.compression.is_ccitt()
        && !matches!(
            p.kind,
            EncodePixelFormat::Bilevel { .. } | EncodePixelFormat::TransparencyMask { .. }
        )
    {
        return Err(Error::invalid(
            "TIFF encode: CCITT compression (Compression=2/3) requires Bilevel or \
             TransparencyMask input",
        ));
    }

    // PlanarConfiguration=2 (separate component planes) is only
    // meaningful when there is more than one sample per pixel; TIFF 6.0
    // §"PlanarConfiguration" (page 38): "If SamplesPerPixel is 1,
    // PlanarConfiguration is irrelevant." Reject the single-sample
    // formats and the bit-packed bilevel format up front. The
    // multi-sample formats the encoder writes are Rgb24 (SPP=3),
    // CieLab8 (SPP=3 — three 8-bit L*/a*/b* component planes per
    // §"PlanarConfiguration"), and Cmyk32 (SPP=4 — four 8-bit
    // C / M / Y / K component planes per §16 + §"PlanarConfiguration").
    if p.planar
        && !matches!(
            p.kind,
            EncodePixelFormat::Rgb24 { .. }
                | EncodePixelFormat::CieLab8 { .. }
                | EncodePixelFormat::Cmyk32 { .. }
        )
    {
        return Err(Error::invalid(
            "TIFF encode: PlanarConfiguration=2 (separate planes) requires a multi-sample \
             format; TIFF 6.0 §\"PlanarConfiguration\" says the field is irrelevant when \
             SamplesPerPixel is 1 (Gray8 / Gray16Le / Palette8 / Bilevel / TransparencyMask / \
             CieLabL8)",
        ));
    }
    // CCITT schemes are bilevel-only (rejected above) so they never
    // reach the planar path; the predictor is handled per-plane below.

    // YCbCr encode: `PlanarConfiguration = 2` and the §14 predictor both
    // depend on the §21 "Ordering of Component Samples" data-unit shape
    // and remain deferred. Tiled layout is now supported for both 4:4:4
    // (the data unit collapses to a plain chunky `(Y, Cb, Cr)` triple,
    // handled by the generic byte-aligned tile packer) and the non-1:1
    // chroma-subsampled case (`build_tiles_ycbcr_subsampled` packs each
    // tile's §21 data units, the layout `decode_tiles` /
    // `decode_tiles_ycbcr_subsampled` reverses). §21 page 90 requires
    // `TileWidth` / `TileLength` to be integer multiples of the
    // subsampling factors, which the geometry check below enforces.
    let is_ycbcr = matches!(
        p.kind,
        EncodePixelFormat::YCbCr24 { .. } | EncodePixelFormat::YCbCrSubsampled24 { .. }
    );
    if is_ycbcr {
        if p.planar {
            return Err(Error::invalid(
                "TIFF encode: PlanarConfiguration=2 with YCbCr is not supported in this round \
                 (the §21 data-unit ordering changes shape under non-1:1 subsampling)",
            ));
        }
        if p.predictor {
            return Err(Error::invalid(
                "TIFF encode: Predictor=2 with YCbCr is not supported in this round \
                 (the §21 chroma-difference samples and the §14 cumulative-add reversal \
                 interact through the §22 / §20 reference coding range)",
            ));
        }
        // §21 page 90: under subsampling, TileWidth / TileLength must be
        // integer multiples of the subsampling factors so a tile is a
        // whole number of data units.
        if let (Some((tw, th)), EncodePixelFormat::YCbCrSubsampled24 { subsampling, .. }) =
            (p.tiling, &p.kind)
        {
            let (sh, sv) = *subsampling;
            if tw % (sh as u32) != 0 || th % (sv as u32) != 0 {
                return Err(Error::invalid(format!(
                    "TIFF encode: subsampled-YCbCr TileWidth/TileLength ({tw}x{th}) must be \
                     integer multiples of YCbCrSubSampling ({sh},{sv}) per TIFF 6.0 §21 page 90"
                )));
            }
        }
    }

    // Tiled layout (TIFF 6.0 §15). Validate the geometry up front.
    if let Some((tw, th)) = p.tiling {
        // §15 TileWidth / TileLength: "TileWidth must be a multiple of
        // 16 … TileLength must be a multiple of 16 for compatibility
        // with compression schemes such as JPEG."
        if tw == 0 || th == 0 || tw % 16 != 0 || th % 16 != 0 {
            return Err(Error::invalid(format!(
                "TIFF encode: tile dimensions must be non-zero multiples of 16 \
                 (TIFF 6.0 §15 TileWidth / TileLength); got {tw}x{th}"
            )));
        }
        // 1-bit Bilevel / TransparencyMask input now tiles via the
        // dedicated sub-byte slicer `build_tiles_bilevel` below (§15: each
        // tile row is an independent byte-padded scanline, and `tile_w`
        // being a multiple of 16 keeps every tile-column boundary
        // byte-aligned at BitsPerSample = 1). It only combines with the
        // byte-aligned compressors the decoder's sub-byte tile path reads
        // (None / PackBits / LZW / Deflate / ZSTD): the CCITT schemes are
        // facsimile-line coders the encoder emits strip-oriented, and the
        // predictor is undefined for 1-bit data (both rejected just below
        // / above), so reaching `build_tiles_bilevel` implies a plain
        // byte-aligned compressor.
        if p.compression.is_ccitt() {
            return Err(Error::invalid(
                "TIFF encode: tiled layout cannot combine with CCITT compression \
                 (Compression=2/3), which is bilevel-only",
            ));
        }
        // Planar tile write (one tile grid per component plane, TIFF 6.0
        // §15 TileOffsets: "For PlanarConfiguration = 2, the offsets for
        // the first component plane are stored first, followed by all the
        // offsets for the second component plane, and so on") is handled
        // by `build_tiles_planar` below. It is only meaningful for the
        // multi-sample formats, which `p.planar` already restricts to
        // Rgb24 (SPP=3) / CieLab8 (SPP=3) / Cmyk32 (SPP=4) — the
        // single-sample formats are rejected above.
    }

    // The §14 horizontal-differencing predictor operates on whole
    // sample components; it has no meaning for bit-packed bilevel data,
    // and §14 only defines it for the LZW-family lossless coders. Reject
    // the impossible combinations up front so they surface precisely
    // rather than producing a file the decoder can't reverse.
    if p.predictor {
        if matches!(
            p.kind,
            EncodePixelFormat::Bilevel { .. } | EncodePixelFormat::TransparencyMask { .. }
        ) {
            return Err(Error::invalid(
                "TIFF encode: Predictor=2 (horizontal differencing) is undefined for 1-bit \
                 input (Bilevel / TransparencyMask) — TIFF 6.0 §14 differences whole sample \
                 components",
            ));
        }
        if p.compression.is_ccitt() {
            return Err(Error::invalid(
                "TIFF encode: Predictor=2 cannot combine with CCITT compression \
                 (Compression=2/3); TIFF 6.0 §14 ties the predictor to the LZW family",
            ));
        }
    }

    let (samples_per_pixel, bits_per_sample, photometric, mut raw_pixels, color_map_words) =
        match &p.kind {
            EncodePixelFormat::Bilevel { pixels } => {
                let row_bytes = (p.width as usize).div_ceil(8);
                let want = row_bytes * (p.height as usize);
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Bilevel: pixel buffer is {} bytes, expected {want} \
                         (row_bytes={row_bytes}, height={})",
                        pixels.len(),
                        p.height
                    )));
                }
                // §10 / §11: "The 'normal' PhotometricInterpretation
                // for bilevel CCITT compressed data is WhiteIsZero".
                // We follow the same default for uncompressed bilevel
                // so a Bilevel input round-trips through any
                // compression scheme without changing meaning. The
                // decoder applies the photometric inversion on the
                // way to Gray8.
                (1u16, vec![1u16], PHOTO_WHITE_IS_ZERO, pixels.to_vec(), None)
            }
            EncodePixelFormat::TransparencyMask { pixels } => {
                // TIFF 6.0 page 37 "PhotometricInterpretation = 4":
                // SamplesPerPixel and BitsPerSample must be 1; bytes
                // are packed MSB-first row-by-row exactly like Bilevel.
                // The bit polarity is fixed by spec — 1 = interior,
                // 0 = exterior — and the encoder does not apply any
                // inversion, so the input is written through verbatim.
                // The NewSubfileType bit-2 flag is set further down so
                // multi-page readers can recognise the IFD as a mask
                // for a sibling image without consulting the photometric
                // tag.
                let row_bytes = (p.width as usize).div_ceil(8);
                let want = row_bytes * (p.height as usize);
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/TransparencyMask: pixel buffer is {} bytes, expected \
                         {want} (row_bytes={row_bytes}, height={})",
                        pixels.len(),
                        p.height
                    )));
                }
                (
                    1u16,
                    vec![1u16],
                    PHOTO_TRANSPARENCY_MASK,
                    pixels.to_vec(),
                    None,
                )
            }
            EncodePixelFormat::Gray8 { pixels } => {
                let want = (p.width as usize) * (p.height as usize);
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Gray8: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (1u16, vec![8u16], PHOTO_BLACK_IS_ZERO, pixels.to_vec(), None)
            }
            EncodePixelFormat::Gray16Le { pixels } => {
                let want = (p.width as usize) * (p.height as usize) * 2;
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Gray16Le: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (
                    1u16,
                    vec![16u16],
                    PHOTO_BLACK_IS_ZERO,
                    pixels.to_vec(),
                    None,
                )
            }
            EncodePixelFormat::Rgb24 { pixels } => {
                let want = (p.width as usize) * (p.height as usize) * 3;
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Rgb24: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (3u16, vec![8u16, 8, 8], PHOTO_RGB, pixels.to_vec(), None)
            }
            EncodePixelFormat::Palette8 { indices, palette } => {
                let want = (p.width as usize) * (p.height as usize);
                if indices.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Palette8: index buffer is {} bytes, expected {want}",
                        indices.len()
                    )));
                }
                if palette.is_empty() || palette.len() > 256 {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Palette8: palette must have 1..=256 entries (got {})",
                        palette.len()
                    )));
                }
                // ColorMap stores 256 SHORTs per channel for an 8-bpp
                // palette per spec; pad missing entries with 0.
                let mut cm = vec![0u16; 256 * 3];
                for (i, c) in palette.iter().enumerate() {
                    // Replicate 8-bit channel into the high byte of
                    // the 16-bit ColorMap entry (upper bits =
                    // intensity). The canonical (v << 8) | v
                    // expansion ensures a 0xFF 8-bit value reads
                    // back as 0xFFFF in the 16-bit field.
                    cm[i] = ((c[0] as u16) << 8) | c[0] as u16;
                    cm[256 + i] = ((c[1] as u16) << 8) | c[1] as u16;
                    cm[512 + i] = ((c[2] as u16) << 8) | c[2] as u16;
                }
                (1u16, vec![8u16], PHOTO_PALETTE, indices.to_vec(), Some(cm))
            }
            EncodePixelFormat::CieLab8 { pixels } => {
                // TIFF 6.0 §23 "CIE L*a*b* Images" (page 110):
                // 3-sample chunky `(L*, a*, b*)` at 8 bits per sample.
                // The on-disk bit interpretation is fixed by the spec
                // — L* is unsigned 0..255 mapping to the 0..100
                // perceptual lightness scale, and a*, b* are
                // two's-complement signed bytes — so the encoder
                // writes the caller-supplied bytes through verbatim.
                // BitsPerSample is a 3-entry [8, 8, 8] SHORT array,
                // identical to Rgb24, so the inline-vs-out-of-line
                // spill machinery already in `plan_page_full` handles
                // it for both classic (spill out-of-line, 6 > 4) and
                // BigTIFF (stay inline, 6 <= 8).
                let want = (p.width as usize) * (p.height as usize) * 3;
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/CieLab8: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (3u16, vec![8u16, 8, 8], PHOTO_CIELAB, pixels.to_vec(), None)
            }
            EncodePixelFormat::CieLabL8 { pixels } => {
                // TIFF 6.0 §23 page 110 "Usage of other Fields":
                // "SamplesPerPixel - ExtraSamples: 3 for L*a*b*, 1
                // implies L* only, for monochrome data". One byte per
                // pixel, BitsPerSample = [8]. Bytes are L* on the
                // 0..255 -> 0..100 perceptual lightness scale.
                let want = (p.width as usize) * (p.height as usize);
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/CieLabL8: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (1u16, vec![8u16], PHOTO_CIELAB, pixels.to_vec(), None)
            }
            EncodePixelFormat::Cmyk32 { pixels } => {
                // TIFF 6.0 §16 "CMYK Images" (page 69) — 4-sample chunky
                // (C, M, Y, K) at 8 bits per sample. The on-disk bit
                // interpretation is fixed by the spec (§16 Requirements:
                // "SamplesPerPixel = N. SHORT. The number of inks. … For
                // CMYK, … N = 4 … BitsPerSample = 8,8,8,8 … the
                // larger component values represent a higher percentage
                // of ink dot coverage and smaller values represent less
                // coverage"), so the encoder writes the caller-supplied
                // bytes through verbatim. BitsPerSample is a 4-entry
                // [8, 8, 8, 8] SHORT array (8 bytes) — it spills
                // out-of-line on classic TIFF (8 > 4 inline threshold)
                // and stays inline on BigTIFF (8 <= 8), exactly as the
                // existing `bps_inline_bytes <= inline_threshold` switch
                // already handles.
                let want = (p.width as usize) * (p.height as usize) * 4;
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/Cmyk32: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (4u16, vec![8u16, 8, 8, 8], PHOTO_CMYK, pixels.to_vec(), None)
            }
            EncodePixelFormat::YCbCr24 { pixels } => {
                // TIFF 6.0 §21 "YCbCr Images" (page 89) — 3-sample
                // chunky `(Y, Cb, Cr)` at 8 bits per sample under the
                // chunky-444 layout (`YCbCrSubSampling = [1, 1]`,
                // `PlanarConfiguration = 1`). At the 1:1 chroma
                // sampling factor the §21 "Ordering of Component
                // Samples" data-unit (ChromaSubsampleVert rows of
                // ChromaSubsampleHoriz Y samples, then one Cb and one
                // Cr) collapses to a single Y followed by Cb and Cr
                // per pixel — i.e. the caller-supplied bytes are the
                // on-disk byte order. BitsPerSample is a 3-entry
                // [8, 8, 8] SHORT array (6 bytes) — it spills
                // out-of-line on classic TIFF (6 > 4) and stays inline
                // on BigTIFF (6 <= 8), same as the Rgb24 / CieLab8
                // paths. The §21-required `YCbCrSubSampling = [1, 1]`
                // (tag 530), `YCbCrPositioning = 1` (tag 531,
                // "centered"), and `ReferenceBlackWhite = [0/1, 255/1,
                // 128/1, 255/1, 128/1, 255/1]` (tag 532, the §20
                // page 87 "no headroom/footroom" full-range value)
                // are emitted further down in the IFD-build pass.
                let want = (p.width as usize) * (p.height as usize) * 3;
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/YCbCr24: pixel buffer is {} bytes, expected {want}",
                        pixels.len()
                    )));
                }
                (3u16, vec![8u16, 8, 8], PHOTO_YCBCR, pixels.to_vec(), None)
            }
            EncodePixelFormat::YCbCrSubsampled24 {
                pixels,
                subsampling,
            } => {
                let (sh, sv) = *subsampling;
                // §21 page 90: each subsampling factor is 1, 2, or 4 and
                // YCbCrSubsampleVert <= YCbCrSubsampleHoriz. The decoder
                // reverses exactly this legal set.
                if !matches!((sh, sv), (1, 1) | (2, 1) | (2, 2) | (4, 1) | (4, 2)) {
                    return Err(Error::invalid(format!(
                        "TIFF encode/YCbCrSubsampled24: YCbCrSubSampling=({sh},{sv}) is not a \
                         TIFF 6.0 §21 legal pair — each factor must be 1/2/4 and \
                         YCbCrSubsampleVert <= YCbCrSubsampleHoriz; supported pairs are \
                         (1,1), (2,1), (2,2), (4,1), (4,2)"
                    )));
                }
                // §21 page 90: "ImageWidth and ImageLength are
                // constrained to be integer multiples of
                // YCbCrSubsampleHoriz and YCbCrSubsampleVert
                // respectively."
                if p.width % (sh as u32) != 0 || p.height % (sv as u32) != 0 {
                    return Err(Error::invalid(format!(
                        "TIFF encode/YCbCrSubsampled24: ImageWidth ({}) must be a multiple of \
                         ChromaSubsampleHoriz ({sh}) and ImageLength ({}) a multiple of \
                         ChromaSubsampleVert ({sv}) — TIFF 6.0 §21 page 90",
                        p.width, p.height
                    )));
                }
                let want = (p.width as usize) * (p.height as usize) * 3;
                if pixels.len() != want {
                    return Err(Error::invalid(format!(
                        "TIFF encode/YCbCrSubsampled24: pixel buffer is {} bytes, expected {want} \
                         (full-resolution width * height * 3)",
                        pixels.len()
                    )));
                }
                // Strip layout packs the whole-image §21 data-unit stream
                // up front. Tiled layout instead packs per-tile data units
                // (`build_tiles_ycbcr_subsampled` below) directly from the
                // full-resolution pixels, so leave them unpacked here.
                let buf = if p.tiling.is_some() {
                    pixels.to_vec()
                } else {
                    pack_ycbcr_data_units(
                        pixels,
                        p.width as usize,
                        p.height as usize,
                        sh as usize,
                        sv as usize,
                    )
                };
                (3u16, vec![8u16, 8, 8], PHOTO_YCBCR, buf, None)
            }
        };

    // Build the page's compressed image segments. Chunky pages are a
    // single strip over the interleaved data; PlanarConfiguration=2
    // pages emit one strip per component plane (full image height), in
    // plane order (component 0 first), matching the decoder's planar
    // walker; tiled pages emit one segment per tile (row-major, TIFF 6.0
    // §15), each independently compressed.
    // Non-1:1 chroma-subsampled YCbCr tiles pack §21 data units, not a
    // per-pixel chunky interleave, so they take a dedicated tile packer.
    let ycbcr_subsampling_nontrivial: Option<(usize, usize)> = match &p.kind {
        EncodePixelFormat::YCbCrSubsampled24 { subsampling, .. } if *subsampling != (1, 1) => {
            Some((subsampling.0 as usize, subsampling.1 as usize))
        }
        _ => None,
    };

    let bps = bits_per_sample[0] as usize;
    let strips: Vec<Vec<u8>> = if let Some((tile_w, tile_h)) = p.tiling {
        if let Some((sh, sv)) = ycbcr_subsampling_nontrivial {
            // Tiled chroma-subsampled YCbCr (TIFF 6.0 §15 + §21). Each
            // tile carries a whole tile_w/sh × tile_h/sv grid of §21 data
            // units packed from the full-resolution pixels (`raw_pixels`
            // holds the unpacked pixels for this case — see the
            // YCbCrSubsampled24 match arm). The decoder reverses this in
            // `decode_tiles_ycbcr_subsampled`.
            build_tiles_ycbcr_subsampled(
                &raw_pixels,
                p.width as usize,
                p.height as usize,
                tile_w as usize,
                tile_h as usize,
                sh,
                sv,
                p.compression,
            )?
        } else if p.planar {
            // Tiled PlanarConfiguration=2 (TIFF 6.0 §15 + §"Planar-
            // Configuration"): one row-major tile grid per component
            // plane, emitted plane-0 first then plane-1, etc. — exactly
            // the order §15 TileOffsets prescribes ("the offsets for the
            // first component plane are stored first, followed by all the
            // offsets for the second component plane, and so on"). Only
            // Rgb24 (SPP=3) reaches here.
            build_tiles_planar(
                &raw_pixels,
                p.width as usize,
                p.height as usize,
                tile_w as usize,
                tile_h as usize,
                samples_per_pixel as usize,
                bps,
                p.predictor,
                p.compression,
            )?
        } else if bps == 1 {
            // Tiled 1-bit bilevel layout (TIFF 6.0 §15). The bilevel /
            // mask raster is MSB-first packed at ceil(width/8) bytes per
            // row; `build_tiles_bilevel` slices it into byte-padded tile
            // rows with §15 edge replication. The predictor is already
            // rejected for 1-bit input above, so this never differences.
            build_tiles_bilevel(
                &raw_pixels,
                p.width as usize,
                p.height as usize,
                tile_w as usize,
                tile_h as usize,
                p.compression,
            )?
        } else {
            // Tiled chunky layout (TIFF 6.0 §15). Split the interleaved
            // chunky raster into a row-major grid of tile_w x tile_h
            // tiles, padding boundary tiles by replicating the last
            // visible column / row (§15 "Padding": "Some compression
            // schemes work best if the padding is accomplished by
            // replicating the last column and last row"). Each tile is
            // differenced (when the predictor is on) and compressed
            // independently — §15: "Tiles are compressed individually,
            // just as strips are compressed."
            build_tiles(
                &raw_pixels,
                p.width as usize,
                p.height as usize,
                tile_w as usize,
                tile_h as usize,
                samples_per_pixel as usize,
                bps,
                p.predictor,
                p.compression,
            )?
        }
    } else if p.planar {
        // De-interleave chunky RGBRGB… into separate R / G / B planes
        // (TIFF 6.0 §"PlanarConfiguration": "Red components in one
        // component plane, the Green in another, and the Blue in
        // another"). Only Rgb24 (SPP=3, 8 bits) reaches here.
        let spp = samples_per_pixel as usize;
        let bytes_per_sample = bps / 8;
        let pixels = (p.width as usize) * (p.height as usize);
        let plane_len = pixels * bytes_per_sample;
        let mut out_strips = Vec::with_capacity(spp);
        for plane in 0..spp {
            let mut plane_buf = vec![0u8; plane_len];
            for px in 0..pixels {
                let src = (px * spp + plane) * bytes_per_sample;
                let dst = px * bytes_per_sample;
                plane_buf[dst..dst + bytes_per_sample]
                    .copy_from_slice(&raw_pixels[src..src + bytes_per_sample]);
            }
            // §14: "If PlanarConfiguration is 2 … Differencing works the
            // same as it does for grayscale data." Each plane is a
            // single-component image, so the predictor runs with an
            // offset of one sample.
            if p.predictor {
                let plane_row_bytes = (p.width as usize) * bytes_per_sample;
                forward_horizontal_predictor(
                    &mut plane_buf,
                    p.width as usize,
                    p.height as usize,
                    1,
                    bps,
                    plane_row_bytes,
                )?;
            }
            out_strips.push(p.compression.pack(&plane_buf, p.width, p.height)?);
        }
        out_strips
    } else {
        // Apply the §14 horizontal-differencing predictor *before*
        // compression. The encoder stores first differences; the
        // decoder's cumulative left-to-right add reverses it exactly.
        // Chunky single-strip layout, so `row_bytes` is the packed
        // sample stride and the whole image is one differencing region.
        if p.predictor {
            let row_bytes = (p.width as usize) * (samples_per_pixel as usize) * (bps / 8);
            forward_horizontal_predictor(
                &mut raw_pixels,
                p.width as usize,
                p.height as usize,
                samples_per_pixel as usize,
                bps,
                row_bytes,
            )?;
        }
        vec![p.compression.pack(&raw_pixels, p.width, p.height)?]
    };
    let planar_config = if p.planar {
        PLANAR_SEPARATE
    } else {
        PLANAR_CHUNKY
    };
    let strip_count: u64 = strips.len() as u64;

    // Build the IFD entry list. Tags must appear in ascending order
    // per spec.
    let mut entries: Vec<PageIfdEntry> = Vec::new();
    let mut externals: Vec<(BlobId, Vec<u8>)> = Vec::new();

    // 254 NewSubfileType — TIFF 6.0 page 36, 32-bit bit-flag field.
    // Bit 0: reduced-resolution version of another image. Bit 1:
    // single page of a multi-page image. Bit 2: defines a
    // transparency mask for another image in this TIFF file (the
    // spec then requires PhotometricInterpretation = 4). Defaults
    // to 0 (full-resolution single image). The encoder sets bit 2
    // when the caller asked for a TransparencyMask page so that a
    // multi-page reader can spot the mask IFD without consulting
    // PhotometricInterpretation. Other bits stay clear; we never
    // emit reduced-resolution or generic multi-page-numbering hints.
    let new_subfile_type: u32 = if matches!(p.kind, EncodePixelFormat::TransparencyMask { .. }) {
        1 << 2
    } else {
        0
    };
    entries.push(PageIfdEntry {
        tag: TAG_NEW_SUBFILE_TYPE,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::Inline(new_subfile_type.to_le_bytes().to_vec()),
    });
    // 256 ImageWidth (LONG)
    entries.push(PageIfdEntry {
        tag: TAG_IMAGE_WIDTH,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::Inline(p.width.to_le_bytes().to_vec()),
    });
    // 257 ImageLength (LONG)
    entries.push(PageIfdEntry {
        tag: TAG_IMAGE_LENGTH,
        field_type: TYPE_LONG,
        count: 1,
        value: IfdValue::Inline(p.height.to_le_bytes().to_vec()),
    });
    // 258 BitsPerSample (SHORT × samples_per_pixel). BigTIFF widens the
    // inline value/offset slot from 4 to 8 bytes, so the Rgb24 3-entry
    // SHORT array (6 bytes) now fits inline; classic TIFF still has
    // to spill it out-of-line.
    let bps_inline_bytes: Vec<u8> = bits_per_sample
        .iter()
        .flat_map(|b| b.to_le_bytes())
        .collect();
    let inline_threshold: usize = if bigtiff { 8 } else { 4 };
    if bps_inline_bytes.len() <= inline_threshold {
        entries.push(PageIfdEntry {
            tag: TAG_BITS_PER_SAMPLE,
            field_type: TYPE_SHORT,
            count: bits_per_sample.len() as u64,
            value: IfdValue::Inline(bps_inline_bytes),
        });
    } else {
        externals.push((BlobId::BitsPerSample, bps_inline_bytes));
        entries.push(PageIfdEntry {
            tag: TAG_BITS_PER_SAMPLE,
            field_type: TYPE_SHORT,
            count: bits_per_sample.len() as u64,
            value: IfdValue::ExternalBlob(BlobId::BitsPerSample),
        });
    }
    // 259 Compression (SHORT)
    entries.push(PageIfdEntry {
        tag: TAG_COMPRESSION,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(p.compression.tag_value().to_le_bytes().to_vec()),
    });
    // 262 PhotometricInterpretation (SHORT)
    entries.push(PageIfdEntry {
        tag: TAG_PHOTOMETRIC_INTERPRETATION,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(photometric.to_le_bytes().to_vec()),
    });
    // 273 StripOffsets. Omitted entirely for tiled pages — §15: "When
    // the tiling fields … are used, they replace the StripOffsets,
    // StripByteCounts, and RowsPerStrip fields … Do not use both
    // strip-oriented and tile-oriented fields in the same TIFF file."
    // count=1 for chunky (one strip), or SamplesPerPixel for
    // PlanarConfiguration=2 (one strip per plane). Classic TIFF stores
    // offsets as LONG (4 bytes); BigTIFF as LONG8 (8 bytes) so a single
    // 4 GiB+ strip can still be addressed inline in the value slot.
    let offset_field_type = if bigtiff { TYPE_LONG8 } else { TYPE_LONG };
    if p.tiling.is_none() {
        entries.push(PageIfdEntry {
            tag: TAG_STRIP_OFFSETS,
            field_type: offset_field_type,
            count: strip_count,
            value: IfdValue::StripOffsets,
        });
    }
    // 277 SamplesPerPixel (SHORT)
    entries.push(PageIfdEntry {
        tag: TAG_SAMPLES_PER_PIXEL,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(samples_per_pixel.to_le_bytes().to_vec()),
    });
    // 278 RowsPerStrip (LONG). Omitted for tiled pages (§15: TileLength
    // "Replaces RowsPerStrip in tiled TIFF files").
    if p.tiling.is_none() {
        entries.push(PageIfdEntry {
            tag: TAG_ROWS_PER_STRIP,
            field_type: TYPE_LONG,
            count: 1,
            value: IfdValue::Inline(p.height.to_le_bytes().to_vec()),
        });
    }
    // 279 StripByteCounts. count matches StripOffsets. Omitted for
    // tiled pages (replaced by TileByteCounts). LONG/LONG8 picks the
    // variant-appropriate offset width (above).
    if p.tiling.is_none() {
        entries.push(PageIfdEntry {
            tag: TAG_STRIP_BYTE_COUNTS,
            field_type: offset_field_type,
            count: strip_count,
            value: IfdValue::StripByteCounts,
        });
    }
    // 284 PlanarConfiguration (SHORT) — 1 (chunky) or 2 (separate
    // planes) per the page's `planar` flag.
    entries.push(PageIfdEntry {
        tag: TAG_PLANAR_CONFIGURATION,
        field_type: TYPE_SHORT,
        count: 1,
        value: IfdValue::Inline(planar_config.to_le_bytes().to_vec()),
    });
    // 292 T4Options (LONG) — only for Compression=3. Bit 0 (2D
    // coding, T4OPT_2D_CODING) is set for the T.4 2-D variant per
    // TIFF 6.0 §11; bit 1 (uncompressed mode) is always clear
    // (uncompressed mode is unsupported on both sides of the codec);
    // bit 2 (EOL byte-aligned) is set per the variant flag.
    match p.compression {
        TiffCompression::CcittT4OneD { eol_byte_aligned } => {
            let mut flags: u32 = 0;
            if eol_byte_aligned {
                flags |= T4OPT_EOL_BYTE_ALIGNED;
            }
            entries.push(PageIfdEntry {
                tag: TAG_T4_OPTIONS,
                field_type: TYPE_LONG,
                count: 1,
                value: IfdValue::Inline(flags.to_le_bytes().to_vec()),
            });
        }
        TiffCompression::CcittT4TwoD { eol_byte_aligned } => {
            let mut flags: u32 = T4OPT_2D_CODING;
            if eol_byte_aligned {
                flags |= T4OPT_EOL_BYTE_ALIGNED;
            }
            entries.push(PageIfdEntry {
                tag: TAG_T4_OPTIONS,
                field_type: TYPE_LONG,
                count: 1,
                value: IfdValue::Inline(flags.to_le_bytes().to_vec()),
            });
        }
        TiffCompression::CcittT6 => {
            // 293 T6Options (LONG). Per §11, bit 0 is reserved and
            // bit 1 ("uncompressed mode allowed") is the only
            // defined option flag; we never emit T.6 uncompressed
            // extensions so the field is all zeros.
            entries.push(PageIfdEntry {
                tag: TAG_T6_OPTIONS,
                field_type: TYPE_LONG,
                count: 1,
                value: IfdValue::Inline(0u32.to_le_bytes().to_vec()),
            });
        }
        _ => {}
    }
    // 317 Predictor (SHORT) — only when horizontal differencing is on.
    // Default (Predictor=1, no prediction) is omitted; the decoder
    // treats an absent tag as 1.
    if p.predictor {
        entries.push(PageIfdEntry {
            tag: TAG_PREDICTOR,
            field_type: TYPE_SHORT,
            count: 1,
            value: IfdValue::Inline(PREDICTOR_HORIZONTAL.to_le_bytes().to_vec()),
        });
    }
    // 320 ColorMap (SHORT, 3*2^bps) — palette only.
    if let Some(cm) = color_map_words {
        let bytes: Vec<u8> = cm.iter().flat_map(|w| w.to_le_bytes()).collect();
        let count = cm.len() as u64;
        externals.push((BlobId::ColorMapWords, bytes));
        entries.push(PageIfdEntry {
            tag: TAG_COLOR_MAP,
            field_type: TYPE_SHORT,
            count,
            value: IfdValue::ExternalBlob(BlobId::ColorMapWords),
        });
    }
    // 322/323/324/325 Tile fields (TIFF 6.0 §15) — only for tiled pages.
    // These come after ColorMap (320) in ascending tag order. TileWidth
    // / TileLength carry the grid geometry; TileOffsets / TileByteCounts
    // index the per-tile payloads in `strips` (row-major), reusing the
    // same out-of-line LONG-array machinery the strip arrays use when
    // count > 1.
    if let Some((tile_w, tile_h)) = p.tiling {
        // 322 TileWidth (LONG)
        entries.push(PageIfdEntry {
            tag: TAG_TILE_WIDTH,
            field_type: TYPE_LONG,
            count: 1,
            value: IfdValue::Inline(tile_w.to_le_bytes().to_vec()),
        });
        // 323 TileLength (LONG)
        entries.push(PageIfdEntry {
            tag: TAG_TILE_LENGTH,
            field_type: TYPE_LONG,
            count: 1,
            value: IfdValue::Inline(tile_h.to_le_bytes().to_vec()),
        });
        // 324 TileOffsets (LONG, or LONG8 in BigTIFF; N = TilesPerImage
        // for chunky, SamplesPerPixel × TilesPerImage for planar).
        entries.push(PageIfdEntry {
            tag: TAG_TILE_OFFSETS,
            field_type: offset_field_type,
            count: strip_count,
            value: IfdValue::StripOffsets,
        });
        // 325 TileByteCounts (LONG, or LONG8 in BigTIFF).
        entries.push(PageIfdEntry {
            tag: TAG_TILE_BYTE_COUNTS,
            field_type: offset_field_type,
            count: strip_count,
            value: IfdValue::StripByteCounts,
        });
    }

    // 332 InkSet (SHORT) and 334 NumberOfInks (SHORT) — TIFF 6.0 §16
    // pages 70 / 70. Both are optional in §16 (defaults: `InkSet = 1`,
    // CMYK; `NumberOfInks = 4`), but the encoder emits them on every
    // CMYK page so a reader keying on `InkSet` does not need to fall
    // back on the default. We write `InkSet = 1` (canonical CMYK
    // ordering: cyan, magenta, yellow, black) and `NumberOfInks = 4`
    // to match the four `BitsPerSample` entries. The `InkNames` field
    // (tag 333, ASCII per-ink names) only exists when `InkSet = 2`
    // ("not CMYK") per §16 InkSet ("The InkNames field should not
    // exist when InkSet=1"), so it is never emitted here. Tags 332 +
    // 334 sit between 325 (TileByteCounts) and 338 (ExtraSamples, the
    // next baseline tag the encoder might later add) in ascending
    // IFD-tag order.
    if matches!(p.kind, EncodePixelFormat::Cmyk32 { .. }) {
        entries.push(PageIfdEntry {
            tag: TAG_INK_SET,
            field_type: TYPE_SHORT,
            count: 1,
            value: IfdValue::Inline(INK_SET_CMYK.to_le_bytes().to_vec()),
        });
        entries.push(PageIfdEntry {
            tag: TAG_NUMBER_OF_INKS,
            field_type: TYPE_SHORT,
            count: 1,
            value: IfdValue::Inline(4u16.to_le_bytes().to_vec()),
        });
    }

    // 530 YCbCrSubSampling (SHORT × 2), 531 YCbCrPositioning (SHORT),
    // 532 ReferenceBlackWhite (RATIONAL × 6) — TIFF 6.0 §21 "YCbCr
    // Images" pages 91 / 92 + §20 "ReferenceBlackWhite" page 87.
    // YCbCrCoefficients (tag 529) is omitted because §21 says its
    // default is the CCIR Recommendation 601-1 luma weights
    // `{299/1000, 587/1000, 114/1000}` and the decoder's matrix is the
    // Q16 inverse of those same weights — writing the tag would just
    // restate the spec default. The three tags sit between 334
    // (NumberOfInks) and the §20-only TransferRange (342) / 700 (XMP)
    // / 33432 (Copyright) tags the encoder might later add.
    let ycbcr_subsampling: Option<(u16, u16)> = match &p.kind {
        EncodePixelFormat::YCbCr24 { .. } => Some((1, 1)),
        EncodePixelFormat::YCbCrSubsampled24 { subsampling, .. } => Some(*subsampling),
        _ => None,
    };
    if let Some((sh, sv)) = ycbcr_subsampling {
        // 530 YCbCrSubSampling — `[sh, sv]` (§21 page 90). Two SHORTs
        // pack into 4 bytes and so fit inline on classic TIFF.
        let mut ss = [0u8; 4];
        ss[..2].copy_from_slice(&sh.to_le_bytes());
        ss[2..4].copy_from_slice(&sv.to_le_bytes());
        entries.push(PageIfdEntry {
            tag: TAG_YCBCR_SUBSAMPLING,
            field_type: TYPE_SHORT,
            count: 2,
            value: IfdValue::Inline(ss.to_vec()),
        });
        // 531 YCbCrPositioning — 1 (centered). §21: "Field value 1
        // (centered) must be specified for compatibility with
        // industry standards such as PostScript Level 2 and
        // QuickTime." At `YCbCrSubSampling = [1, 1]` the positioning
        // choice is degenerate (no subsampling means there is no
        // Cb/Cr offset to specify), but the §21 default is 1 so
        // emit it explicitly for self-describing files.
        entries.push(PageIfdEntry {
            tag: TAG_YCBCR_POSITIONING,
            field_type: TYPE_SHORT,
            count: 1,
            value: IfdValue::Inline(1u16.to_le_bytes().to_vec()),
        });
        // 532 ReferenceBlackWhite — six RATIONALs carrying the §20
        // page 87 "no headroom/footroom" full-range YCbCr values:
        // `[0/1, 255/1, 128/1, 255/1, 128/1, 255/1]`. §21 says
        // ReferenceBlackWhite "must be used explicitly" for Class Y
        // images, and §20 page 87 lists the no-headroom variant
        // first among the "Useful ReferenceBlackWhite values for
        // YCbCr images." Six RATIONALs = 48 bytes, always
        // out-of-line. Each RATIONAL is two LONGs: numerator,
        // denominator.
        let rbw_pairs: [(u32, u32); 6] = [(0, 1), (255, 1), (128, 1), (255, 1), (128, 1), (255, 1)];
        let mut rbw_bytes: Vec<u8> = Vec::with_capacity(48);
        for (num, den) in rbw_pairs {
            rbw_bytes.extend_from_slice(&num.to_le_bytes());
            rbw_bytes.extend_from_slice(&den.to_le_bytes());
        }
        externals.push((BlobId::ReferenceBlackWhite, rbw_bytes));
        entries.push(PageIfdEntry {
            tag: TAG_REFERENCE_BLACK_WHITE,
            field_type: TYPE_RATIONAL,
            count: 6,
            value: IfdValue::ExternalBlob(BlobId::ReferenceBlackWhite),
        });
    }

    // Spec: entries must be in ascending tag order. The pushes
    // above are already sorted (254/256/257/258/259/262/273?/277/
    // 278?/279?/284/292?/317?/320?/322?/323?/324?/325?/332?/334?/
    // 530?/531?/532?), but assert defensively.
    debug_assert!(entries.windows(2).all(|w| w[0].tag <= w[1].tag));

    Ok(PlannedPage {
        strips,
        ifd: PageIfd { entries },
        externals,
    })
}

/// Decimate the chroma of a full-resolution interleaved `(Y, Cb, Cr)`
/// raster and pack it into the TIFF 6.0 §21 `PlanarConfiguration = 1`
/// data-unit byte order for the given `(sh, sv)` subsampling factors.
///
/// `src` is `width * height * 3` bytes of full-resolution interleaved
/// `(Y, Cb, Cr)`. `width` is an integer multiple of `sh` and `height`
/// of `sv` (the caller validates the §21 page 90 constraint before
/// calling). The returned buffer is
/// `(width / sh) * (height / sv) * (sh * sv + 2)` bytes — one data unit
/// per `sh × sv` luminance block.
///
/// Each data unit (§21 page 93) is laid out as `sv` rows of `sh`
/// full-resolution Y samples (row-major), then one Cb, then one Cr.
/// For the §21 page 94 worked `(4, 2)` example this emits
/// `Y00 Y01 Y02 Y03 Y10 Y11 Y12 Y13 Cb00 Cr00` for the first block.
///
/// The Cb / Cr written for a block is the rounded arithmetic mean of
/// the block's `sh × sv` full-resolution Cb / Cr samples — the
/// symmetric even-tap box filter §21 page 92 associates with the
/// centered (`YCbCrPositioning = 1`) sample positioning the encoder
/// declares. Luminance is transported unchanged.
fn pack_ycbcr_data_units(src: &[u8], width: usize, height: usize, sh: usize, sv: usize) -> Vec<u8> {
    let block_w = width / sh;
    let block_h = height / sv;
    let unit_len = sh * sv + 2;
    let mut out = vec![0u8; block_w * block_h * unit_len];
    for by in 0..block_h {
        for bx in 0..block_w {
            let unit_off = (by * block_w + bx) * unit_len;
            // sv rows of sh Y samples, row-major (§21 page 93).
            let mut cb_sum: u32 = 0;
            let mut cr_sum: u32 = 0;
            for sy in 0..sv {
                for sx in 0..sh {
                    let px = bx * sh + sx;
                    let py = by * sv + sy;
                    let s = (py * width + px) * 3;
                    out[unit_off + sy * sh + sx] = src[s];
                    cb_sum += src[s + 1] as u32;
                    cr_sum += src[s + 2] as u32;
                }
            }
            // Rounded mean of the block's chroma — box filter.
            let n = (sh * sv) as u32;
            let cb = ((cb_sum + n / 2) / n) as u8;
            let cr = ((cr_sum + n / 2) / n) as u8;
            out[unit_off + sh * sv] = cb;
            out[unit_off + sh * sv + 1] = cr;
        }
    }
    out
}

/// Pack a chroma-subsampled YCbCr (TIFF 6.0 §21) image into a row-major
/// grid of tiles, each carrying a whole `tile_w/sh × tile_h/sv` block of
/// §21 "data units" (each `sh*sv` Y samples in row-major order, then the
/// box-averaged Cb and Cr). §21 page 90 constrains `TileWidth` /
/// `TileLength` to integer multiples of the subsampling factors, so a
/// tile is a whole number of data units and no unit straddles a tile
/// boundary. Boundary tiles replicate the last visible pixel column /
/// row (§15 "Padding") by clamping the source pixel coordinate, so every
/// tile holds exactly `(tile_w/sh)*(tile_h/sv)` units before
/// compression. Returns one compressed payload per tile, row-major
/// (§15 `TileOffsets`). The §14 predictor is undefined over the packed
/// data-unit layout and is rejected upstream, so no differencing runs.
#[allow(clippy::too_many_arguments)]
fn build_tiles_ycbcr_subsampled(
    pixels: &[u8],
    width: usize,
    height: usize,
    tile_w: usize,
    tile_h: usize,
    sh: usize,
    sv: usize,
    compression: TiffCompression,
) -> Result<Vec<Vec<u8>>> {
    let unit_len = sh * sv + 2;
    let tile_du_across = tile_w / sh;
    let tile_du_down = tile_h / sv;
    let tile_unit_count = tile_du_across * tile_du_down;

    let tiles_across = width.div_ceil(tile_w);
    let tiles_down = height.div_ceil(tile_h);

    let mut out = Vec::with_capacity(tiles_across * tiles_down);
    for ty in 0..tiles_down {
        for tx in 0..tiles_across {
            let mut tile = vec![0u8; tile_unit_count * unit_len];
            for ur in 0..tile_du_down {
                for uc in 0..tile_du_across {
                    let unit_off = (ur * tile_du_across + uc) * unit_len;
                    let mut cb_sum: u32 = 0;
                    let mut cr_sum: u32 = 0;
                    for sy in 0..sv {
                        for sx in 0..sh {
                            // Full-resolution pixel for this Y sample,
                            // clamped to the image bounds (§15 edge
                            // replication for boundary tiles).
                            let px = (tx * tile_w + uc * sh + sx).min(width - 1);
                            let py = (ty * tile_h + ur * sv + sy).min(height - 1);
                            let s = (py * width + px) * 3;
                            tile[unit_off + sy * sh + sx] = pixels[s];
                            cb_sum += pixels[s + 1] as u32;
                            cr_sum += pixels[s + 2] as u32;
                        }
                    }
                    let n = (sh * sv) as u32;
                    tile[unit_off + sh * sv] = ((cb_sum + n / 2) / n) as u8;
                    tile[unit_off + sh * sv + 1] = ((cr_sum + n / 2) / n) as u8;
                }
            }
            // §15: tiles are compressed individually. CCITT is rejected
            // upstream, so the tile geometry passed here is only used by
            // the byte-aligned coders that ignore it.
            out.push(compression.pack(&tile, tile_w as u32, tile_h as u32)?);
        }
    }
    Ok(out)
}

/// Split a chunky raster into a row-major grid of `tile_w x tile_h`
/// tiles, padding boundary tiles by replicating the last visible
/// column / row, then (optionally) difference and compress each tile
/// independently. Returns one compressed payload per tile, ordered
/// left-to-right then top-to-bottom (TIFF 6.0 §15 `TileOffsets`).
///
/// §15 "Padding": "Boundary tiles are padded to the tile boundaries …
/// It doesn't matter what value is used for padding, because good TIFF
/// readers display only the pixels defined by ImageWidth and
/// ImageLength and ignore any padded pixels. Some compression schemes
/// work best if the padding is accomplished by replicating the last
/// column and last row instead of padding with 0's." We replicate the
/// edge samples so the compressed boundary tiles stay small. §15:
/// "Compression includes any padded areas of the rightmost and bottom
/// tiles so that all the tiles in an image are the same size when
/// uncompressed" — every tile is exactly `tile_w x tile_h` before
/// compression.
///
/// `pixels` is the interleaved chunky raster (`samples` components per
/// pixel, `bps` bits each — only 8 / 16 reach here). The predictor,
/// when on, is applied per-tile with an offset of `samples`, matching
/// the decoder's per-tile `apply_horizontal_predictor`.
#[allow(clippy::too_many_arguments)]
fn build_tiles(
    pixels: &[u8],
    width: usize,
    height: usize,
    tile_w: usize,
    tile_h: usize,
    samples: usize,
    bps: usize,
    predictor: bool,
    compression: TiffCompression,
) -> Result<Vec<Vec<u8>>> {
    let bytes_per_sample = bps / 8;
    let pixel_bytes = samples * bytes_per_sample;
    let image_row_bytes = width * pixel_bytes;
    let tile_row_bytes = tile_w * pixel_bytes;
    let tile_size_bytes = tile_row_bytes * tile_h;

    let tiles_across = width.div_ceil(tile_w);
    let tiles_down = height.div_ceil(tile_h);

    let mut out = Vec::with_capacity(tiles_across * tiles_down);
    for ty in 0..tiles_down {
        for tx in 0..tiles_across {
            // Extract this tile, replicating the last visible column /
            // row into the padded region (§15 "Padding").
            let mut tile = vec![0u8; tile_size_bytes];
            for r in 0..tile_h {
                // Source image row, clamped to the last visible row.
                let src_y = (ty * tile_h + r).min(height - 1);
                for c in 0..tile_w {
                    // Source image column, clamped to the last visible
                    // column.
                    let src_x = (tx * tile_w + c).min(width - 1);
                    let src_off = src_y * image_row_bytes + src_x * pixel_bytes;
                    let dst_off = r * tile_row_bytes + c * pixel_bytes;
                    tile[dst_off..dst_off + pixel_bytes]
                        .copy_from_slice(&pixels[src_off..src_off + pixel_bytes]);
                }
            }
            // §14 / §15: each tile is a self-contained image for the
            // predictor — difference per-tile with the tile's own row
            // stride so the decoder's per-tile cumulative add reverses
            // it exactly.
            if predictor {
                forward_horizontal_predictor(
                    &mut tile,
                    tile_w,
                    tile_h,
                    samples,
                    bps,
                    tile_row_bytes,
                )?;
            }
            // §15: "Tiles are compressed individually, just as strips
            // are compressed." Pass the tile geometry through (only the
            // CCITT coders read it, and tiling rejects CCITT upstream).
            out.push(compression.pack(&tile, tile_w as u32, tile_h as u32)?);
        }
    }
    Ok(out)
}

/// Tiled 1-bit bilevel layout (TIFF 6.0 §15). The input `pixels` is the
/// MSB-first packed bilevel raster — `ceil(width / 8)` bytes per image
/// row — that the `Bilevel` / `TransparencyMask` strip path also stores.
/// Each tile is sliced into a row-major grid of `tile_w x tile_h` tiles
/// (`tile_w` is a multiple of 16, validated by the caller), and within a
/// tile "each row of data … is treated as a separate 'scanline'" (§15
/// page 66), i.e. every tile row is independently packed MSB-first and
/// padded to a byte boundary — `tile_row_bytes = tile_w / 8` bytes. This
/// is the exact on-disk shape `decode_tiles` reassembles: it copies the
/// visible portion of each tile row as a whole number of bytes at a
/// byte-aligned destination offset (`tile_w` being a multiple of 16 makes
/// every tile-column boundary land on a byte boundary at
/// `BitsPerSample = 1`).
///
/// Boundary tiles are padded out to the full tile geometry by replicating
/// the last visible column / row (§15 "Padding": "replicating the last
/// column and last row"). The predictor (§14) is never applied — it is
/// undefined for 1-bit input and the caller rejects `predictor` for
/// bilevel — so each padded tile is packed and handed straight to the
/// compressor (§15: "Tiles are compressed individually, just as strips
/// are compressed"). Only `BitsPerSample = 1`, `SamplesPerPixel = 1`
/// reaches here.
fn build_tiles_bilevel(
    pixels: &[u8],
    width: usize,
    height: usize,
    tile_w: usize,
    tile_h: usize,
    compression: TiffCompression,
) -> Result<Vec<Vec<u8>>> {
    let image_row_bytes = width.div_ceil(8);
    // `tile_w` is a multiple of 16 (caller-validated), so this is exact.
    let tile_row_bytes = tile_w.div_ceil(8);
    let tile_size_bytes = tile_row_bytes * tile_h;

    let tiles_across = width.div_ceil(tile_w);
    let tiles_down = height.div_ceil(tile_h);

    // Read bit (x, y) from the MSB-first packed source raster.
    let src_bit = |x: usize, y: usize| -> u8 {
        let byte = pixels[y * image_row_bytes + (x >> 3)];
        (byte >> (7 - (x & 7))) & 1
    };

    let mut out = Vec::with_capacity(tiles_across * tiles_down);
    for ty in 0..tiles_down {
        for tx in 0..tiles_across {
            let mut tile = vec![0u8; tile_size_bytes];
            for r in 0..tile_h {
                // Source row, clamped to the last visible row (§15 edge
                // replication).
                let src_y = (ty * tile_h + r).min(height - 1);
                let row_base = r * tile_row_bytes;
                for c in 0..tile_w {
                    // Source column, clamped to the last visible column.
                    let src_x = (tx * tile_w + c).min(width - 1);
                    if src_bit(src_x, src_y) != 0 {
                        // Pack MSB-first into the byte-padded tile row.
                        tile[row_base + (c >> 3)] |= 0x80 >> (c & 7);
                    }
                }
            }
            out.push(compression.pack(&tile, tile_w as u32, tile_h as u32)?);
        }
    }
    Ok(out)
}

/// Tiled `PlanarConfiguration = 2` layout (TIFF 6.0 §15 + §"Planar-
/// Configuration", page 38). De-interleave the chunky raster into one
/// single-component plane per sample, tile each plane on the same
/// `tile_w x tile_h` grid as the chunky path, and return the compressed
/// tiles in plane order: all of plane 0's tiles (row-major,
/// left-to-right then top-to-bottom) first, then all of plane 1's, etc.
///
/// §15 TileOffsets: "For PlanarConfiguration = 2, the offsets for the
/// first component plane are stored first, followed by all the offsets
/// for the second component plane, and so on." The per-plane tile grid
/// is identical to the chunky grid (`TilesPerImage` tiles each), so the
/// returned vector has `SamplesPerPixel * TilesPerImage` entries —
/// exactly the `N` §15 specifies for TileOffsets / TileByteCounts under
/// PlanarConfiguration = 2.
///
/// Each plane is a single-component image, so boundary padding (§15
/// "Padding": replicate the last visible column / row) and the §14
/// horizontal-differencing predictor both run with `samples = 1` —
/// §14: "If PlanarConfiguration is 2 … Differencing works the same as
/// it does for grayscale data." This matches the decoder's
/// `decode_tiles_planar`, which reverses each tile with an offset of one
/// sample. Only Rgb24 (SPP=3, 8 bits) reaches here.
#[allow(clippy::too_many_arguments)]
fn build_tiles_planar(
    pixels: &[u8],
    width: usize,
    height: usize,
    tile_w: usize,
    tile_h: usize,
    samples: usize,
    bps: usize,
    predictor: bool,
    compression: TiffCompression,
) -> Result<Vec<Vec<u8>>> {
    let bytes_per_sample = bps / 8;
    let pixel_bytes = samples * bytes_per_sample;
    let plane_len = width * height * bytes_per_sample;
    let tiles_across = width.div_ceil(tile_w);
    let tiles_down = height.div_ceil(tile_h);

    let mut out = Vec::with_capacity(samples * tiles_across * tiles_down);
    for plane in 0..samples {
        // De-interleave this component into a full-resolution
        // single-channel plane (§"PlanarConfiguration": "Red components
        // in one component plane, the Green in another, and the Blue in
        // another").
        let mut plane_buf = vec![0u8; plane_len];
        let total_pixels = width * height;
        for px in 0..total_pixels {
            let src = px * pixel_bytes + plane * bytes_per_sample;
            let dst = px * bytes_per_sample;
            plane_buf[dst..dst + bytes_per_sample]
                .copy_from_slice(&pixels[src..src + bytes_per_sample]);
        }
        // Tile the single-component plane exactly like the chunky path
        // with `samples = 1`, so padding and the per-tile predictor reuse
        // the same code. The returned tiles are row-major (§15
        // TileOffsets: "ordered left-to-right and top-to-bottom").
        let plane_tiles = build_tiles(
            &plane_buf,
            width,
            height,
            tile_w,
            tile_h,
            1,
            bps,
            predictor,
            compression,
        )?;
        out.extend(plane_tiles);
    }
    Ok(out)
}

/// Forward horizontal-differencing predictor (TIFF 6.0 §14): replace
/// each component with the difference from the previous pixel of the
/// same component, in place. The inverse of the decoder's
/// `apply_horizontal_predictor`: that routine adds left-to-right, so
/// the encoder subtracts right-to-left to keep each "previous" value
/// at its *original* magnitude while the difference is taken. §14:
/// "we will do our horizontal differences with an offset of
/// SamplesPerPixel ... subtract red from red, green from green, and
/// blue from blue." Encoder output is always II (little-endian), so
/// 16-bit components are read/written little-endian.
///
/// `row_bytes` is the packed sample stride (chunky, single-strip). The
/// "ignore the overflow bits" wrap-around §14 relies on is exactly
/// two's-complement `wrapping_sub`.
fn forward_horizontal_predictor(
    buf: &mut [u8],
    width: usize,
    rows: usize,
    samples: usize,
    bps: usize,
    row_bytes: usize,
) -> Result<()> {
    if width == 0 || rows == 0 {
        return Ok(());
    }
    match bps {
        8 => {
            for r in 0..rows {
                let row = &mut buf[r * row_bytes..r * row_bytes + width * samples];
                // Right-to-left so row[x - samples] is still the
                // original sample when we difference row[x].
                for x in (samples..(width * samples)).rev() {
                    row[x] = row[x].wrapping_sub(row[x - samples]);
                }
            }
        }
        16 => {
            for r in 0..rows {
                let row = &mut buf[r * row_bytes..r * row_bytes + width * samples * 2];
                let pixels = width * samples;
                for x in (samples..pixels).rev() {
                    let cur_off = x * 2;
                    let prev_off = (x - samples) * 2;
                    let cur = u16::from_le_bytes([row[cur_off], row[cur_off + 1]]);
                    let prev = u16::from_le_bytes([row[prev_off], row[prev_off + 1]]);
                    let new = cur.wrapping_sub(prev);
                    let bytes = new.to_le_bytes();
                    row[cur_off] = bytes[0];
                    row[cur_off + 1] = bytes[1];
                }
            }
        }
        _ => {
            // The encoder only emits 8- and 16-bit components, so this
            // is unreachable from the public API; keep the precise
            // error for defensiveness / future bit depths.
            return Err(Error::invalid(format!(
                "TIFF encode: Predictor=2 at bits_per_sample={bps} unsupported"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_tiff;
    use crate::image::TiffPixelFormat;

    fn ramp_gray8(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                v.push(((x + y) & 0xFF) as u8);
            }
        }
        v
    }

    fn pattern_rgb(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h as u8 {
            for x in 0..w as u8 {
                v.push(x.wrapping_mul(7));
                v.push(y.wrapping_mul(11));
                v.push((x ^ y).wrapping_mul(13));
            }
        }
        v
    }

    #[test]
    fn encode_gray8_uncompressed_roundtrip() {
        let pixels = ramp_gray8(32, 32);
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (32, 32));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_gray16_packbits_roundtrip() {
        let mut pixels = Vec::with_capacity(16 * 16 * 2);
        for y in 0u16..16 {
            for x in 0u16..16 {
                let v = (x.wrapping_mul(257)).wrapping_add(y.wrapping_mul(513));
                pixels.extend_from_slice(&v.to_le_bytes());
            }
        }
        let page = EncodePage {
            width: 16,
            height: 16,
            kind: EncodePixelFormat::Gray16Le { pixels: &pixels },
            compression: TiffCompression::PackBits,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (16, 16));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_rgb24_lzw_roundtrip() {
        let pixels = pattern_rgb(20, 20);
        let page = EncodePage {
            width: 20,
            height: 20,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (20, 20));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_rgb24_deflate_roundtrip() {
        let pixels = pattern_rgb(48, 24);
        let page = EncodePage {
            width: 48,
            height: 24,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::Deflate,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (48, 24));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_palette_roundtrip() {
        // 4-color palette: black, red, green, white.
        let palette = vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [255, 255, 255]];
        let mut indices = Vec::with_capacity(8 * 8);
        for y in 0..8 {
            for x in 0..8 {
                indices.push(((x ^ y) & 0x3) as u8);
            }
        }
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Palette8 {
                indices: &indices,
                palette: &palette,
            },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        // Decoder expands palette → Rgb24.
        let mut want = Vec::with_capacity(8 * 8 * 3);
        for &idx in &indices {
            let p = palette[idx as usize];
            want.extend_from_slice(&p);
        }
        assert_eq!(d.frame.planes[0].data, want);
    }

    fn bilevel_checkerboard(w: u32, h: u32) -> Vec<u8> {
        // Pack an MSB-first 1-bit bilevel buffer with a 1-pixel
        // checkerboard pattern. Used to exercise the run-length coder
        // on the worst-case input (every pixel is a run boundary).
        let row_bytes = (w as usize).div_ceil(8);
        let mut out = vec![0u8; row_bytes * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let on = ((x ^ y) & 1) == 1;
                if on {
                    out[y * row_bytes + x / 8] |= 1 << (7 - (x % 8));
                }
            }
        }
        out
    }

    fn bilevel_stripes(w: u32, h: u32, period: u32) -> Vec<u8> {
        // Wider runs to exercise the make-up-code paths.
        let row_bytes = (w as usize).div_ceil(8);
        let mut out = vec![0u8; row_bytes * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let on = ((x as u32) / period) & 1 == 1;
                if on {
                    out[y * row_bytes + x / 8] |= 1 << (7 - (x % 8));
                }
            }
        }
        out
    }

    /// Inflate a packed MSB-first bilevel buffer to Gray8 the same
    /// way the decoder would, with the WhiteIsZero convention the
    /// `Bilevel` encoder emits (bit 0 = white = 0xFF in Gray8).
    fn bilevel_to_gray8(packed: &[u8], w: u32, h: u32) -> Vec<u8> {
        let row_bytes = (w as usize).div_ceil(8);
        let mut out = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            let row = &packed[y * row_bytes..(y + 1) * row_bytes];
            for x in 0..w as usize {
                let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                // WhiteIsZero photometric: bit 0 -> white -> 0xFF.
                out.push(if bit == 0 { 0xFF } else { 0x00 });
            }
        }
        out
    }

    #[test]
    fn encode_bilevel_uncompressed_roundtrip() {
        let packed = bilevel_checkerboard(24, 16);
        let page = EncodePage {
            width: 24,
            height: 16,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (24, 16));
        let want = bilevel_to_gray8(&packed, 24, 16);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_bilevel_ccitt_rle_roundtrip_checkerboard() {
        let packed = bilevel_checkerboard(16, 8);
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let want = bilevel_to_gray8(&packed, 16, 8);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_bilevel_ccitt_rle_roundtrip_stripes() {
        // Width 128 with a 16-pixel stripe period exercises the
        // make-up-code path on every run (each run is exactly 16
        // pixels = white-terminating or black-terminating directly).
        let packed = bilevel_stripes(128, 4, 16);
        let page = EncodePage {
            width: 128,
            height: 4,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let want = bilevel_to_gray8(&packed, 128, 4);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_bilevel_ccitt_t4_1d_roundtrip() {
        let packed = bilevel_stripes(96, 6, 8);
        let page = EncodePage {
            width: 96,
            height: 6,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittT4OneD {
                eol_byte_aligned: false,
            },
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let want = bilevel_to_gray8(&packed, 96, 6);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_bilevel_ccitt_t4_1d_byte_aligned_roundtrip() {
        // T4Options bit 2 must end up in the IFD and the decoder
        // must read it back to find the byte-aligned EOLs.
        let packed = bilevel_stripes(64, 8, 4);
        let page = EncodePage {
            width: 64,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittT4OneD {
                eol_byte_aligned: true,
            },
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let want = bilevel_to_gray8(&packed, 64, 8);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_ccitt_rejects_non_bilevel() {
        // Asking for CCITT compression with a Gray8 input must be a
        // clean error, not a silent mis-encode.
        let pixels = ramp_gray8(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let r = encode_tiff(&page);
        assert!(r.is_err());
    }

    #[test]
    fn encode_multi_page_chain() {
        let p1 = ramp_gray8(8, 8);
        let p2 = pattern_rgb(8, 8);
        let pages = vec![
            EncodePage {
                width: 8,
                height: 8,
                kind: EncodePixelFormat::Gray8 { pixels: &p1 },
                compression: TiffCompression::None,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            },
            EncodePage {
                width: 8,
                height: 8,
                kind: EncodePixelFormat::Rgb24 { pixels: &p2 },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            },
        ];
        let bytes = encode_tiff_multi(&pages).unwrap();
        let imgs = crate::decoder::decode_tiff_all(&bytes).unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].width, 8);
        assert_eq!(imgs[0].planes[0].data, p1);
        assert_eq!(imgs[1].width, 8);
        assert_eq!(imgs[1].planes[0].data, p2);
    }

    // ---- Predictor=2 (horizontal differencing, TIFF 6.0 §14) ----

    fn pattern_gray16(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 2) as usize);
        for y in 0..h {
            for x in 0..w {
                // Smoothly-varying so differences are small; the
                // predictor's correctness is independent of magnitude
                // (two's-complement wrap), but a ramp is the realistic
                // case §14 targets.
                let s = (x.wrapping_mul(311)).wrapping_add(y.wrapping_mul(101)) as u16;
                v.extend_from_slice(&s.to_le_bytes());
            }
        }
        v
    }

    /// Helper: encode `kind` with Predictor=2 + `comp`, decode, and
    /// assert the round-trip is bit-exact.
    fn predictor_roundtrip(
        width: u32,
        height: u32,
        kind: EncodePixelFormat<'_>,
        comp: TiffCompression,
    ) -> Vec<u8> {
        let page = EncodePage {
            width,
            height,
            kind,
            compression: comp,
            predictor: true,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (width, height));
        d.frame.planes[0].data.clone()
    }

    #[test]
    fn encode_gray8_predictor_lzw_roundtrip() {
        let pixels = ramp_gray8(40, 24);
        let out = predictor_roundtrip(
            40,
            24,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::Lzw,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray8_predictor_deflate_roundtrip() {
        let pixels = ramp_gray8(33, 17);
        let out = predictor_roundtrip(
            33,
            17,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::Deflate,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray8_predictor_none_roundtrip() {
        // §14 ties the predictor to LZW, but the tag is orthogonal to
        // the compressor; Compression=1 + Predictor=2 must still
        // round-trip (the decoder reverses the differencing regardless).
        let pixels = ramp_gray8(16, 16);
        let out = predictor_roundtrip(
            16,
            16,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::None,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray16_predictor_lzw_roundtrip() {
        let pixels = pattern_gray16(24, 20);
        let out = predictor_roundtrip(
            24,
            20,
            EncodePixelFormat::Gray16Le { pixels: &pixels },
            TiffCompression::Lzw,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray16_predictor_deflate_roundtrip() {
        let pixels = pattern_gray16(15, 9);
        let out = predictor_roundtrip(
            15,
            9,
            EncodePixelFormat::Gray16Le { pixels: &pixels },
            TiffCompression::Deflate,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_rgb24_predictor_lzw_roundtrip() {
        // §14: per-component differencing with an offset of
        // SamplesPerPixel (3). A pattern where R/G/B differ ensures a
        // plane swap or wrong offset would corrupt the round-trip.
        let pixels = pattern_rgb(28, 19);
        let out = predictor_roundtrip(
            28,
            19,
            EncodePixelFormat::Rgb24 { pixels: &pixels },
            TiffCompression::Lzw,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_rgb24_predictor_deflate_roundtrip() {
        let pixels = pattern_rgb(11, 13);
        let out = predictor_roundtrip(
            11,
            13,
            EncodePixelFormat::Rgb24 { pixels: &pixels },
            TiffCompression::Deflate,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_rgb24_predictor_packbits_roundtrip() {
        let pixels = pattern_rgb(9, 7);
        let out = predictor_roundtrip(
            9,
            7,
            EncodePixelFormat::Rgb24 { pixels: &pixels },
            TiffCompression::PackBits,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_palette_predictor_roundtrip() {
        // Palette indices are single-component 8-bit values; §14
        // differencing applies as for grayscale. The decoder expands
        // the (reversed) indices through the colormap to Rgb24.
        let palette = vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [255, 255, 255]];
        let mut indices = Vec::with_capacity(12 * 8);
        for y in 0..8u32 {
            for x in 0..12u32 {
                indices.push(((x + y) & 0x3) as u8);
            }
        }
        let page = EncodePage {
            width: 12,
            height: 8,
            kind: EncodePixelFormat::Palette8 {
                indices: &indices,
                palette: &palette,
            },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let mut want = Vec::with_capacity(12 * 8 * 3);
        for &idx in &indices {
            want.extend_from_slice(&palette[idx as usize]);
        }
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_predictor_emits_tag_317() {
        // The encoded file must carry Predictor=2 (tag 317, SHORT) so a
        // third-party reader reverses the differencing. Walk the
        // single IFD looking for the 12-byte entry whose tag is 317.
        let pixels = ramp_gray8(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let b = encode_tiff(&page).unwrap();
        let ifd_off = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let count = u16::from_le_bytes([b[ifd_off], b[ifd_off + 1]]) as usize;
        let mut found = None;
        for k in 0..count {
            let e = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([b[e], b[e + 1]]);
            if tag == TAG_PREDICTOR {
                let ty = u16::from_le_bytes([b[e + 2], b[e + 3]]);
                let val = u16::from_le_bytes([b[e + 8], b[e + 9]]);
                found = Some((ty, val));
            }
        }
        assert_eq!(found, Some((TYPE_SHORT, PREDICTOR_HORIZONTAL)));

        // No-predictor encode must omit the tag entirely (decoder
        // defaults to Predictor=1).
        let page2 = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let b2 = encode_tiff(&page2).unwrap();
        let ifd2 = u32::from_le_bytes([b2[4], b2[5], b2[6], b2[7]]) as usize;
        let count2 = u16::from_le_bytes([b2[ifd2], b2[ifd2 + 1]]) as usize;
        for k in 0..count2 {
            let e = ifd2 + 2 + k * 12;
            let tag = u16::from_le_bytes([b2[e], b2[e + 1]]);
            assert_ne!(tag, TAG_PREDICTOR);
        }
    }

    #[test]
    fn encode_predictor_rejects_bilevel() {
        let packed = bilevel_checkerboard(16, 8);
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::Lzw,
            predictor: true,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        assert!(encode_tiff(&page).is_err());
    }

    #[test]
    fn encode_predictor_rejects_ccitt() {
        let packed = bilevel_checkerboard(16, 8);
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittRle,
            predictor: true,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        assert!(encode_tiff(&page).is_err());
    }

    #[test]
    fn forward_predictor_inverts_decoder_add_gray8() {
        // Direct unit check that forward differencing is the exact
        // inverse of the decoder's cumulative add for a known row.
        let mut row = vec![10u8, 12, 9, 9, 200, 201];
        let orig = row.clone();
        forward_horizontal_predictor(&mut row, 6, 1, 1, 8, 6).unwrap();
        // First sample unchanged; rest are first differences.
        assert_eq!(row[0], 10);
        assert_eq!(row[1], 12u8.wrapping_sub(10));
        assert_eq!(row[5], 201u8.wrapping_sub(200));
        // Reverse via the decoder's algorithm (left-to-right add).
        for x in 1..6 {
            row[x] = row[x].wrapping_add(row[x - 1]);
        }
        assert_eq!(row, orig);
    }

    // ---- PlanarConfiguration = 2 (separate planes) encode ----

    fn planar_roundtrip(w: u32, h: u32, compression: TiffCompression, predictor: bool) {
        let pixels = pattern_rgb(w, h);
        let page = EncodePage {
            width: w,
            height: h,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression,
            predictor,
            planar: true,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (w, h));
        // Decoder re-interleaves the planes into chunky order, so the
        // output must match the original chunky RGB input bit-exactly.
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_rgb24_planar_none_roundtrip() {
        planar_roundtrip(20, 16, TiffCompression::None, false);
    }

    #[test]
    fn encode_rgb24_planar_packbits_roundtrip() {
        planar_roundtrip(33, 9, TiffCompression::PackBits, false);
    }

    #[test]
    fn encode_rgb24_planar_lzw_roundtrip() {
        planar_roundtrip(48, 24, TiffCompression::Lzw, false);
    }

    #[test]
    fn encode_rgb24_planar_deflate_roundtrip() {
        planar_roundtrip(17, 31, TiffCompression::Deflate, false);
    }

    #[test]
    fn encode_rgb24_planar_predictor_lzw_roundtrip() {
        // §14 + PlanarConfiguration=2: each plane is differenced
        // independently with an offset of one sample.
        planar_roundtrip(40, 20, TiffCompression::Lzw, true);
    }

    #[test]
    fn encode_rgb24_planar_predictor_deflate_roundtrip() {
        planar_roundtrip(28, 28, TiffCompression::Deflate, true);
    }

    /// Inspect the encoded IFD: PlanarConfiguration must read 2, and
    /// StripOffsets / StripByteCounts must each carry SamplesPerPixel
    /// (= 3) entries — the spec's "SamplesPerPixel rows and
    /// StripsPerImage columns" array with StripsPerImage = 1.
    #[test]
    fn encode_planar_emits_three_strips_and_config_2() {
        let pixels = pattern_rgb(16, 8);
        let page = EncodePage {
            width: 16,
            height: 8,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: true,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();

        // Walk the IFD by hand (II classic TIFF).
        assert_eq!(&bytes[0..2], b"II");
        let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
        let mut planar_cfg = None;
        let mut strip_offsets_count = None;
        let mut strip_byte_counts_count = None;
        for k in 0..count {
            let e = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
            let cnt = u32::from_le_bytes([bytes[e + 4], bytes[e + 5], bytes[e + 6], bytes[e + 7]]);
            match tag {
                TAG_PLANAR_CONFIGURATION => {
                    planar_cfg = Some(u16::from_le_bytes([bytes[e + 8], bytes[e + 9]]));
                }
                TAG_STRIP_OFFSETS => strip_offsets_count = Some(cnt),
                TAG_STRIP_BYTE_COUNTS => strip_byte_counts_count = Some(cnt),
                _ => {}
            }
        }
        assert_eq!(planar_cfg, Some(PLANAR_SEPARATE));
        assert_eq!(strip_offsets_count, Some(3));
        assert_eq!(strip_byte_counts_count, Some(3));
    }

    /// `planar = true` requires a multi-sample format; the single-sample
    /// formats (where the spec says PlanarConfiguration is irrelevant)
    /// must be rejected with a precise error rather than silently
    /// emitting a meaningless `PlanarConfiguration = 2`.
    #[test]
    fn encode_planar_rejects_single_sample_formats() {
        let g = ramp_gray8(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Gray8 { pixels: &g },
            compression: TiffCompression::None,
            predictor: false,
            planar: true,
            tiling: None,
            bigtiff: false,
        };
        assert!(encode_tiff(&page).is_err());

        let palette = vec![[0u8, 0, 0], [255, 255, 255]];
        let indices = vec![0u8; 64];
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Palette8 {
                indices: &indices,
                palette: &palette,
            },
            compression: TiffCompression::None,
            predictor: false,
            planar: true,
            tiling: None,
            bigtiff: false,
        };
        assert!(encode_tiff(&page).is_err());
    }

    /// Chunky output stays single-strip (PlanarConfiguration = 1) when
    /// `planar` is off — the planar refactor must not regress the
    /// default layout.
    #[test]
    fn encode_chunky_still_single_strip_config_1() {
        let pixels = pattern_rgb(12, 6);
        let page = EncodePage {
            width: 12,
            height: 6,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
        for k in 0..count {
            let e = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
            let cnt = u32::from_le_bytes([bytes[e + 4], bytes[e + 5], bytes[e + 6], bytes[e + 7]]);
            if tag == TAG_PLANAR_CONFIGURATION {
                assert_eq!(
                    u16::from_le_bytes([bytes[e + 8], bytes[e + 9]]),
                    PLANAR_CHUNKY
                );
            }
            if tag == TAG_STRIP_OFFSETS || tag == TAG_STRIP_BYTE_COUNTS {
                assert_eq!(cnt, 1);
            }
        }
    }

    // ---- Tiled layout (TIFF 6.0 §15) ----

    /// Encode `kind` with `tiling` + `comp` (+ optional predictor),
    /// decode through our own tile-reading path, return the decoded
    /// first plane.
    fn tile_roundtrip(
        width: u32,
        height: u32,
        kind: EncodePixelFormat<'_>,
        comp: TiffCompression,
        tiling: (u32, u32),
        predictor: bool,
    ) -> Vec<u8> {
        let page = EncodePage {
            width,
            height,
            kind,
            compression: comp,
            predictor,
            planar: false,
            tiling: Some(tiling),
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (width, height));
        d.frame.planes[0].data.clone()
    }

    #[test]
    fn encode_gray8_tiled_single_tile_roundtrip() {
        // Image exactly one tile (16x16) — single-tile grid keeps the
        // TileOffsets / TileByteCounts arrays inline (count = 1).
        let pixels = ramp_gray8(16, 16);
        let out = tile_roundtrip(
            16,
            16,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::None,
            (16, 16),
            false,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray8_tiled_grid_roundtrip() {
        // 48x32 image with 16x16 tiles => 3x2 = 6 tiles, exact fit.
        let pixels = ramp_gray8(48, 32);
        for comp in [
            TiffCompression::None,
            TiffCompression::PackBits,
            TiffCompression::Lzw,
            TiffCompression::Deflate,
        ] {
            let out = tile_roundtrip(
                48,
                32,
                EncodePixelFormat::Gray8 { pixels: &pixels },
                comp,
                (16, 16),
                false,
            );
            assert_eq!(out, pixels, "compression {comp:?}");
        }
    }

    #[test]
    fn encode_gray8_tiled_edge_padding_roundtrip() {
        // 40x20 image with 16x16 tiles => 3x2 grid, right column and
        // bottom row are partial (40 = 2*16 + 8, 20 = 16 + 4). The
        // padded boundary samples must be ignored on decode, so the
        // visible region round-trips exactly.
        let pixels = ramp_gray8(40, 20);
        let out = tile_roundtrip(
            40,
            20,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::Lzw,
            (16, 16),
            false,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray16_tiled_roundtrip() {
        let pixels = pattern_gray16(48, 32);
        let out = tile_roundtrip(
            48,
            32,
            EncodePixelFormat::Gray16Le { pixels: &pixels },
            TiffCompression::Deflate,
            (16, 16),
            false,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_rgb24_tiled_roundtrip() {
        // Non-square tile (32 wide, 16 tall) with edge padding on both
        // axes: 50x30 => 2x2 grid with partial right/bottom tiles.
        let pixels = pattern_rgb(50, 30);
        for comp in [
            TiffCompression::None,
            TiffCompression::PackBits,
            TiffCompression::Lzw,
            TiffCompression::Deflate,
        ] {
            let out = tile_roundtrip(
                50,
                30,
                EncodePixelFormat::Rgb24 { pixels: &pixels },
                comp,
                (32, 16),
                false,
            );
            assert_eq!(out, pixels, "compression {comp:?}");
        }
    }

    #[test]
    fn encode_rgb24_tiled_predictor_roundtrip() {
        // §14 predictor applied per-tile must reverse exactly through
        // the decoder's per-tile cumulative add, including across the
        // padded boundary tiles.
        let pixels = pattern_rgb(48, 32);
        let out = tile_roundtrip(
            48,
            32,
            EncodePixelFormat::Rgb24 { pixels: &pixels },
            TiffCompression::Lzw,
            (16, 16),
            true,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_gray8_tiled_predictor_edge_roundtrip() {
        // Predictor + edge padding on a single-component image.
        let pixels = ramp_gray8(40, 20);
        let out = tile_roundtrip(
            40,
            20,
            EncodePixelFormat::Gray8 { pixels: &pixels },
            TiffCompression::Deflate,
            (16, 16),
            true,
        );
        assert_eq!(out, pixels);
    }

    #[test]
    fn encode_palette_tiled_roundtrip() {
        let palette = vec![[0, 0, 0], [255, 0, 0], [0, 255, 0], [255, 255, 255]];
        let mut indices = Vec::with_capacity(40 * 20);
        for y in 0..20u32 {
            for x in 0..40u32 {
                indices.push(((x + y) & 0x3) as u8);
            }
        }
        let page = EncodePage {
            width: 40,
            height: 20,
            kind: EncodePixelFormat::Palette8 {
                indices: &indices,
                palette: &palette,
            },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: false,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        let mut want = Vec::with_capacity(40 * 20 * 3);
        for &idx in &indices {
            want.extend_from_slice(&palette[idx as usize]);
        }
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_tiled_emits_tile_tags_not_strip_tags() {
        // A tiled IFD must carry TileWidth/TileLength/TileOffsets/
        // TileByteCounts and NOT StripOffsets/RowsPerStrip/
        // StripByteCounts (§15: the tile fields "replace" the strip
        // fields; "Do not use both … in the same TIFF file").
        let pixels = ramp_gray8(48, 32);
        let page = EncodePage {
            width: 48,
            height: 32,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let b = encode_tiff(&page).unwrap();
        let ifd_off = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let count = u16::from_le_bytes([b[ifd_off], b[ifd_off + 1]]) as usize;
        let mut seen = std::collections::HashMap::new();
        for k in 0..count {
            let e = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([b[e], b[e + 1]]);
            let cnt = u32::from_le_bytes([b[e + 4], b[e + 5], b[e + 6], b[e + 7]]);
            seen.insert(tag, cnt);
        }
        // No strip tags.
        assert!(!seen.contains_key(&TAG_STRIP_OFFSETS));
        assert!(!seen.contains_key(&TAG_STRIP_BYTE_COUNTS));
        assert!(!seen.contains_key(&TAG_ROWS_PER_STRIP));
        // Tile tags present; TilesPerImage = 3*2 = 6.
        assert!(seen.contains_key(&TAG_TILE_WIDTH));
        assert!(seen.contains_key(&TAG_TILE_LENGTH));
        assert_eq!(seen.get(&TAG_TILE_OFFSETS), Some(&6));
        assert_eq!(seen.get(&TAG_TILE_BYTE_COUNTS), Some(&6));
        // Ascending tag order across the whole IFD.
        let mut prev = 0u16;
        for k in 0..count {
            let e = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([b[e], b[e + 1]]);
            assert!(tag > prev, "tag {tag} not after {prev}");
            prev = tag;
        }
    }

    #[test]
    fn encode_tiling_rejects_non_multiple_of_16() {
        let pixels = ramp_gray8(32, 32);
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Gray8 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: Some((20, 16)),
            bigtiff: false,
        };
        assert!(encode_tiff(&page).is_err());
    }

    #[test]
    fn encode_tiling_bilevel_roundtrips() {
        // Sub-byte (1-bit) tile writing (TIFF 6.0 §15) is now supported
        // for the byte-aligned compressors. The tiled encode of a bilevel
        // raster decodes to the same Gray8 plane as the strip encode of
        // the identical pixels (the strip path is the independent oracle).
        let packed = bilevel_checkerboard(32, 16);
        let strip = EncodePage {
            width: 32,
            height: 16,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let tiled = EncodePage {
            tiling: Some((16, 16)),
            ..strip.clone()
        };
        let ds = decode_tiff(&encode_tiff(&strip).unwrap()).unwrap();
        let dt = decode_tiff(&encode_tiff(&tiled).unwrap()).unwrap();
        assert_eq!(dt.frame.planes[0].data, ds.frame.planes[0].data);
    }

    #[test]
    fn encode_tiling_rejects_ccitt() {
        let packed = bilevel_checkerboard(32, 16);
        let page = EncodePage {
            width: 32,
            height: 16,
            kind: EncodePixelFormat::Bilevel { pixels: &packed },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        assert!(encode_tiff(&page).is_err());
    }

    #[test]
    fn encode_tiling_planar_rgb24_roundtrips() {
        // Planar + tiled Rgb24 (TIFF 6.0 §15 + §"PlanarConfiguration"):
        // one tile grid per component plane, plane-major TileOffsets.
        // Encodes and self-decodes back to the source pixels.
        let pixels = pattern_rgb(32, 32);
        let page = EncodePage {
            width: 32,
            height: 32,
            kind: EncodePixelFormat::Rgb24 { pixels: &pixels },
            compression: TiffCompression::Lzw,
            predictor: false,
            planar: true,
            tiling: Some((16, 16)),
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).expect("planar tiled encode");
        let d = decode_tiff(&bytes).expect("planar tiled decode");
        assert_eq!((d.width, d.height), (32, 32));
        assert_eq!(d.frame.planes[0].data, pixels);
    }

    #[test]
    fn encode_tiled_multi_page_chain() {
        // Tiled pages must chain correctly in a multi-IFD file, mixed
        // with strip pages.
        let p1 = ramp_gray8(48, 32);
        let p2 = pattern_rgb(16, 16);
        let pages = vec![
            EncodePage {
                width: 48,
                height: 32,
                kind: EncodePixelFormat::Gray8 { pixels: &p1 },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
                tiling: Some((16, 16)),
                bigtiff: false,
            },
            EncodePage {
                width: 16,
                height: 16,
                kind: EncodePixelFormat::Rgb24 { pixels: &p2 },
                compression: TiffCompression::None,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            },
        ];
        let bytes = encode_tiff_multi(&pages).unwrap();
        let imgs = crate::decoder::decode_tiff_all(&bytes).unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].planes[0].data, p1);
        assert_eq!(imgs[1].planes[0].data, p2);
    }

    // ----------------------------------------------------------------
    // CIELab encode (TIFF 6.0 §23, PhotometricInterpretation = 8)
    // ----------------------------------------------------------------
    //
    // The encoder writes the caller-supplied L*/a*/b* bytes through
    // verbatim — the decoder takes them back off disk verbatim too,
    // so a self-roundtrip can compare on-disk-bytes-in vs
    // bytes-the-decoder-saw at the strip / tile layer. The
    // colourimetric Lab → Rgb24 conversion the decoder applies *after*
    // that is exercised separately by `tests/decode_cielab.rs`.

    /// Pack a logical (L%, a, b) where L is the 0..100 perceptual scale
    /// and a, b are -127..127, into the three on-disk bytes per §23.
    fn pack_lab_byte(l_pct: f64, a_signed: i32, b_signed: i32) -> [u8; 3] {
        let l_byte = (l_pct * 255.0 / 100.0).round().clamp(0.0, 255.0) as u8;
        [l_byte, (a_signed as i8) as u8, (b_signed as i8) as u8]
    }

    /// Build a 3-sample (L*, a*, b*) raster as a deterministic mix of
    /// the four chromatic primaries plus the neutral gradient.
    fn lab_pattern_3sample(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                // L* swings 0..100 across the row, a* sweeps -127..127
                // across the column, b* picks up alternate sign rows.
                let l_pct = (x as f64) * 100.0 / (w as f64).max(1.0);
                let a_signed = -127 + (2 * 127 * (y as i32) / (h as i32).max(1));
                let b_signed = if (x ^ y) & 1 == 0 { 50 } else { -50 };
                v.extend_from_slice(&pack_lab_byte(l_pct, a_signed, b_signed));
            }
        }
        v
    }

    /// 1-sample L*-only ramp.
    fn lab_l_ramp(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                v.push(((x.wrapping_add(y)) & 0xFF) as u8);
            }
        }
        v
    }

    /// Helper: take the public-API decoded (Lab → Rgb24) output of a
    /// CIELab fixture as the round-trip "ground truth" and check that
    /// the same source bytes round-trip through both the hand-built
    /// classic fixture path and our `encode_tiff(CieLab8)`. If both
    /// produce identical Rgb24 outputs, the encoder is writing the
    /// strip / IFD / photometric the decoder is expecting.
    fn decode_3sample_cielab(pixels: &[u8], w: u32, h: u32) -> Vec<u8> {
        // Build the same classic-TIFF the decode-side tests use,
        // independently of our encoder, to get the canonical Rgb24 the
        // decoder produces from a verbatim L*/a*/b* strip. The encoder
        // path's Rgb24 must match this.
        let row_bytes = (w as u64) * 3;
        let strip_bytes = row_bytes * (h as u64);
        assert_eq!(pixels.len() as u64, strip_bytes);
        let num_entries: u16 = 8;
        let ifd_offset: u32 = 8;
        let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
        let bps_blob_bytes: u32 = 3 * 2;
        let blobs_offset: u32 = ifd_offset + ifd_size;
        let bps_off = blobs_offset;
        let pixels_off = bps_off + bps_blob_bytes;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"II");
        buf.extend_from_slice(&42u16.to_le_bytes());
        buf.extend_from_slice(&ifd_offset.to_le_bytes());
        buf.extend_from_slice(&num_entries.to_le_bytes());
        let push = |buf: &mut Vec<u8>, tag: u16, ft: u16, count: u32, v: [u8; 4]| {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&ft.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
            buf.extend_from_slice(&v);
        };
        push(&mut buf, 256, 4, 1, w.to_le_bytes());
        push(&mut buf, 257, 4, 1, h.to_le_bytes());
        push(&mut buf, 258, 3, 3, bps_off.to_le_bytes());
        let mut comp = [0u8; 4];
        comp[..2].copy_from_slice(&1u16.to_le_bytes());
        push(&mut buf, 259, 3, 1, comp);
        let mut ph = [0u8; 4];
        ph[..2].copy_from_slice(&8u16.to_le_bytes());
        push(&mut buf, 262, 3, 1, ph);
        push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes());
        let mut spp = [0u8; 4];
        spp[..2].copy_from_slice(&3u16.to_le_bytes());
        push(&mut buf, 277, 3, 1, spp);
        push(&mut buf, 279, 4, 1, (strip_bytes as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..3u16 {
            buf.extend_from_slice(&8u16.to_le_bytes());
        }
        buf.extend_from_slice(pixels);
        decode_tiff(&buf).unwrap().frame.planes[0].data.clone()
    }

    fn decode_1sample_cielab(pixels: &[u8], w: u32, h: u32) -> Vec<u8> {
        let strip_bytes = (w as u64) * (h as u64);
        assert_eq!(pixels.len() as u64, strip_bytes);
        let num_entries: u16 = 8;
        let ifd_offset: u32 = 8;
        let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
        let blobs_offset: u32 = ifd_offset + ifd_size;
        let pixels_off = blobs_offset;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"II");
        buf.extend_from_slice(&42u16.to_le_bytes());
        buf.extend_from_slice(&ifd_offset.to_le_bytes());
        buf.extend_from_slice(&num_entries.to_le_bytes());
        let push = |buf: &mut Vec<u8>, tag: u16, ft: u16, count: u32, v: [u8; 4]| {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&ft.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
            buf.extend_from_slice(&v);
        };
        push(&mut buf, 256, 4, 1, w.to_le_bytes());
        push(&mut buf, 257, 4, 1, h.to_le_bytes());
        let mut bps = [0u8; 4];
        bps[..2].copy_from_slice(&8u16.to_le_bytes());
        push(&mut buf, 258, 3, 1, bps);
        let mut comp = [0u8; 4];
        comp[..2].copy_from_slice(&1u16.to_le_bytes());
        push(&mut buf, 259, 3, 1, comp);
        let mut ph = [0u8; 4];
        ph[..2].copy_from_slice(&8u16.to_le_bytes());
        push(&mut buf, 262, 3, 1, ph);
        push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes());
        let mut spp = [0u8; 4];
        spp[..2].copy_from_slice(&1u16.to_le_bytes());
        push(&mut buf, 277, 3, 1, spp);
        push(&mut buf, 279, 4, 1, (strip_bytes as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(pixels);
        decode_tiff(&buf).unwrap().frame.planes[0].data.clone()
    }

    #[test]
    fn encode_cielab8_uncompressed_roundtrip() {
        // 3-sample (L*, a*, b*) at 8 bits each: encoder must write
        // PhotometricInterpretation = 8 + SamplesPerPixel = 3 +
        // BitsPerSample = [8,8,8], so the decoder takes the strip bytes
        // through the §23 colourimetric pipeline. Self-roundtrip checks
        // the output matches the hand-built fixture's decode.
        let pixels = lab_pattern_3sample(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (8, 8));
        assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
        let want = decode_3sample_cielab(&pixels, 8, 8);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_cielab8_compressors_match_uncompressed() {
        // PackBits / LZW / Deflate must all produce decoder output
        // identical to the uncompressed encode (lossless byte-aligned
        // compressors, photometric-agnostic).
        let pixels = lab_pattern_3sample(16, 8);
        let baseline = {
            let page = EncodePage {
                width: 16,
                height: 8,
                kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
                compression: TiffCompression::None,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        for c in [
            TiffCompression::PackBits,
            TiffCompression::Lzw,
            TiffCompression::Deflate,
        ] {
            let page = EncodePage {
                width: 16,
                height: 8,
                kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
                compression: c,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
            assert_eq!(d.frame.planes[0].data, baseline, "compressor {:?}", c);
        }
    }

    #[test]
    fn encode_cielab8_predictor_composes() {
        // Predictor=2 must round-trip on chunky 3-sample CIELab —
        // the decoder undoes the per-component differencing with
        // SamplesPerPixel = 3, identical to Rgb24.
        let pixels = lab_pattern_3sample(20, 12);
        let no_pred = {
            let page = EncodePage {
                width: 20,
                height: 12,
                kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        let with_pred = {
            let page = EncodePage {
                width: 20,
                height: 12,
                kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
                compression: TiffCompression::Lzw,
                predictor: true,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        assert_eq!(no_pred, with_pred);
    }

    #[test]
    fn encode_cielab8_planar_composes() {
        // PlanarConfiguration = 2 splits L* / a* / b* into three
        // single-component planes (§"PlanarConfiguration"). The
        // re-interleaved decode must match the chunky path.
        let pixels = lab_pattern_3sample(16, 8);
        let chunky = {
            let page = EncodePage {
                width: 16,
                height: 8,
                kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
                compression: TiffCompression::Deflate,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        let planar = {
            let page = EncodePage {
                width: 16,
                height: 8,
                kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
                compression: TiffCompression::Deflate,
                predictor: false,
                planar: true,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        assert_eq!(chunky, planar);
    }

    #[test]
    fn encode_cielab8_tiled_composes() {
        // Tiled §15 chunky write — `tiffcp` round-trip pattern, applied
        // to L*/a*/b* via the byte-aligned tile splitter.
        let pixels = lab_pattern_3sample(32, 32);
        let strip = {
            let page = EncodePage {
                width: 32,
                height: 32,
                kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        let tiled = {
            let page = EncodePage {
                width: 32,
                height: 32,
                kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
                tiling: Some((16, 16)),
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        assert_eq!(strip, tiled);
    }

    #[test]
    fn encode_cielab8_bigtiff_composes() {
        // BigTIFF: BitsPerSample[3] should now sit inline in the
        // widened 8-byte value/offset slot. Self-roundtrip the same as
        // the classic path.
        let pixels = lab_pattern_3sample(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
            compression: TiffCompression::Deflate,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: true,
        };
        let bytes = encode_tiff(&page).unwrap();
        // BigTIFF magic 43.
        assert_eq!(&bytes[..2], b"II");
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 43);
        let d = decode_tiff(&bytes).unwrap();
        let want = decode_3sample_cielab(&pixels, 8, 8);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_cielab8_rejects_ccitt() {
        // CCITT is bilevel-only per §10 / §11; CieLab8 is 3-sample
        // 8-bit, so the bilevel-input gate must reject.
        let pixels = lab_pattern_3sample(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let err = encode_tiff(&page).unwrap_err();
        assert!(format!("{err}").contains("CCITT"));
    }

    #[test]
    fn encode_cielab8_wrong_buffer_size_rejected() {
        // 8x8 wants 192 bytes (3 * 64); pass 100 to exercise the size
        // validator.
        let bad = vec![0u8; 100];
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::CieLab8 { pixels: &bad },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let err = encode_tiff(&page).unwrap_err();
        assert!(format!("{err}").contains("CieLab8"));
    }

    #[test]
    fn encode_cielab_l8_roundtrip() {
        // 1-sample L*-only — §23 "1 implies L* only, for monochrome
        // data". Decoder must produce Gray8 matching the hand-built
        // fixture.
        let pixels = lab_l_ramp(8, 4);
        let page = EncodePage {
            width: 8,
            height: 4,
            kind: EncodePixelFormat::CieLabL8 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (8, 4));
        assert_eq!(d.pixel_format, TiffPixelFormat::Gray8);
        let want = decode_1sample_cielab(&pixels, 8, 4);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_cielab_l8_rejects_planar() {
        // SamplesPerPixel = 1 → §"PlanarConfiguration" "irrelevant" →
        // rejected (mirrors Gray8 / Gray16Le / Palette8 / Bilevel).
        let pixels = lab_l_ramp(8, 4);
        let page = EncodePage {
            width: 8,
            height: 4,
            kind: EncodePixelFormat::CieLabL8 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: true,
            tiling: None,
            bigtiff: false,
        };
        let err = encode_tiff(&page).unwrap_err();
        assert!(format!("{err}").contains("PlanarConfiguration"));
    }

    #[test]
    fn encode_cielab_writes_photometric_8() {
        // Decode the encoder's output through a byte-level IFD walker
        // (independent of our decoder) and confirm
        // PhotometricInterpretation = 8 lands in tag 262.
        let pixels = lab_pattern_3sample(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::CieLab8 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
        let mut found = None;
        for k in 0..count {
            let entry_off = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([bytes[entry_off], bytes[entry_off + 1]]);
            if tag == 262 {
                let val = u16::from_le_bytes([bytes[entry_off + 8], bytes[entry_off + 9]]);
                found = Some(val);
            }
        }
        assert_eq!(found, Some(8), "expected PhotometricInterpretation = 8");
    }

    // ---- CMYK (TIFF 6.0 §16) encode tests ---------------------------
    //
    // §16 fixes the on-disk bit layout: 4 chunky samples per pixel
    // ordered (C, M, Y, K), each unsigned 8-bit with `0` = 0 % ink
    // and `255` = 100 % ink. The encoder writes those bytes through
    // verbatim, so a self-roundtrip compares strip-bytes-in vs
    // strip-bytes-out at the strip layer; the §16 additive-RGB
    // collapse the decoder performs (`build_rgb24_from_cmyk` in
    // `src/decoder.rs`) is exercised separately by tests against the
    // hand-built classic fixture below.

    /// Build a deterministic CMYK raster that exercises pure ink
    /// channels, neutrals, and a black-only column so the encoder
    /// sees a non-trivial value distribution per component.
    fn cmyk_pattern_4sample(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                // C sweeps the row, M sweeps the column, Y is the
                // XOR plane, K is a per-row ramp. Each of the four
                // components gets a meaningful 0..=255 distribution.
                let c = ((x.wrapping_mul(7)) & 0xFF) as u8;
                let m = ((y.wrapping_mul(11)) & 0xFF) as u8;
                let y_byte = ((x ^ y).wrapping_mul(13) & 0xFF) as u8;
                let k = ((y * 255) / h.max(1).saturating_sub(1).max(1)) as u8;
                v.extend_from_slice(&[c, m, y_byte, k]);
            }
        }
        v
    }

    /// Hand-build a classic chunky CMYK TIFF (no encoder), decode it
    /// through `decode_tiff`, and return the resulting Rgb24 plane.
    /// This anchors what an §16-conforming CMYK page is supposed to
    /// look like off-disk so the encoder output can be checked
    /// against the *same* decoded image. Mirrors the
    /// `decode_3sample_cielab` helper above.
    fn decode_cmyk_4sample(pixels: &[u8], w: u32, h: u32) -> Vec<u8> {
        let row_bytes = (w as u64) * 4;
        let strip_bytes = row_bytes * (h as u64);
        assert_eq!(pixels.len() as u64, strip_bytes);
        // 254 + 256/257/258/259/262/273/277/279 + 332/334 = 11 entries.
        let num_entries: u16 = 11;
        let ifd_offset: u32 = 8;
        let ifd_size: u32 = 2 + (num_entries as u32) * 12 + 4;
        let bps_blob_bytes: u32 = 4 * 2; // four SHORT entries — spills out-of-line in classic TIFF (8 > 4)
        let blobs_offset: u32 = ifd_offset + ifd_size;
        let bps_off = blobs_offset;
        let pixels_off = bps_off + bps_blob_bytes;
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"II");
        buf.extend_from_slice(&42u16.to_le_bytes());
        buf.extend_from_slice(&ifd_offset.to_le_bytes());
        buf.extend_from_slice(&num_entries.to_le_bytes());
        let push = |buf: &mut Vec<u8>, tag: u16, ft: u16, count: u32, v: [u8; 4]| {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&ft.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
            buf.extend_from_slice(&v);
        };
        // 254 NewSubfileType = 0.
        push(&mut buf, 254, 4, 1, 0u32.to_le_bytes());
        // 256 ImageWidth, 257 ImageLength (LONG).
        push(&mut buf, 256, 4, 1, w.to_le_bytes());
        push(&mut buf, 257, 4, 1, h.to_le_bytes());
        // 258 BitsPerSample = [8,8,8,8] SHORT (spills out-of-line).
        push(&mut buf, 258, 3, 4, bps_off.to_le_bytes());
        // 259 Compression = 1.
        let mut comp = [0u8; 4];
        comp[..2].copy_from_slice(&1u16.to_le_bytes());
        push(&mut buf, 259, 3, 1, comp);
        // 262 PhotometricInterpretation = 5 (CMYK).
        let mut ph = [0u8; 4];
        ph[..2].copy_from_slice(&5u16.to_le_bytes());
        push(&mut buf, 262, 3, 1, ph);
        // 273 StripOffsets (LONG, inline for 1 strip).
        push(&mut buf, 273, 4, 1, pixels_off.to_le_bytes());
        // 277 SamplesPerPixel = 4.
        let mut spp = [0u8; 4];
        spp[..2].copy_from_slice(&4u16.to_le_bytes());
        push(&mut buf, 277, 3, 1, spp);
        // 279 StripByteCounts (LONG, inline).
        push(&mut buf, 279, 4, 1, (strip_bytes as u32).to_le_bytes());
        // 332 InkSet = 1 (CMYK).
        let mut ink = [0u8; 4];
        ink[..2].copy_from_slice(&1u16.to_le_bytes());
        push(&mut buf, 332, 3, 1, ink);
        // 334 NumberOfInks = 4.
        let mut nink = [0u8; 4];
        nink[..2].copy_from_slice(&4u16.to_le_bytes());
        push(&mut buf, 334, 3, 1, nink);
        // Next IFD = 0.
        buf.extend_from_slice(&0u32.to_le_bytes());
        // BitsPerSample blob (four SHORTs = 8 bytes).
        for _ in 0..4u16 {
            buf.extend_from_slice(&8u16.to_le_bytes());
        }
        buf.extend_from_slice(pixels);
        decode_tiff(&buf).unwrap().frame.planes[0].data.clone()
    }

    #[test]
    fn encode_cmyk32_uncompressed_roundtrip() {
        // 4-sample chunky (C, M, Y, K) at 8 bits each — encoder must
        // write PhotometricInterpretation = 5, SamplesPerPixel = 4,
        // BitsPerSample = [8,8,8,8], so the decoder takes the strip
        // bytes through the §16 additive-RGB collapse. The encoder
        // output's Rgb24 must match the hand-built fixture's decode.
        let pixels = cmyk_pattern_4sample(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!((d.width, d.height), (8, 8));
        assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
        let want = decode_cmyk_4sample(&pixels, 8, 8);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_cmyk32_compressors_match_uncompressed() {
        // PackBits / LZW / Deflate must all produce decoder output
        // identical to the uncompressed encode — they are lossless
        // byte-aligned compressors, photometric-agnostic.
        let pixels = cmyk_pattern_4sample(16, 8);
        let baseline = {
            let page = EncodePage {
                width: 16,
                height: 8,
                kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                compression: TiffCompression::None,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        for c in [
            TiffCompression::PackBits,
            TiffCompression::Lzw,
            TiffCompression::Deflate,
        ] {
            let page = EncodePage {
                width: 16,
                height: 8,
                kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                compression: c,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            let d = decode_tiff(&encode_tiff(&page).unwrap()).unwrap();
            assert_eq!(d.frame.planes[0].data, baseline, "compressor {:?}", c);
        }
    }

    #[test]
    fn encode_cmyk32_predictor_composes() {
        // Predictor=2 must round-trip on chunky 4-sample CMYK — the
        // decoder undoes the per-component differencing with
        // SamplesPerPixel = 4 (analogous to the Rgb24 / CieLab8 paths
        // that already exercise SPP=3).
        let pixels = cmyk_pattern_4sample(20, 12);
        let no_pred = {
            let page = EncodePage {
                width: 20,
                height: 12,
                kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        let with_pred = {
            let page = EncodePage {
                width: 20,
                height: 12,
                kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                compression: TiffCompression::Lzw,
                predictor: true,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        assert_eq!(no_pred, with_pred);
    }

    #[test]
    fn encode_cmyk32_planar_composes() {
        // PlanarConfiguration = 2 splits C / M / Y / K into four
        // single-component planes (§"PlanarConfiguration"). The
        // re-interleaved decode must match the chunky path.
        let pixels = cmyk_pattern_4sample(16, 8);
        let chunky = {
            let page = EncodePage {
                width: 16,
                height: 8,
                kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                compression: TiffCompression::Deflate,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        let planar = {
            let page = EncodePage {
                width: 16,
                height: 8,
                kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                compression: TiffCompression::Deflate,
                predictor: false,
                planar: true,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        assert_eq!(chunky, planar);
    }

    #[test]
    fn encode_cmyk32_tiled_composes() {
        // Tiled §15 chunky write — the strip-vs-tile decode must
        // collapse to the same Rgb24, mirroring the Rgb24 / CieLab8
        // tiled-roundtrip tests.
        let pixels = cmyk_pattern_4sample(32, 32);
        let strip = {
            let page = EncodePage {
                width: 32,
                height: 32,
                kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
                tiling: None,
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        let tiled = {
            let page = EncodePage {
                width: 32,
                height: 32,
                kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
                compression: TiffCompression::Lzw,
                predictor: false,
                planar: false,
                tiling: Some((16, 16)),
                bigtiff: false,
            };
            decode_tiff(&encode_tiff(&page).unwrap())
                .unwrap()
                .frame
                .planes[0]
                .data
                .clone()
        };
        assert_eq!(strip, tiled);
    }

    #[test]
    fn encode_cmyk32_bigtiff_composes() {
        // BigTIFF: BitsPerSample[4] (8 bytes) now sits inline in the
        // widened 8-byte value/offset slot. Self-roundtrip the same
        // as the classic path.
        let pixels = cmyk_pattern_4sample(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
            compression: TiffCompression::Deflate,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: true,
        };
        let bytes = encode_tiff(&page).unwrap();
        assert_eq!(&bytes[..2], b"II");
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 43);
        let d = decode_tiff(&bytes).unwrap();
        let want = decode_cmyk_4sample(&pixels, 8, 8);
        assert_eq!(d.frame.planes[0].data, want);
    }

    #[test]
    fn encode_cmyk32_rejects_ccitt() {
        // CCITT is bilevel-only per §10 / §11; Cmyk32 is 4-sample
        // 8-bit, so the bilevel-input gate must reject.
        let pixels = cmyk_pattern_4sample(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
            compression: TiffCompression::CcittRle,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let err = encode_tiff(&page).unwrap_err();
        assert!(format!("{err}").contains("CCITT"));
    }

    #[test]
    fn encode_cmyk32_wrong_buffer_size_rejected() {
        // 8x8 wants 256 bytes (4 * 64); pass 100 to exercise the size
        // validator.
        let bad = vec![0u8; 100];
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Cmyk32 { pixels: &bad },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let err = encode_tiff(&page).unwrap_err();
        assert!(format!("{err}").contains("Cmyk32"));
    }

    #[test]
    fn encode_cmyk32_writes_photometric_inkset_and_numberofinks() {
        // Decode the encoder's output through a byte-level IFD walker
        // (independent of our decoder) and confirm
        // PhotometricInterpretation = 5 lands in tag 262, InkSet = 1
        // in tag 332, and NumberOfInks = 4 in tag 334.
        let pixels = cmyk_pattern_4sample(8, 8);
        let page = EncodePage {
            width: 8,
            height: 8,
            kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let ifd_off = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let count = u16::from_le_bytes([bytes[ifd_off], bytes[ifd_off + 1]]) as usize;
        let mut photo = None;
        let mut ink_set = None;
        let mut num_inks = None;
        for k in 0..count {
            let entry_off = ifd_off + 2 + k * 12;
            let tag = u16::from_le_bytes([bytes[entry_off], bytes[entry_off + 1]]);
            let val = u16::from_le_bytes([bytes[entry_off + 8], bytes[entry_off + 9]]);
            match tag {
                262 => photo = Some(val),
                332 => ink_set = Some(val),
                334 => num_inks = Some(val),
                _ => {}
            }
        }
        assert_eq!(photo, Some(5), "expected PhotometricInterpretation = 5");
        assert_eq!(ink_set, Some(1), "expected InkSet = 1 (CMYK)");
        assert_eq!(num_inks, Some(4), "expected NumberOfInks = 4");
    }

    #[test]
    fn encode_cmyk32_pure_inks_collapse_to_expected_rgb() {
        // §16 fixes the additive-RGB collapse as `R = (1-C)(1-K)`,
        // `G = (1-M)(1-K)`, `B = (1-Y)(1-K)` (all scaled to 8-bit),
        // implemented by `build_rgb24_from_cmyk` in the decoder.
        // Check the four canonical "pure ink" pixels — pure C, pure
        // M, pure Y, pure K — land at the spec-mandated RGB triples
        // through the full encode -> decode path. This pins the
        // §16-required `0 = no ink` orientation: writing `(255, 0, 0,
        // 0)` (full cyan, no other ink, no black) must decode to a
        // red-absent pixel `(0, 255, 255)`, not the opposite.
        let pixels = [
            255, 0, 0, 0, // pure cyan
            0, 255, 0, 0, // pure magenta
            0, 0, 255, 0, // pure yellow
            0, 0, 0, 255, // pure black
        ];
        let page = EncodePage {
            width: 4,
            height: 1,
            kind: EncodePixelFormat::Cmyk32 { pixels: &pixels },
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
        };
        let bytes = encode_tiff(&page).unwrap();
        let d = decode_tiff(&bytes).unwrap();
        assert_eq!(d.pixel_format, TiffPixelFormat::Rgb24);
        let rgb = &d.frame.planes[0].data;
        // Pure cyan -> R = (1-1)(1-0) = 0, G = B = 255.
        assert_eq!(&rgb[0..3], &[0, 255, 255]);
        // Pure magenta -> R = 255, G = 0, B = 255.
        assert_eq!(&rgb[3..6], &[255, 0, 255]);
        // Pure yellow -> R = 255, G = 255, B = 0.
        assert_eq!(&rgb[6..9], &[255, 255, 0]);
        // Pure black -> all zero (the (1-K) factor multiplies out).
        assert_eq!(&rgb[9..12], &[0, 0, 0]);
    }
}
