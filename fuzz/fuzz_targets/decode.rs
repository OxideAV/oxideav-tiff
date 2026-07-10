#![no_main]

//! Panic-freedom fuzz target for the `oxideav-tiff` decoder.
//!
//! Feeds arbitrary attacker-controlled bytes through every public
//! decoder entry point and asserts that none of them panic, abort,
//! debug-overflow, or index out of bounds. Return values are
//! intentionally discarded — the contract under test is *the call
//! returns*, not what it returns.
//!
//! Classic TIFF parser danger spots this target drives:
//!
//! * **IFD header + entry overflow** — the 12-byte (classic) /
//!   20-byte (BigTIFF) entries each declare a `type * count` value
//!   payload that lives either inline (`<=4` bytes classic, `<=8`
//!   bytes BigTIFF) or at an attacker-supplied offset elsewhere in
//!   the file. The `count` field is u32 (classic) or u64 (BigTIFF);
//!   `type_size * count` can overflow `u64` for crafted BigTIFF
//!   entries and silently wrap to a small number that bypasses the
//!   "fits in 8 bytes" inline check, then indexes into the value
//!   slot with the wrapped length.
//! * **Strip / tile byte-count arithmetic** — `StripOffsets`,
//!   `StripByteCounts`, `TileOffsets`, `TileByteCounts` are all u32
//!   or u64 vectors that drive `offset + bytecount` reads into the
//!   raw file. A malformed pair (offset = u32::MAX, bytecount = 1024)
//!   must produce `Err(...)` from the `checked_add` slot, not panic.
//! * **Predictor decode on under-sized strips** — `Predictor = 2`
//!   horizontal-differencing walks `samples_per_pixel * (width - 1)`
//!   bytes per row; a malformed strip that decompresses short must
//!   error from the length check, not OOB-index the predictor.
//! * **LZW dictionary growth** — the 12-bit (max 4095 codes) table
//!   resets on `CLEAR` (256); a malformed stream that emits a code
//!   above `next_code` before any CLEAR must error, not panic the
//!   `prefix[code]` index. Likewise an unterminated stream that
//!   exhausts input before EOI must short-decompress, not loop.
//! * **JPEG-in-TIFF (Compression = 7) embedding** — each strip /
//!   tile is a complete ISO JPEG datastream re-entered through the
//!   workspace's `oxideav-mjpeg` decoder. A malformed JPEG slice
//!   must propagate as `Err(...)`, not panic the JPEG framer.
//! * **BigTIFF 64-bit offsets** — `as usize` on a u64 offset is
//!   identity on 64-bit hosts but truncates on 32-bit hosts; the
//!   post-cast bound check must reject truncations that pass.
//! * **Cyclic next-IFD pointer** — `decode_tiff_all` walks the
//!   chain; a crafted file that points back to itself must error
//!   from the visited-set check, not OOM on the next-IFD walk.
//! * **Compression unpacker direct entry** — `unpack_packbits` /
//!   `unpack_lzw` / `unpack_deflate` / `unpack_zstd` are reachable
//!   as public API for standalone consumers; each must
//!   short-decompress on truncated input rather than panic.
//!
//! No external library worth dlopen-ing as a cross-decode oracle:
//! workspace policy bars libtiff / image-tiff vendoring or reference
//! read, and even if it didn't, the contract here is panic-freedom,
//! not pixel equality. The harness is decoder-only — the encoder
//! controls its own input runs and is exercised through the
//! crate-local roundtrip tests.

use libfuzzer_sys::fuzz_target;
use oxideav_tiff::compress::{unpack_deflate, unpack_lzw, unpack_packbits, unpack_zstd};
use oxideav_tiff::ifd::{parse_header, parse_ifd};
use oxideav_tiff::metadata::{extract_format_info, extract_metadata};
use oxideav_tiff::{decode_tiff, decode_tiff_all, decode_tiff_all_pages, decode_tiff_at};

/// Cap the standalone-decompressor `expected_len` so a tiny fuzz
/// input claiming a huge expected size cannot drive a multi-gibibyte
/// `Vec::with_capacity` (cargo-fuzz inherits the process RLIMIT but
/// is happier never reaching the limit at all).
const MAX_EXPECTED_LEN: usize = 1 << 20; // 1 MiB

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // 1. The high-level public surface: `decode_tiff` (first IFD) and
    //    `decode_tiff_all` (full multi-page chain). Together these
    //    exercise the header parser, IFD walker, every compression
    //    unpacker, the strip and tile reassemblers, the predictor,
    //    and every photometric expander. The single richest fuzz
    //    surface in the crate.
    // -----------------------------------------------------------------
    let _ = decode_tiff(data);
    let _ = decode_tiff_all(data);
    // `decode_tiff_all_pages` walks the same chain but also runs the
    // metadata + format-info extractors per page; drive it too.
    let _ = decode_tiff_all_pages(data);
    // `decode_tiff_at` takes an explicit (attacker-controllable in a
    // hostile SubIFDs entry) IFD offset — probe it with a
    // fuzzer-chosen offset and a couple of fixed danger spots (header
    // overlap, EOF boundary).
    if let Some((&prefix, _)) = data.split_first() {
        let _ = decode_tiff_at(data, prefix as u64);
        let _ = decode_tiff_at(data, data.len() as u64);
        let _ = decode_tiff_at(data, u64::MAX);
    }

    // -----------------------------------------------------------------
    // 2. Direct IFD entry-points. Reachable as public API for
    //    consumers that want to introspect tag tables without
    //    decoding pixels. Drives the 12-byte (classic) / 20-byte
    //    (BigTIFF) entry parsers, including `type_size * count`
    //    overflow on BigTIFF and the inline-vs-offset value split.
    // -----------------------------------------------------------------
    if let Ok(hdr) = parse_header(data) {
        // The first IFD's offset is attacker-controlled; the parser
        // must reject it cleanly if it points past EOF.
        if let Ok((entries, _next)) =
            parse_ifd(data, hdr.byte_order, hdr.variant, hdr.first_ifd_offset)
        {
            // The metadata / format-info extractors are *total*: a
            // wrong-type / truncated / unterminated informational tag
            // must leave that one field empty, never panic. Drive them
            // over whatever entry table the parser accepted.
            let _ = extract_metadata(&entries, hdr.byte_order);
            let _ = extract_format_info(&entries, hdr.byte_order);
        }
        // Also probe `parse_ifd` at offset 0 explicitly — a TIFF
        // file whose first IFD overlaps the header is malformed but
        // must not panic the parser.
        let _ = parse_ifd(data, hdr.byte_order, hdr.variant, 0);
    }

    // -----------------------------------------------------------------
    // 3. Compression unpackers. Each is a pure byte→`Result`
    //    function over `(input, expected_len)`. We cap
    //    `expected_len` to keep per-iteration cost bounded — a
    //    huge claim alone shouldn't drive multi-gibibyte allocs.
    //
    //    First-byte split lets the fuzzer choose `expected_len`
    //    against the remaining payload without burning a 32-bit
    //    prefix on every iteration; modulo cap keeps it sane.
    // -----------------------------------------------------------------
    if let Some((&prefix, rest)) = data.split_first() {
        let expected = (prefix as usize) << 8; // 0..=65280
        let expected = expected.min(MAX_EXPECTED_LEN);
        let _ = unpack_packbits(rest, expected);
        let _ = unpack_lzw(rest, expected);
        let _ = unpack_deflate(rest, expected);
        let _ = unpack_zstd(rest, expected);
    }
});
