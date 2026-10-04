//! The standalone image types: the shapes every `oxideav-<format>`
//! image crate shares (`IMAGE_CRATE_API`), specialised for TIFF.
//!
//! * [`TiffImage`] — the native-layout image [`crate::decode`] returns
//!   and [`crate::encode`] consumes: dimensions, a [`PixelFormat`] tag,
//!   one packed [`Plane`], [`ColorInfo`], [`Metadata`] and an optional
//!   [`Palette`].
//! * [`RgbImage`] / [`RgbaImage`] — the tightly packed 8-bit raw paths
//!   ([`crate::decode_rgb8`] / [`crate::decode_rgba8`],
//!   [`TiffImage::to_rgb8`] / [`TiffImage::to_rgba8`]).
//! * [`Frame`] — one entry of [`crate::decode_all`] (one per IFD on the
//!   next-IFD chain).
//! * [`Page`] — one IFD with everything the decoder learned about it:
//!   the image, the descriptive [`TiffMetadata`] and the structural
//!   [`TiffFormatInfo`] (the format-specific depth type the contract
//!   leaves to each crate).
//! * [`ImageInfo`] — what [`crate::info`] reads from the IFD chain
//!   without decoding pixels.
//!
//! None of these depend on `oxideav-core`; with the `registry` feature
//! [`crate::registry`] adds the `From<TiffImage> for VideoFrame`
//! conversion and its inverse so the framework `Decoder` / `Encoder`
//! are thin adapters over the same functions.

use std::time::Duration;

use crate::error::{Result, TiffError};
use crate::metadata::{TiffFormatInfo, TiffMetadata};

/// Pixel layouts the standalone `oxideav-tiff` API can produce / consume.
///
/// Variant names mirror `oxideav_core::PixelFormat` exactly, so the
/// [`crate::registry`] conversion layer is a 1:1 match-and-rebuild
/// rather than a re-pack. Every TIFF layout this crate emits is packed
/// (one plane, `stride == width × bytes_per_pixel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TiffPixelFormat {
    /// 8-bit single-channel grayscale, one plane (1 byte per pixel).
    /// Bilevel and 4-bit sources are expanded to this layout
    /// (`WhiteIsZero` is inverted so 0 is always black).
    Gray8,
    /// 16-bit single-channel grayscale, little-endian, one plane
    /// (2 bytes per pixel).
    Gray16Le,
    /// 8-bit packed RGB, one plane (3 bytes per pixel).
    Rgb24,
    /// 16-bit packed RGB, little-endian, one plane (6 bytes per pixel).
    Rgb48Le,
    /// 8-bit packed RGBA, one plane (4 bytes per pixel), straight
    /// (non-premultiplied) alpha — TIFF 6.0 §ExtraSamples value 2
    /// (unassociated alpha).
    Rgba,
    /// 8-bit palette index (1 byte per pixel). The matching colour
    /// table lives on [`TiffImage::palette`]; 4-bit indexed sources
    /// are expanded to one index per byte.
    Pal8,
    /// 8-bit packed CMYK, one plane (4 bytes per pixel) in byte order
    /// C, M, Y, K — TIFF 6.0 §16 `InkSet = 1`, where 0 means no ink
    /// and 255 means full ink coverage (the `oxideav_core::PixelFormat::Cmyk`
    /// "regular" convention).
    Cmyk,
}

/// The contract name for [`TiffPixelFormat`].
pub type PixelFormat = TiffPixelFormat;

impl TiffPixelFormat {
    /// Bytes per pixel of the packed layout.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Gray8 | Self::Pal8 => 1,
            Self::Gray16Le => 2,
            Self::Rgb24 => 3,
            Self::Rgba | Self::Cmyk => 4,
            Self::Rgb48Le => 6,
        }
    }

    /// `true` when the layout carries an alpha channel of its own
    /// (`Rgba`).
    pub fn has_alpha(self) -> bool {
        matches!(self, Self::Rgba)
    }

    /// Bits per sample of the layout (8 or 16).
    pub fn bits_per_sample(self) -> u8 {
        match self {
            Self::Gray8 | Self::Pal8 | Self::Rgb24 | Self::Rgba | Self::Cmyk => 8,
            Self::Gray16Le | Self::Rgb48Le => 16,
        }
    }
}

