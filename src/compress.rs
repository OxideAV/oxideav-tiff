//! Strip decompression for the byte-stream compression schemes we
//! support: Compression=1 (None), Compression=32773 (PackBits),
//! Compression=5 (LZW), Compression=8 (Adobe Deflate / zlib), and
//! Compression=50000 (Zstandard, the de-facto registry extension —
//! see `docs/image/tiff/tiff-zstd-compression-50000.md`).
//!
//! All routines take the raw on-disk strip bytes and the expected
//! decompressed length (in bytes) and return the decompressed bytes.
//! Deflate and Zstandard defer to the `compcol` crate (the
//! workspace-wide compression collection); PackBits and LZW are
//! implemented here from the TIFF 6.0 spec text.

use crate::error::{Result, TiffError as Error};

/// Upper bound on the upfront `Vec::with_capacity` reservation made
/// for a single strip / tile. `expected_len` is derived from
/// attacker-supplied IFD fields (`ImageWidth * ImageLength *
/// SamplesPerPixel * BitsPerSample / 8`) and may be many gigabytes
/// for malformed input; capping the *initial* reservation here
/// prevents an attacker from forcing a multi-GiB allocation before
/// the decompressor has even consumed a single byte. The vector
/// grows naturally past the cap as the actual decompression
/// progresses, which is bounded by `input.len()` for PackBits / LZW
/// (the only schemes here whose output is loop-driven from a
/// per-iteration input read).
const MAX_INITIAL_RESERVE: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// PackBits — TIFF 6.0 Section 9
// ---------------------------------------------------------------------------

