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

// --- Planar config (Section 7) ------------------------------------------
pub const PLANAR_CHUNKY: u16 = 1;
pub const PLANAR_SEPARATE: u16 = 2;
