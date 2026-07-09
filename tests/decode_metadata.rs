//! Decode-side metadata exposure — the descriptive + structural tags
//! ([`oxideav_tiff::TiffMetadata`]) gathered alongside the pixels.
//!
//! Two oracles:
//!
//!   * **Encoder round-trip.** The encoder's `PageExtras` writes the
//!     TIFF 6.0 §8 ASCII fields, resolution triple, orientation, page
//!     number and the NewSubfileType bits; decoding must read back
//!     exactly what was written.
//!   * **Hand-built bytes.** For the tags the encoder does not (yet)
//!     write — Make / Model / HostComputer / DocumentName / PageName /
//!     SubfileType — a minimal classic-II TIFF is assembled by hand so
//!     the reader is exercised directly.
//!
//! Every assertion is spec-grounded in TIFF 6.0 §8 ("Baseline Field
//! Reference") and the resolution / page tags there.

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, PageExtras, PageResolution,
    ResolutionUnit, TiffCompression, TiffMetadata,
};

fn gray8_page<'a>(w: u32, h: u32, pixels: &'a [u8], extras: PageExtras<'a>) -> EncodePage<'a> {
    EncodePage {
        width: w,
        height: h,
        kind: EncodePixelFormat::Gray8 { pixels },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras,
    }
}

#[test]
fn ascii_section8_fields_round_trip() {
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let extras = PageExtras {
        description: Some("A test raster"),
        software: Some("oxideav-tiff"),
        date_time: Some("2026:07:10 12:34:56"),
        artist: Some("Karpeles Lab"),
        copyright: Some("(c) 2026"),
        ..Default::default()
    };
    let tiff = encode_tiff(&gray8_page(4, 4, &px, extras)).expect("encode");
    let d = decode_tiff(&tiff).expect("decode");
    let m = &d.metadata;
    assert_eq!(m.image_description.as_deref(), Some("A test raster"));
    assert_eq!(m.software.as_deref(), Some("oxideav-tiff"));
    assert_eq!(m.date_time.as_deref(), Some("2026:07:10 12:34:56"));
    assert_eq!(m.artist.as_deref(), Some("Karpeles Lab"));
    assert_eq!(m.copyright.as_deref(), Some("(c) 2026"));
    // Tags the writer did not emit stay `None`.
    assert_eq!(m.make, None);
    assert_eq!(m.model, None);
    assert_eq!(m.host_computer, None);
}

#[test]
fn resolution_triple_round_trips_as_raw_rationals() {
    let px: Vec<u8> = vec![0x11; 4];
    let extras = PageExtras {
        resolution: Some(PageResolution {
            x: (300, 1),
            y: (150, 2),
            unit: 3, // centimetre
        }),
        ..Default::default()
    };
    let tiff = encode_tiff(&gray8_page(2, 2, &px, extras)).expect("encode");
    let m = decode_tiff(&tiff).expect("decode").metadata;
    assert_eq!(m.x_resolution, Some((300, 1)));
    assert_eq!(m.y_resolution, Some((150, 2)));
    assert_eq!(m.resolution_unit, Some(ResolutionUnit::Centimeter));
}

#[test]
fn orientation_tag_preserved_alongside_applied_transform() {
    let px: Vec<u8> = (0..12u32).map(|i| i as u8).collect();
    // Orientation 6 = top-left rotated; the decoder applies the pixel
    // transform, but the metadata must still report the original tag.
    let extras = PageExtras {
        orientation: Some(6),
        ..Default::default()
    };
    let tiff = encode_tiff(&gray8_page(4, 3, &px, extras)).expect("encode");
    let m = decode_tiff(&tiff).expect("decode").metadata;
    assert_eq!(m.orientation, Some(6));
}

#[test]
fn page_number_and_subfile_bits_round_trip() {
    let px: Vec<u8> = vec![0x22; 4];
    let extras = PageExtras {
        page_number: Some((2, 5)),
        multi_page: true,         // NewSubfileType bit 1
        reduced_resolution: true, // NewSubfileType bit 0
        ..Default::default()
    };
    let tiff = encode_tiff(&gray8_page(2, 2, &px, extras)).expect("encode");
    let m = decode_tiff(&tiff).expect("decode").metadata;
    assert_eq!(m.page_number, Some((2, 5)));
    // bit 0 (reduced) | bit 1 (page) = 0b11 = 3.
    assert_eq!(m.new_subfile_type, Some(3));
}

