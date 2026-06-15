//! TIFF 6.0 §SampleFormat (tag 339, page 80) value 2 — two's-complement
//! signed-integer grayscale decode.
//!
//! §SampleFormat lists value `2` as "two's complement signed integer
//! data" and notes that SampleFormat "does not specify the size of data
//! samples; this is still done by the BitsPerSample field." The
//! companion SMinSampleValue (tag 340) / SMaxSampleValue (tag 341)
//! "default ... is the full range of the data type." This decoder
//! renders signed-integer grayscale at the two integer widths it
//! assembles (8- and 16-bit, BlackIsZero / WhiteIsZero) onto its
//! unsigned display planes through the order-preserving offset-binary
//! map: the signed minimum lands at display 0, the signed maximum at
//! the unsigned ceiling, and a stored signed 0 at the display midpoint.
//! For 8-bit that map is `XOR 0x80`; for 16-bit `XOR 0x8000`.
//!
//! These tests build minimal hand-crafted classic-II TIFF byte strings
//! and drive them through the public `decode_tiff` entry point, so they
//! are binary-independent oracles: the expected display bytes are
//! computed directly from the offset-binary definition.

use oxideav_tiff::decode_tiff;

/// IFD entry, SHORT (field-type = 3) with a single inline value.
fn entry_short(tag: u16, value: u16) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..10].copy_from_slice(&value.to_le_bytes());
    e
}

/// IFD entry, LONG (field-type = 4) with a single inline value.
fn entry_long(tag: u16, value: u32) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&4u16.to_le_bytes()); // LONG
    e[4..8].copy_from_slice(&1u32.to_le_bytes()); // count = 1
    e[8..12].copy_from_slice(&value.to_le_bytes());
    e
}