/// One pixel plane: `stride` bytes per row, `data` holding
/// `stride × height` bytes (rows may carry padding past the visible
/// width). TIFF layouts are packed, so a [`TiffImage`] has exactly one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plane {
    /// Bytes per row in `data` (may be larger than the logical row width).
    pub stride: usize,
    /// Row-major bytes, `stride × height` long.
    pub data: Vec<u8>,
}

impl Plane {
    /// Wrap a plane buffer with its row stride.
    pub fn new(stride: usize, data: Vec<u8>) -> Self {
        Self { stride, data }
    }
}

/// Pre-contract name of [`Plane`].
#[deprecated(note = "use oxideav_tiff::Plane (IMAGE_CRATE_API)")]
pub type TiffPlane = Plane;

/// Nominal sample range (H.273 `VideoFullRangeFlag`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ColorRange {
    /// No range was signalled.
    #[default]
    Unspecified,
    /// Limited (video / studio) range: `VideoFullRangeFlag == 0`.
    Limited,
    /// Full (PC) range: `VideoFullRangeFlag == 1`.
    Full,
}

/// Colour signalling of an image: the sample range plus the H.273
/// `ColourPrimaries` / `TransferCharacteristics` /
/// `MatrixCoefficients` code points (`2` = unspecified).
///
/// TIFF 6.0 §20 ("RGB Image Colorimetry") carries no code points: an
/// image "has a colorimetric interpretation if and only if both the
/// WhitePoint and PrimaryChromaticities fields are present", and
/// otherwise "will be displayed in an application and hardware
/// dependent manner". [`crate::decode`] therefore fills `primaries`
/// from the WhitePoint (318) + PrimaryChromaticities (319) pair when
/// the chromaticities match one of the H.273 Table 2 rows (BT.709,
/// BT.470 M / B-G, BT.601 525, SMPTE 240, BT.2020, P3-D65), leaves
/// `transfer` unspecified (the §20 TransferFunction is a table, not a
/// code point), and reports every integer layout the decoder emits as
/// full range with the identity matrix (the decoder hands back RGB,
/// gray, palette or CMYK samples, never a YCbCr layout). The TIFF
/// default when the colorimetry fields are absent is
/// [`ColorInfo::tiff_default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ColorInfo {
    /// Sample range.
    pub range: ColorRange,
    /// H.273 `ColourPrimaries` code point (`1` = BT.709 / sRGB, `9` =
    /// BT.2020, `2` = unspecified).
    pub primaries: u8,
    /// H.273 `TransferCharacteristics` code point (`13` = sRGB, `2` =
    /// unspecified).
    pub transfer: u8,
    /// H.273 `MatrixCoefficients` code point (`0` = identity / RGB).
    pub matrix: u8,
}

impl ColorInfo {
    /// H.273 "unspecified" code point.
    pub const UNSPECIFIED: u8 = 2;
    /// H.273 `MatrixCoefficients` identity (RGB / GBR) code point.
    pub const MATRIX_IDENTITY: u8 = 0;
    /// H.273 `ColourPrimaries` BT.709 / sRGB code point.
    pub const PRIMARIES_BT709: u8 = 1;
    /// H.273 `ColourPrimaries` BT.2020 / BT.2100 code point.
    pub const PRIMARIES_BT2020: u8 = 9;
    /// H.273 `TransferCharacteristics` IEC 61966-2-1 sRGB code point.
    pub const TRANSFER_SRGB: u8 = 13;

    /// Build a description from its four parts.
    pub const fn new(range: ColorRange, primaries: u8, transfer: u8, matrix: u8) -> Self {
        Self {
            range,
            primaries,
            transfer,
            matrix,
        }
    }

