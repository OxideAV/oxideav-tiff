//! TIFF 6.0 §ExtraSamples (tag 338, pages 31-32) decoder coverage.
//!
//! "Specifies that each pixel has m extra components whose
//! interpretation is defined by one of the values listed below. When
//! this field is used, the SamplesPerPixel field has a value greater
//! than the PhotometricInterpretation field suggests." The defined
//! values:
//!
//!   0 = Unspecified data
//!   1 = Associated alpha data (with pre-multiplied color)
//!   2 = Unassociated alpha data
//!
//! "By convention, extra components that are present must be stored
//! as the 'last components' in each pixel." "The default is no extra
//! samples. This field must be present if there are extra samples."
//!
//! Reader policy under test: values 0, 1, and 2 all decode with the
//! trailing extra component(s) dropped and the leading color
//! components rendered verbatim. For unspecified (0) and unassociated
//! alpha (2) the stored color is straight; for associated alpha (1)
//! the stored pre-multiplied color is the §18 "Associated Alpha
//! Handling" (page 78) composite-over-black display value a display
//! reader shows directly ("naive applications … can do so simply by
//! displaying the RGB component values … the same as merging the
//! image with a black background"). Values ≥ 3 are `InvalidData`; and
//! a count that does not leave a photometric-defined color-component
//! count is `InvalidData` per the spec's "If SamplesPerPixel is, say,
//! 5 then ExtraSamples will contain 2 values, one for each extra
//! sample" arithmetic.
//!
//! ## On-disk layout each test produces
//!
//! A minimal 1x1 RGB classic-II TIFF with `SamplesPerPixel = 3 + m`,
//! a single uncompressed strip of `3 + m` bytes at offset 8, an
//! out-of-line `BitsPerSample` SHORT array (count = SamplesPerPixel,
//! which no longer fits the 4-byte inline slot), and optionally an
//! `ExtraSamples` (338) entry whose 1-2 SHORT values stay inline.
//! Tag 338 sorts after every other tag in the fixture so it is always
//! appended last in tag-order.

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

/// IFD entry, SHORT array. `count <= 2` packs the values inline;
/// larger counts store `offset` in the value slot (the caller is
/// responsible for placing the array bytes there).
fn entry_short_array(
    tag: u16,
    count: u32,
    inline_or_offset: &[u16],
    offset: Option<u32>,
) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    e[4..8].copy_from_slice(&count.to_le_bytes());
    match offset {
        Some(off) => e[8..12].copy_from_slice(&off.to_le_bytes()),
        None => {
            for (i, v) in inline_or_offset.iter().enumerate().take(2) {
                e[8 + 2 * i..10 + 2 * i].copy_from_slice(&v.to_le_bytes());
            }
        }
    }
    e
}

/// Assemble the minimal 1x1 RGB(+extras) TIFF described in the module
/// header. `spp` is the SamplesPerPixel to declare (also the strip
/// length in bytes — `pixel` must be `spp` bytes), and
/// `extra_samples`, when present, is appended as an `ExtraSamples`
/// entry with `extra_samples.len()` inline SHORT values (max 2).
fn build_1x1_rgb(spp: u16, pixel: &[u8], extra_samples: Option<&[u16]>) -> Vec<u8> {
    assert_eq!(pixel.len(), spp as usize);
    let es_len = extra_samples.map(|v| v.len()).unwrap_or(0);
    assert!(es_len <= 2, "inline SHORT slot holds at most 2 values");

    let mut out = Vec::new();

    // 0..8 — classic LE header; first-IFD offset patched below.
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    let ifd_off_pos = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());

    // 8.. — the strip's pixel bytes (so StripOffsets = 8 lines up).
    out.extend_from_slice(pixel);
    // Pad to a word boundary for the BitsPerSample array (TIFF 6.0
    // §2: field values must begin on a word boundary).
    if out.len() % 2 != 0 {
        out.push(0);
    }

    // Out-of-line BitsPerSample SHORT array, count = spp, all 8.
    let bps_off = out.len() as u32;
    for _ in 0..spp {
        out.extend_from_slice(&8u16.to_le_bytes());
    }

    // IFD starts here.
    let ifd_off = out.len() as u32;
    out[ifd_off_pos..ifd_off_pos + 4].copy_from_slice(&ifd_off.to_le_bytes());

    // Entry count. Tag 338 sorts after every other tag carried here
    // (256, 257, 258, 259, 262, 273, 277, 278, 279), so the
    // `ExtraSamples` entry is always appended last when present.
    let n: u16 = 9 + if extra_samples.is_some() { 1 } else { 0 };
    out.extend_from_slice(&n.to_le_bytes());

    out.extend_from_slice(&entry_short(256, 1)); // ImageWidth
    out.extend_from_slice(&entry_short(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short_array(258, spp as u32, &[], Some(bps_off))); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, 2)); // Photometric = RGB
    out.extend_from_slice(&entry_short(273, 8)); // StripOffsets → byte 8
    out.extend_from_slice(&entry_short(277, spp)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_short(279, spp)); // StripByteCounts
    if let Some(es) = extra_samples {
        out.extend_from_slice(&entry_short_array(338, es.len() as u32, es, None));
    }

    // next_ifd = 0 — single IFD.
    out.extend_from_slice(&0u32.to_le_bytes());

    out
}

