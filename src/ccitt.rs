//! CCITT bilevel decompression for TIFF.
//!
//! This module implements all three CCITT compression schemes defined
//! by TIFF 6.0:
//!
//! * `Compression = 2`: Modified Huffman, TIFF 6.0 Section 10. A
//!   simple row-by-row 1-D run-length coder using the white/black
//!   terminating + make-up code tables borrowed verbatim from CCITT
//!   T.4. There are NO EOL codes; rows always begin on the next
//!   available byte boundary, and no fill bits are used except for
//!   ignored padding at the end of the last byte of a row.
//!
//! * `Compression = 3` with `T4Options` bit 0 cleared: T.4 1-D
//!   encoding, TIFF 6.0 Section 11. Same code tables as Modified
//!   Huffman, but each line is preceded by the 12-bit EOL code
//!   `000000000001`. With `T4Options` bit 2 set, the encoder pads
//!   the bit stream with zeros so each EOL ends on a byte boundary
//!   (`xxxx-0000 0000-0001`).
//!
//! * `Compression = 3` with `T4Options` bit 0 set: T.4 2-D
//!   (Modified READ / MR). Each EOL is followed by a one-bit
//!   "tag bit" — `1` for the next row being 1-D coded, `0` for 2-D
//!   coded. The 2-D rows use the Pass / Horizontal / Vertical mode
//!   codes (T.4 Table 4/T.4 = T.6 Table 1/T.6) against the previous
//!   decoded row as the reference. ITU-T T.4 §4.2 is the normative
//!   description, transcribed into
//!   `docs/image/tiff/ccitt-t4-t6-fax-codes.md` (§1, Table 4/T.4) for
//!   clean-room consumption here.
//!
//! * `Compression = 4`: T.6 (Modified Modified READ / MMR), TIFF 6.0
//!   Section 11 (deferred to ITU-T T.6). T.6 is "T.4 2-D, but only":
//!   every row is 2-D, there are no EOL codes, no 1-D fallback, no
//!   K parameter, and the first reference line is an imaginary
//!   all-white line. The on-wire trailer is an EOFB sentinel
//!   (`0000_0000_0001_0000_0000_0001` — two contiguous EOL codes)
//!   followed by zero-fill to the end of the strip. The clean-room
//!   transcription of the T.6 mode codes lives in the same
//!   `ccitt-t4-t6-fax-codes.md` file (§1 + §3 footnote — T.6 reuses
//!   the same mode codes as T.4 2-D, sans the 1-D extension).
//!
//! All run lengths and code words below are transcribed verbatim from
//! "TIFF Revision 6.0" (Aldus, June 1992), Section 10, Table 1/T.4
//! (Terminating codes) and Table 2/T.4 (Make-up codes), plus the
//! "Additional make-up codes" table on page 47. The white and black
//! columns are kept distinct because they really are distinct
//! prefixes: a black `00` is not the same code as a white `00` (one
//! exists, the other doesn't). The 2-D mode codes (Pass, Horizontal,
//! Vertical) come from `docs/image/tiff/ccitt-t4-t6-fax-codes.md` §1
//! (= Table 4/T.4 = Table 1/T.6).

use crate::error::{Result, TiffError as Error};

/// TIFF FillOrder (tag 266, §FillOrder, page 32 of the TIFF 6.0 PDF).
///
/// * `MsbFirst` (FillOrder=1, the baseline default): the high-order
///   bit of the first compression code is stored in the high-order
///   bit of byte 0, the next-highest bit in the next-highest bit, and
///   so on.
/// * `LsbFirst` (FillOrder=2): the same bit stream is captured into
///   storage with the bit transmission order reversed within each
///   octet — the first transmitted bit lands in the low-order bit of
///   byte 0, the next-transmitted in the next-lowest, etc. (Common
///   when CCITT data is captured straight off a Group 3 facsimile
///   serial link; see TIFF 6.0 §11 "Modified Huffman Compression /
///   FillOrder", pp. 50–51 of the PDF.)
///
/// Per spec page 32, FillOrder=2 is meaningful only when
/// `BitsPerSample = 1` and the data is either uncompressed or
/// compressed with CCITT 1-D / 2-D. The decoder rejects FillOrder=2
/// in any other context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillOrder {
    MsbFirst, // FillOrder = 1
    LsbFirst, // FillOrder = 2
}

/// Variant flag controlling row framing, mode selection (1-D vs 2-D)
/// and whether EOL codes are byte-aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcittVariant {
    /// Compression=2: no EOL codes; rows align to byte boundaries.
    ModifiedHuffman,
    /// Compression=3, T4Options bit 0 = 0: each row begins with the
    /// 12-bit EOL code (`000000000001`). Setting `eol_byte_aligned`
    /// reproduces the writer behaviour from T4Options bit 2 (pad
    /// with leading zeros so EOL ends on a byte boundary).
    T4OneD { eol_byte_aligned: bool },
    /// Compression=3, T4Options bit 0 = 1: T.4 2-D / Modified READ
    /// (MR). Each row is preceded by an EOL+tag-bit pair: tag=1
    /// selects a 1-D coded line for the next row, tag=0 selects a
    /// 2-D coded line referencing the previous decoded row. The
    /// `eol_byte_aligned` flag matches T4Options bit 2 as in
    /// `T4OneD`.
    T4TwoD { eol_byte_aligned: bool },
    /// Compression=4: T.6 / Modified Modified READ (MMR). Every row
    /// is 2-D, against the previous decoded row (the first reference
    /// line is an imaginary all-white line). No EOL codes between
    /// rows. The stream is followed by an end-of-fax-block (EOFB)
    /// sentinel — two contiguous EOL codes — but encoders sometimes
    /// emit it and sometimes don't; the decoder stops at `rows` lines
    /// regardless of what follows.
    T6,
}