    /// Every field unspecified.
    pub const fn unspecified() -> Self {
        Self::new(
            ColorRange::Unspecified,
            Self::UNSPECIFIED,
            Self::UNSPECIFIED,
            Self::UNSPECIFIED,
        )
    }

    /// TIFF's documented default when the §20 colorimetry fields are
    /// absent: full-range samples with the identity matrix (the
    /// decoder's layouts are RGB / gray / palette / CMYK) and
    /// unspecified primaries and transfer ("application and hardware
    /// dependent", TIFF 6.0 §20).
    pub const fn tiff_default() -> Self {
        Self::new(
            ColorRange::Full,
            Self::UNSPECIFIED,
            Self::UNSPECIFIED,
            Self::MATRIX_IDENTITY,
        )
    }

    /// sRGB (IEC 61966-2-1): BT.709 primaries, sRGB transfer, identity
    /// matrix, full range.
    pub const fn srgb() -> Self {
        Self::new(
            ColorRange::Full,
            Self::PRIMARIES_BT709,
            Self::TRANSFER_SRGB,
            Self::MATRIX_IDENTITY,
        )
    }

    /// Set the range.
    pub fn with_range(mut self, range: ColorRange) -> Self {
        self.range = range;
        self
    }

    /// Set the primaries code point.
    pub fn with_primaries(mut self, primaries: u8) -> Self {
        self.primaries = primaries;
        self
    }

    /// Set the transfer code point.
    pub fn with_transfer(mut self, transfer: u8) -> Self {
        self.transfer = transfer;
        self
    }

    /// Set the matrix code point.
    pub fn with_matrix(mut self, matrix: u8) -> Self {
        self.matrix = matrix;
        self
    }

    /// `true` when both primaries and transfer are specified (`!= 2`).
    pub fn is_specified(&self) -> bool {
        self.primaries != Self::UNSPECIFIED && self.transfer != Self::UNSPECIFIED
    }
}

impl Default for ColorInfo {
    /// [`ColorInfo::tiff_default`].
    fn default() -> Self {
        Self::tiff_default()
    }
}

/// The metadata blobs every image crate surfaces: an ICC profile, an
/// Exif payload, an XMP packet and a file gamma. TIFF sources them
/// from the ICC profile tag (34675), the Exif IFD (34665, plus the GPS
/// IFD 34853) re-serialised as a standalone Exif TIFF payload, and the
/// XMP packet tag (700); TIFF has no gamma tag (its §20
/// `TransferFunction` is a table), so `gamma` is always `None` on
/// decode. The full TIFF 6.0 §8 descriptive field set is on
/// [`TiffMetadata`] (through [`Page`] / [`crate::decode_page`]).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Metadata {
    /// ICC profile bytes (tag 34675, verbatim).
    pub icc: Option<Vec<u8>>,
    /// Exif payload starting at a TIFF header: a little-endian classic
    /// TIFF whose 0th IFD holds only the `ExifIFDPointer` (34665) and,
    /// when the page had one, the `GPSInfoIFDPointer` (34853), each
    /// pointing at a verbatim copy of the page's child IFD entries.
    pub exif: Option<Vec<u8>>,
    /// XMP packet bytes (tag 700, UTF-8, verbatim).
    pub xmp: Option<Vec<u8>>,
    /// File gamma. TIFF carries none; always `None` after decode and
    /// ignored on encode.
    pub gamma: Option<f32>,
}

impl Metadata {
    /// Empty metadata.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or clear) the ICC profile.
    pub fn with_icc(mut self, icc: impl Into<Option<Vec<u8>>>) -> Self {
        self.icc = icc.into();
        self
    }

    /// Set (or clear) the Exif payload.
    pub fn with_exif(mut self, exif: impl Into<Option<Vec<u8>>>) -> Self {
        self.exif = exif.into();
        self
    }

