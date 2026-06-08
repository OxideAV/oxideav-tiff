//! TIFF 6.0 tag numbers, field-type ids, compression scheme ids,
//! photometric interpretations, and friends. Pure data, no logic.
//!
//! All numbers come from the *TIFF Revision 6.0* spec (Aldus, June
//! 1992): tags from Section 8 (Baseline Field Reference Guide), field
//! types from Section 2 (TIFF Structure / Types), compression from
//! Section 3 (Bilevel Images / Compression), photometric from
//! Section 3 (Bilevel Images / PhotometricInterpretation) plus
//! extensions in Sections 4..6.

// --- Byte-order header ---------------------------------------------------
pub const BYTE_ORDER_LE: u16 = 0x4949; // "II"
pub const BYTE_ORDER_BE: u16 = 0x4D4D; // "MM"
pub const TIFF_MAGIC: u16 = 42;
pub const BIGTIFF_MAGIC: u16 = 43;

// --- Field types (Section 2) --------------------------------------------
pub const TYPE_BYTE: u16 = 1;
pub const TYPE_ASCII: u16 = 2;
pub const TYPE_SHORT: u16 = 3;
pub const TYPE_LONG: u16 = 4;
pub const TYPE_RATIONAL: u16 = 5;
pub const TYPE_SBYTE: u16 = 6;
pub const TYPE_UNDEFINED: u16 = 7;
pub const TYPE_SSHORT: u16 = 8;
pub const TYPE_SLONG: u16 = 9;
pub const TYPE_SRATIONAL: u16 = 10;
pub const TYPE_FLOAT: u16 = 11;
pub const TYPE_DOUBLE: u16 = 12;
// BigTIFF additions (Adobe Pagemaker 6.0 → BigTIFF spec):
pub const TYPE_LONG8: u16 = 16;
pub const TYPE_SLONG8: u16 = 17;
pub const TYPE_IFD8: u16 = 18;

/// Size in bytes of a single value of the given field type. `0` for
/// unknown types (per spec, readers must skip those gracefully).
pub fn type_size(t: u16) -> u32 {
    match t {
        TYPE_BYTE | TYPE_ASCII | TYPE_SBYTE | TYPE_UNDEFINED => 1,
        TYPE_SHORT | TYPE_SSHORT => 2,
        TYPE_LONG | TYPE_SLONG | TYPE_FLOAT => 4,
        TYPE_RATIONAL | TYPE_SRATIONAL | TYPE_DOUBLE => 8,
        // BigTIFF: 64-bit unsigned/signed long + 64-bit IFD pointer.
        TYPE_LONG8 | TYPE_SLONG8 | TYPE_IFD8 => 8,
        _ => 0,
    }
}

// --- Baseline tags (Section 8) ------------------------------------------
pub const TAG_NEW_SUBFILE_TYPE: u16 = 254;
pub const TAG_IMAGE_WIDTH: u16 = 256;
pub const TAG_IMAGE_LENGTH: u16 = 257;
pub const TAG_BITS_PER_SAMPLE: u16 = 258;
pub const TAG_COMPRESSION: u16 = 259;
pub const TAG_PHOTOMETRIC_INTERPRETATION: u16 = 262;
pub const TAG_FILL_ORDER: u16 = 266;
pub const TAG_STRIP_OFFSETS: u16 = 273;
pub const TAG_ORIENTATION: u16 = 274;
pub const TAG_SAMPLES_PER_PIXEL: u16 = 277;
pub const TAG_ROWS_PER_STRIP: u16 = 278;
pub const TAG_STRIP_BYTE_COUNTS: u16 = 279;
pub const TAG_X_RESOLUTION: u16 = 282;
pub const TAG_Y_RESOLUTION: u16 = 283;
pub const TAG_PLANAR_CONFIGURATION: u16 = 284;
pub const TAG_RESOLUTION_UNIT: u16 = 296;
pub const TAG_SOFTWARE: u16 = 305;
pub const TAG_DATE_TIME: u16 = 306;
pub const TAG_PREDICTOR: u16 = 317;
pub const TAG_COLOR_MAP: u16 = 320;
pub const TAG_EXTRA_SAMPLES: u16 = 338;
pub const TAG_SAMPLE_FORMAT: u16 = 339;

