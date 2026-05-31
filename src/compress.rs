//! Strip decompression for the four compression schemes we support:
//! Compression=1 (None), Compression=32773 (PackBits), Compression=5
//! (LZW), Compression=8 (Adobe Deflate / zlib).
//!
//! All routines take the raw on-disk strip bytes and the expected
//! decompressed length (in bytes) and return the decompressed bytes.

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

/// Upper bound on the *expansion ratio* of a single Deflate strip.
/// `unpack_deflate` defers to `miniz_oxide`, which has no built-in
/// bound on output size; without an explicit limit a 100-byte
/// zlib stream can claim to expand to several gigabytes (a classic
/// "zip bomb"). The cap is `expected_len` (from the IFD) clamped to
/// `MAX_DEFLATE_OUTPUT`; both bounds are needed because
/// `expected_len` itself can be attacker-chosen on a malformed file.
const MAX_DEFLATE_OUTPUT: usize = 64 * 1024 * 1024;

/// Upper bound on the expansion ratio of a single Zstandard (RFC 8878)
/// strip / tile. The streaming decoder we drive (`ruzstd`) is
/// `io::Read`-shaped and does not, on its own, enforce a maximum
/// decompressed size — a malformed frame can claim to expand a few
/// hundred bytes into many gigabytes. Mirror of `MAX_DEFLATE_OUTPUT`:
/// 64 MiB per strip/tile is well above any legitimate TIFF strip
/// (encoders chunk at well under this) while bounding worst-case
/// per-call allocation when fed a frame shaped like a decompression
/// bomb.
#[cfg(feature = "zstd")]
const MAX_ZSTD_OUTPUT: usize = 64 * 1024 * 1024;

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

/// Bit reader for TIFF LZW: codes are stored MSB-first (FillOrder=1,
/// per spec: "LZW compression codes are stored into bytes in
/// high-to-low-order fashion").
struct LzwBits<'a> {
    src: &'a [u8],
    bit_pos: usize, // global bit index from the start of `src`
}