#[test]
fn no_metadata_tags_yields_empty_default() {
    let px: Vec<u8> = vec![0x33; 4];
    let tiff = encode_tiff(&gray8_page(2, 2, &px, PageExtras::default())).expect("encode");
    let m = decode_tiff(&tiff).expect("decode").metadata;
    // The encoder writes no §8 / resolution / page tags without extras,
    // but it always emits NewSubfileType = 0 (a baseline structural tag,
    // not descriptive metadata); the reader reports it faithfully. Every
    // descriptive / resolution field stays `None`, and the resolution
    // default is not synthesised by the reader.
    assert_eq!(
        m,
        TiffMetadata {
            new_subfile_type: Some(0),
            ..Default::default()
        }
    );
}

// --------------------------------------------------------------------
// Hand-built classic-II TIFF for tags the encoder does not write.
// --------------------------------------------------------------------

/// A 12-byte classic IFD entry with an inline (<=4 byte) value.
fn entry_inline(tag: u16, ty: u16, count: u32, value: [u8; 4]) -> [u8; 12] {
    let mut e = [0u8; 12];
    e[0..2].copy_from_slice(&tag.to_le_bytes());
    e[2..4].copy_from_slice(&ty.to_le_bytes());
    e[4..8].copy_from_slice(&count.to_le_bytes());
    e[8..12].copy_from_slice(&value);
    e
}

/// A 12-byte classic IFD entry whose value lives at an out-of-line
/// offset (for ASCII strings > 4 bytes).
fn entry_offset(tag: u16, ty: u16, count: u32, offset: u32) -> [u8; 12] {
    entry_inline(tag, ty, count, offset.to_le_bytes())
}

/// Build a 1x1 8-bit BlackIsZero TIFF carrying an arbitrary set of
/// extra ASCII / SHORT metadata entries (each supplies its 12-byte IFD
/// record plus, optionally, out-of-line bytes appended after the IFD).
fn build_meta_tiff(extra_entries: &[[u8; 12]], trailing_blobs: &[u8]) -> Vec<u8> {
    // Fixed core entries: ImageWidth, ImageLength, BitsPerSample,
    // Compression, Photometric, StripOffsets, RowsPerStrip,
    // StripByteCounts, SamplesPerPixel.
    let mut core = vec![
        entry_inline(256, 3, 1, 1u32.to_le_bytes()), // ImageWidth = 1
        entry_inline(257, 3, 1, 1u32.to_le_bytes()), // ImageLength = 1
        entry_inline(258, 3, 1, 8u32.to_le_bytes()), // BitsPerSample = 8
        entry_inline(259, 3, 1, 1u32.to_le_bytes()), // Compression = None
        entry_inline(262, 3, 1, 1u32.to_le_bytes()), // Photometric = BlackIsZero
        entry_inline(273, 4, 1, 0u32.to_le_bytes()), // StripOffsets (patched)
        entry_inline(278, 3, 1, 1u32.to_le_bytes()), // RowsPerStrip = 1
        entry_inline(279, 4, 1, 1u32.to_le_bytes()), // StripByteCounts = 1
        entry_inline(277, 3, 1, 1u32.to_le_bytes()), // SamplesPerPixel = 1
    ];
    core.extend_from_slice(extra_entries);
    // Sort by tag (TIFF 6.0 §2 requires ascending IFD order).
    core.sort_by_key(|e| u16::from_le_bytes([e[0], e[1]]));

    let n = core.len() as u16;
    // Layout: header(8) + count(2) + n*12 + next(4) + pixel(1) + blobs.
    let ifd_start = 8u32;
    let after_ifd = ifd_start + 2 + n as u32 * 12 + 4;
    let pixel_off = after_ifd;
    let blob_off = pixel_off + 1;

    let mut v = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&ifd_start.to_le_bytes());
    v.extend_from_slice(&n.to_le_bytes());
    for e in &core {
        // Patch StripOffsets (tag 273) to point at the pixel byte, and
        // rebase any out-of-line ASCII offset (encoded relative to the
        // blob region) to its absolute position.
        let tag = u16::from_le_bytes([e[0], e[1]]);
        let mut e = *e;
        if tag == 273 {
            e[8..12].copy_from_slice(&pixel_off.to_le_bytes());
        }
        v.extend_from_slice(&e);
    }
    v.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    v.push(0xAB); // the single pixel
    v.extend_from_slice(trailing_blobs);
    // Sanity: the blob region begins right after the pixel.
    debug_assert_eq!(blob_off as usize, v.len() - trailing_blobs.len());
    v
}

