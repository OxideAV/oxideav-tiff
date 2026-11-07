//! Bidirectional metadata: the encoder writes every TIFF 6.0 §8 ASCII
//! descriptive field the reader exposes, and the round-trip reads them
//! all back. Also asserts the produced IFD keeps its entries in the
//! §2-required ascending tag order once the new metadata tags (269 /
//! 271 / 272 / 285 / 316) are interleaved with the structural tags.

use oxideav_tiff::ifd::{parse_header, parse_ifd};
use oxideav_tiff::{
    EncodePage, EncodePixelFormat, PageExtras, TiffCompression, decode_page, encode_page,
};

fn page<'a>(w: u32, h: u32, pixels: &'a [u8], extras: PageExtras<'a>) -> EncodePage<'a> {
    EncodePage::new(
        w,
        h,
        EncodePixelFormat::Gray8 { pixels },
        TiffCompression::None,
        false,
        false,
        None,
        false,
        extras,
    )
}

#[test]
fn all_ten_ascii_fields_round_trip() {
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let extras = PageExtras::default()
        .with_document_name(Some("Contract"))
        .with_description(Some("Signature page"))
        .with_make(Some("AcmeScan"))
        .with_model(Some("QuadScan 9000"))
        .with_page_name(Some("Cover"))
        .with_software(Some("oxideav-tiff 0.0"))
        .with_date_time(Some("2026:07:10 09:15:00"))
        .with_artist(Some("Karpeles Lab"))
        .with_host_computer(Some("build-host-01"))
        .with_copyright(Some("(c) 2026 Karpeles Lab"));
    let tiff = encode_page(&page(4, 4, &px, extras)).expect("encode");
    let m = decode_page(&tiff).expect("decode").metadata;
    assert_eq!(m.document_name.as_deref(), Some("Contract"));
    assert_eq!(m.image_description.as_deref(), Some("Signature page"));
    assert_eq!(m.make.as_deref(), Some("AcmeScan"));
    assert_eq!(m.model.as_deref(), Some("QuadScan 9000"));
    assert_eq!(m.page_name.as_deref(), Some("Cover"));
    assert_eq!(m.software.as_deref(), Some("oxideav-tiff 0.0"));
    assert_eq!(m.date_time.as_deref(), Some("2026:07:10 09:15:00"));
    assert_eq!(m.artist.as_deref(), Some("Karpeles Lab"));
    assert_eq!(m.host_computer.as_deref(), Some("build-host-01"));
    assert_eq!(m.copyright.as_deref(), Some("(c) 2026 Karpeles Lab"));
}

#[test]
fn ifd_tags_stay_in_ascending_order_with_all_metadata() {
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let extras = PageExtras::default()
        .with_document_name(Some("D"))
        .with_description(Some("Desc"))
        .with_make(Some("MakeCo"))
        .with_model(Some("ModelX"))
        .with_page_name(Some("P1"))
        .with_software(Some("SW"))
        .with_artist(Some("Art"))
        .with_host_computer(Some("Host"))
        .with_copyright(Some("Copy"))
        .with_page_number(Some((0, 1)));
    let tiff = encode_page(&page(4, 4, &px, extras)).expect("encode");
    let hdr = parse_header(&tiff).expect("header");
    let (entries, _next) =
        parse_ifd(&tiff, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).expect("parse ifd");
    let tags: Vec<u16> = entries.iter().map(|e| e.tag).collect();
    let mut sorted = tags.clone();
    sorted.sort_unstable();
    assert_eq!(tags, sorted, "IFD entries must be in ascending tag order");
    // Every metadata tag we wrote is present exactly once.
    for tag in [269u16, 270, 271, 272, 285, 305, 315, 316, 33432] {
        assert_eq!(
            tags.iter().filter(|&&t| t == tag).count(),
            1,
            "tag {tag} should appear exactly once"
        );
    }
}
