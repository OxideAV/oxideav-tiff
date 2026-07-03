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
/// IFD — a 4-byte child-IFD file offset (the 32-bit sibling of
/// [`TYPE_IFD8`]; type code 13 is a registered-identifier fact).
/// Value-shaped exactly like [`TYPE_LONG`]; readers treat it as an
/// unsigned 32-bit IFD offset.
pub const TYPE_IFD: u16 = 13;
pub const TYPE_LONG8: u16 = 16;
pub const TYPE_SLONG8: u16 = 17;
pub const TYPE_IFD8: u16 = 18;

/// Size in bytes of a single value of the given field type. `0` for
/// unknown types (per spec, readers must skip those gracefully).
pub fn type_size(t: u16) -> u32 {
    match t {
        TYPE_BYTE | TYPE_ASCII | TYPE_SBYTE | TYPE_UNDEFINED => 1,
        TYPE_SHORT | TYPE_SSHORT => 2,
        TYPE_LONG | TYPE_SLONG | TYPE_FLOAT | TYPE_IFD => 4,
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
/// PageNumber (TIFF 6.0 §"PageNumber", page 40): SHORT × 2 — the
/// 0-based page number and the total page count (0 = unknown total).
pub const TAG_PAGE_NUMBER: u16 = 297;
pub const TAG_SOFTWARE: u16 = 305;
pub const TAG_DATE_TIME: u16 = 306;
pub const TAG_PREDICTOR: u16 = 317;
pub const TAG_COLOR_MAP: u16 = 320;
pub const TAG_EXTRA_SAMPLES: u16 = 338;
pub const TAG_SAMPLE_FORMAT: u16 = 339;
// SMinSampleValue / SMaxSampleValue (TIFF 6.0 §SampleFormat page 80):
// "Type = the field type that best matches the sample data." For a
// SampleFormat = 3 image these carry the floating-point sample extent
// the decoder maps onto the display range; their default is "the full
// range of the data type."
pub const TAG_S_MIN_SAMPLE_VALUE: u16 = 340;
pub const TAG_S_MAX_SAMPLE_VALUE: u16 = 341;

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

// --- Old-style JPEG fields (TIFF 6.0 §22 "JPEG Compression") -------------
//
// TIFF 6.0 §22 (pages 95-109) defines `Compression = 6` together with
// nine auxiliary fields. TIFF Technical Note 2 (17-Mar-95) replaces
// the design with `Compression = 7` but states that "the 6.0 JPEG
// fields and tag values will remain reserved indefinitely" so readers
// can continue to read existing files — which is exactly what this
// crate does (see `jpeg_old`).
pub const TAG_JPEG_PROC: u16 = 512; // SHORT, N=1 — mandatory, no default
pub const TAG_JPEG_INTERCHANGE_FORMAT: u16 = 513; // LONG, N=1 — offset of SOI
pub const TAG_JPEG_INTERCHANGE_FORMAT_LENGTH: u16 = 514; // LONG, N=1 — bytes
pub const TAG_JPEG_RESTART_INTERVAL: u16 = 515; // SHORT, N=1 — MCUs per RSTn
pub const TAG_JPEG_LOSSLESS_PREDICTORS: u16 = 517; // SHORT, N=SamplesPerPixel
pub const TAG_JPEG_POINT_TRANSFORMS: u16 = 518; // SHORT, N=SamplesPerPixel
pub const TAG_JPEG_Q_TABLES: u16 = 519; // LONG, N=SamplesPerPixel — offsets
pub const TAG_JPEG_DC_TABLES: u16 = 520; // LONG, N=SamplesPerPixel — offsets
pub const TAG_JPEG_AC_TABLES: u16 = 521; // LONG, N=SamplesPerPixel — offsets

/// §22 "JPEGProc" (tag 512) value 1: "Baseline sequential process".
pub const JPEG_PROC_BASELINE: u16 = 1;
/// §22 "JPEGProc" (tag 512) value 14: "Lossless process with Huffman
/// coding". Per the spec, "values indicating JPEG processes other
/// than those specified above will be defined in the future."
pub const JPEG_PROC_LOSSLESS: u16 = 14;

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

// --- Child-IFD pointer tags ----------------------------------------------
// The child-IFD *mechanism* is plain TIFF 6.0 §2 IFD structure (an IFD
// at a file offset, reached through a LONG-typed entry). The tag
// numbers below are registered-identifier facts:
/// SubIFDs — array of child-IFD file offsets (one per child image,
/// e.g. reduced-resolution versions of the main image).
pub const TAG_SUB_IFDS: u16 = 330;
/// Exif IFD pointer — offset of a private IFD carrying Exif metadata
/// entries. This crate transports the child IFD's entries verbatim
/// (tag / type / count / value bytes); it does not interpret Exif
/// semantics.
pub const TAG_EXIF_IFD: u16 = 34665;
/// GPS IFD pointer — offset of a private IFD carrying GPS metadata
/// entries; transported verbatim exactly as for [`TAG_EXIF_IFD`].
pub const TAG_GPS_IFD: u16 = 34853;

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
/// Zstandard (RFC 8478). De-facto registry extension value — there is
/// no Adobe technical note for it; see the OxideAV trace doc
/// `docs/image/tiff/tiff-zstd-compression-50000.md`. Structurally it
/// follows the Compression=8 Deflate template: each strip / tile is
/// one self-contained Zstandard frame (magic `0x28 0xB5 0x2F 0xFD`)
/// over the post-predictor sample bytes, independent of photometric,
/// planar configuration, and bit depth. The encoder compression level
/// is an out-of-band runtime parameter, never stored in the file.
pub const COMPRESSION_ZSTD: u16 = 50000;

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
// Predictor = 3 — the IEEE floating-point predictor. Defined for 16-,
// 32- and 64-bit IEEE 754 samples only: each scan-line's sample bytes
// are split into per-significance byte planes (most-significant byte of
// every sample, then the next byte of every sample, …) and then run
// through horizontal byte differencing across the reordered stream.
// Decode reverses both steps (accumulate the byte differences, then
// re-interleave the planes back into per-sample bytes).
pub const PREDICTOR_FLOAT: u16 = 3;

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

// --- ExtraSamples (TIFF 6.0 §ExtraSamples, tag 338 pages 31-32) ----------
//
// "Specifies that each pixel has m extra components whose interpretation
// is defined by one of the values listed below. When this field is used,
// the SamplesPerPixel field has a value greater than the
// PhotometricInterpretation field suggests." SHORT, N = m (one value per
// extra sample). The three defined interpretations:
//
//   0 = Unspecified data
//   1 = Associated alpha data (with pre-multiplied color)
//   2 = Unassociated alpha data
//
// "By convention, extra components that are present must be stored as
// the 'last components' in each pixel," and "the size of such components
// is defined by the value of the BitsPerSample field." "The default is
// no extra samples. This field must be present if there are extra
// samples."
pub const EXTRA_SAMPLE_UNSPECIFIED: u16 = 0;
pub const EXTRA_SAMPLE_ASSOCIATED_ALPHA: u16 = 1;
pub const EXTRA_SAMPLE_UNASSOCIATED_ALPHA: u16 = 2;

// --- Planar config (Section 7) ------------------------------------------
pub const PLANAR_CHUNKY: u16 = 1;
pub const PLANAR_SEPARATE: u16 = 2;

// --- ResolutionUnit (Section 8 / "Physical Dimensions" page 18) ---------
//
// TIFF 6.0 §ResolutionUnit (tag 296, SHORT, N = 1) defines the unit of
// measurement that `XResolution` (tag 282) and `YResolution` (tag 283)
// are denominated in:
//
//   1 = No absolute unit of measurement. Used for images that may have a
//       non-square aspect ratio but no meaningful absolute dimensions.
//   2 = Inch.
//   3 = Centimeter.
//
// "Default = 2 (inch)." The decoder only inspects the value for spec
// conformance — the on-disk pixel bytes are unaffected by the resolution
// metadata — so the field is treated as a passive integrity check rather
// than a feature the decoder dispatches on.
pub const RESOLUTION_UNIT_NONE: u16 = 1;
pub const RESOLUTION_UNIT_INCH: u16 = 2;
pub const RESOLUTION_UNIT_CENTIMETER: u16 = 3;