#[test]
fn hand_built_make_model_hostcomputer_document_page_read_back() {
    // Out-of-line ASCII blobs (each NUL-terminated) placed after the
    // pixel byte. Compute absolute offsets as we append.
    let mut blobs = Vec::new();
    let base = 8u32 + 2 + 14 * 12 + 4 + 1; // header+count+14 entries+next+pixel
    let make = b"AcmeScan\0";
    let make_off = base;
    blobs.extend_from_slice(make);
    let model = b"Model-9000\0";
    let model_off = make_off + make.len() as u32;
    blobs.extend_from_slice(model);
    let host = b"workstation-01\0";
    let host_off = model_off + model.len() as u32;
    blobs.extend_from_slice(host);
    let doc = b"Contract.pdf\0";
    let doc_off = host_off + host.len() as u32;
    blobs.extend_from_slice(doc);
    let page = b"Cover\0";
    let page_off = doc_off + doc.len() as u32;
    blobs.extend_from_slice(page);

    let extras = [
        entry_offset(269, 2, doc.len() as u32, doc_off), // DocumentName
        entry_offset(271, 2, make.len() as u32, make_off), // Make
        entry_offset(272, 2, model.len() as u32, model_off), // Model
        entry_offset(285, 2, page.len() as u32, page_off), // PageName
        entry_offset(316, 2, host.len() as u32, host_off), // HostComputer
    ];
    // 9 core + 5 extra = 14 entries — matches `base` above.
    let tiff = build_meta_tiff(&extras, &blobs);
    let m = decode_tiff(&tiff).expect("decode").metadata;
    assert_eq!(m.make.as_deref(), Some("AcmeScan"));
    assert_eq!(m.model.as_deref(), Some("Model-9000"));
    assert_eq!(m.host_computer.as_deref(), Some("workstation-01"));
    assert_eq!(m.document_name.as_deref(), Some("Contract.pdf"));
    assert_eq!(m.page_name.as_deref(), Some("Cover"));
}

#[test]
fn subfile_type_short_read_back() {
    // SubfileType (255) = 1 (full-resolution) fits inline.
    let extras = [entry_inline(255, 3, 1, 1u32.to_le_bytes())];
    let tiff = build_meta_tiff(&extras, &[]);
    let m = decode_tiff(&tiff).expect("decode").metadata;
    assert_eq!(m.subfile_type, Some(1));
}

#[test]
fn inline_short_ascii_fits_in_value_slot() {
    // ImageDescription (270) = "Hi\0\0" fits inline (<=4 bytes incl NUL).
    let extras = [entry_inline(270, 2, 3, *b"Hi\0\0")];
    let tiff = build_meta_tiff(&extras, &[]);
    let m = decode_tiff(&tiff).expect("decode").metadata;
    assert_eq!(m.image_description.as_deref(), Some("Hi"));
}

#[test]
fn malformed_metadata_entry_does_not_gate_decode() {
    // ImageDescription with field type SHORT (wrong) — must be dropped,
    // decode still succeeds and the pixel is intact.
    let extras = [entry_inline(270, 3, 1, 7u32.to_le_bytes())];
    let tiff = build_meta_tiff(&extras, &[]);
    let d = decode_tiff(&tiff).expect("decode still succeeds");
    assert_eq!(d.metadata.image_description, None);
    assert_eq!(d.frame.planes[0].data, vec![0xAB]);
}