    /// Set (or clear) the XMP packet.
    pub fn with_xmp(mut self, xmp: impl Into<Option<Vec<u8>>>) -> Self {
        self.xmp = xmp.into();
        self
    }

    /// Set (or clear) the file gamma.
    pub fn with_gamma(mut self, gamma: impl Into<Option<f32>>) -> Self {
        self.gamma = gamma.into();
        self
    }

    /// `true` when no field is set.
    pub fn is_empty(&self) -> bool {
        self.icc.is_none() && self.exif.is_none() && self.xmp.is_none() && self.gamma.is_none()
    }
}

/// Colour table of an indexed ([`TiffPixelFormat::Pal8`]) image: RGBA
/// entries, index `i` at `entries[i]`. TIFF builds it from the
/// `ColorMap` tag (320): `3 × 2^BitsPerSample` 16-bit words, all red
/// first, then green, then blue, each reduced to 8 bits by keeping the
/// high byte; TIFF palettes carry no alpha, so every entry is opaque
/// (`255`). The encoder writes `ColorMap` from the entries (each 8-bit
/// channel replicated into both bytes of its 16-bit word).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Palette {
    /// `[r, g, b, a]` per entry, at most 256 entries.
    pub entries: Vec<[u8; 4]>,
}

impl Palette {
    /// Wrap a list of RGBA entries.
    pub fn new(entries: Vec<[u8; 4]>) -> Self {
        Self { entries }
    }