/// Assemble a 1-row, `w`-pixel single-channel grayscale classic-II
/// TIFF. `photometric` is 0 (WhiteIsZero) or 1 (BlackIsZero);
/// `bits_per_sample` is 8 or 16; `sample_format` is appended as a
/// SampleFormat (339) SHORT entry; `strip` is the raw uncompressed
/// strip payload placed at offset 8.
fn build_gray_row(
    w: u32,
    bits_per_sample: u16,
    photometric: u16,
    sample_format: u16,
    strip: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();

    // 0..8 — classic LE header; first-IFD offset patched below.
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    let ifd_off_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());

    // 8.. — the strip payload (StripOffsets = 8 lines up).
    let strip_off = out.len() as u32;
    out.extend_from_slice(strip);
    if out.len() % 2 != 0 {
        out.push(0); // word-align the IFD start (TIFF 6.0 §2).
    }

    // IFD starts here.
    let ifd_off = out.len() as u32;
    out[ifd_off_pos..ifd_off_pos + 4].copy_from_slice(&ifd_off.to_le_bytes());

    // 10 entries in ascending tag order (256,257,258,259,262,273,277,
    // 278,279,339).
    out.extend_from_slice(&10u16.to_le_bytes());
    out.extend_from_slice(&entry_short(256, w as u16)); // ImageWidth
    out.extend_from_slice(&entry_short(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short(258, bits_per_sample)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, photometric)); // Photometric
    out.extend_from_slice(&entry_long(273, strip_off)); // StripOffsets
    out.extend_from_slice(&entry_short(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_long(279, strip.len() as u32)); // StripByteCounts
    out.extend_from_slice(&entry_short(339, sample_format)); // SampleFormat

    // next_ifd = 0.
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

#[test]
fn signed_8bit_blackiszero_full_range_offset_binary() {
    // Stored signed bytes spanning the range: -128 (0x80), -1 (0xFF),
    // 0 (0x00), +1 (0x01), +127 (0x7F). Offset-binary (XOR 0x80) maps
    // these to display 0x00, 0x7F, 0x80, 0x81, 0xFF — monotone, with
    // signed-min at 0 and signed-max at 255.
    let strip = [0x80u8, 0xFF, 0x00, 0x01, 0x7F];
    let bytes = build_gray_row(5, 8, 1, 2, &strip);
    let d = decode_tiff(&bytes).expect("signed 8-bit grayscale must decode");
    assert_eq!((d.width, d.height), (5, 1));
    assert_eq!(d.frame.planes[0].data, vec![0x00u8, 0x7F, 0x80, 0x81, 0xFF]);
}

#[test]
fn signed_8bit_whiteiszero_composes_with_polarity() {
    // WhiteIsZero (photometric 0): the offset-binary map runs first
    // (producing the unsigned display sample), then the polarity
    // inversion `255 - v`. Stored 0x80 (-128) -> offset 0x00 ->
    // inverted 0xFF; 0x7F (+127) -> offset 0xFF -> inverted 0x00.
    let strip = [0x80u8, 0x00, 0x7F];
    let bytes = build_gray_row(3, 8, 0, 2, &strip);
    let d = decode_tiff(&bytes).expect("signed 8-bit WhiteIsZero must decode");
    assert_eq!(d.frame.planes[0].data, vec![0xFFu8, 0x7F, 0x00]);
}

#[test]
fn signed_16bit_blackiszero_offset_binary_le() {
    // 16-bit signed, little-endian on-disk. Stored words: -32768
    // (0x8000), 0 (0x0000), +32767 (0x7FFF). Offset-binary (XOR
    // 0x8000) -> 0x0000, 0x8000, 0xFFFF. Output plane is Gray16Le.
    let words: [i16; 3] = [-32768, 0, 32767];
    let mut strip = Vec::new();
    for w in words {
        strip.extend_from_slice(&(w as u16).to_le_bytes());
    }
    let bytes = build_gray_row(3, 16, 1, 2, &strip);
    let d = decode_tiff(&bytes).expect("signed 16-bit grayscale must decode");
    let mut expect = Vec::new();
    for v in [0x0000u16, 0x8000, 0xFFFF] {
        expect.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(d.frame.planes[0].data, expect);
}

#[test]
fn signed_16bit_big_endian_on_disk() {
    // Big-endian file ("MM"): the 16-bit reader honours the file byte
    // order when reading the sample, applies the offset-binary flip,
    // then writes the result little-endian (Gray16Le output). Build
    // the MM fixture by hand.
    let mut out = Vec::new();
    out.extend_from_slice(b"MM");
    out.extend_from_slice(&42u16.to_be_bytes());
    let ifd_off_pos = out.len();
    out.extend_from_slice(&0u32.to_be_bytes());
    // Strip: two big-endian signed words -32768, +32767.
    let strip_off = out.len() as u32;
    out.extend_from_slice(&(-32768i16 as u16).to_be_bytes());
    out.extend_from_slice(&(32767i16 as u16).to_be_bytes());
    let ifd_off = out.len() as u32;
    out[ifd_off_pos..ifd_off_pos + 4].copy_from_slice(&ifd_off.to_be_bytes());

    let entry_short_be = |tag: u16, value: u16| -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..2].copy_from_slice(&tag.to_be_bytes());
        e[2..4].copy_from_slice(&3u16.to_be_bytes());
        e[4..8].copy_from_slice(&1u32.to_be_bytes());
        // SHORT inline value occupies the FIRST two bytes of the slot.
        e[8..10].copy_from_slice(&value.to_be_bytes());
        e
    };
    let entry_long_be = |tag: u16, value: u32| -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..2].copy_from_slice(&tag.to_be_bytes());
        e[2..4].copy_from_slice(&4u16.to_be_bytes());
        e[4..8].copy_from_slice(&1u32.to_be_bytes());
        e[8..12].copy_from_slice(&value.to_be_bytes());
        e
    };

    out.extend_from_slice(&10u16.to_be_bytes());
    out.extend_from_slice(&entry_short_be(256, 2)); // ImageWidth
    out.extend_from_slice(&entry_short_be(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short_be(258, 16)); // BitsPerSample
    out.extend_from_slice(&entry_short_be(259, 1)); // Compression
    out.extend_from_slice(&entry_short_be(262, 1)); // Photometric BlackIsZero
    out.extend_from_slice(&entry_long_be(273, strip_off)); // StripOffsets
    out.extend_from_slice(&entry_short_be(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short_be(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_long_be(279, 4)); // StripByteCounts
    out.extend_from_slice(&entry_short_be(339, 2)); // SampleFormat signed
    out.extend_from_slice(&0u32.to_be_bytes());

    let d = decode_tiff(&out).expect("big-endian signed 16-bit must decode");
    let mut expect = Vec::new();
    for v in [0x0000u16, 0xFFFF] {
        expect.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(d.frame.planes[0].data, expect);
}

/// Assert `decode_tiff` returned an error whose Display includes the
/// given substring (`DecodedTiff` is not `Debug`).
fn expect_err_containing(bytes: &[u8], needle: &str) {
    match decode_tiff(bytes) {
        Ok(_) => panic!("expected an error containing {needle:?}, got Ok(..)"),
        Err(e) => {
            let msg = format!("{e}");
            assert!(
                msg.contains(needle),
                "expected error to contain {needle:?}, got: {msg}"
            );
        }
    }
}

#[test]
fn signed_rgb_is_rejected() {
    // SampleFormat = 2 on a 3-channel RGB image has no single
    // defensible display mapping in this build, so it is refused per
    // the §SampleFormat "terminate gracefully" reader rule rather than
    // silently mis-rendered. Build a 1x1 RGB signed fixture.
    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let strip_off = out.len() as u32; // 8
    out.extend_from_slice(&[0x00u8, 0x10, 0x20]); // R,G,B
    out.push(0); // word-align
                 // Out-of-line BitsPerSample [8,8,8].
    let bps_off = out.len() as u32;
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    let ifd_off = out.len() as u32;
    out[4..8].copy_from_slice(&ifd_off.to_le_bytes());

    let entry_short_arr = |tag: u16, count: u32, off: u32| -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..2].copy_from_slice(&tag.to_le_bytes());
        e[2..4].copy_from_slice(&3u16.to_le_bytes());
        e[4..8].copy_from_slice(&count.to_le_bytes());
        e[8..12].copy_from_slice(&off.to_le_bytes());
        e
    };

    out.extend_from_slice(&10u16.to_le_bytes());
    out.extend_from_slice(&entry_short(256, 1)); // ImageWidth
    out.extend_from_slice(&entry_short(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short_arr(258, 3, bps_off)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression
    out.extend_from_slice(&entry_short(262, 2)); // Photometric RGB
    out.extend_from_slice(&entry_long(273, strip_off)); // StripOffsets
    out.extend_from_slice(&entry_short(277, 3)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_long(279, 3)); // StripByteCounts
    out.extend_from_slice(&entry_short(339, 2)); // SampleFormat signed
    out.extend_from_slice(&0u32.to_le_bytes());

    expect_err_containing(&out, "SampleFormat=2");
}

#[test]
fn signed_4bit_is_rejected() {
    // 4-bit signed grayscale is outside the assembled integer widths
    // (8 / 16), so it is refused with the precise SampleFormat=2
    // message rather than mis-decoded.
    let strip = [0x12u8]; // two 4-bit samples
    let bytes = build_gray_row(2, 4, 1, 2, &strip);
    expect_err_containing(&bytes, "SampleFormat=2");
}

#[test]
fn signed_zero_maps_to_midpoint() {
    // The offset-binary invariant a downstream consumer relies on:
    // a stored signed 0 renders at the display midpoint (0x80 for
    // 8-bit, 0x8000 for 16-bit), so neutral data is mid-gray.
    let bytes8 = build_gray_row(1, 8, 1, 2, &[0x00u8]);
    let d8 = decode_tiff(&bytes8).expect("8-bit signed zero must decode");
    assert_eq!(d8.frame.planes[0].data, vec![0x80u8]);

    let bytes16 = build_gray_row(1, 16, 1, 2, &0i16.to_le_bytes());
    let d16 = decode_tiff(&bytes16).expect("16-bit signed zero must decode");
    assert_eq!(d16.frame.planes[0].data, 0x8000u16.to_le_bytes());
}
