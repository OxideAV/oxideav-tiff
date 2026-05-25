//! Regression tests for the panic vectors discovered by the
//! `fuzz/fuzz_targets/decode.rs` cargo-fuzz target (round 126).
//!
//! Each test minimally reconstructs the malformed-input shape the
//! fuzzer used to trigger an abort / OOM and asserts the decoder
//! now surfaces it as a normal `TiffError`. The fixes themselves
//! live in `src/compress.rs` (LZW first-after-Clear leaf check,
//! deflate output cap, packbits / LZW initial-reserve cap) and
//! `src/ifd.rs` (BigTIFF `offset + 8` checked_add, BigTIFF entry
//! `total = type_size * count` checked_mul) and `src/decoder.rs`
//! (`MAX_IMAGE_PIXELS` sanity gate up-front against attacker-claimed
//! `ImageWidth * ImageLength`).

use oxideav_tiff::{decode_tiff, decode_tiff_all};

#[test]
fn fuzz_r126_bigtiff_first_ifd_offset_u64_max_does_not_panic() {
    // Original fuzz reproducer:
    //   [49 49 2B 00 08 00 00 00 FF FF FF FF FF FF FF FF
    //    00 00 00 00 00 00 00 00 02 3D B1]
    //
    // II + BigTIFF magic (43) + off_size=8 + reserved=0 +
    // first_ifd_offset=u64::MAX. The `parse_ifd_big` `off + 8`
    // expression debug-panicked on usize overflow.
    let bytes = [
        0x49, 0x49, 0x2B, 0x00, 0x08, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x3D, 0xB1,
    ];
    // Either return path is acceptable; the contract is "does not panic".
    let _ = decode_tiff(&bytes);
    let _ = decode_tiff_all(&bytes);
}

#[test]
fn fuzz_r126_huge_dimensions_rejected_up_front() {
    // Build the smallest possible classic-II TIFF whose IFD claims
    // `ImageWidth = u32::MAX` / `ImageLength = u32::MAX`. Without
    // the `MAX_IMAGE_PIXELS` gate the strip allocator would attempt
    // a multi-exabyte reservation.
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes()); // classic magic
    v.extend_from_slice(&8u32.to_le_bytes()); // first IFD at offset 8
                                              // IFD: 2 bytes count + N*12 entries + 4 bytes next-IFD pointer.
    let entries: &[(u16, u16, u32, u32)] = &[
        (256, 4, 1, u32::MAX), // ImageWidth = u32::MAX
        (257, 4, 1, u32::MAX), // ImageLength = u32::MAX
        (258, 3, 1, 8),        // BitsPerSample
        (259, 3, 1, 1),        // Compression = None
        (262, 3, 1, 1),        // PhotometricInterpretation = BlackIsZero
        (273, 4, 1, 100),      // StripOffsets
        (278, 4, 1, 1),        // RowsPerStrip
        (279, 4, 1, 1),        // StripByteCounts
        (277, 3, 1, 1),        // SamplesPerPixel
    ];
    v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for &(tag, ty, cnt, val) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&cnt.to_le_bytes());
        v.extend_from_slice(&val.to_le_bytes());
    }
    v.extend_from_slice(&0u32.to_le_bytes()); // no more IFDs
                                              // Pad to make the StripOffsets=100 dereference legal so we
                                              // exercise the dimension gate, not the EOF check.
    v.resize(200, 0);
    let err = match decode_tiff(&v) {
        Ok(_) => panic!("expected dimension-rejection error"),
        Err(e) => e,
    };
    let msg = format!("{err:?}");
    assert!(
        msg.contains("image too large")
            || msg.contains("MAX_IMAGE_PIXELS")
            || msg.contains("pixels"),
        "expected too-large message, got: {msg}"
    );
}