    /// Build an opaque palette from RGB triples.
    pub fn from_rgb_triples(rgb: &[[u8; 3]]) -> Self {
        Self {
            entries: rgb.iter().map(|c| [c[0], c[1], c[2], 255]).collect(),
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when the palette has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entry `index`, if present.
    pub fn get(&self, index: u8) -> Option<[u8; 4]> {
        self.entries.get(usize::from(index)).copied()
    }

    /// The entries as RGB triples (alpha dropped).
    pub fn to_rgb_triples(&self) -> Vec<[u8; 3]> {
        self.entries.iter().map(|e| [e[0], e[1], e[2]]).collect()
    }

    /// `true` when any entry is not fully opaque.
    pub fn has_alpha(&self) -> bool {
        self.entries.iter().any(|e| e[3] != 255)
    }
}

/// Decoded TIFF image in its native layout, as returned by
/// [`crate::decode`] and consumed by [`crate::encode`].
///
/// `planes` holds exactly one packed plane (every layout this crate
/// emits is packed); `color` / `metadata` are filled from the IFD's
/// colorimetry and metadata tags; `palette` is `Some` for `Pal8`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TiffImage {
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// Native pixel layout. Determines how many bytes per pixel the
    /// plane holds and how to interpret them.
    pub format: PixelFormat,
    /// Pixel planes — exactly one for TIFF.
    pub planes: Vec<Plane>,
    /// Colour signalling (range + H.273 code points).
    pub color: ColorInfo,
    /// ICC / Exif / XMP / gamma.
    pub metadata: Metadata,
    /// Colour table for `Pal8`.
    pub palette: Option<Palette>,
}

impl TiffImage {
    /// Assemble an image from its geometry, layout and planes (one for
    /// TIFF), validating the geometry: a non-zero width and height,
    /// exactly one plane, `stride ≥ width × bytes_per_pixel` and
    /// `data.len() ≥ stride × height`. Colour is
    /// [`ColorInfo::tiff_default`], metadata empty, no palette; the
    /// `with_*` builders fill those in.
    pub fn new(width: u32, height: u32, format: PixelFormat, planes: Vec<Plane>) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(TiffError::invalid("TiffImage: zero dimension"));
        }
        if planes.len() != 1 {
            return Err(TiffError::invalid(format!(
                "TiffImage: {} layouts are packed (exactly one plane), got {}",
                format_name(format),
                planes.len()
            )));
        }
        let p = &planes[0];
        let row = (width as usize)
            .checked_mul(format.bytes_per_pixel())
            .ok_or_else(|| TiffError::invalid("TiffImage: row size overflows"))?;
        if p.stride < row {
            return Err(TiffError::invalid(format!(
                "TiffImage: stride {} < {} bytes per row",
                p.stride, row
            )));
        }
        let need = p
            .stride
            .checked_mul(height as usize)
            .ok_or_else(|| TiffError::invalid("TiffImage: plane size overflows"))?;
        if p.data.len() < need {
            return Err(TiffError::invalid(format!(
                "TiffImage: plane holds {} bytes, {} needed",
                p.data.len(),
                need
            )));
        }
        Ok(Self::from_parts(width, height, format, planes))
    }

    /// Unchecked assembly used by the decoder, whose plane geometry is
    /// correct by construction.
    pub(crate) fn from_parts(
        width: u32,
        height: u32,
        format: PixelFormat,
        planes: Vec<Plane>,
    ) -> Self {
        Self {
            width,
            height,
            format,
            planes,
            color: ColorInfo::tiff_default(),
            metadata: Metadata::default(),
            palette: None,
        }
    }

    /// One packed plane with an explicit row stride, validated as by
    /// [`Self::new`].
    pub fn packed(
        width: u32,
        height: u32,
        format: PixelFormat,
        stride: usize,
        data: Vec<u8>,
    ) -> Result<Self> {
        Self::new(width, height, format, vec![Plane::new(stride, data)])
    }

    /// Tightly packed `Rgb24` from `3 × width × height` bytes (stride
    /// `3 × width`), validated as by [`Self::new`]: a zero dimension or
    /// a buffer shorter than the geometry is `Error::InvalidData`.
    pub fn from_rgb8(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        Self::from_tight(width, height, PixelFormat::Rgb24, data)
    }

    /// Tightly packed `Rgba` (straight alpha) from `4 × width × height`
    /// bytes (stride `4 × width`), validated as by [`Self::new`].
    pub fn from_rgba8(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        Self::from_tight(width, height, PixelFormat::Rgba, data)
    }

    fn from_tight(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Result<Self> {
        let stride = (width as usize)
            .checked_mul(format.bytes_per_pixel())
            .ok_or_else(|| TiffError::invalid("TiffImage: row size overflows"))?;
        Self::new(width, height, format, vec![Plane::new(stride, data)])
    }

    /// Set the colour signalling.
    pub fn with_color(mut self, color: ColorInfo) -> Self {
        self.color = color;
        self
    }

    /// Set the metadata.
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Set (or clear) the palette.
    pub fn with_palette(mut self, palette: impl Into<Option<Palette>>) -> Self {
        self.palette = palette.into();
        self
    }

    /// Image width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Native pixel layout.
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// Bytes per pixel of [`Self::format`].
    pub fn bytes_per_pixel(&self) -> usize {
        self.format.bytes_per_pixel()
    }

    /// Row stride in bytes of the pixel plane (`0` if the image has no
    /// plane).
    pub fn stride(&self) -> usize {
        self.planes.first().map(|p| p.stride).unwrap_or(0)
    }

    /// The pixel bytes — `Some` for every TIFF image that has its plane
    /// (TIFF layouts are all packed), `None` only for an image built
    /// without planes.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        self.planes.first().map(|p| p.data.as_slice())
    }

    /// Consume the image and return its plane bytes (planes
    /// concatenated in order, strides as reported).
    pub fn into_raw(self) -> Vec<u8> {
        let mut planes = self.planes.into_iter();
        let mut out = planes.next().map(|p| p.data).unwrap_or_default();
        for p in planes {
            out.extend_from_slice(&p.data);
        }
        out
    }

    /// `true` when the decoded pixels can be transparent: the `Rgba`
    /// layout or a non-opaque palette entry.
    pub fn has_alpha(&self) -> bool {
        self.format.has_alpha() || self.palette.as_ref().is_some_and(Palette::has_alpha)
    }

    /// Pixel bytes of the single plane (empty if none).
    pub(crate) fn data(&self) -> &[u8] {
        self.as_bytes().unwrap_or(&[])
    }

    /// Tightly packed 8-bit RGB, `3 × width` bytes per row.
    ///
    /// Exact integer kernels per layout (no gamma / colour management
    /// is applied — `color` and `metadata` describe the samples, they
    /// do not transform them):
    ///
    /// | Source     | RGB                                                       |
    /// |------------|-----------------------------------------------------------|
    /// | `Gray8`    | `(g, g, g)`                                               |
    /// | `Gray16Le` | high byte of the sample, replicated                       |
    /// | `Rgb24`    | copy                                                      |
    /// | `Rgb48Le`  | high byte per channel                                     |
    /// | `Rgba`     | alpha dropped (straight alpha, so the colour is unchanged) |
    /// | `Pal8`     | palette lookup; an index past the palette is black        |
    /// | `Cmyk`     | TIFF 6.0 §16 ink inversion: `R = (255−C)(255−K)/255`, `G = (255−M)(255−K)/255`, `B = (255−Y)(255−K)/255` (integer division) |
    ///
    /// 16-bit samples reduce by dropping the low-order byte — the same
    /// reduction the decoder applies to the 16-bit `ColorMap` words.
    /// Infallible: a plane shorter than its geometry claims reads as
    /// zero bytes past its end (the decoder never produces one;
    /// [`Self::new`] rejects one).
    pub fn to_rgb8(&self) -> Vec<u8> {
        let w = self.width as usize;
        let h = self.height as usize;
        let mut out = vec![0u8; w * h * 3];
        if w == 0 || h == 0 {
            return out;
        }
        let bpp = self.bytes_per_pixel();
        let stride = self.stride();
        let src = self.data();
        let lut = self.palette_lut();
        for y in 0..h {
            let row_off = y * stride;
            let drow = &mut out[y * w * 3..(y + 1) * w * 3];
            for x in 0..w {
                let s = row_off + x * bpp;
                let px = |i: usize| src.get(s + i).copied().unwrap_or(0);
                let d = &mut drow[x * 3..x * 3 + 3];
                match self.format {
                    PixelFormat::Gray8 => {
                        let g = px(0);
                        d.copy_from_slice(&[g, g, g]);
                    }
                    PixelFormat::Gray16Le => {
                        let g = px(1);
                        d.copy_from_slice(&[g, g, g]);
                    }
                    PixelFormat::Rgb24 | PixelFormat::Rgba => {
                        d.copy_from_slice(&[px(0), px(1), px(2)]);
                    }
                    PixelFormat::Rgb48Le => {
                        d.copy_from_slice(&[px(1), px(3), px(5)]);
                    }
                    PixelFormat::Pal8 => {
                        let e = lut[px(0) as usize];
                        d.copy_from_slice(&[e[0], e[1], e[2]]);
                    }
                    PixelFormat::Cmyk => {
                        let (c, m, yy, k) =
                            (px(0) as u32, px(1) as u32, px(2) as u32, px(3) as u32);
                        d.copy_from_slice(&[
                            ((255 - c) * (255 - k) / 255) as u8,
                            ((255 - m) * (255 - k) / 255) as u8,
                            ((255 - yy) * (255 - k) / 255) as u8,
                        ]);
                    }
                }
            }
        }
        out
    }

    /// Tightly packed 8-bit RGBA, `4 × width` bytes per row: the
    /// [`Self::to_rgb8`] colour with the source alpha for `Rgba`, the
    /// palette entry's alpha for `Pal8`, and `255` (opaque) for every
    /// other layout.
    pub fn to_rgba8(&self) -> Vec<u8> {
        let w = self.width as usize;
        let h = self.height as usize;
        let rgb = self.to_rgb8();
        let mut out = vec![0u8; w * h * 4];
        let bpp = self.bytes_per_pixel();
        let stride = self.stride();
        let src = self.data();
        let lut = self.palette_lut();
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let a = match self.format {
                    PixelFormat::Rgba => src.get(y * stride + x * bpp + 3).copied().unwrap_or(0),
                    PixelFormat::Pal8 => {
                        lut[src.get(y * stride + x).copied().unwrap_or(0) as usize][3]
                    }
                    _ => 255,
                };
                out[i * 4..i * 4 + 3].copy_from_slice(&rgb[i * 3..i * 3 + 3]);
                out[i * 4 + 3] = a;
            }
        }
        out
    }

    /// 256-entry RGBA lookup for `Pal8`: palette entries, then
    /// black-opaque for indices the palette does not cover (the
    /// decoder's historical rendering of an out-of-range index).
    fn palette_lut(&self) -> [[u8; 4]; 256] {
        let mut lut = [[0, 0, 0, 255u8]; 256];
        if let (PixelFormat::Pal8, Some(p)) = (self.format, &self.palette) {
            for (slot, e) in lut.iter_mut().zip(p.entries.iter()) {
                *slot = *e;
            }
        }
        lut
    }
}

