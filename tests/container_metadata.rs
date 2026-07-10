//! The registry-integrated container surface: the TIFF demuxer's
//! `Demuxer::metadata()` exposes the first IFD's §8 ASCII descriptive
//! fields as flat key/value pairs, and `Demuxer::attachments()`
//! carries the embedded ICC profile (tag 34675) and XMP packet (tag
//! 700) as structured byte attachments — verbatim, per
//! `docs/image/tiff/tiff-icc-xmp-tags.md`.

#![cfg(feature = "registry")]

use std::io::Cursor;

use oxideav_core::CodecRegistry;
use oxideav_tiff::container::open_demuxer;
use oxideav_tiff::{encode_tiff, EncodePage, EncodePixelFormat, PageExtras, TiffCompression};

fn minimal_icc(total_len: usize) -> Vec<u8> {
    assert!(total_len >= 132);
    let mut p = vec![0u8; total_len];
    p[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
    p[36..40].copy_from_slice(b"acsp");
    for (i, b) in p[128..].iter_mut().enumerate() {
        *b = (i % 241) as u8;
    }
    p
}

#[test]
fn demuxer_exposes_metadata_and_attachments() {
    let px: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let xmp = b"<?xpacket begin=\"\"?><x:xmpmeta/><?xpacket end=\"w\"?>".to_vec();
    let icc = minimal_icc(180);
    let extras = PageExtras {
        software: Some("oxideav-tiff test"),
        artist: Some("Karpeles Lab"),
        description: Some("demuxer surface"),
        date_time: Some("2026:07:10 12:00:00"),
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let tiff = encode_tiff(&EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Gray8 { pixels: &px },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras,
    })
    .expect("encode");

    let codecs = CodecRegistry::new();
    let dmx = open_demuxer(Box::new(Cursor::new(tiff)), &codecs).expect("open demuxer");

    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    assert_eq!(get("software"), Some("oxideav-tiff test"));
    assert_eq!(get("artist"), Some("Karpeles Lab"));
    assert_eq!(get("description"), Some("demuxer surface"));
    assert_eq!(get("date"), Some("2026:07:10 12:00:00"));
    assert_eq!(get("make"), None); // absent tag => no entry

    let atts = dmx.attachments();
    assert_eq!(atts.len(), 2);
    let icc_att = atts
        .iter()
        .find(|a| a.name == "profile.icc")
        .expect("icc attachment");
    assert_eq!(icc_att.mime.as_deref(), Some("application/vnd.iccprofile"));
    assert_eq!(icc_att.data, icc);
    let xmp_att = atts
        .iter()
        .find(|a| a.name == "packet.xmp")
        .expect("xmp attachment");
    assert_eq!(xmp_att.mime.as_deref(), Some("application/rdf+xml"));
    assert_eq!(xmp_att.data, xmp);
}

#[test]
fn demuxer_without_payloads_has_empty_surfaces() {
    let px: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let tiff = encode_tiff(&EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Gray8 { pixels: &px },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras::default(),
    })
    .expect("encode");
    let codecs = CodecRegistry::new();
    let dmx = open_demuxer(Box::new(Cursor::new(tiff)), &codecs).expect("open demuxer");
    assert!(dmx.metadata().is_empty());
    assert!(dmx.attachments().is_empty());
}