// --- CMYK / separated-image tags (Section 16) ---------------------------
// TIFF 6.0 §16 "CMYK Images" page 70/71:
//   InkSet         (tag 332, SHORT, N = 1, default 1 / CMYK)
//   NumberOfInks   (tag 334, SHORT, N = 1, default 4)
// `InkSet = 1` is the only writer-side value this encoder emits — the
// canonical 4-ink CMYK ordering (cyan, magenta, yellow, black). Higher-
// fidelity "more than 4 inks" separations (§16 motivation paragraph)
// would carry `InkSet = 2` plus `InkNames` (tag 333); both are out of
// scope here.
pub const TAG_INK_SET: u16 = 332;
pub const TAG_NUMBER_OF_INKS: u16 = 334;
pub const INK_SET_CMYK: u16 = 1;

// --- Tile tags (Section 15) ---------------------------------------------
pub const TAG_TILE_WIDTH: u16 = 322;
pub const TAG_TILE_LENGTH: u16 = 323;
pub const TAG_TILE_OFFSETS: u16 = 324;
pub const TAG_TILE_BYTE_COUNTS: u16 = 325;

// --- CCITT tags (Section 11) --------------------------------------------
pub const TAG_T4_OPTIONS: u16 = 292;
pub const TAG_T6_OPTIONS: u16 = 293;

// --- JPEG-in-TIFF tag (TIFF Tech Note 2 §"JPEGTables field") ------------
//
// Tag 347, type UNDEFINED. Optional. When present, holds a JPEG
// "abbreviated table specification" datastream (SOI + DQT/DHT/DRI
// markers + EOI) whose tables apply by reference to every image
// segment (strip or tile) in the IFD. Each segment is itself a
// complete JPEG datastream (SOI..EOI), so the decoder synthesises a
// per-segment input by splicing the table bytes (between the tables
// stream's SOI and EOI) in front of the segment's image bytes
// (between the segment's SOI and EOI) and prepending one SOI + one
// EOI around the merged result.
pub const TAG_JPEG_TABLES: u16 = 347;

// T4Options bits (Section 11):
pub const T4OPT_2D_CODING: u32 = 1 << 0;
pub const T4OPT_UNCOMPRESSED: u32 = 1 << 1;
pub const T4OPT_EOL_BYTE_ALIGNED: u32 = 1 << 2;

// T6Options bits (Section 11). Only bit 1 ("uncompressed mode
// allowed") is defined; bit 0 is reserved.
pub const T6OPT_UNCOMPRESSED: u32 = 1 << 1;

// --- YCbCr tags (Section 21) --------------------------------------------
pub const TAG_YCBCR_COEFFICIENTS: u16 = 529;
pub const TAG_YCBCR_SUBSAMPLING: u16 = 530;
pub const TAG_YCBCR_POSITIONING: u16 = 531;
pub const TAG_REFERENCE_BLACK_WHITE: u16 = 532;

// --- Compression schemes (Section 3 + Section 13) -----------------------
pub const COMPRESSION_NONE: u16 = 1;
pub const COMPRESSION_CCITT_HUFFMAN: u16 = 2;
pub const COMPRESSION_CCITT_T4: u16 = 3;
pub const COMPRESSION_CCITT_T6: u16 = 4;
pub const COMPRESSION_LZW: u16 = 5;
pub const COMPRESSION_JPEG_OLD: u16 = 6;
pub const COMPRESSION_JPEG_NEW: u16 = 7;
pub const COMPRESSION_DEFLATE_ADOBE: u16 = 8;
pub const COMPRESSION_PACKBITS: u16 = 32773;

// --- Photometric interpretations (Section 3..6) -------------------------
pub const PHOTO_WHITE_IS_ZERO: u16 = 0;
pub const PHOTO_BLACK_IS_ZERO: u16 = 1;
pub const PHOTO_RGB: u16 = 2;
pub const PHOTO_PALETTE: u16 = 3;
pub const PHOTO_TRANSPARENCY_MASK: u16 = 4;
pub const PHOTO_CMYK: u16 = 5;
pub const PHOTO_YCBCR: u16 = 6;
pub const PHOTO_CIELAB: u16 = 8;

// --- Predictor (Section 14) ---------------------------------------------
pub const PREDICTOR_NONE: u16 = 1;
pub const PREDICTOR_HORIZONTAL: u16 = 2;