fn format_name(f: PixelFormat) -> &'static str {
    match f {
        PixelFormat::Gray8 => "Gray8",
        PixelFormat::Gray16Le => "Gray16Le",
        PixelFormat::Rgb24 => "Rgb24",
        PixelFormat::Rgb48Le => "Rgb48Le",
        PixelFormat::Rgba => "Rgba",
        PixelFormat::Pal8 => "Pal8",
        PixelFormat::Cmyk => "Cmyk",
    }
}

/// Tightly packed 8-bit RGB (3 bytes per pixel, row-major), the
/// [`crate::decode_rgb8`] result.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RgbImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `width × height × 3` bytes.
    pub data: Vec<u8>,
}

impl RgbImage {
    /// Wrap a tightly packed `width × height × 3` RGB buffer.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    /// The pixel bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consume the image and return the pixel bytes.
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }

    /// Stride (bytes per row) — always `width × 3`.
    pub fn stride(&self) -> usize {
        self.width as usize * 3
    }
}

/// Tightly packed 8-bit RGBA (4 bytes per pixel, row-major), the
/// [`crate::decode_rgba8`] result.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RgbaImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `width × height × 4` bytes.
    pub data: Vec<u8>,
}

impl RgbaImage {
    /// Wrap a tightly packed `width × height × 4` RGBA buffer.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
        }
    }

    /// The pixel bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consume the image and return the pixel bytes.
    pub fn into_raw(self) -> Vec<u8> {
        self.data
    }

    /// Stride (bytes per row) — always `width × 4`.
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