/// Decode PackBits per spec pseudocode:
///
/// ```text
/// Loop until you get the number of unpacked bytes you are expecting:
///   Read the next source byte into n.
///   If n is between 0 and 127 inclusive, copy the next n+1 bytes literally.
///   Else if n is between -127 and -1 inclusive, copy the next byte -n+1 times.
///   Else if n is -128, noop.
/// Endloop
/// ```
///
/// `expected_len` is a hint — we stop when we've produced that many
/// bytes OR when we've consumed all input, whichever comes first
/// (some encoders pad to even lengths).
pub fn unpack_packbits(input: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    // Bound the upfront reservation by the smaller of `expected_len`
    // (the legitimate maximum), `input.len() * 128 + 1` (PackBits's
    // worst-case 1-byte-header / 128-byte-replicate expansion), and
    // a hard `MAX_INITIAL_RESERVE`. The vector still grows naturally
    // up to `expected_len` as decompression proceeds; the cap only
    // affects how much memory is *pre-reserved* before we've seen
    // input back the claim.
    let reserve = expected_len
        .min(input.len().saturating_mul(128).saturating_add(1))
        .min(MAX_INITIAL_RESERVE);
    let mut out = Vec::with_capacity(reserve);
    let mut i = 0;
    while i < input.len() && out.len() < expected_len {
        let n = input[i] as i8;
        i += 1;
        if n >= 0 {
            // Literal run: next n+1 bytes copied as-is.
            let len = n as usize + 1;
            if i + len > input.len() {
                return Err(Error::invalid("TIFF/PackBits: literal run past EOF"));
            }
            out.extend_from_slice(&input[i..i + len]);
            i += len;
        } else if n == -128 {
            // No-op header byte. Skip.
            continue;
        } else {
            // Replicate run: next byte repeated -n+1 times.
            let count = (-(n as i32) + 1) as usize;
            if i >= input.len() {
                return Err(Error::invalid("TIFF/PackBits: replicate run past EOF"));
            }
            let b = input[i];
            i += 1;
            for _ in 0..count {
                out.push(b);
                if out.len() >= expected_len {
                    break;
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// LZW — TIFF 6.0 Section 13
// ---------------------------------------------------------------------------

const LZW_CLEAR_CODE: u16 = 256;
const LZW_EOI_CODE: u16 = 257;
const LZW_FIRST_CODE: u16 = 258;
const LZW_MAX_CODE: u16 = 4095; // 12-bit max

/// Decompress a TIFF LZW strip to the expected number of output bytes.
///
/// Per spec: the decompressor mirrors the compressor's table. The
/// table starts with 256 single-byte entries (codes 0..=255),
/// reserves code 256 for `ClearCode` (resets the table) and code 257
/// for `EndOfInformation`. The first multi-character entry is added
/// at table index 258. Codes start at 9 bits and grow to 10 / 11 /
/// 12 bits whenever the next-table-entry-to-be-added crosses
/// 511 / 1023 / 2047 (because of the well-known "bump one early"
/// quirk: switch to N+1-bit codes when the next code to write would
/// be entry 2^N - 1).
pub fn unpack_lzw(input: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    // Bound the upfront reservation: a hostile `expected_len` must not
    // size the buffer by itself.
    let mut out: Vec<u8> = Vec::with_capacity(expected_len.min(MAX_INITIAL_RESERVE));

    // The string table as spans of the output. The string of a code
    // `c >= 258` is `out[pos[c] .. pos[c] + len[c]]`: a new entry is
    // "the previous string + the first byte of the current one", and
    // the current string is written directly behind the previous one,
    // so the entry is the previous string's span grown by one byte.
    // Emitting a code is then one block copy instead of a walk along a
    // prefix chain. Codes below 256 are their own byte. A Clear only
    // resets `next_code`: entries at or above it are never read.
    const TABLE: usize = (LZW_MAX_CODE as usize) + 1;
    let mut pos = [0u32; TABLE];
    let mut len = [0u32; TABLE];
    let mut code_size: u32 = 9;
    let mut next_code: u16 = LZW_FIRST_CODE;
    // Span of the previously emitted string (None right after a Clear).
    let mut prev: Option<(u32, u32)> = None;

    // MSB-first bit reader over a 64-bit window.
    let mut acc: u64 = 0;
    let mut have: u32 = 0;
    let mut bytes = input.iter();
    loop {
        while have < code_size {
            match bytes.next() {
                Some(&b) => {
                    acc = (acc << 8) | b as u64;
                    have += 8;
                }
                // Stream exhausted before a full code: end of data.
                None => return Ok(out),
            }
        }
        have -= code_size;
        let code = ((acc >> have) & ((1u64 << code_size) - 1)) as u16;
        if code == LZW_EOI_CODE {
            break;
        }
        if code == LZW_CLEAR_CODE {
            code_size = 9;
            next_code = LZW_FIRST_CODE;
            prev = None;
            continue;
        }
        let start = out.len() as u32;
        let cur_len = match prev {
            None => {
                // First code after a Clear: must be a single byte —
                // nothing else is in the table yet.
                if code >= LZW_CLEAR_CODE {
                    return Err(Error::invalid(format!(
                        "TIFF/LZW: first code after Clear is {code}, must be a leaf (<256)"
                    )));
                }
                out.push(code as u8);
                1
            }
            Some((prev_pos, prev_len)) => {
                let cur_len = if code < LZW_CLEAR_CODE {
                    out.push(code as u8);
                    1
                } else if code < next_code {
                    let (p, l) = (pos[code as usize] as usize, len[code as usize] as usize);
                    out.extend_from_within(p..p + l);
                    l as u32
                } else if code == next_code {
                    // The code being defined right now: the previous
                    // string followed by its own first byte.
                    let (p, l) = (prev_pos as usize, prev_len as usize);
                    out.extend_from_within(p..p + l);
                    let first = out[p];
                    out.push(first);
                    prev_len + 1
                } else {
                    return Err(Error::invalid(format!(
                        "TIFF/LZW: code {code} > next_code {next_code}"
                    )));
                };
                if next_code <= LZW_MAX_CODE {
                    pos[next_code as usize] = prev_pos;
                    len[next_code as usize] = prev_len + 1;
                    next_code += 1;
                }
                cur_len
            }
        };
        prev = Some((start, cur_len));
        // TIFF section 13: the code width grows one code early — as
        // soon as table entry 2^N - 2 has been added, the next code is
        // read with N + 1 bits.
        if code_size < 12 && next_code >= (1u16 << code_size) - 1 {
            code_size += 1;
        }
        if out.len() >= expected_len {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Deflate — Adobe Compression=8 (zlib stream)
// ---------------------------------------------------------------------------

/// Inflate one Compression=8 strip / tile (a zlib stream).
///
/// Neither Deflate nor Zstandard bounds its own output — a 100-byte
/// stream can claim to expand to gigabytes — so the output is capped
/// at `expected_len`, the size the directory says the segment has, and
/// a stream that holds more is an error rather than an allocation. The
/// decoder bounds `expected_len` itself by `DecodeOptions::max_bytes`
/// (strips through the image geometry, tiles through the tile-size
/// gate), so the cap is as large as the page legitimately needs: a
/// single strip may be the whole image.
pub fn unpack_deflate(input: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let limit = expected_len as u64;
    compcol::vec::decompress_to_vec_capped::<compcol::zlib::Zlib>(input, limit)
        .map_err(|e| Error::invalid(format!("TIFF/Deflate: zlib inflate failed: {e:?}")))
}

// ---------------------------------------------------------------------------
// Zstandard — Compression=50000 (de-facto registry extension)
// ---------------------------------------------------------------------------

/// Decode one Compression=50000 strip / tile payload: a single
/// self-contained Zstandard frame (RFC 8478, magic `0x28B52FFD`
/// little-endian) over the post-predictor sample bytes. Per the trace
/// doc `docs/image/tiff/tiff-zstd-compression-50000.md` §3, the codec
/// is applied independently per strip / tile — no file-global stream,
/// no cross-strip dictionary — so a plain one-shot frame decode with
/// the same output cap as Deflate (see [`unpack_deflate`]) is the whole
/// job.
pub fn unpack_zstd(input: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let limit = expected_len as u64;
    compcol::vec::decompress_to_vec_capped::<compcol::zstd::Zstd>(input, limit)
        .map_err(|e| Error::invalid(format!("TIFF/ZSTD: Zstandard frame decode failed: {e:?}")))
}

// ---------------------------------------------------------------------------
// Encoders (used by the writer in `encoder.rs`)
// ---------------------------------------------------------------------------

/// PackBits-encode a strip / tile of `rows` scan-lines row by row —
/// TIFF 6.0 §9: "Pack each row separately. Do not compress across row
/// boundaries." Readers that decode a scan-line at a time depend on
/// it. `rows` is the luma row count; a chroma-subsampled YCbCr strip
/// holds one data-unit row per 2 or 4 of those, so the row length is
/// taken from the first of `rows`, `⌈rows / 2⌉`, `⌈rows / 4⌉` that
/// divides the buffer (splitting finer than the true row is still
/// row-aligned).
pub fn pack_packbits_rows(input: &[u8], rows: usize) -> Vec<u8> {
    let chunk = [rows, rows.div_ceil(2), rows.div_ceil(4)]
        .into_iter()
        .find(|&d| d > 0 && input.len().is_multiple_of(d))
        .map(|d| input.len() / d)
        .filter(|&c| c > 0)
        .unwrap_or(input.len().max(1));
    let mut out = Vec::with_capacity(input.len() / 2 + 16);
    for row in input.chunks(chunk) {
        out.extend_from_slice(&pack_packbits(row));
    }
    out
}

/// PackBits encode per TIFF 6.0 §9 pseudocode. Greedy: emit replicate
/// runs of length >=3, otherwise literal runs up to 128 bytes.
pub fn pack_packbits(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        // Find run length at i (max 128).
        let mut run = 1usize;
        while run < 128 && i + run < input.len() && input[i + run] == input[i] {
            run += 1;
        }
        if run >= 3 {
            out.push((-(run as i32 - 1)) as i8 as u8);
            out.push(input[i]);
            i += run;
        } else {
            // Literal: gather up to 128 bytes that don't start a
            // 3-byte+ replicate.
            let lit_start = i;
            let mut lit_end = i + 1;
            while lit_end - lit_start < 128 && lit_end < input.len() {
                let r = lit_end + 2 < input.len()
                    && input[lit_end] == input[lit_end + 1]
                    && input[lit_end + 1] == input[lit_end + 2];
                if r {
                    break;
                }
                lit_end += 1;
            }
            let n = lit_end - lit_start;
            out.push((n as i32 - 1) as u8);
            out.extend_from_slice(&input[lit_start..lit_end]);
            i = lit_end;
        }
    }
    out
}

/// Dictionary of the LZW encoder: `(prefix code, next byte) → code`.
///
/// An open-addressing table with linear probing over 2¹³ slots (the
/// table never holds more than 3838 strings, so it stays under half
/// full). Every slot carries the generation it was written in: a Clear
/// bumps the generation instead of wiping the table, and a slot from
/// an older generation reads as empty.
struct LzwDict {
    keys: Vec<u32>,
    codes: Vec<u16>,
    stamps: Vec<u32>,
    generation: u32,
}

impl LzwDict {
    const BITS: u32 = 13;
    const SLOTS: usize = 1 << Self::BITS;

    fn new() -> Self {
        Self {
            keys: vec![0; Self::SLOTS],
            codes: vec![0; Self::SLOTS],
            stamps: vec![0; Self::SLOTS],
            generation: 1,
        }
    }

    #[inline]
    fn key(prefix: u16, byte: u8) -> u32 {
        (prefix as u32) << 8 | byte as u32
    }

    /// Fibonacci hashing of the 20-bit key onto the slot index.
    #[inline]
    fn home(key: u32) -> usize {
        (key.wrapping_mul(0x9E37_79B1) >> (32 - Self::BITS)) as usize
    }

    #[inline]
    fn lookup(&self, prefix: u16, byte: u8) -> Option<u16> {
        let key = Self::key(prefix, byte);
        let mut i = Self::home(key);
        loop {
            if self.stamps[i] != self.generation {
                return None;
            }
            if self.keys[i] == key {
                return Some(self.codes[i]);
            }
            i = (i + 1) & (Self::SLOTS - 1);
        }
    }

    #[inline]
    fn insert(&mut self, prefix: u16, byte: u8, code: u16) {
        let key = Self::key(prefix, byte);
        let mut i = Self::home(key);
        while self.stamps[i] == self.generation {
            i = (i + 1) & (Self::SLOTS - 1);
        }
        self.stamps[i] = self.generation;
        self.keys[i] = key;
        self.codes[i] = code;
    }

    fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.stamps.fill(0);
            self.generation = 1;
        }
    }
}

/// LZW encode for TIFF (TIFF 6.0 §13). Builds the same code table
/// the decoder expects, writes codes MSB-first ("FillOrder=1, codes
/// stored high-to-low-order"), bumps code width "one early" exactly
/// as `unpack_lzw` does on the read side, and emits `ClearCode`
/// when the table fills.
///
/// The dictionary is a hashed table keyed by `(prefix code, next
/// byte)` (`LzwDict`): one probe per input byte, and a Clear costs a
/// counter bump. The bitstream is the same greedy longest-match stream
/// as before (same code-width bump points, same Clear-on-fill timing),
/// so the change is invisible to any decoder.
pub fn pack_lzw(input: &[u8]) -> Vec<u8> {
    let mut trie = LzwDict::new();
    let mut next_code: u16 = LZW_FIRST_CODE;
    let mut code_size: u32 = 9;

    let mut bit_buf: u64 = 0;
    let mut bit_count: u32 = 0;
    let mut out = Vec::new();
    let write_code = |c: u16, n: u32, bit_buf: &mut u64, bit_count: &mut u32, out: &mut Vec<u8>| {
        *bit_buf = (*bit_buf << n) | (c as u64);
        *bit_count += n;
        while *bit_count >= 8 {
            let shift = *bit_count - 8;
            let b = ((*bit_buf >> shift) & 0xFF) as u8;
            out.push(b);
            *bit_count -= 8;
            *bit_buf &= (1u64 << *bit_count) - 1;
        }
    };

    // Emit ClearCode first.
    write_code(
        LZW_CLEAR_CODE,
        code_size,
        &mut bit_buf,
        &mut bit_count,
        &mut out,
    );

    if input.is_empty() {
        write_code(
            LZW_EOI_CODE,
            code_size,
            &mut bit_buf,
            &mut bit_count,
            &mut out,
        );
    } else {
        let mut current: u16 = input[0] as u16;
        for &b in &input[1..] {
            if let Some(c) = trie.lookup(current, b) {
                current = c;
            } else {
                write_code(current, code_size, &mut bit_buf, &mut bit_count, &mut out);
                if next_code <= LZW_MAX_CODE {
                    trie.insert(current, b, next_code);
                    if code_size < 12 && next_code >= (1u16 << code_size) - 1 {
                        code_size += 1;
                    }
                    next_code += 1;
                } else {
                    write_code(
                        LZW_CLEAR_CODE,
                        code_size,
                        &mut bit_buf,
                        &mut bit_count,
                        &mut out,
                    );
                    trie.reset();
                    next_code = LZW_FIRST_CODE;
                    code_size = 9;
                }
                current = b as u16;
            }
        }
        write_code(current, code_size, &mut bit_buf, &mut bit_count, &mut out);
        write_code(
            LZW_EOI_CODE,
            code_size,
            &mut bit_buf,
            &mut bit_count,
            &mut out,
        );
    }
    if bit_count > 0 {
        let b = ((bit_buf << (8 - bit_count)) & 0xFF) as u8;
        out.push(b);
    }
    out
}

/// Deflate encode (zlib stream) via `compcol` at its default
/// compression level (6). Matches Compression=8 (Adobe Deflate).
pub fn pack_deflate(input: &[u8]) -> Result<Vec<u8>> {
    compcol::vec::compress_to_vec::<compcol::zlib::Zlib>(input)
        .map_err(|e| Error::invalid(format!("TIFF/Deflate: zlib deflate failed: {e:?}")))
}

/// Zstandard encode (one RFC 8478 frame) via `compcol` at its default
/// compression level. Matches Compression=50000. The level is an
/// encoder-side runtime parameter only — it is never stored in the
/// TIFF file and the decoder is level-agnostic (trace doc §2) — so no
/// knob is exposed here.
/// [`pack_deflate`] at an explicit zlib level (`0..=9`, clamped).
pub fn pack_deflate_level(input: &[u8], level: u8) -> Result<Vec<u8>> {
    let config = compcol::zlib::EncoderConfig {
        level: level.min(9),
    };
    compcol::vec::compress_to_vec_with::<compcol::zlib::Zlib>(input, config)
        .map_err(|e| Error::invalid(format!("TIFF/Deflate: zlib deflate failed: {e:?}")))
}

/// [`pack_zstd`] at an explicit Zstandard level (`1..=22`, clamped).
pub fn pack_zstd_level(input: &[u8], level: u8) -> Result<Vec<u8>> {
    let config = compcol::zstd::EncoderConfig {
        level: level.clamp(1, 22) as i32,
    };
    compcol::vec::compress_to_vec_with::<compcol::zstd::Zstd>(input, config)
        .map_err(|e| Error::invalid(format!("TIFF/ZSTD: Zstandard frame encode failed: {e:?}")))
}

pub fn pack_zstd(input: &[u8]) -> Result<Vec<u8>> {
    compcol::vec::compress_to_vec::<compcol::zstd::Zstd>(input)
        .map_err(|e| Error::invalid(format!("TIFF/ZSTD: Zstandard frame encode failed: {e:?}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packbits_roundtrip_replicates() {
        let src = vec![0xAA; 200]; // 200 bytes of one value
        let p = pack_packbits(&src);
        let back = unpack_packbits(&p, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn packbits_roundtrip_literals() {
        let src: Vec<u8> = (0..50).collect();
        let p = pack_packbits(&src);
        let back = unpack_packbits(&p, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn packbits_mixed() {
        let mut src = vec![0u8; 10];
        src.extend_from_slice(&[1, 2, 3, 4, 5]);
        src.extend(std::iter::repeat_n(0xFF, 20));
        let p = pack_packbits(&src);
        let back = unpack_packbits(&p, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn packbits_noop_byte_skipped() {
        // 0x80 is the no-op marker. Build an input that includes
        // one and confirm decode skips it.
        let input = vec![0x80, 0x00, b'A']; // noop, then 1-byte literal 'A'
        let out = unpack_packbits(&input, 1).unwrap();
        assert_eq!(out, b"A");
    }

    #[test]
    fn lzw_roundtrip_short() {
        let src = b"TOBEORNOTTOBEORTOBEORNOT".to_vec();
        let encoded = pack_lzw(&src);
        let back = unpack_lzw(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn lzw_roundtrip_zeros() {
        // Pure zero strip — exercises long replicate-style patterns.
        let src = vec![0u8; 4096];
        let encoded = pack_lzw(&src);
        let back = unpack_lzw(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn lzw_rejects_first_code_above_leaf_range() {
        // Fuzz reproducer (r126): a code >= 256 emitted before any
        // dictionary entries have been added would index into an
        // uninitialised table slot, then form a self-referential
        // prefix chain on the next iteration and spin the emit loop
        // forever. The decoder must reject first-after-Clear codes
        // outside the 0..=255 leaf range up front.
        //
        // Encoding: nine MSB-first bits = `100000010` = 258 (=
        // LZW_FIRST_CODE) → followed by enough bits to keep
        // `bits.read(9)` happy. `[0x81, 0x00]` is exactly 16 bits
        // of which the first 9 read as 258.
        let err = unpack_lzw(&[0x81, 0x00, 0x00], 64).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("first code after Clear"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn lzw_initial_reserve_is_bounded() {
        // A massive `expected_len` claim must not force a
        // multi-gibibyte upfront allocation; the cap defined in
        // `MAX_INITIAL_RESERVE` keeps the reserve sane while
        // letting the vector grow naturally as decompression
        // progresses. We test this by passing an absurd
        // `expected_len` on an empty input and asserting we still
        // get back a valid (empty) result.
        let out = unpack_lzw(&[], usize::MAX / 2).unwrap();
        assert!(out.is_empty());
        let out = unpack_packbits(&[], usize::MAX / 2).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn deflate_output_is_bounded() {
        // The raw zlib wire format has no built-in bound on output
        // size; `unpack_deflate` caps at the caller's `expected_len`.
        // Encode a small zlib stream and confirm decode succeeds
        // within the cap — the bound-violation half of the contract is
        // exercised explicitly below (`deflate_cap_exceeded_errors`)
        // and by the fuzz target.
        let src = vec![0u8; 1024];
        let encoded = pack_deflate(&src).unwrap();
        let back = unpack_deflate(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn deflate_cap_exceeded_errors() {
        // A stream that inflates past the IFD-claimed expected_len
        // must surface a decode error, not silently truncate or
        // OOM. 4096 zero bytes deflate to a few dozen bytes; claim
        // the strip should only have been 16.
        let src = vec![0u8; 4096];
        let encoded = pack_deflate(&src).unwrap();
        assert!(unpack_deflate(&encoded, 16).is_err());
    }

    #[test]
    fn zstd_roundtrip_zeros() {
        // Long single-byte run — the frame's RLE_Block sweet spot.
        let src = vec![0u8; 4096];
        let encoded = pack_zstd(&src).unwrap();
        // One self-contained frame per strip: RFC 8478 magic first.
        assert_eq!(&encoded[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        let back = unpack_zstd(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn zstd_roundtrip_random_8k() {
        // Pseudo-random bytes (xorshift) — mostly incompressible,
        // exercises the raw/compressed block fallback choices.
        let mut s: u32 = 0x2545_F491;
        let src: Vec<u8> = (0..8192)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as u8
            })
            .collect();
        let encoded = pack_zstd(&src).unwrap();
        let back = unpack_zstd(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn zstd_roundtrip_textlike() {
        // Repetitive structured payload — LZ77 matches + Huffman
        // literals, the typical post-predictor image-row shape.
        let motif = b"row of sample bytes 01234567 ";
        let mut src = Vec::with_capacity(motif.len() * 512);
        for _ in 0..512 {
            src.extend_from_slice(motif);
        }
        let encoded = pack_zstd(&src).unwrap();
        let back = unpack_zstd(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn zstd_cap_exceeded_errors() {
        // Same zip-bomb contract as Deflate: a frame expanding past
        // the IFD-claimed expected_len is a decode error.
        let src = vec![0u8; 4096];
        let encoded = pack_zstd(&src).unwrap();
        assert!(unpack_zstd(&encoded, 16).is_err());
    }

    #[test]
    fn zstd_garbage_input_errors() {
        // Not a Zstandard frame at all (wrong magic) — must error,
        // not panic.
        assert!(unpack_zstd(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00], 64).is_err());
        // Truncated: valid magic, nothing after it.
        assert!(unpack_zstd(&[0x28, 0xB5, 0x2F, 0xFD], 64).is_err());
        // Empty payload.
        assert!(unpack_zstd(&[], 64).is_err());
    }

    #[test]
    fn lzw_roundtrip_random_8k() {
        // Pseudo-random bytes (xorshift) — exercises code-size
        // bumps + KwKwK occasionally.
        let mut s: u32 = 0x9E37_79B9;
        let src: Vec<u8> = (0..8192)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as u8
            })
            .collect();
        let encoded = pack_lzw(&src);
        let back = unpack_lzw(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn lzw_roundtrip_table_fill_clear() {
        // Force the encoder past `LZW_MAX_CODE = 4095` distinct
        // dictionary entries so the Clear-on-fill path runs at least
        // once. A 64 KiB strip of slowly-shifted xorshift output
        // produces well over 4096 unique short prefixes; r129's trie
        // resets via `reset()` (zeroing `first_child`) and the test
        // confirms the decoder reads the resulting bitstream back
        // bit-exact.
        let mut s: u32 = 0xC0FF_EE17;
        let src: Vec<u8> = (0..(64 * 1024))
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as u8
            })
            .collect();
        let encoded = pack_lzw(&src);
        let back = unpack_lzw(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn lzw_roundtrip_byte_pattern_repeated() {
        // Tightly-repeating 16-byte motif — the inverse of a
        // pseudo-random fixture: long dictionary strings, many Clears.
        // A roundtrip-correctness check; the bench file exercises
        // throughput.
        let motif = b"abcdefghijklmnop";
        let mut src = Vec::with_capacity(motif.len() * 2048);
        for _ in 0..2048 {
            src.extend_from_slice(motif);
        }
        let encoded = pack_lzw(&src);
        let back = unpack_lzw(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    #[test]
    fn lzw_dictionary_lookup_insert_and_reset() {
        // Direct unit test of the encoder dictionary: insert two
        // strings with the same prefix, look both up, look up missing
        // ones, and check that a reset empties it without a wipe.
        let mut t = LzwDict::new();
        t.insert(b'A' as u16, b'B', 258);
        t.insert(b'A' as u16, b'C', 259);
        assert_eq!(t.lookup(b'A' as u16, b'B'), Some(258));
        assert_eq!(t.lookup(b'A' as u16, b'C'), Some(259));
        assert_eq!(t.lookup(b'A' as u16, b'X'), None);
        assert_eq!(t.lookup(b'Z' as u16, b'B'), None);
        // A reset starts a new generation: old slots read as empty.
        t.reset();
        assert_eq!(t.lookup(b'A' as u16, b'B'), None);
        // Post-reset insert is visible again.
        t.insert(b'A' as u16, b'B', 258);
        assert_eq!(t.lookup(b'A' as u16, b'B'), Some(258));
    }
}