/// Same minimal 1x1 Gray8 fixture the §SampleFormat / §Orientation /
/// §ResolutionUnit tests use, with an `ExtraSamples` entry appended —
/// used for the SamplesPerPixel=1 count-mismatch case.
fn build_1x1_gray8_with_extras(extra_samples: &[u16]) -> Vec<u8> {
    assert!(extra_samples.len() <= 2);
    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&16u32.to_le_bytes());
    out.push(0xAB); // the strip's one pixel byte at offset 8
    out.extend_from_slice(&[0u8; 7]); // pad so the IFD starts at 16

    let n: u16 = 10;
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&entry_short(256, 1)); // ImageWidth
    out.extend_from_slice(&entry_short(257, 1)); // ImageLength
    out.extend_from_slice(&entry_short(258, 8)); // BitsPerSample
    out.extend_from_slice(&entry_short(259, 1)); // Compression = None
    out.extend_from_slice(&entry_short(262, 1)); // Photometric = BlackIsZero
    out.extend_from_slice(&entry_short(273, 8)); // StripOffsets → byte 8
    out.extend_from_slice(&entry_short(277, 1)); // SamplesPerPixel
    out.extend_from_slice(&entry_short(278, 1)); // RowsPerStrip
    out.extend_from_slice(&entry_short(279, 1)); // StripByteCounts
    out.extend_from_slice(&entry_short_array(
        338,
        extra_samples.len() as u32,
        extra_samples,
        None,
    ));
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// Helper: assert `decode_tiff` returned an error whose Display
/// includes the given substring.
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
fn rgb_spp4_without_extra_samples_decodes_by_skipping() {
    // "The default is no extra samples." An RGB IFD with
    // SamplesPerPixel = 4 and no tag 338 keeps decoding through the
    // trailing-component skip path (regression pin of the
    // pre-inspection behaviour).
    let bytes = build_1x1_rgb(4, &[10, 20, 30, 99], None);
    let d = decode_tiff(&bytes).expect("RGB SamplesPerPixel=4 without tag 338 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![10, 20, 30]);
}

#[test]
fn extra_samples_zero_unspecified_decodes_by_skipping() {
    // `ExtraSamples = [0]` (unspecified data): the extra component
    // carries no defined meaning, so the R, G, B triple renders
    // correctly without it.
    let bytes = build_1x1_rgb(4, &[10, 20, 30, 99], Some(&[0]));
    let d = decode_tiff(&bytes).expect("ExtraSamples=[0] must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![10, 20, 30]);
}

#[test]
fn extra_samples_two_unassociated_alpha_decodes_by_skipping() {
    // `ExtraSamples = [2]` (unassociated alpha): "transparency
    // information that logically exists independent of an image" —
    // the color components are stored straight (not pre-multiplied),
    // so skipping the soft matte yields the correctly-colored
    // fully-opaque render.
    let bytes = build_1x1_rgb(4, &[10, 20, 30, 99], Some(&[2]));
    let d = decode_tiff(&bytes).expect("ExtraSamples=[2] must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![10, 20, 30]);
}

#[test]
fn extra_samples_one_associated_alpha_displays_premultiplied_rgb() {
    // `ExtraSamples = [1]` (associated alpha, pre-multiplied color):
    // §18 "Associated Alpha Handling" (page 78) states "naive
    // applications that want to display an RGBA image on a display can
    // do so simply by displaying the RGB component values. This works
    // because it is effectively the same as merging the image with a
    // black background. … Cr = Cover * Aover" — "which is exactly the
    // pre-multiplied color; i.e. what is stored in the image." So the
    // stored pre-multiplied leading triple IS the composite-over-black
    // display value; the decoder renders it directly and drops the
    // trailing alpha. The stored `(10, 20, 30)` is already
    // pre-multiplied by alpha = 99, so it renders verbatim.
    let bytes = build_1x1_rgb(4, &[10, 20, 30, 99], Some(&[1]));
    let d = decode_tiff(&bytes).expect("ExtraSamples=[1] (associated alpha) must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![10, 20, 30]);
}

#[test]
fn extra_samples_associated_alpha_zero_alpha_renders_stored_black() {
    // §18 page 78: "If A is zero, then the color components should be
    // interpreted as zero." A well-formed pre-multiplied page with
    // alpha = 0 therefore stores `(0, 0, 0)` for the color; displaying
    // the stored triple directly (composite over black) reproduces the
    // fully-transparent pixel as black, which is the §18 naive-display
    // result.
    let bytes = build_1x1_rgb(4, &[0, 0, 0, 0], Some(&[1]));
    let d = decode_tiff(&bytes).expect("ExtraSamples=[1] alpha=0 must decode");
    assert_eq!(d.frame.planes[0].data, vec![0, 0, 0]);
}

#[test]
fn extra_samples_two_extras_spp5_decode_by_skipping() {
    // Spec example arithmetic: "If SamplesPerPixel is, say, 5 then
    // ExtraSamples will contain 2 values, one for each extra
    // sample." Both extras are skippable kinds (0 and 2).
    let bytes = build_1x1_rgb(5, &[10, 20, 30, 99, 77], Some(&[0, 2]));
    let d = decode_tiff(&bytes).expect("ExtraSamples=[0,2] on SamplesPerPixel=5 must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![10, 20, 30]);
}

#[test]
fn extra_samples_mixed_with_associated_alpha_decodes() {
    // A `SamplesPerPixel = 5` RGB page declaring `[0, 1]` (one
    // unspecified extra, one associated-alpha extra) still renders the
    // leading R, G, B triple verbatim: the unspecified extra carries no
    // meaning and the associated-alpha pre-multiplied color is the §18
    // composite-over-black display value. Both trailing extras drop.
    let bytes = build_1x1_rgb(5, &[10, 20, 30, 99, 77], Some(&[0, 1]));
    let d = decode_tiff(&bytes).expect("ExtraSamples=[0,1] must decode");
    assert_eq!((d.width, d.height), (1, 1));
    assert_eq!(d.frame.planes[0].data, vec![10, 20, 30]);
}

#[test]
fn extra_samples_unknown_value_is_rejected_as_invalid() {
    // The spec defines 0..=2 only. Value 3 comes from a malformed
    // writer; the decoder rejects rather than guessing a meaning.
    let bytes = build_1x1_rgb(4, &[10, 20, 30, 99], Some(&[3]));
    expect_err_containing(&bytes, "ExtraSamples=3");
}

#[test]
fn extra_samples_large_value_is_rejected_as_invalid() {
    // Highest single-SHORT value — same invalid-data outcome as 3.
    let bytes = build_1x1_rgb(4, &[10, 20, 30, 99], Some(&[65535]));
    expect_err_containing(&bytes, "ExtraSamples=65535");
}

#[test]
fn extra_samples_count_mismatch_on_rgb_is_rejected() {
    // SamplesPerPixel = 4 with TWO declared extras leaves only 2
    // color components — but "full-color RGB data normally has
    // SamplesPerPixel=3", so the count arithmetic fails.
    let bytes = build_1x1_rgb(4, &[10, 20, 30, 99], Some(&[2, 2]));
    expect_err_containing(&bytes, "ExtraSamples count 2");
}

#[test]
fn extra_samples_on_spp3_rgb_is_rejected() {
    // SamplesPerPixel = 3 RGB with a declared extra leaves only 2
    // color components: "When this field is used, the SamplesPerPixel
    // field has a value greater than the PhotometricInterpretation
    // field suggests" — 3 is not greater than 3.
    let bytes = build_1x1_rgb(3, &[10, 20, 30], Some(&[2]));
    expect_err_containing(&bytes, "ExtraSamples count 1");
}

#[test]
fn extra_samples_on_spp1_gray_is_rejected() {
    // Grayscale suggests one color component; SamplesPerPixel = 1
    // with a declared extra would leave zero.
    let bytes = build_1x1_gray8_with_extras(&[2]);
    expect_err_containing(&bytes, "ExtraSamples count 1");
}
