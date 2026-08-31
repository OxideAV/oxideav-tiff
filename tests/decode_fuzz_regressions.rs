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
fn fuzz_r454_huge_samples_per_pixel_allocation_rejected() {
    // Fuzz r454 finding (oom-32b0ea25…): MAX_IMAGE_PIXELS bounds
    // width × height only, so an IFD claiming a huge SamplesPerPixel
    // (unbounded SHORT) at 16-bit depth drove an ~8.5 GiB
    // `Vec::with_capacity` in the strip walker before the photometric
    // dispatch could reject the shape. The total-bytes gate must fire
    // first.
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    let entries: &[(u16, u16, u32, u32)] = &[
        (256, 4, 1, 16384), // ImageWidth
        (257, 4, 1, 16384), // ImageLength (w*h well under MAX_IMAGE_PIXELS)
        (259, 3, 1, 1),     // Compression = None
        (262, 3, 1, 1),     // PhotometricInterpretation
        (273, 4, 1, 100),   // StripOffsets
        (277, 3, 1, 40000), // SamplesPerPixel — hostile
        (278, 4, 1, 16384), // RowsPerStrip
        (279, 4, 1, 8),     // StripByteCounts
    ];
    v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for &(tag, ty, cnt, val) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&cnt.to_le_bytes());
        v.extend_from_slice(&val.to_le_bytes());
    }
    v.extend_from_slice(&0u32.to_le_bytes());
    let Err(e) = decode_tiff(&v) else {
        panic!("hostile SamplesPerPixel allocation must not decode");
    };
    assert!(format!("{e:?}").contains("too large"), "{e:?}");
}

#[test]
fn fuzz_r454_samples_per_pixel_zero_does_not_panic() {
    // Fuzz r454 finding (crash-1f3b276c…): `SamplesPerPixel = 0`
    // with the BitsPerSample tag absent made `decode_bits_per_sample`
    // return an empty vector, and the `bits_per_sample[0]` read
    // panicked ("index out of bounds: the len is 0"). Minimal
    // reconstruction: classic-II IFD claiming SPP = 0 and no tag 258.
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    let entries: &[(u16, u16, u32, u32)] = &[
        (256, 4, 1, 8),   // ImageWidth
        (257, 4, 1, 8),   // ImageLength
        (259, 3, 1, 1),   // Compression = None
        (262, 3, 1, 1),   // PhotometricInterpretation
        (273, 4, 1, 100), // StripOffsets
        (277, 3, 1, 0),   // SamplesPerPixel = 0 (malformed)
        (278, 4, 1, 8),   // RowsPerStrip
        (279, 4, 1, 8),   // StripByteCounts
    ];
    v.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for &(tag, ty, cnt, val) in entries {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&cnt.to_le_bytes());
        v.extend_from_slice(&val.to_le_bytes());
    }
    v.extend_from_slice(&0u32.to_le_bytes());
    let Err(e) = decode_tiff(&v) else {
        panic!("SamplesPerPixel=0 must not decode");
    };
    assert!(format!("{e:?}").contains("SamplesPerPixel=0"), "{e:?}");
    let _ = decode_tiff_all(&v);
}

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