impl<'a> LzwBits<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self { src, bit_pos: 0 }
    }

    /// Read `n` bits (n <= 12). Returns None if the stream is
    /// exhausted before `n` bits could be read.
    fn read(&mut self, n: u32) -> Option<u16> {
        let total_bits = self.src.len() * 8;
        if self.bit_pos + n as usize > total_bits {
            return None;
        }
        let mut value: u32 = 0;
        let mut remaining = n;
        let mut pos = self.bit_pos;
        while remaining > 0 {
            let byte = self.src[pos / 8];
            let bit_in_byte = (pos % 8) as u32;
            // bits remaining in this byte starting at bit_in_byte
            // (high-bit-first)
            let avail = 8 - bit_in_byte;
            let take = avail.min(remaining);
            let shift_right = avail - take;
            let mask = (1u32 << take) - 1;
            let chunk = ((byte as u32) >> shift_right) & mask;
            value = (value << take) | chunk;
            pos += take as usize;
            remaining -= take;
        }
        self.bit_pos = pos;
        Some(value as u16)
    }
}

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
    let mut bits = LzwBits::new(input);
    // Bound the upfront reservation. LZW's worst-case ratio per
    // codeword is the longest dictionary string (capped at 4095
    // codes, so a single 12-bit code can emit at most a few thousand
    // bytes), times `input.len() * 8 / 9` codewords for a 9-bit
    // run; we use `MAX_INITIAL_RESERVE` directly because that bound
    // is already much larger than any realistic single-strip output.
    let reserve = expected_len.min(MAX_INITIAL_RESERVE);
    let mut out = Vec::with_capacity(reserve);

    // Per-strip table: each entry is the prefix-code + last-byte
    // pair, plus a cached length so we can pre-size on emit.
    // entries[0..=255] are leaf single-byte strings; we store
    // (prefix=NONE, byte=i, len=1) for them. NONE = u16::MAX.
    const NONE: u16 = u16::MAX;
    let mut prefix: Vec<u16> = vec![0; (LZW_MAX_CODE as usize) + 1];
    let mut suffix: Vec<u8> = vec![0; (LZW_MAX_CODE as usize) + 1];
    let mut len: Vec<u32> = vec![0; (LZW_MAX_CODE as usize) + 1];
    let init = |prefix: &mut [u16], suffix: &mut [u8], len: &mut [u32]| {
        for i in 0..=255u16 {
            prefix[i as usize] = NONE;
            suffix[i as usize] = i as u8;
            len[i as usize] = 1;
        }
    };
    init(&mut prefix, &mut suffix, &mut len);

    let mut code_size: u32 = 9;
    let mut next_code: u16 = LZW_FIRST_CODE;
    let mut prev_code: Option<u16> = None;

    // Reusable scratch for decoding a string in reverse.
    let mut scratch: Vec<u8> = Vec::with_capacity(64);

    // Helper: write the string at `code` to `out`, returning its
    // first byte (used as the suffix when adding the new entry).
    //
    // Returns `None` if the prefix chain failed to terminate within
    // `LZW_MAX_CODE + 1` hops — a well-formed TIFF/LZW table has at
    // most 4096 entries, and each `prefix[c]` walk strictly shortens
    // the implied string by one byte (the table is built bottom-up
    // from leaves), so a chain longer than that is a structural
    // invariant violation (self-cycle or cross-cycle introduced by
    // a malformed stream). Defence-in-depth against any future
    // construction that bypasses the leaf-only entry guard above.
    let emit = |code: u16,
                out: &mut Vec<u8>,
                scratch: &mut Vec<u8>,
                prefix: &[u16],
                suffix: &[u8]|
     -> Option<u8> {
        scratch.clear();
        let mut c = code;
        for _ in 0..=(LZW_MAX_CODE as usize) {
            scratch.push(suffix[c as usize]);
            let p = prefix[c as usize];
            if p == NONE {
                // scratch now holds the string in reverse order.
                for &b in scratch.iter().rev() {
                    out.push(b);
                }
                return Some(*scratch.last().unwrap());
            }
            c = p;
        }
        None
    };

    while let Some(code) = bits.read(code_size) {
        if code == LZW_EOI_CODE {
            break;
        }
        if code == LZW_CLEAR_CODE {
            init(&mut prefix, &mut suffix, &mut len);
            code_size = 9;
            next_code = LZW_FIRST_CODE;
            prev_code = None;
            continue;
        }
        if let Some(prev) = prev_code {
            if code < next_code {
                // Code is in the table.
                let first = emit(code, &mut out, &mut scratch, &prefix, &suffix)
                    .ok_or_else(|| Error::invalid("TIFF/LZW: prefix chain cycle"))?;
                if next_code <= LZW_MAX_CODE {
                    prefix[next_code as usize] = prev;
                    suffix[next_code as usize] = first;
                    len[next_code as usize] = len[prev as usize] + 1;
                    next_code += 1;
                }
            } else if code == next_code {
                // The "KwKwK" special case: the new code refers to
                // an entry that's about to be added. The string is
                // prev-string + first-char-of-prev-string.
                // Emit prev string then append its first char.
                let first = emit(prev, &mut out, &mut scratch, &prefix, &suffix)
                    .ok_or_else(|| Error::invalid("TIFF/LZW: prefix chain cycle"))?;
                out.push(first);
                if next_code <= LZW_MAX_CODE {
                    prefix[next_code as usize] = prev;
                    suffix[next_code as usize] = first;
                    len[next_code as usize] = len[prev as usize] + 1;
                    next_code += 1;
                }
            } else {
                return Err(Error::invalid(format!(
                    "TIFF/LZW: code {code} > next_code {next_code}"
                )));
            }
        } else {
            // First code after a Clear: must be a single byte (table
            // indices 0..=255). Any larger value would index into an
            // uninitialised table slot, and feeding that slot back
            // through `emit` can chase a self-referential prefix
            // chain forever (because the *next* iteration's
            // `code == next_code` branch writes `prefix[next_code]
            // = prev_code`, and the first-after-Clear we are about
            // to record as `prev_code` could itself equal
            // `next_code`). Reject explicitly so the cycle never
            // forms.
            if code >= LZW_CLEAR_CODE {
                return Err(Error::invalid(format!(
                    "TIFF/LZW: first code after Clear is {code}, must be a leaf (<256)"
                )));
            }
            let _first = emit(code, &mut out, &mut scratch, &prefix, &suffix)
                .ok_or_else(|| Error::invalid("TIFF/LZW: prefix chain cycle"))?;
        }
        prev_code = Some(code);
        // Bump the code size BEFORE writing the next-code-to-add
        // exceeds the current width — TIFF Section 13 specifies the
        // "bump one early" quirk: switch to N+1-bit reads as soon
        // as table entry (2^N - 1) has just been added (next_code
        // == 2^N - 1 was the entry just stored, so the *next* add
        // will be at next_code which now equals 2^N).
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

pub fn unpack_deflate(input: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    // Cap output to the smaller of the IFD-claimed `expected_len`
    // and `MAX_DEFLATE_OUTPUT`. Without an explicit limit, a
    // malformed stream can claim to expand a 100-byte payload into
    // gigabytes; `miniz_oxide`'s `decompress_to_vec_zlib_with_limit`
    // bails with `TINFLStatus::HasMoreOutput` once the cap is hit
    // and we surface that as a normal decode error rather than an
    // OOM abort. The cap is loose enough not to interfere with any
    // legitimate single-strip TIFF (most encoders chunk at well
    // under 64 MiB per strip), but tight enough to bound the
    // worst-case per-call allocation.
    let limit = expected_len.min(MAX_DEFLATE_OUTPUT);
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(input, limit)
        .map_err(|e| Error::invalid(format!("TIFF/Deflate: zlib inflate failed: {:?}", e.status)))
}

// ---------------------------------------------------------------------------
// Zstandard — extension Compression=50000 (one RFC 8878 frame per strip/tile)
// ---------------------------------------------------------------------------

/// Decode one TIFF strip / tile payload as a single RFC 8878 Zstandard
/// frame and return the (predicted) byte stream that the predictor
/// reversal stage then walks.
///
/// Contract:
///
/// * `input` is the strip's / tile's on-disk bytes, which must begin
///   with the zstd magic number `0x28 0xB5 0x2F 0xFD` (little-endian
///   `0xFD2FB528`). A single TIFF block carries exactly one zstd
///   frame — there is no whole-file framing and no cross-strip
///   dictionary sharing.
/// * `expected_len` is the post-decompression strip / tile size in
///   bytes, derived from the IFD geometry (`ImageWidth *
///   ImageLength * SamplesPerPixel * BitsPerSample / 8`, or the
///   tile-grid analogue). It is the upper bound the decoder will
///   produce; output is capped at `min(expected_len, MAX_ZSTD_OUTPUT)`
///   to bound worst-case per-call allocation on attacker-supplied
///   IFDs.
/// * Returns the decompressed bytes verbatim (no Predictor reversal —
///   the caller in `decoder.rs` applies `apply_horizontal_predictor`
///   in a subsequent step, exactly as it does for Compression=5 and
///   Compression=8).
///
/// Errors map a missing zstd magic, a truncated frame, or any
/// decoder failure to `Error::invalid` with a textual reason; an
/// output that exceeds the cap is treated as a corrupt /
/// adversarial input and surfaced as a decode error rather than an
/// OOM abort.
#[cfg(feature = "zstd")]
pub fn unpack_zstd(input: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    use std::io::Read;

    if input.len() < 4 || input[..4] != [0x28, 0xB5, 0x2F, 0xFD] {
        return Err(Error::invalid(
            "TIFF/Zstd: strip payload does not begin with RFC 8878 frame magic 0x28B52FFD",
        ));
    }

    let limit = expected_len.min(MAX_ZSTD_OUTPUT);

    let mut dec = ruzstd::streaming_decoder::StreamingDecoder::new(input)
        .map_err(|e| Error::invalid(format!("TIFF/Zstd: frame init failed: {e}")))?;

    // Bound output via `Read::take` to keep a malformed / bomb-shaped
    // frame from forcing an unbounded allocation. We read `limit + 1`
    // bytes worth: hitting `limit + 1` means the frame would expand
    // past the cap, which is an error; a normal frame stops short on
    // its own at `limit` or below.
    let cap = (limit as u64).saturating_add(1);
    let reserve = limit.min(MAX_INITIAL_RESERVE);
    let mut out = Vec::with_capacity(reserve);
    (&mut dec)
        .take(cap)
        .read_to_end(&mut out)
        .map_err(|e| Error::invalid(format!("TIFF/Zstd: frame decode failed: {e}")))?;
    if out.len() > limit {
        return Err(Error::invalid(format!(
            "TIFF/Zstd: decompressed strip exceeds {limit}-byte cap (frame would expand past expected size)"
        )));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Encoders (used by the writer in `encoder.rs`)
// ---------------------------------------------------------------------------

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

/// Size of the trie's parent-indexed arrays. One slot per possible
/// dictionary entry (codes `0..=LZW_MAX_CODE`), so the trie covers
/// the entire 12-bit code space without any allocation past
/// initialisation.
const LZW_TRIE_SIZE: usize = (LZW_MAX_CODE as usize) + 1;

/// Sentinel "no child" / "no sibling" value. The dictionary never
/// stores codes below `LZW_FIRST_CODE` as child entries (codes 0..=255
/// are roots, 256/257 are control codes), so 0 is unambiguously
/// "empty slot" for both `first_child` and `next_sibling`.
const LZW_TRIE_NONE: u16 = 0;

/// LZW encoder trie. Each dictionary entry `c` is identified by its
/// `(parent_code, suffix_byte)` pair; the trie indexes by parent and
/// walks a singly-linked list of children to find the matching
/// suffix, replacing the old `HashMap<(u16, u8), u16>` lookup with
/// pure array math.
///
/// Three flat parallel arrays of `LZW_TRIE_SIZE` elements:
///
/// * `first_child[parent]` — head of `parent`'s child list, or
///   `LZW_TRIE_NONE` if no children have been added under that prefix
///   yet.
/// * `next_sibling[c]` — next entry in `c`'s parent's child list, or
///   `LZW_TRIE_NONE` at end of list.
/// * `suffix[c]` — the byte that, concatenated to `parent`'s string,
///   produces `c`'s string.
///
/// Total RAM footprint per encoder invocation: `LZW_TRIE_SIZE * (2 +
/// 2 + 1) = 4096 * 5 = 20 480` bytes, all stack-resident through the
/// `Box<LzwTrie>` heap allocation (so a deep recursion can't blow the
/// stack on small-default-stack platforms).
struct LzwTrie {
    first_child: [u16; LZW_TRIE_SIZE],
    next_sibling: [u16; LZW_TRIE_SIZE],
    suffix: [u8; LZW_TRIE_SIZE],
}

impl LzwTrie {
    fn new() -> Box<Self> {
        // 20 KiB zero-initialised; `LZW_TRIE_NONE = 0` matches the
        // zeroes, so the trie starts empty correctly.
        Box::new(Self {
            first_child: [0; LZW_TRIE_SIZE],
            next_sibling: [0; LZW_TRIE_SIZE],
            suffix: [0; LZW_TRIE_SIZE],
        })
    }

    /// Reset on `ClearCode`: every child list head becomes empty.
    /// We only need to zero `first_child` because once a parent has
    /// no head, the `next_sibling` / `suffix` slots are unreachable
    /// from a lookup walk regardless of their stale values, and the
    /// next `insert` overwrites them.
    fn reset(&mut self) {
        self.first_child.fill(LZW_TRIE_NONE);
    }

    /// Lookup `(parent, byte)` in the trie. Returns `Some(code)` if a
    /// dictionary entry already exists for this pair, else `None`.
    /// Walks the singly-linked child list at `first_child[parent]`
    /// (in insertion order) comparing `suffix[c] == byte`.
    #[inline]
    fn lookup(&self, parent: u16, byte: u8) -> Option<u16> {
        let mut c = self.first_child[parent as usize];
        while c != LZW_TRIE_NONE {
            if self.suffix[c as usize] == byte {
                return Some(c);
            }
            c = self.next_sibling[c as usize];
        }
        None
    }

    /// Insert a new entry `new_code` as a child of `parent` with the
    /// given `byte`. The new entry is pushed at the *head* of
    /// `parent`'s child list so it's the first one a subsequent
    /// lookup sees — modest cache-locality win for repeated motifs in
    /// the input.
    #[inline]
    fn insert(&mut self, parent: u16, byte: u8, new_code: u16) {
        self.suffix[new_code as usize] = byte;
        self.next_sibling[new_code as usize] = self.first_child[parent as usize];
        self.first_child[parent as usize] = new_code;
    }
}

/// LZW encode for TIFF (TIFF 6.0 §13). Builds the same code table
/// the decoder expects, writes codes MSB-first ("FillOrder=1, codes
/// stored high-to-low-order"), bumps code width "one early" exactly
/// as `unpack_lzw` does on the read side, and emits `ClearCode`
/// when the table fills.
///
/// Round 129: the dictionary is a flat-array trie keyed by
/// `(parent_code, suffix_byte)` instead of the previous
/// `HashMap<(u16, u8), u16>`. The bitstream output is byte-for-byte
/// identical to the old implementation (same greedy match, same
/// code-width bump points, same Clear-on-fill timing), so the change
/// is invisible to any decoder, but lookup is now a short chain walk
/// over up-to-12-bit codes rather than a `(u16, u8)` hash + bucket
/// scan, eliminating the per-pixel HashMap overhead that dominated
/// the encoder profile on Rgb24 / Gray8 strips.
pub fn pack_lzw(input: &[u8]) -> Vec<u8> {
    let mut trie = LzwTrie::new();
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

/// Deflate encode (zlib stream) using `miniz_oxide` at default
/// compression level. Matches Compression=8 (Adobe Deflate).
pub fn pack_deflate(input: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(input, 6)
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
        src.extend(std::iter::repeat(0xFF).take(20));
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
        // miniz_oxide's `decompress_to_vec_zlib` has no built-in
        // bound on output size; `unpack_deflate` must cap at
        // `MAX_DEFLATE_OUTPUT` to prevent zip-bomb OOMs. Encode a
        // 64-byte zlib stream and confirm decode succeeds within
        // the cap — we don't currently have a synthetic zip bomb in
        // tree, so this is the "happy path doesn't regress"
        // half of the contract; the panic-freedom half is covered
        // by the fuzz target.
        let src = vec![0u8; 1024];
        let encoded = pack_deflate(&src);
        let back = unpack_deflate(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
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
        // pseudo-random fixture: the trie's child lists for the
        // motif's prefix codes get hit repeatedly, so this is the
        // case where the head-insertion order in `LzwTrie::insert`
        // earns its keep (most-recently-added child is found first).
        // Mainly a roundtrip-correctness check; the bench file
        // exercises throughput.
        let motif = b"abcdefghijklmnop";
        let mut src = Vec::with_capacity(motif.len() * 2048);
        for _ in 0..2048 {
            src.extend_from_slice(motif);
        }
        let encoded = pack_lzw(&src);
        let back = unpack_lzw(&encoded, src.len()).unwrap();
        assert_eq!(back, src);
    }

    /// Synthesise a minimal RFC 8878 zstd frame whose decompressed
    /// payload is the literal `data` slice, using only:
    ///
    /// * the frame magic `0x28B52FFD`,
    /// * a frame header byte with `Single_Segment_Flag = 1` and
    ///   `Content_Checksum_Flag = 0` (so Frame_Content_Size is
    ///   present, no checksum field follows, and there is no
    ///   Window_Descriptor),
    /// * a `Frame_Content_Size` field whose width is chosen by the
    ///   2-bit `Frame_Content_Size_Flag` (here always FCS_Field_Size=4
    ///   so the size is a `u32 LE`),
    /// * exactly one `Raw_Block` carrying the literal bytes (the
    ///   simplest of the three block kinds: zero entropy stages),
    /// * the `Last_Block` flag set in the block header.
    ///
    /// This lets the decode tests run without depending on any
    /// external zstd encoder (`tiffcp`, `zstd`); the integration test
    /// suite still uses `tiffcp -c zstd` to cross-check.
    #[cfg(feature = "zstd")]
    fn build_raw_zstd_frame(data: &[u8]) -> Vec<u8> {
        assert!(
            data.len() <= u32::MAX as usize,
            "test helper accepts up to u32::MAX-sized payloads"
        );
        let mut out = Vec::with_capacity(data.len() + 32);
        // Magic 0xFD2FB528 (LE bytes 0x28 0xB5 0x2F 0xFD).
        out.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
        // Frame_Header_Descriptor:
        //   bits 7..6  Frame_Content_Size_Flag = 2 (FCS field is 4 bytes)
        //   bit  5     Single_Segment_Flag     = 1 (no Window_Descriptor)
        //   bit  4     Unused                  = 0
        //   bit  3     Reserved                = 0
        //   bit  2     Content_Checksum_Flag   = 0
        //   bits 1..0  Dictionary_ID_Flag      = 0 (no dictionary)
        // → 0b10100000 = 0xA0
        out.push(0xA0);
        // Frame_Content_Size: u32 little-endian.
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        // One block of kind Raw_Block (Block_Type = 0), Last_Block = 1.
        // Block_Header is 3 bytes, packed little-endian as:
        //   bit  0      Last_Block (set: this is the only block)
        //   bits 1..2   Block_Type = 0 (Raw_Block — the low two bits
        //               above the Last_Block bit are zero, so the
        //               Block_Type field contributes nothing to the
        //               packed value)
        //   bits 3..23  Block_Size (raw byte count)
        let bh: u32 = 1 | ((data.len() as u32) << 3);
        out.extend_from_slice(&bh.to_le_bytes()[..3]);
        out.extend_from_slice(data);
        out
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_decodes_synthetic_raw_block_frame() {
        // Decoder side only: feed a hand-built RFC 8878 frame whose
        // single Raw_Block carries 64 known bytes and verify we get
        // them back. Confirms the magic-check + StreamingDecoder
        // wiring + 64-MiB cap interaction is sane for small inputs.
        let payload: Vec<u8> = (0u8..64).collect();
        let frame = build_raw_zstd_frame(&payload);
        let back = unpack_zstd(&frame, payload.len()).unwrap();
        assert_eq!(back, payload);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_rejects_missing_magic() {
        // 16 bytes that don't start with 0x28 0xB5 0x2F 0xFD: must
        // surface a precise textual error rather than reaching the
        // StreamingDecoder. Negative test #1 per the round brief.
        let not_a_frame = [0u8; 16];
        let err = unpack_zstd(&not_a_frame, 64).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("frame magic") || msg.contains("0x28B52FFD"),
            "expected magic-check error, got: {msg}"
        );
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_rejects_truncated_frame() {
        // Build a valid frame, then chop off the trailing block bytes
        // so the StreamingDecoder hits EOF mid-block. Must surface a
        // textual error rather than panicking. Negative test #2 per
        // the round brief.
        let payload: Vec<u8> = (0u8..64).collect();
        let mut frame = build_raw_zstd_frame(&payload);
        // Drop the last 16 bytes — well into the Raw_Block body.
        frame.truncate(frame.len() - 16);
        let err = unpack_zstd(&frame, payload.len()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("decode failed") || msg.contains("frame init") || msg.contains("Zstd"),
            "expected truncated-frame error, got: {msg}"
        );
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_output_is_bounded() {
        // A frame that claims to expand to more than MAX_ZSTD_OUTPUT
        // (64 MiB) must be rejected as a decode error, not aborted
        // for OOM. Build a small valid frame and pass an absurd
        // `expected_len` — the cap is `min(expected_len, 64 MiB)` so
        // a 64-byte payload still decodes fine even with a huge
        // upper bound.
        let payload: Vec<u8> = (0u8..64).collect();
        let frame = build_raw_zstd_frame(&payload);
        let back = unpack_zstd(&frame, usize::MAX / 2).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn lzw_trie_lookup_and_insert() {
        // Direct unit test of the trie API: insert two children of
        // the same parent, look both up, look up a missing one.
        // Verifies head-insertion ordering and the lookup-walk
        // termination contract.
        let mut t = LzwTrie::new();
        t.insert(b'A' as u16, b'B', 258);
        t.insert(b'A' as u16, b'C', 259);
        assert_eq!(t.lookup(b'A' as u16, b'B'), Some(258));
        assert_eq!(t.lookup(b'A' as u16, b'C'), Some(259));
        assert_eq!(t.lookup(b'A' as u16, b'X'), None);
        assert_eq!(t.lookup(b'Z' as u16, b'B'), None);
        // Reset clears the parent's child list.
        t.reset();
        assert_eq!(t.lookup(b'A' as u16, b'B'), None);
        // Post-reset insert is visible again.
        t.insert(b'A' as u16, b'B', 258);
        assert_eq!(t.lookup(b'A' as u16, b'B'), Some(258));
    }
}