/// Decode a CCITT-compressed strip / tile.
///
/// * `input` — the raw compressed bytes for the segment.
/// * `width` — the number of pixels per row (`ImageWidth` for a
///   strip, or `TileWidth` for a tile).
/// * `rows` — the number of rows in the segment (`min(RowsPerStrip,
///   ImageLength - rows_done)` for a strip, `TileLength` for a tile).
/// * `variant` — see [`CcittVariant`].
/// * `fill` — bit order on disk; only `FillOrder = 1` is implemented.
///
/// Returns a buffer of `rows * ceil(width / 8)` bytes containing the
/// raw bilevel pixel data, MSB-first within each byte, with the
/// CCITT convention that "white" is 0 and "black" is 1. The caller
/// is responsible for applying `PhotometricInterpretation` (per TIFF
/// 6.0 Sections 10 and 11, the *normal* case is `WhiteIsZero`, where
/// CCITT white and black map directly).
pub fn decode_ccitt(
    input: &[u8],
    width: u32,
    rows: u32,
    variant: CcittVariant,
    fill: FillOrder,
) -> Result<Vec<u8>> {
    // FillOrder=2 (LSB-first) just permutes the bit order inside each
    // byte; per TIFF 6.0 §FillOrder (page 32) the writer "can reverse
    // bit order by using a 256-byte lookup table". We do the same:
    // pre-reverse every input byte, then run the MSB-first parser
    // unchanged. This keeps the run-length tables and EOL scanner in
    // one canonical bit order.
    let owned: Vec<u8>;
    let stream: &[u8] = match fill {
        FillOrder::MsbFirst => input,
        FillOrder::LsbFirst => {
            owned = input.iter().map(|&b| reverse_bits_u8(b)).collect();
            &owned
        }
    };

    let row_bytes = (width as usize).div_ceil(8);
    let mut out = vec![0u8; row_bytes * rows as usize];
    let mut bits = BitReader::new(stream);

    // For 2-D variants the "reference line" is the previously decoded
    // line, packed in the same MSB-first 0=white / 1=black layout as
    // `out` rows. T.6 §2.2.1 stipulates that the first reference line
    // is "an imaginary all-white line"; T.4 §4.2 says the same once a
    // 2-D row appears at the start of a 2-D block. We initialise this
    // once and refresh it per decoded row.
    let mut reference: Vec<u8> = vec![0u8; row_bytes];

    for r in 0..rows {
        // Read & discard the row-leading EOL when the variant has
        // one. For byte-aligned EOLs (T4Options bit 2), skip enough
        // zero bits first that the next 12-bit code is the EOL and
        // ends on a byte boundary. The spec phrasing
        // "xxxx-0000 0000-0001" means: the trailing nibble of the
        // current byte is zeros, then a 0x00 byte, then an 0x01
        // byte; equivalently the next set bit must land in bit 0 of
        // a byte. We implement that by reading ahead: search for
        // the first 1-bit, verify it was preceded by ≥ 11 zero bits,
        // and that the 1-bit landed at bit position 7 (mod 8) =
        // last bit of a byte.
        //
        // For T.4 2-D the EOL is followed by a one-bit tag selecting
        // 1-D (tag=1) vs 2-D (tag=0) for the *next* row. T.6 has no
        // EOL framing at all.
        let row_is_two_d: bool = match variant {
            CcittVariant::ModifiedHuffman => false,
            CcittVariant::T4OneD { eol_byte_aligned } => {
                expect_eol(&mut bits, eol_byte_aligned)?;
                false
            }
            CcittVariant::T4TwoD { eol_byte_aligned } => {
                expect_eol(&mut bits, eol_byte_aligned)?;
                let tag = bits.read_bit().ok_or_else(|| {
                    Error::invalid("TIFF/CCITT: stream exhausted reading T.4-2D tag bit")
                })?;
                tag == 0
            }
            CcittVariant::T6 => true,
        };

        let row_off = (r as usize) * row_bytes;

        if row_is_two_d {
            // 2-D row: decode against `reference` using Pass /
            // Horizontal / Vertical mode codes.
            decode_two_d_row(
                &mut bits,
                &reference,
                &mut out[row_off..row_off + row_bytes],
                width as usize,
            )?;
        } else {
            // 1-D row (MH-style runs). Each row begins with a white
            // run by convention; if the line truly starts with a
            // black pixel, the encoder emits a zero-length white
            // run-length code word first (TIFF 6.0 §10 "Coding
            // Scheme").
            let mut x: usize = 0;
            let mut color_is_white = true;
            while x < width as usize {
                let run = read_run(&mut bits, color_is_white)?;
                if x + run > width as usize {
                    return Err(Error::invalid(format!(
                        "TIFF/CCITT: run {run} at column {x} overflows width {width}"
                    )));
                }
                if !color_is_white {
                    // Write `run` black pixels (bit=1) starting at x.
                    for k in 0..run {
                        let bit = x + k;
                        out[row_off + bit / 8] |= 1 << (7 - (bit % 8));
                    }
                }
                x += run;
                color_is_white = !color_is_white;
            }

            if x != width as usize {
                return Err(Error::invalid(format!(
                    "TIFF/CCITT: row {r} pixel count {x} != width {width}"
                )));
            }
        }

        // §10: "New rows always begin on the next available byte
        // boundary." This applies to Compression=2 only; for
        // Compression=3 / 4 the encoder doesn't byte-align between
        // rows (EOL for T.4, nothing for T.6).
        if matches!(variant, CcittVariant::ModifiedHuffman) {
            bits.align_to_byte();
        }

        // Refresh the reference line for the next iteration's 2-D
        // decode. We always copy (rather than swap) so MH and T.4 1-D
        // paths can ignore it.
        reference.copy_from_slice(&out[row_off..row_off + row_bytes]);
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Two-dimensional row decoder (T.4 2-D, T.6).
//
// The mode codes (Pass, Horizontal, Vertical) are transcribed in
// `docs/image/tiff/ccitt-t4-t6-fax-codes.md` §1 (= Table 4/T.4 =
// Table 1/T.6). The READ algorithm is the line-by-line walk described
// by T.4 §4.2 / T.6 §2.2.1: we maintain `a0` on the coding line
// (initially "just to the left of the first pixel", treated as an
// imaginary white element) and locate `b1` / `b2` on the reference
// line as needed.
// ---------------------------------------------------------------------------

/// Decode one 2-D coded row of length `width` pixels against
/// `reference` (the previous decoded row, same `row_bytes` layout)
/// and write the resulting bilevel row into `out` (MSB-first, 0 = white,
/// 1 = black).
fn decode_two_d_row(
    bits: &mut BitReader<'_>,
    reference: &[u8],
    out: &mut [u8],
    width: usize,
) -> Result<()> {
    // `a0` is the index of the next changing element to be coded.
    // T.4 §4.2.1.1 says "the position of a0 is just before the first
    // picture element of the coding line" — i.e. notionally at -1 of
    // an imaginary white pel; we represent that as a starting position
    // 0 with the colour-of-a0 being white. The algorithm then picks
    // a0 forward according to the mode code emitted by the encoder.
    let mut a0: usize = 0;
    let mut a0_is_white = true;

    while a0 < width {
        let mode = next_two_d_mode(bits)?;
        match mode {
            TwoDMode::Pass => {
                // Pass mode: a0 advances to the pixel below b2 (the
                // second next changing element on the reference line
                // after a0 in the direction we're scanning). No
                // pixels are written for the coded line during a Pass
                // — they're implicitly set to the colour of a0 in the
                // span [a0 .. b2].
                let b1 = first_change_after(reference, a0, !a0_is_white, width);
                let b2 = next_change_after(reference, b1, width);
                fill_run(out, a0, b2, a0_is_white);
                a0 = b2;
                // Pass mode does NOT flip a0's colour (T.4 §4.2.1.3.1).
            }
            TwoDMode::Horizontal => {
                // Horizontal mode: encoder sent two MH runs back to
                // back. The first run uses the colour of a0; the
                // second uses the opposite colour.
                let run1 = read_run(bits, a0_is_white)?;
                let run2 = read_run(bits, !a0_is_white)?;
                let end1 = a0
                    .checked_add(run1)
                    .ok_or_else(|| Error::invalid("TIFF/CCITT: H mode run1 overflows usize"))?;
                if end1 > width {
                    return Err(Error::invalid(format!(
                        "TIFF/CCITT: H mode run1 {run1} at a0={a0} overflows width {width}"
                    )));
                }
                fill_run(out, a0, end1, a0_is_white);
                let end2 = end1
                    .checked_add(run2)
                    .ok_or_else(|| Error::invalid("TIFF/CCITT: H mode run2 overflows usize"))?;
                if end2 > width {
                    return Err(Error::invalid(format!(
                        "TIFF/CCITT: H mode run2 {run2} at {end1} overflows width {width}"
                    )));
                }
                fill_run(out, end1, end2, !a0_is_white);
                a0 = end2;
                // Two flips: a0_is_white stays the same (two
                // colour-toggle runs returns it to its starting
                // colour).
            }
            TwoDMode::Vertical(offset) => {
                // Vertical mode V(n), VR(n), VL(n): a1 is positioned
                // `offset` pixels (signed) from b1. Pixels [a0, a1)
                // are coloured a0's colour; a1 then becomes the new
                // a0 and the colour flips.
                let b1 = first_change_after(reference, a0, !a0_is_white, width);
                let a1_signed = (b1 as isize) + offset;
                if a1_signed < a0 as isize || a1_signed > width as isize {
                    return Err(Error::invalid(format!(
                        "TIFF/CCITT: V({offset}) places a1={a1_signed} outside [a0={a0}, width={width}]"
                    )));
                }
                let a1 = a1_signed as usize;
                fill_run(out, a0, a1, a0_is_white);
                a0 = a1;
                a0_is_white = !a0_is_white;
            }
            TwoDMode::Uncompressed => {
                // T.4 Table 5 / T.6 §2.2.4 uncompressed mode is
                // optional and the docs flag it as out of scope for
                // this implementation. Reject explicitly.
                return Err(Error::invalid(
                    "TIFF/CCITT: 2-D uncompressed mode (extension `0000001111`) not supported",
                ));
            }
            TwoDMode::Extension1D => {
                // T.4 only (not T.6). The 1-D extension `000000001xxx`
                // is not normally seen in a 2-D row's mode-code
                // stream; treat as an error rather than silently
                // mis-decoding.
                return Err(Error::invalid(
                    "TIFF/CCITT: 1-D extension code `000000001xxx` inside a 2-D row",
                ));
            }
        }
    }

    if a0 != width {
        return Err(Error::invalid(format!(
            "TIFF/CCITT: 2-D row finished at a0={a0}, expected {width}"
        )));
    }
    Ok(())
}

/// 2-D mode codes, decoded form. The `Vertical` offset is in pixels,
/// signed relative to `b1`: V(0) = 0, VR(1..3) = +1..+3, VL(1..3) =
/// -1..-3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TwoDMode {
    Pass,
    Horizontal,
    Vertical(isize),
    Uncompressed,
    Extension1D,
}

/// Read one 2-D mode code from the bit stream. Code values are
/// transcribed from `docs/image/tiff/ccitt-t4-t6-fax-codes.md` §1
/// (Table 4/T.4 = Table 1/T.6).
fn next_two_d_mode(bits: &mut BitReader<'_>) -> Result<TwoDMode> {
    // Build the code bit by bit, longest = 10 bits ("Extension 2-D").
    // We accept the first prefix that matches, with explicit length
    // checks because the prefixes overlap on a flat bit walk (e.g.
    // V(0) = "1" is one bit long, so we must commit immediately when
    // we see it rather than read more bits speculatively).
    let mut acc: u32 = 0;
    for len in 1..=12u32 {
        let b = bits.read_bit().ok_or_else(|| {
            Error::invalid("TIFF/CCITT: bit stream exhausted reading 2-D mode code")
        })?;
        acc = (acc << 1) | (b as u32);
        // Sorted by length so longer prefixes get matched after
        // their shorter cousins have been ruled out by `acc`'s value.
        match (len, acc) {
            (1, 0b1) => return Ok(TwoDMode::Vertical(0)),
            (3, 0b001) => return Ok(TwoDMode::Horizontal),
            (3, 0b010) => return Ok(TwoDMode::Vertical(-1)),
            (3, 0b011) => return Ok(TwoDMode::Vertical(1)),
            (4, 0b0001) => return Ok(TwoDMode::Pass),
            (6, 0b000010) => return Ok(TwoDMode::Vertical(-2)),
            (6, 0b000011) => return Ok(TwoDMode::Vertical(2)),
            (7, 0b0000010) => return Ok(TwoDMode::Vertical(-3)),
            (7, 0b0000011) => return Ok(TwoDMode::Vertical(3)),
            // Extension 2-D code: `0000001xxx`. The `xxx` distinguishes
            // optional sub-extensions; per
            // `ccitt-t4-t6-fax-codes.md` §1 Note 2, `xxx = 111`
            // selects the optional uncompressed mode (Table 5/T.4).
            (10, v) if (v & 0b1111111000) == 0b0000001000 => {
                let xxx = (v & 0b111) as u8;
                if xxx == 0b111 {
                    return Ok(TwoDMode::Uncompressed);
                }
                return Err(Error::invalid(format!(
                    "TIFF/CCITT: unknown 2-D extension xxx={xxx:03b}"
                )));
            }
            // Extension 1-D code: `000000001xxx` (T.4 only, longer
            // prefix than the 2-D extension). 11 bits of zeros then
            // a 1.
            (12, v) if (v & 0b111111111000) == 0b000000001000 => {
                return Ok(TwoDMode::Extension1D);
            }
            _ => {}
        }
    }
    Err(Error::invalid(format!(
        "TIFF/CCITT: unknown 2-D mode code prefix {acc:012b}"
    )))
}

/// Locate the column of the first "changing element" on the
/// reference line that is strictly to the right of `from` and whose
/// colour is `target_white`. If none exists before `width`, return
/// `width` (T.4 §4.2.1.2: "If no such element exists, `b1` is assumed
/// to be at the position immediately to the right of the last
/// picture element of the line").
///
/// A "changing element" at column `k` is a pixel whose colour differs
/// from its left neighbour at column `k-1`; column 0 uses an
/// imaginary white left neighbour (so an all-black line starts with
/// a black changing element at column 0). The READ algorithm walks
/// the reference line left-to-right; this function is the canonical
/// "next changing element of colour C strictly right of position F"
/// query the algorithm needs.
fn first_change_after(reference: &[u8], from: usize, target_white: bool, width: usize) -> usize {
    // Walk forward starting at column `from + 1` (since b1 must be
    // strictly to the right of a0; T.4 §4.2.1.2 phrases this as the
    // FIRST changing element). Column 0 is special: a colour
    // transition at k=0 only counts if the imaginary-white left
    // context at column -1 differs from `ref[0]`, but since `from` is
    // a coding-line position rather than a reference-line one and our
    // caller only invokes us with `from < width`, we treat
    // `from + 1 .. width` as the search range and let the caller
    // handle the from == 0 / row-start case by passing from = 0
    // (which we then treat as "look for the first changing element
    // anywhere on the line, allowing column 0 if ref[0] is target").
    if from >= width {
        return width;
    }

    // Compute the colour at column `from`'s left neighbour. For the
    // start-of-row case (`from == 0`) this is the imaginary white
    // pel; otherwise it is the literal `ref[from - 1]`.
    let mut prev_white = if from == 0 {
        true
    } else {
        get_bit(reference, from - 1) == 0
    };

    // The spec's "strictly to the right of a0" means column > from
    // when from > 0. When from == 0, column 0 itself counts (the
    // imaginary white pel sits at column -1, so a transition at
    // column 0 — ref[0] differing from white — is a legitimate first
    // changing element). We start at `from + 1` and special-case
    // from == 0 by considering column 0 first.
    let mut k_start = from + 1;
    if from == 0 {
        // Consider column 0 against the imaginary white pel.
        let here_white = get_bit(reference, 0) == 0;
        if here_white != prev_white && here_white == target_white {
            return 0;
        }
        prev_white = here_white;
        k_start = 1;
    } else {
        // The column at `from` is part of the trailing run that
        // contains a0; b1 must be strictly past it. Update
        // `prev_white` to track the colour at column `from` so the
        // loop's transition detection is correct.
        if from < width {
            prev_white = get_bit(reference, from) == 0;
        }
    }

    for k in k_start..width {
        let here_white = get_bit(reference, k) == 0;
        if here_white != prev_white && here_white == target_white {
            return k;
        }
        prev_white = here_white;
    }
    width
}

/// Locate the index of the next change of colour on the reference
/// line strictly to the right of `from`, used to find b2 once b1 is
/// known. Returns `width` if no such transition exists.
fn next_change_after(reference: &[u8], from: usize, width: usize) -> usize {
    if from >= width {
        return width;
    }
    let here_white = get_bit(reference, from) == 0;
    let mut i = from + 1;
    while i < width && (get_bit(reference, i) == 0) == here_white {
        i += 1;
    }
    i
}

/// Read the bit at `idx` from an MSB-first packed bilevel buffer.
/// Returns 0 (white) or 1 (black).
#[inline]
fn get_bit(buf: &[u8], idx: usize) -> u8 {
    (buf[idx / 8] >> (7 - (idx % 8))) & 1
}

/// Fill `out[start..end]` with `is_white ? 0 : 1` at the MSB-first
/// bit-packed level. Out-of-bounds writes are caller's responsibility.
fn fill_run(out: &mut [u8], start: usize, end: usize, is_white: bool) {
    if is_white {
        // White is the default (zero) initialisation. Nothing to do
        // — `out` was zeroed by the caller.
        let _ = (start, end);
        return;
    }
    for bit in start..end {
        out[bit / 8] |= 1 << (7 - (bit % 8));
    }
}

/// Read one run length (a sequence of one or more make-up codes
/// followed by exactly one terminating code, per §10). `is_white`
/// picks the table.
fn read_run(bits: &mut BitReader<'_>, is_white: bool) -> Result<usize> {
    let mut total: usize = 0;
    loop {
        let (len, code) = next_code(bits, is_white)?;
        match code {
            Code::Terminating(n) => {
                total += n;
                return Ok(total);
            }
            Code::MakeUp(n) => {
                total += n;
                // Per §10: "Run lengths in the range of lengths
                // longer than or equal to 2624 pels are coded first
                // by the Make-up code of 2560. If the remaining
                // part of the run (after the first Make-up code of
                // 2560) is 2560 pels or greater, additional Make-up
                // code(s) of 2560 are issued until the remaining
                // part of the run becomes less than 2560 pels."
                // We just loop and keep consuming make-up codes
                // until a terminating code arrives. The spec is
                // explicit: "Each run-length is represented by zero
                // or more Make-up code words followed by exactly
                // one Terminating code word."
                // The `len` (unused below) is the bit length we
                // read off the wire; we keep accumulating.
                let _ = len;
            }
            Code::Eol => {
                return Err(Error::invalid("TIFF/CCITT: unexpected EOL inside a run"));
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Code {
    Terminating(usize),
    MakeUp(usize),
    Eol,
}

/// Read one variable-length code from `bits`, looking it up in the
/// table for the requested colour (white or black). Returns
/// `(bit_length, decoded_code)`.
///
/// We implement matching as a linear walk over the bit-length-sorted
/// table. The longest code is 13 bits; 26 colour-class entries times
/// 13 bits is well below any cache-line cost worth optimising. Trace
/// the bits one at a time, building up `acc`, and look the
/// (length, acc) pair up in the table; bail out when no entry of any
/// length matches.
fn next_code(bits: &mut BitReader<'_>, is_white: bool) -> Result<(u32, Code)> {
    let mut acc: u32 = 0;
    for len in 1..=13u32 {
        let b = bits
            .read_bit()
            .ok_or_else(|| Error::invalid("TIFF/CCITT: bit stream exhausted mid-code"))?;
        acc = (acc << 1) | (b as u32);
        // EOL is identifiable by 11 zero bits followed by a 1 — but
        // only after we've seen 12 bits total. Check it first so
        // that the all-zeros prefix in `acc` doesn't get confused
        // for any other code.
        if len == 12 && acc == 0b0000_0000_0001 {
            return Ok((12, Code::Eol));
        }
        let table = if is_white { WHITE } else { BLACK };
        for &(plen, pcode, run, is_makeup) in table {
            if plen == len && pcode == acc {
                let code = if is_makeup {
                    Code::MakeUp(run as usize)
                } else {
                    Code::Terminating(run as usize)
                };
                return Ok((len, code));
            }
        }
    }
    Err(Error::invalid(format!(
        "TIFF/CCITT: unknown {} code prefix {acc:013b}",
        if is_white { "white" } else { "black" }
    )))
}

/// Consume one EOL code from the stream. When `byte_aligned`, first
/// skip any leading zero bits required so the EOL's trailing `1`
/// lands at a byte boundary. Mirrors TIFF 6.0 Section 11 "T4Options
/// bit 2".
fn expect_eol(bits: &mut BitReader<'_>, byte_aligned: bool) -> Result<()> {
    // Search bit-by-bit for at least 11 zero bits followed by a 1.
    // (If `byte_aligned`, the trailing 1 must land at bit position
    // 7 mod 8, i.e. the last bit of a byte.)
    let mut zero_run: u32 = 0;
    loop {
        let b = bits
            .read_bit()
            .ok_or_else(|| Error::invalid("TIFF/CCITT: bit stream exhausted searching for EOL"))?;
        if b == 0 {
            zero_run += 1;
            if zero_run > 1024 {
                return Err(Error::invalid("TIFF/CCITT: runaway zero-bit run, no EOL"));
            }
        } else {
            // We've just read the trailing `1` of an EOL candidate.
            if zero_run < 11 {
                return Err(Error::invalid(format!(
                    "TIFF/CCITT: stray 1-bit after only {zero_run} zeros (expected EOL)"
                )));
            }
            if byte_aligned && bits.bit_pos % 8 != 0 {
                return Err(Error::invalid("TIFF/CCITT: byte-aligned EOL not aligned"));
            }
            return Ok(());
        }
    }
}

/// Reverse the bit order of a single byte: the byte that previously
/// had its low-order bit holding pixel 0 now has its high-order bit
/// holding pixel 0. Used by the FillOrder=2 → FillOrder=1 normaliser
/// in [`decode_ccitt`] and by the uncompressed-bilevel path in
/// `decoder.rs`. Equivalent to `u8::reverse_bits`; spelled out here
/// because the call site cares about the spec language ("256-byte
/// lookup table" from TIFF 6.0 §FillOrder).
#[inline]
pub fn reverse_bits_u8(b: u8) -> u8 {
    b.reverse_bits()
}

/// In-place LSB-first → MSB-first bit-order normalisation across a
/// whole buffer. Exposed so `decoder.rs` can normalise the raw
/// `Compression=None` bilevel strips/tiles before handing them to
/// `build_gray8_from_1bpp`, which assumes MSB-first byte layout
/// per spec page 32 ("FillOrder = 1 ... pixel 0 of a row is stored
/// in the high-order bit of byte 0").
pub fn reverse_bits_in_place(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = b.reverse_bits();
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Encode a bilevel raster into a CCITT-compressed byte stream.
///
/// `input` is the MSB-first, byte-packed bilevel buffer (1 = black,
/// 0 = white, the CCITT/TIFF convention). `width` is the number of
/// pixels per row; `rows` is the row count; `input` must therefore be
/// at least `rows * ceil(width / 8)` bytes long.
///
/// `variant` controls Compression=2 (Modified Huffman, no EOL codes,
/// rows byte-aligned per TIFF 6.0 §10) vs Compression=3 with bit 0
/// cleared (T.4 1-D, 12-bit EOL prefix per row, optional byte
/// alignment per T4Options bit 2 per §11).
///
/// `fill` controls whether the *returned* byte sequence is laid out
/// MSB-first (FillOrder=1, baseline default) or has every byte
/// bit-reversed for FillOrder=2 storage. The encoder builds the
/// canonical MSB-first stream then optionally reverses bytes in place
/// — matching exactly what the decoder's `FillOrder` normalisation
/// undoes on the read side.
pub fn encode_ccitt(
    input: &[u8],
    width: u32,
    rows: u32,
    variant: CcittVariant,
    fill: FillOrder,
) -> Result<Vec<u8>> {
    // 2-D encode (T.4 2-D and T.6) is out of scope for the present
    // implementation — only the decode path has been added against
    // the staged T.4/T.6 mode-code tables. Reject the variants
    // explicitly rather than silently falling through to the 1-D
    // path below.
    match variant {
        CcittVariant::ModifiedHuffman | CcittVariant::T4OneD { .. } => {}
        CcittVariant::T4TwoD { .. } => {
            return Err(Error::invalid(
                "TIFF/CCITT encode: T.4 2-D (Modified READ / MR) encode not implemented",
            ));
        }
        CcittVariant::T6 => {
            return Err(Error::invalid(
                "TIFF/CCITT encode: T.6 (MMR) encode not implemented",
            ));
        }
    }

    let row_bytes = (width as usize).div_ceil(8);
    let need = row_bytes * rows as usize;
    if input.len() < need {
        return Err(Error::invalid(format!(
            "TIFF/CCITT encode: input buffer is {} bytes, need {need} (rows={rows}, row_bytes={row_bytes})",
            input.len()
        )));
    }

    let mut bw = BitWriter::new();
    for r in 0..rows {
        // T.4 1-D: each row begins with the 12-bit EOL code, optionally
        // preceded by zero-fill bits so the trailing 1 lands on a
        // byte boundary (T4Options bit 2). MH has no EOL codes.
        if let CcittVariant::T4OneD { eol_byte_aligned } = variant {
            if eol_byte_aligned {
                // EOL: 12 bits, the trailing 1 in bit position 7 of a
                // byte. So the leading 11 zeros plus our zero-fill
                // pad must end at bit position 7 mod 8 = bit 7 of a
                // byte; equivalently, the 1-bit lands at byte_pos % 8
                // == 7, equivalently the bit BEFORE the leading 11
                // zeros lands at byte_pos % 8 == 4 — i.e. after
                // emitting the fill we want `bit_count % 8 == 4` so
                // that 11 zeros + 1 = 12 bits fills the rest of the
                // current byte and one more byte cleanly. Compute the
                // pad and emit it as zeros.
                let cur = bw.bit_count % 8;
                let pad = (4 + 8 - cur) % 8;
                for _ in 0..pad {
                    bw.write(0, 1);
                }
            }
            // 12-bit EOL = 000000000001.
            bw.write(0b0000_0000_0001, 12);
        }

        // Decompose the row into alternating white/black runs (white
        // first, per §10: "all data lines begin with a white
        // run-length code word set. If the actual scan line begins
        // with a black run, a white run-length of zero is sent").
        let row_off = (r as usize) * row_bytes;
        let runs = scan_runs(&input[row_off..row_off + row_bytes], width as usize);
        let mut is_white = true;
        for &run in &runs {
            emit_run(&mut bw, run, is_white);
            is_white = !is_white;
        }

        // §10: "New rows always begin on the next available byte
        // boundary." This is the row separator for Compression=2. For
        // Compression=3, the EOL code at the next row's start is the
        // separator — no inter-row alignment.
        if matches!(variant, CcittVariant::ModifiedHuffman) {
            bw.align();
        }
    }

    let mut out = bw.finish();
    if matches!(fill, FillOrder::LsbFirst) {
        reverse_bits_in_place(&mut out);
    }
    Ok(out)
}

/// Decompose one row of MSB-first packed bilevel bytes into a
/// sequence of run lengths. The first run is always the white run
/// (0-length if the row starts with a black pixel), then runs
/// alternate. Total of all runs == `width`.
fn scan_runs(row: &[u8], width: usize) -> Vec<usize> {
    let mut runs: Vec<usize> = Vec::new();
    let mut cur_color_is_white = true;
    let mut cur_len: usize = 0;
    for x in 0..width {
        let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
        let pixel_is_white = bit == 0;
        if pixel_is_white == cur_color_is_white {
            cur_len += 1;
        } else {
            runs.push(cur_len);
            cur_color_is_white = !cur_color_is_white;
            cur_len = 1;
        }
    }
    runs.push(cur_len);
    runs
}

/// Emit one run length into `bw` per TIFF 6.0 §10 "Coding Scheme":
///
/// * 0..=63 — one terminating code from the (white or black) column.
/// * 64..=2623 — make-up code for the largest multiple-of-64 not
///   greater than `run`, followed by the terminating code for the
///   remainder.
/// * 2624.. — repeated make-up code of 2560 until the remainder is
///   under 2560, then one final make-up + terminating pair as above.
fn emit_run(bw: &mut BitWriter, mut run: usize, is_white: bool) {
    // Drain ≥ 2624 with repeated 2560 make-up codes.
    while run >= 2624 {
        let (l, c) = lookup_makeup(2560, is_white).expect("makeup 2560 in table");
        bw.write(c, l);
        run -= 2560;
    }
    // Range [64, 2623] now (or under 64). Emit at most one make-up
    // code for the largest multiple-of-64 <= run.
    if run >= 64 {
        let bucket = (run / 64) * 64;
        let (l, c) = lookup_makeup(bucket as u32, is_white)
            .unwrap_or_else(|| panic!("missing make-up code for run bucket {bucket}"));
        bw.write(c, l);
        run -= bucket;
    }
    // 0..=63 left → exactly one terminating code.
    let (l, c) = lookup_terminating(run as u32, is_white)
        .unwrap_or_else(|| panic!("missing terminating code for run {run}"));
    bw.write(c, l);
}

/// Look up the terminating code for a run of length `run` in the
/// given colour table. Returns `(bit_length, code)` or `None` if no
/// terminator exists (only possible if the caller passes `run > 63`,
/// which is a bug).
fn lookup_terminating(run: u32, is_white: bool) -> Option<(u32, u32)> {
    let table = if is_white { WHITE } else { BLACK };
    for &(len, code, r, makeup) in table {
        if !makeup && r == run {
            return Some((len, code));
        }
    }
    None
}

/// Look up the make-up code for a run of length `run` in the given
/// colour table. The "additional make-up" entries 1792..=2560 live in
/// both tables (we duplicated them in each); §10 says they are shared
/// between white and black so this is correct.
fn lookup_makeup(run: u32, is_white: bool) -> Option<(u32, u32)> {
    let table = if is_white { WHITE } else { BLACK };
    for &(len, code, r, makeup) in table {
        if makeup && r == run {
            return Some((len, code));
        }
    }
    None
}

/// Bit writer used by the CCITT encoder. Bits are appended MSB-first
/// into a byte vector (FillOrder=1). `align` pads the current partial
/// byte to a boundary with zero bits, matching §10's "new rows always
/// begin on the next available byte boundary".
struct BitWriter {
    buf: Vec<u8>,
    bit_count: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            bit_count: 0,
        }
    }
    fn write(&mut self, code: u32, nbits: u32) {
        debug_assert!(nbits <= 32);
        for i in (0..nbits).rev() {
            let bit = ((code >> i) & 1) as u8;
            let byte_idx = self.bit_count / 8;
            if byte_idx >= self.buf.len() {
                self.buf.push(0);
            }
            let bit_in_byte = 7 - (self.bit_count % 8);
            self.buf[byte_idx] |= bit << bit_in_byte;
            self.bit_count += 1;
        }
    }
    fn align(&mut self) {
        let rem = self.bit_count % 8;
        if rem != 0 {
            self.bit_count += 8 - rem;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        // Pad up to byte boundary with zeros so the byte vector
        // length matches `(bit_count+7) / 8`.
        self.align();
        // Ensure the underlying buffer has exactly `bit_count / 8`
        // bytes (the writer may not have grown to a clean byte if the
        // last bit emitted was bit 0 of a fresh byte then we aligned).
        let want = self.bit_count / 8;
        if self.buf.len() < want {
            self.buf.resize(want, 0);
        } else {
            self.buf.truncate(want);
        }
        self.buf
    }
}

// ---------------------------------------------------------------------------
// Bit stream (MSB-first read; TIFF FillOrder = 1)
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    src: &'a [u8],
    bit_pos: usize, // index of the next bit to read (high bit of byte 0 = bit 0)
}

impl<'a> BitReader<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self { src, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let byte_idx = self.bit_pos / 8;
        if byte_idx >= self.src.len() {
            return None;
        }
        let bit_in_byte = 7 - (self.bit_pos % 8);
        let b = (self.src[byte_idx] >> bit_in_byte) & 1;
        self.bit_pos += 1;
        Some(b)
    }

    /// Advance to the next byte boundary, dropping the current
    /// partial byte's remaining bits.
    fn align_to_byte(&mut self) {
        let rem = self.bit_pos % 8;
        if rem != 0 {
            self.bit_pos += 8 - rem;
        }
    }
}

// ---------------------------------------------------------------------------
// Tables (TIFF 6.0 Section 10, Tables 1/T.4 + 2/T.4 + Additional make-up)
//
// Each row: (bit length, packed code, run length, is_makeup).
// Codes are stored right-aligned in a u32 (MSB of the code = bit
// (length - 1)). This matches what `next_code` builds bit-by-bit.
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const WHITE: &[(u32, u32, u32, bool)] = &[
    // --- Terminating codes (Table 1/T.4), white column ---
    (8, 0b00110101, 0, false),
    (6, 0b000111, 1, false),
    (4, 0b0111, 2, false),
    (4, 0b1000, 3, false),
    (4, 0b1011, 4, false),
    (4, 0b1100, 5, false),
    (4, 0b1110, 6, false),
    (4, 0b1111, 7, false),
    (5, 0b10011, 8, false),
    (5, 0b10100, 9, false),
    (5, 0b00111, 10, false),
    (5, 0b01000, 11, false),
    (6, 0b001000, 12, false),
    (6, 0b000011, 13, false),
    (6, 0b110100, 14, false),
    (6, 0b110101, 15, false),
    (6, 0b101010, 16, false),
    (6, 0b101011, 17, false),
    (7, 0b0100111, 18, false),
    (7, 0b0001100, 19, false),
    (7, 0b0001000, 20, false),
    (7, 0b0010111, 21, false),
    (7, 0b0000011, 22, false),
    (7, 0b0000100, 23, false),
    (7, 0b0101000, 24, false),
    (7, 0b0101011, 25, false),
    (7, 0b0010011, 26, false),
    (7, 0b0100100, 27, false),
    (7, 0b0011000, 28, false),
    (8, 0b00000010, 29, false),
    (8, 0b00000011, 30, false),
    (8, 0b00011010, 31, false),
    (8, 0b00011011, 32, false),
    (8, 0b00010010, 33, false),
    (8, 0b00010011, 34, false),
    (8, 0b00010100, 35, false),
    (8, 0b00010101, 36, false),
    (8, 0b00010110, 37, false),
    (8, 0b00010111, 38, false),
    (8, 0b00101000, 39, false),
    (8, 0b00101001, 40, false),
    (8, 0b00101010, 41, false),
    (8, 0b00101011, 42, false),
    (8, 0b00101100, 43, false),
    (8, 0b00101101, 44, false),
    (8, 0b00000100, 45, false),
    (8, 0b00000101, 46, false),
    (8, 0b00001010, 47, false),
    (8, 0b00001011, 48, false),
    (8, 0b01010010, 49, false),
    (8, 0b01010011, 50, false),
    (8, 0b01010100, 51, false),
    (8, 0b01010101, 52, false),
    (8, 0b00100100, 53, false),
    (8, 0b00100101, 54, false),
    (8, 0b01011000, 55, false),
    (8, 0b01011001, 56, false),
    (8, 0b01011010, 57, false),
    (8, 0b01011011, 58, false),
    (8, 0b01001010, 59, false),
    (8, 0b01001011, 60, false),
    (8, 0b00110010, 61, false),
    (8, 0b00110011, 62, false),
    (8, 0b00110100, 63, false),
    // --- Make-up codes (Table 2/T.4), white column ---
    (5, 0b11011, 64, true),
    (5, 0b10010, 128, true),
    (6, 0b010111, 192, true),
    (7, 0b0110111, 256, true),
    (8, 0b00110110, 320, true),
    (8, 0b00110111, 384, true),
    (8, 0b01100100, 448, true),
    (8, 0b01100101, 512, true),
    (8, 0b01101000, 576, true),
    (8, 0b01100111, 640, true),
    (9, 0b011001100, 704, true),
    (9, 0b011001101, 768, true),
    (9, 0b011010010, 832, true),
    (9, 0b011010011, 896, true),
    (9, 0b011010100, 960, true),
    (9, 0b011010101, 1024, true),
    (9, 0b011010110, 1088, true),
    (9, 0b011010111, 1152, true),
    (9, 0b011011000, 1216, true),
    (9, 0b011011001, 1280, true),
    (9, 0b011011010, 1344, true),
    (9, 0b011011011, 1408, true),
    (9, 0b010011000, 1472, true),
    (9, 0b010011001, 1536, true),
    (9, 0b010011010, 1600, true),
    (6, 0b011000, 1664, true),
    (9, 0b010011011, 1728, true),
    // --- Additional make-up codes (page 47 in the PDF), shared
    //     between white and black; we duplicate them in each table.
    (11, 0b00000001000, 1792, true),
    (11, 0b00000001100, 1856, true),
    (11, 0b00000001101, 1920, true),
    (12, 0b000000010010, 1984, true),
    (12, 0b000000010011, 2048, true),
    (12, 0b000000010100, 2112, true),
    (12, 0b000000010101, 2176, true),
    (12, 0b000000010110, 2240, true),
    (12, 0b000000010111, 2304, true),
    (12, 0b000000011100, 2368, true),
    (12, 0b000000011101, 2432, true),
    (12, 0b000000011110, 2496, true),
    (12, 0b000000011111, 2560, true),
];

#[rustfmt::skip]
const BLACK: &[(u32, u32, u32, bool)] = &[
    // --- Terminating codes (Table 1/T.4), black column ---
    (10, 0b0000110111, 0, false),
    (3, 0b010, 1, false),
    (2, 0b11, 2, false),
    (2, 0b10, 3, false),
    (3, 0b011, 4, false),
    (4, 0b0011, 5, false),
    (4, 0b0010, 6, false),
    (5, 0b00011, 7, false),
    (6, 0b000101, 8, false),
    (6, 0b000100, 9, false),
    (7, 0b0000100, 10, false),
    (7, 0b0000101, 11, false),
    (7, 0b0000111, 12, false),
    (8, 0b00000100, 13, false),
    (8, 0b00000111, 14, false),
    (9, 0b000011000, 15, false),
    (10, 0b0000010111, 16, false),
    (10, 0b0000011000, 17, false),
    (10, 0b0000001000, 18, false),
    (11, 0b00001100111, 19, false),
    (11, 0b00001101000, 20, false),
    (11, 0b00001101100, 21, false),
    (11, 0b00000110111, 22, false),
    (11, 0b00000101000, 23, false),
    (11, 0b00000010111, 24, false),
    (11, 0b00000011000, 25, false),
    (12, 0b000011001010, 26, false),
    (12, 0b000011001011, 27, false),
    (12, 0b000011001100, 28, false),
    (12, 0b000011001101, 29, false),
    (12, 0b000001101000, 30, false),
    (12, 0b000001101001, 31, false),
    (12, 0b000001101010, 32, false),
    (12, 0b000001101011, 33, false),
    (12, 0b000011010010, 34, false),
    (12, 0b000011010011, 35, false),
    (12, 0b000011010100, 36, false),
    (12, 0b000011010101, 37, false),
    (12, 0b000011010110, 38, false),
    (12, 0b000011010111, 39, false),
    (12, 0b000001101100, 40, false),
    (12, 0b000001101101, 41, false),
    (12, 0b000011011010, 42, false),
    (12, 0b000011011011, 43, false),
    (12, 0b000001010100, 44, false),
    (12, 0b000001010101, 45, false),
    (12, 0b000001010110, 46, false),
    (12, 0b000001010111, 47, false),
    (12, 0b000001100100, 48, false),
    (12, 0b000001100101, 49, false),
    (12, 0b000001010010, 50, false),
    (12, 0b000001010011, 51, false),
    (12, 0b000000100100, 52, false),
    (12, 0b000000110111, 53, false),
    (12, 0b000000111000, 54, false),
    (12, 0b000000100111, 55, false),
    (12, 0b000000101000, 56, false),
    (12, 0b000001011000, 57, false),
    (12, 0b000001011001, 58, false),
    (12, 0b000000101011, 59, false),
    (12, 0b000000101100, 60, false),
    (12, 0b000001011010, 61, false),
    (12, 0b000001100110, 62, false),
    (12, 0b000001100111, 63, false),
    // --- Make-up codes (Table 2/T.4), black column ---
    (10, 0b0000001111, 64, true),
    (12, 0b000011001000, 128, true),
    (12, 0b000011001001, 192, true),
    (12, 0b000001011011, 256, true),
    (12, 0b000000110011, 320, true),
    (12, 0b000000110100, 384, true),
    (12, 0b000000110101, 448, true),
    (13, 0b0000001101100, 512, true),
    (13, 0b0000001101101, 576, true),
    (13, 0b0000001001010, 640, true),
    (13, 0b0000001001011, 704, true),
    (13, 0b0000001001100, 768, true),
    (13, 0b0000001001101, 832, true),
    (13, 0b0000001110010, 896, true),
    (13, 0b0000001110011, 960, true),
    (13, 0b0000001110100, 1024, true),
    (13, 0b0000001110101, 1088, true),
    (13, 0b0000001110110, 1152, true),
    (13, 0b0000001110111, 1216, true),
    (13, 0b0000001010010, 1280, true),
    (13, 0b0000001010011, 1344, true),
    (13, 0b0000001010100, 1408, true),
    (13, 0b0000001010101, 1472, true),
    (13, 0b0000001011010, 1536, true),
    (13, 0b0000001011011, 1600, true),
    (13, 0b0000001100100, 1664, true),
    (13, 0b0000001100101, 1728, true),
    // --- Additional make-up codes (shared white+black). ---
    (11, 0b00000001000, 1792, true),
    (11, 0b00000001100, 1856, true),
    (11, 0b00000001101, 1920, true),
    (12, 0b000000010010, 1984, true),
    (12, 0b000000010011, 2048, true),
    (12, 0b000000010100, 2112, true),
    (12, 0b000000010101, 2176, true),
    (12, 0b000000010110, 2240, true),
    (12, 0b000000010111, 2304, true),
    (12, 0b000000011100, 2368, true),
    (12, 0b000000011101, 2432, true),
    (12, 0b000000011110, 2496, true),
    (12, 0b000000011111, 2560, true),
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Append `nbits` low-order bits of `code` to `buf` MSB-first.
    /// Used to synthesise CCITT-encoded bit streams in tests without
    /// reaching for the encoder side (which we don't ship yet).
    struct BitWriter {
        buf: Vec<u8>,
        bit_count: u32,
    }
    impl BitWriter {
        fn new() -> Self {
            Self {
                buf: Vec::new(),
                bit_count: 0,
            }
        }
        fn write(&mut self, code: u32, nbits: u32) {
            for i in (0..nbits).rev() {
                let bit = ((code >> i) & 1) as u8;
                let byte_idx = (self.bit_count / 8) as usize;
                if byte_idx >= self.buf.len() {
                    self.buf.push(0);
                }
                let bit_in_byte = 7 - (self.bit_count % 8);
                self.buf[byte_idx] |= bit << bit_in_byte;
                self.bit_count += 1;
            }
        }
        fn align(&mut self) {
            let rem = self.bit_count % 8;
            if rem != 0 {
                self.bit_count += 8 - rem;
            }
        }
        fn finish(mut self) -> Vec<u8> {
            // Pad up to byte boundary with zeros.
            self.align();
            self.buf
        }
    }

    #[test]
    fn mh_decodes_all_white_row() {
        // 8 pixels of white = white run length 8 = white-terminating
        // code "10011" (5 bits). With expected_len=1 byte, MH
        // aligns to the next byte after the row.
        let mut bw = BitWriter::new();
        bw.write(0b10011, 5);
        let bytes = bw.finish();
        let got = decode_ccitt(
            &bytes,
            8,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::MsbFirst,
        )
        .unwrap();
        assert_eq!(got, vec![0u8]);
    }

    #[test]
    fn mh_decodes_all_black_row() {
        // Each row starts with a white run; for an all-black row we
        // emit a zero-length white run first (white code "00110101"),
        // then a black-8 terminator ("000101", 6 bits).
        let mut bw = BitWriter::new();
        bw.write(0b00110101, 8);
        bw.write(0b000101, 6);
        let bytes = bw.finish();
        let got = decode_ccitt(
            &bytes,
            8,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::MsbFirst,
        )
        .unwrap();
        assert_eq!(got, vec![0xFFu8]);
    }

    #[test]
    fn mh_decodes_alternating_8x1() {
        // Pixel pattern: white, black, white, black, white, black,
        // white, black -> w0 b1 w1 b1 w1 b1 w1 b1 ... but the first
        // pixel IS white so we start with white-1.
        // Codes: white-1 ("000111", 6 bits), black-1 ("010", 3),
        // white-1, black-1, white-1, black-1, white-1, black-1.
        let mut bw = BitWriter::new();
        for _ in 0..4 {
            bw.write(0b000111, 6); // white 1
            bw.write(0b010, 3); // black 1
        }
        let bytes = bw.finish();
        let got = decode_ccitt(
            &bytes,
            8,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::MsbFirst,
        )
        .unwrap();
        // Expect 0b01010101 = 0x55.
        assert_eq!(got, vec![0x55u8]);
    }

    #[test]
    fn mh_makeup_plus_terminator_white_72() {
        // 72 = make-up 64 ("11011") + terminator 8 ("10011"). Then
        // the row is white-only so no further codes are needed.
        // Width 72, height 1: 9 bytes, all 0.
        let mut bw = BitWriter::new();
        bw.write(0b11011, 5); // make-up 64
        bw.write(0b10011, 5); // terminator 8
        let bytes = bw.finish();
        let got = decode_ccitt(
            &bytes,
            72,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::MsbFirst,
        )
        .unwrap();
        assert_eq!(got, vec![0u8; 9]);
    }

    #[test]
    fn t4_1d_decodes_eol_then_row() {
        // T.4 1D: each row preceded by 12-bit EOL.
        let mut bw = BitWriter::new();
        // Row 0: EOL + all-white 8.
        bw.write(0b000000000001, 12);
        bw.write(0b10011, 5); // white 8
        let bytes = bw.finish();
        let got = decode_ccitt(
            &bytes,
            8,
            1,
            CcittVariant::T4OneD {
                eol_byte_aligned: false,
            },
            FillOrder::MsbFirst,
        )
        .unwrap();
        assert_eq!(got, vec![0u8]);
    }

    #[test]
    fn t4_1d_decodes_two_rows() {
        // Two rows: row 0 = all white, row 1 = all black.
        let mut bw = BitWriter::new();
        bw.write(0b000000000001, 12); // EOL row 0
        bw.write(0b10011, 5); // white 8
        bw.write(0b000000000001, 12); // EOL row 1
        bw.write(0b00110101, 8); // white 0
        bw.write(0b000101, 6); // black 8
        let bytes = bw.finish();
        let got = decode_ccitt(
            &bytes,
            8,
            2,
            CcittVariant::T4OneD {
                eol_byte_aligned: false,
            },
            FillOrder::MsbFirst,
        )
        .unwrap();
        assert_eq!(got, vec![0u8, 0xFFu8]);
    }

    #[test]
    fn empty_input_errors_out_cleanly() {
        // An empty buffer must trigger the bit-stream-exhausted path
        // rather than panic or silently succeed.
        let r = decode_ccitt(
            &[],
            8,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::MsbFirst,
        );
        assert!(r.is_err());
    }

    #[test]
    fn reverse_bits_u8_matches_spec_examples() {
        // Spec-illustrative cases: 0x01 (low bit set) reversed must
        // produce 0x80 (high bit set), and vice-versa.
        assert_eq!(reverse_bits_u8(0x01), 0x80);
        assert_eq!(reverse_bits_u8(0x80), 0x01);
        assert_eq!(reverse_bits_u8(0x12), 0x48);
        assert_eq!(reverse_bits_u8(0xFF), 0xFF);
        assert_eq!(reverse_bits_u8(0x00), 0x00);
    }

    #[test]
    fn reverse_bits_in_place_round_trips() {
        let mut buf = [0xDE, 0xAD, 0xBE, 0xEFu8];
        let orig = buf;
        reverse_bits_in_place(&mut buf);
        // Applying it twice must reproduce the input.
        reverse_bits_in_place(&mut buf);
        assert_eq!(buf, orig);
    }

    #[test]
    fn mh_lsb_first_decodes_same_as_msb_first() {
        // Build an MSB-first stream for the "all-black 8-pixel row"
        // case (validated against tiffcp in the integration suite),
        // then mirror its bytes and verify the LSB-first decode path
        // produces identical pixels.
        let mut bw = BitWriter::new();
        bw.write(0b00110101, 8); // white 0
        bw.write(0b000101, 6); // black 8
        let msb_bytes = bw.finish();
        let lsb_bytes: Vec<u8> = msb_bytes.iter().map(|&b| reverse_bits_u8(b)).collect();

        let got_msb = decode_ccitt(
            &msb_bytes,
            8,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::MsbFirst,
        )
        .unwrap();
        let got_lsb = decode_ccitt(
            &lsb_bytes,
            8,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::LsbFirst,
        )
        .unwrap();
        assert_eq!(got_msb, vec![0xFFu8]);
        assert_eq!(got_lsb, got_msb);
    }

    #[test]
    fn t4_1d_lsb_first_decodes_same_as_msb_first() {
        let mut bw = BitWriter::new();
        bw.write(0b000000000001, 12); // EOL row 0
        bw.write(0b10011, 5); // white 8
        bw.write(0b000000000001, 12); // EOL row 1
        bw.write(0b00110101, 8); // white 0
        bw.write(0b000101, 6); // black 8
        let msb_bytes = bw.finish();
        let lsb_bytes: Vec<u8> = msb_bytes.iter().map(|&b| reverse_bits_u8(b)).collect();

        let got_msb = decode_ccitt(
            &msb_bytes,
            8,
            2,
            CcittVariant::T4OneD {
                eol_byte_aligned: false,
            },
            FillOrder::MsbFirst,
        )
        .unwrap();
        let got_lsb = decode_ccitt(
            &lsb_bytes,
            8,
            2,
            CcittVariant::T4OneD {
                eol_byte_aligned: false,
            },
            FillOrder::LsbFirst,
        )
        .unwrap();
        assert_eq!(got_msb, vec![0u8, 0xFFu8]);
        assert_eq!(got_lsb, got_msb);
    }

    // -----------------------------------------------------------------
    // Encoder tests — self-roundtrip through `encode_ccitt` +
    // `decode_ccitt` for the run-length spans the spec explicitly
    // calls out: 0..=63 (terminating-only), 64..=2623 (one make-up +
    // terminating), 2624+ (repeated 2560 make-up + terminating).
    // -----------------------------------------------------------------

    /// Pack a length-`width` sequence of pixel values (0=white,
    /// 1=black) into an MSB-first byte buffer matching the
    /// `EncodePixelFormat::Bilevel` layout.
    fn pack_row_msb(pixels: &[u8]) -> Vec<u8> {
        let row_bytes = pixels.len().div_ceil(8);
        let mut out = vec![0u8; row_bytes];
        for (i, &p) in pixels.iter().enumerate() {
            if p == 1 {
                out[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        out
    }

    /// Decompose an MSB-first byte buffer back into a per-pixel
    /// 0/1 vector. Inverse of `pack_row_msb`.
    fn unpack_row_msb(buf: &[u8], width: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width);
        for x in 0..width {
            let b = (buf[x / 8] >> (7 - (x % 8))) & 1;
            out.push(b);
        }
        out
    }

    fn ccitt_roundtrip(pixels: &[u8], width: u32, rows: u32, variant: CcittVariant) {
        let packed = pack_row_msb(pixels);
        let encoded = encode_ccitt(&packed, width, rows, variant, FillOrder::MsbFirst).unwrap();
        let decoded = decode_ccitt(&encoded, width, rows, variant, FillOrder::MsbFirst).unwrap();
        assert_eq!(packed, decoded, "round-trip mismatch (variant={variant:?})");
        // Sanity: cross-check the pixel-level expansion too.
        let back = unpack_row_msb(&decoded, (width * rows) as usize);
        assert_eq!(back, pixels);
    }

    #[test]
    fn encode_mh_all_white_8x1() {
        ccitt_roundtrip(&[0u8; 8], 8, 1, CcittVariant::ModifiedHuffman);
    }

    #[test]
    fn encode_mh_all_black_8x1() {
        ccitt_roundtrip(&[1u8; 8], 8, 1, CcittVariant::ModifiedHuffman);
    }

    #[test]
    fn encode_mh_alternating_8x1() {
        let row: Vec<u8> = (0..8).map(|i| (i % 2) as u8).collect();
        ccitt_roundtrip(&row, 8, 1, CcittVariant::ModifiedHuffman);
    }

    #[test]
    fn encode_mh_long_white_run_72x1() {
        // Hits the make-up code path (64 + terminating 8).
        ccitt_roundtrip(&[0u8; 72], 72, 1, CcittVariant::ModifiedHuffman);
    }

    #[test]
    fn encode_mh_long_black_run_72x1() {
        ccitt_roundtrip(&[1u8; 72], 72, 1, CcittVariant::ModifiedHuffman);
    }

    #[test]
    fn encode_mh_multi_row_aligns_between_rows() {
        // 3 rows of width 16, mix of patterns. Each row should end on
        // a byte boundary because MH aligns between rows.
        let r0 = vec![0u8; 16];
        let r1 = vec![1u8; 16];
        let r2: Vec<u8> = (0..16).map(|i| (i % 2) as u8).collect();
        let mut all = Vec::new();
        all.extend_from_slice(&r0);
        all.extend_from_slice(&r1);
        all.extend_from_slice(&r2);
        ccitt_roundtrip(&all, 16, 3, CcittVariant::ModifiedHuffman);
    }

    #[test]
    fn encode_mh_huge_white_run_5000x1() {
        // Exercises the 2624+ branch: ceil(5000/2560) = 2 sentinel
        // 2560 make-ups + remainder (5000 - 2*2560 = -120, but spec
        // says drop into "after the first 2560 if remainder is still
        // >= 2560 issue another 2560"). 5000 - 2560 = 2440; 2440 <
        // 2560 so just one 2560 then a (2432 make-up + 8
        // terminating). We don't need to verify the exact code count;
        // round-trip equality is enough.
        ccitt_roundtrip(&vec![0u8; 5000], 5000, 1, CcittVariant::ModifiedHuffman);
    }

    #[test]
    fn encode_t4_1d_emits_eol_per_row() {
        // Two rows; T.4 1-D should produce a stream that begins with
        // the EOL code (12 bits all-zeros + trailing 1 = 0x00 0x10
        // when MSB-packed from offset 0). Sanity-check the prefix
        // then round-trip.
        let pixels = vec![0u8; 16]; // 2 rows of 8 white pixels
        let packed = pack_row_msb(&pixels);
        let encoded = encode_ccitt(
            &packed,
            8,
            2,
            CcittVariant::T4OneD {
                eol_byte_aligned: false,
            },
            FillOrder::MsbFirst,
        )
        .unwrap();
        // The first 12 bits must be 0b0000_0000_0001. Packed
        // MSB-first that's bytes[0] == 0x00, bytes[1] high nibble ==
        // 0x10 -> bytes[1] & 0xF0 == 0x10.
        assert_eq!(encoded[0], 0x00, "first byte of T.4 1-D stream");
        assert_eq!(encoded[1] & 0xF0, 0x10, "EOL trailing 1 at bit 11");
        let decoded = decode_ccitt(
            &encoded,
            8,
            2,
            CcittVariant::T4OneD {
                eol_byte_aligned: false,
            },
            FillOrder::MsbFirst,
        )
        .unwrap();
        assert_eq!(decoded, packed);
    }

    #[test]
    fn encode_t4_1d_byte_aligned_eol_ends_on_byte_boundary() {
        // Three rows so the first byte-aligned EOL has work to do.
        // After encoding row 0 (which doesn't byte-align), the EOL
        // for row 1 needs leading zero-fill to pad until the trailing
        // 1 lands on a byte boundary.
        let r0: Vec<u8> = (0..16).map(|i| (i % 2) as u8).collect();
        let r1 = vec![0u8; 16];
        let r2 = vec![1u8; 16];
        let mut all = Vec::new();
        all.extend_from_slice(&r0);
        all.extend_from_slice(&r1);
        all.extend_from_slice(&r2);
        ccitt_roundtrip(
            &all,
            16,
            3,
            CcittVariant::T4OneD {
                eol_byte_aligned: true,
            },
        );
    }

    #[test]
    fn encode_ccitt_lsb_first_yields_bit_reversed_stream() {
        // The encoder's FillOrder parameter only flips the byte
        // layout of the final stream. Decoding the LSB-first output
        // with FillOrder::LsbFirst must reproduce the input
        // identically to the MSB-first round-trip.
        let pixels = vec![1u8; 16];
        let packed = pack_row_msb(&pixels);
        let msb_stream = encode_ccitt(
            &packed,
            16,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::MsbFirst,
        )
        .unwrap();
        let lsb_stream = encode_ccitt(
            &packed,
            16,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::LsbFirst,
        )
        .unwrap();
        let lsb_expected: Vec<u8> = msb_stream.iter().map(|&b| b.reverse_bits()).collect();
        assert_eq!(lsb_stream, lsb_expected);
        let decoded = decode_ccitt(
            &lsb_stream,
            16,
            1,
            CcittVariant::ModifiedHuffman,
            FillOrder::LsbFirst,
        )
        .unwrap();
        assert_eq!(decoded, packed);
    }

    #[test]
    fn encode_buffer_too_short_errors() {
        // Need 4 row_bytes for width=24, rows=2 (3 bytes/row). Give
        // only 5 bytes (3+2) and confirm the encoder refuses cleanly.
        let r = encode_ccitt(
            &[0u8; 5],
            24,
            2,
            CcittVariant::ModifiedHuffman,
            FillOrder::MsbFirst,
        );
        assert!(r.is_err());
    }

    // -----------------------------------------------------------------
    // 2-D mode-code reader tests — exercise the
    // `docs/image/tiff/ccitt-t4-t6-fax-codes.md` §1 table line by
    // line. Each test builds the exact bit sequence from the table
    // and asserts the decoded `TwoDMode`. No round-trip dependency.
    // -----------------------------------------------------------------

    fn read_two_d(bits: &[u8]) -> TwoDMode {
        let mut br = BitReader::new(bits);
        next_two_d_mode(&mut br).expect("decode 2-D mode")
    }

    #[test]
    fn two_d_vertical_0_decodes_one_bit() {
        // V(0) = "1".
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b1, 1);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Vertical(0));
    }

    #[test]
    fn two_d_horizontal_decodes_three_bits() {
        // H = "001".
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b001, 3);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Horizontal);
    }

    #[test]
    fn two_d_vr1_vl1_decode() {
        // VR(1) = "011", VL(1) = "010".
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b011, 3);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Vertical(1));
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b010, 3);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Vertical(-1));
    }

    #[test]
    fn two_d_pass_decodes_four_bits() {
        // Pass = "0001".
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b0001, 4);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Pass);
    }

    #[test]
    fn two_d_vr2_vl2_vr3_vl3_decode() {
        // VR(2) = "000011", VL(2) = "000010".
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b000011, 6);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Vertical(2));
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b000010, 6);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Vertical(-2));
        // VR(3) = "0000011", VL(3) = "0000010".
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b0000011, 7);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Vertical(3));
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b0000010, 7);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Vertical(-3));
    }

    #[test]
    fn two_d_uncompressed_extension_decodes() {
        // Extension 2-D with xxx=111 = "0000001111".
        let bw = {
            let mut bw = BitWriter::new();
            bw.write(0b0000001111, 10);
            bw.finish()
        };
        assert_eq!(read_two_d(&bw), TwoDMode::Uncompressed);
    }

    #[test]
    fn t6_all_white_row_decodes_via_v0() {
        // All-white 8-px T.6 row: V(0) against the imaginary all-white
        // reference line — encoder emits a single V(0) code ("1")
        // because b1 is at width and a1 = width matches.
        let mut bw = BitWriter::new();
        bw.write(0b1, 1); // V(0)
        let bytes = bw.finish();
        let got = decode_ccitt(&bytes, 8, 1, CcittVariant::T6, FillOrder::MsbFirst).unwrap();
        assert_eq!(got, vec![0u8]); // 8 white pixels.
    }

    #[test]
    fn t6_all_black_row_after_white_uses_horizontal() {
        // Two-row T.6: row 0 = all white, row 1 = all black.
        // Row 0: V(0) (b1 = width on imaginary-white ref).
        // Row 1: Horizontal — code "001" then white-0 then black-8.
        let mut bw = BitWriter::new();
        bw.write(0b1, 1); // V(0) for row 0
        bw.write(0b001, 3); // H for row 1
        bw.write(0b00110101, 8); // MH white 0
        bw.write(0b000101, 6); // MH black 8
        let bytes = bw.finish();
        let got = decode_ccitt(&bytes, 8, 2, CcittVariant::T6, FillOrder::MsbFirst).unwrap();
        assert_eq!(got, vec![0u8, 0xFFu8]);
    }

    #[test]
    fn t6_reject_uncompressed_extension() {
        // The decoder rejects uncompressed mode explicitly (docs flag
        // it as out of scope).
        let mut bw = BitWriter::new();
        bw.write(0b0000001111, 10); // Uncompressed-extension
        let bytes = bw.finish();
        let r = decode_ccitt(&bytes, 8, 1, CcittVariant::T6, FillOrder::MsbFirst);
        assert!(r.is_err());
        let msg = format!("{}", r.err().unwrap());
        assert!(msg.contains("uncompressed"), "got: {msg}");
    }

    #[test]
    fn first_change_after_handles_start_of_row() {
        // 8-pixel all-white reference, looking for the first black
        // changing element: there isn't one, so we expect width.
        let ref_line = [0u8]; // 8 white pixels.
        assert_eq!(first_change_after(&ref_line, 0, false, 8), 8);
        // Now looking for the first white changing element starting at
        // 0: since the imaginary-left pel is white and ref[0] is also
        // white, there's no transition into white. Expect width.
        assert_eq!(first_change_after(&ref_line, 0, true, 8), 8);
    }

    #[test]
    fn first_change_after_finds_mid_row_transition() {
        // Reference line: 4 white, 4 black (= 0b00001111 = 0x0F as
        // MSB-first one-byte = 0x0F).
        let ref_line = [0b00001111u8];
        // First black changing element from a0=0: that's column 4.
        assert_eq!(first_change_after(&ref_line, 0, false, 8), 4);
        // From a0=3 (still in the white run), b1 is still column 4.
        assert_eq!(first_change_after(&ref_line, 3, false, 8), 4);
        // From a0=4 (we're now on the black run), b1 must be strictly
        // greater than 4. Looking for the next BLACK transition —
        // there isn't one (no further runs), so we expect width.
        assert_eq!(first_change_after(&ref_line, 4, false, 8), 8);
        // From a0=4 looking for the next WHITE transition: there
        // isn't one either, expect width.
        assert_eq!(first_change_after(&ref_line, 4, true, 8), 8);
    }
}