// --- SampleFormat (TIFF 6.0 §SampleFormat, tag 339 page 80) -------------
//
// TIFF 6.0 lists four sample interpretations:
//
//   1 = unsigned integer data    (default; what every Baseline TIFF carries)
//   2 = two's-complement signed integer data
//   3 = IEEE floating-point data
//   4 = undefined data format    (writer disclaims a known interpretation;
//                                 readers may treat as unsigned)
//
// The §SampleFormat description ends with: "If the SampleFormat field is
// present and the value is not 1, a Baseline TIFF reader that cannot
// handle the SampleFormat value must terminate the import process
// gracefully." This decoder supports only the unsigned-integer
// interpretation, so values 2 and 3 are rejected with a precise error
// rather than silently re-interpreting signed / floating-point samples as
// unsigned bytes / words (which would produce garbage pixels). Value 4 is
// folded into 1 per the §SampleFormat note: "A reader would typically
// treat an image with 'undefined' data as if the field were not present
// (i.e. as unsigned integer data)."
pub const SAMPLE_FORMAT_UINT: u16 = 1;
pub const SAMPLE_FORMAT_SINT: u16 = 2;
pub const SAMPLE_FORMAT_IEEE_FP: u16 = 3;
pub const SAMPLE_FORMAT_UNDEFINED: u16 = 4;

// --- Planar config (Section 7) ------------------------------------------
pub const PLANAR_CHUNKY: u16 = 1;
pub const PLANAR_SEPARATE: u16 = 2;

// --- NewSubfileType (TIFF 6.0 §NewSubfileType tag 254, page 36) ---------
//
// TIFF 6.0 defines NewSubfileType as "a general indication of the kind of
// data contained in this subfile". Tag 254, type LONG, N = 1, default 0.
// The 32-bit value is interpreted as a set of independent flag bits; the
// spec says "Unused bits are expected to be 0. Bit 0 is the low-order
// bit." Three bits are currently defined:
//
//   * Bit 0: 1 if the image is a reduced-resolution version of another
//            image in this TIFF file.
//   * Bit 1: 1 if the image is a single page of a multi-page image (see
//            the PageNumber field description).
//   * Bit 2: 1 if the image defines a transparency mask for another
//            image in this TIFF file. The PhotometricInterpretation
//            value MUST be 4, designating a transparency mask.
//
// The decoder validates two normative constraints when the tag is
// present: (a) the "Unused bits are expected to be 0" rule, by rejecting
// any high-bit set above bit 2 (defined-as-of-1992 ceiling); and (b) the
// bit-2 / Photometric = 4 invariant, since silently decoding a TRUE
// transparency mask whose Photometric was forgotten would produce
// composite-incorrect output downstream.
pub const NEW_SUBFILE_REDUCED_RESOLUTION: u32 = 1 << 0;
pub const NEW_SUBFILE_PAGE_OF_MULTIPAGE: u32 = 1 << 1;
pub const NEW_SUBFILE_TRANSPARENCY_MASK: u32 = 1 << 2;
/// Bitmask of all spec-defined NewSubfileType bits. Any high bit set
/// outside this mask is a spec violation per §NewSubfileType "Unused
/// bits are expected to be 0".
pub const NEW_SUBFILE_DEFINED_MASK: u32 =
    NEW_SUBFILE_REDUCED_RESOLUTION | NEW_SUBFILE_PAGE_OF_MULTIPAGE | NEW_SUBFILE_TRANSPARENCY_MASK;

/// Parsed NewSubfileType (TIFF 6.0 tag 254). Exposes the three
/// spec-defined flag bits via typed accessors; the raw `u32` is also
/// surfaced for callers that want to inspect undefined high bits
/// directly (e.g. for archival / metadata-preservation tooling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NewSubfileType {
    raw: u32,
}

impl NewSubfileType {
    /// The default-0 value (full-resolution single-image, no flags set).
    pub const ZERO: Self = Self { raw: 0 };

    /// Construct from the raw 32-bit value (no spec-validation; for
    /// internal use). Public callers should obtain instances through
    /// the decode-side accessor functions instead.
    pub const fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    /// The raw 32-bit value as stored in the IFD.
    pub const fn raw(self) -> u32 {
        self.raw
    }

    /// True if bit 0 is set (reduced-resolution version of another
    /// image in the same TIFF file).
    pub const fn is_reduced_resolution(self) -> bool {
        (self.raw & NEW_SUBFILE_REDUCED_RESOLUTION) != 0
    }

    /// True if bit 1 is set (single page of a multi-page image; see
    /// the §PageNumber field description).
    pub const fn is_page_of_multipage(self) -> bool {
        (self.raw & NEW_SUBFILE_PAGE_OF_MULTIPAGE) != 0
    }

    /// True if bit 2 is set (defines a transparency mask for another
    /// image in the same TIFF file; PhotometricInterpretation must
    /// be 4 — the decoder enforces that invariant).
    pub const fn is_transparency_mask(self) -> bool {
        (self.raw & NEW_SUBFILE_TRANSPARENCY_MASK) != 0
    }
}