/// One image of a multi-page TIFF, as returned by [`crate::decode_all`]
/// (one per IFD on the next-IFD chain, in file order).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Frame {
    /// The page's image.
    pub image: TiffImage,
    /// Display delay — TIFF pages are not timed, so always `None`.
    pub delay: Option<Duration>,
    /// Zero-based position of the IFD on the next-IFD chain.
    pub index: u32,
    /// `PageNumber` (tag 297) as `(page, total)` when the IFD carries
    /// it (`total == 0` = count unknown).
    pub page_number: Option<(u16, u16)>,
    /// `NewSubfileType` (tag 254) flag word when present: bit 0 =
    /// reduced-resolution image, bit 1 = one page of a multi-page
    /// image, bit 2 = transparency mask.
    pub new_subfile_type: Option<u32>,
}

impl Frame {
    /// Pair an image with its chain position; no delay, no page tags.
    pub fn new(image: TiffImage, index: u32) -> Self {
        Self {
            image,
            delay: None,
            index,
            page_number: None,
            new_subfile_type: None,
        }
    }

    /// Set the `PageNumber` pair.
    pub fn with_page_number(mut self, page_number: impl Into<Option<(u16, u16)>>) -> Self {
        self.page_number = page_number.into();
        self
    }

    /// Set the `NewSubfileType` flags.
    pub fn with_new_subfile_type(mut self, flags: impl Into<Option<u32>>) -> Self {
        self.new_subfile_type = flags.into();
        self
    }

    /// `true` when `NewSubfileType` bit 0 marks a reduced-resolution
    /// (thumbnail / pyramid) image.
    pub fn is_reduced_resolution(&self) -> bool {
        self.new_subfile_type.is_some_and(|f| f & 1 != 0)
    }

    /// `true` when `NewSubfileType` bit 2 marks a transparency mask.
    pub fn is_transparency_mask(&self) -> bool {
        self.new_subfile_type.is_some_and(|f| f & 4 != 0)
    }
}

/// One decoded IFD with everything the decoder learned about it — the
/// TIFF-specific depth behind [`crate::decode`]: the image, the
/// descriptive [`TiffMetadata`] (TIFF 6.0 §8 ASCII fields, resolution,
/// orientation, page tags, raw ICC / XMP) and the structural
/// [`TiffFormatInfo`] (photometric, compression, bit depth, layout).
/// Returned by [`crate::decode_page`] / [`crate::decode_page_at`] /
/// [`crate::decode_pages`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Page {
    /// The decoded image (native layout, colour + metadata filled).
    pub image: TiffImage,
    /// Descriptive + structural metadata of the same IFD.
    pub metadata: TiffMetadata,
    /// Raw on-disk layout tags of the same IFD.
    pub layout: TiffFormatInfo,
}

impl Page {
    /// Assemble a page from its parts.
    pub fn new(image: TiffImage, metadata: TiffMetadata, layout: TiffFormatInfo) -> Self {
        Self {
            image,
            metadata,
            layout,
        }
    }

    /// Image width in pixels.
    pub fn width(&self) -> u32 {
        self.image.width
    }

    /// Image height in pixels.
    pub fn height(&self) -> u32 {
        self.image.height
    }

    /// Native pixel layout of the image.
    pub fn format(&self) -> PixelFormat {
        self.image.format
    }
}

/// What [`crate::info`] learns from the header and the IFD chain
/// without decoding pixels.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ImageInfo {
    /// Width of the first image in pixels (after `Orientation`).
    pub width: u32,
    /// Height of the first image in pixels (after `Orientation`).
    pub height: u32,
    /// The layout [`crate::decode`] would return for the first image.
    pub format: PixelFormat,
    /// Number of images: the IFDs on the next-IFD chain.
    pub frames: u32,
    /// `true` when the first image's layout carries alpha (`Rgba`).
    pub has_alpha: bool,
    /// Colour signalling, resolved as [`crate::decode`] would.
    pub color: ColorInfo,
    /// An ICC profile (tag 34675) is present and well-formed.
    pub has_icc: bool,
    /// An Exif IFD (tag 34665) is present.
    pub has_exif: bool,
    /// An XMP packet (tag 700) is present.
    pub has_xmp: bool,
    /// `BitsPerSample` of the first sample as stored (1 / 4 / 8 / 12 /
    /// 16 / 32 / 64).
    pub bits_per_sample: u16,
    /// `SamplesPerPixel` as stored.
    pub samples_per_pixel: u16,
    /// `PhotometricInterpretation` (tag 262) raw code.
    pub photometric: u16,
    /// `Compression` (tag 259) raw code (1 = none, 5 = LZW, 7 = JPEG,
    /// 50000 = Zstandard, 50001 = WebP, …).
    pub compression: u16,
    /// `PlanarConfiguration = 2` (separate planes on disk).
    pub planar: bool,
    /// The image is tiled (`TileWidth` / `TileLength`) rather than
    /// stripped.
    pub tiled: bool,
    /// BigTIFF (version 43, 8-byte offsets).
    pub big_tiff: bool,
    /// `Orientation` (tag 274) as stored (1..=8; 1 when absent).
    pub orientation: u16,
}
