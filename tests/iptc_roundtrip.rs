//! IPTC-IIM record stream carriage (tag 33723): the opaque payload
//! round-trips byte-for-byte through the contract (`Metadata::iptc`)
//! and depth (`PageExtras::iptc`) surfaces, is read from BYTE- as well
//! as UNDEFINED-typed entries, keeps the IFD in ascending tag order
//! between Copyright (33432) and the Exif pointer (34665), is reported
//! by `info`, can be left out with `embed_iptc = false`, and is never
//! written empty.

use oxideav_tiff::ifd::{find, parse_header, parse_ifd};
use oxideav_tiff::{
    EncodeOptions, EncodePage, EncodePixelFormat, Metadata, PageExtras, PixelFormat, Plane,
    TiffCompression, TiffError, TiffImage, decode, decode_page, encode, encode_page, info,
};

/// Two IPTC-IIM datasets: record 2 dataset 5 (ObjectName) and record 2
/// dataset 120 (Caption), each `1C <record> <dataset> <len:u16 BE>
/// <bytes>`.
fn iptc_records() -> Vec<u8> {
    let mut v = Vec::new();
    for (ds, text) in [(5u8, "oxideav"), (120u8, "IPTC through TIFF tag 33723")] {
        v.extend_from_slice(&[0x1C, 0x02, ds]);
        v.extend_from_slice(&(text.len() as u16).to_be_bytes());
        v.extend_from_slice(text.as_bytes());
    }
    v
}

fn gray(w: u32, h: u32) -> TiffImage {
    let px: Vec<u8> = (0..w * h).map(|i| (i * 7) as u8).collect();
    TiffImage::new(w, h, PixelFormat::Gray8, vec![Plane::new(w as usize, px)]).unwrap()
}

#[test]
fn contract_metadata_iptc_round_trips_verbatim() {
    let records = iptc_records();
    let img = gray(5, 3).with_metadata(Metadata::new().with_iptc(records.clone()));
    assert!(!img.metadata.is_empty());
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    let back = decode(&bytes).unwrap();
    assert_eq!(back.metadata.iptc.as_deref(), Some(records.as_slice()));
    assert_eq!(back, img);
    let i = info(&bytes).unwrap();
    assert!(i.has_iptc);
    assert!(!i.has_icc && !i.has_xmp && !i.has_exif);
    // The depth surface sees the same bytes.
    let page = decode_page(&bytes).unwrap();
    assert_eq!(page.metadata.iptc.as_deref(), Some(records.as_slice()));
}

#[test]
fn embed_iptc_false_drops_the_tag() {
    let img = gray(4, 4).with_metadata(Metadata::new().with_iptc(iptc_records()));
    let bytes = encode(&img, &EncodeOptions::default().with_embed_iptc(false)).unwrap();
    let back = decode(&bytes).unwrap();
    assert_eq!(back.metadata.iptc, None);
    assert!(!info(&bytes).unwrap().has_iptc);
    let hdr = parse_header(&bytes).unwrap();
    let (entries, _) =
        parse_ifd(&bytes, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    assert!(find(&entries, 33723).is_none());
}

#[test]
fn tag_is_undefined_typed_and_in_ascending_order() {
    let records = iptc_records();
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let extras = PageExtras {
        copyright: Some("(c) 2026"),
        iptc: Some(&records),
        xmp: Some(b"<x:xmpmeta/>"),
        ..PageExtras::default()
    };
    let bytes = encode_page(&EncodePage {
        width: 4,
        height: 4,
        kind: EncodePixelFormat::Gray8 { pixels: &px },
        compression: TiffCompression::Lzw,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras,
    })
    .unwrap();
    let hdr = parse_header(&bytes).unwrap();
    let (entries, _) =
        parse_ifd(&bytes, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let tags: Vec<u16> = entries.iter().map(|e| e.tag).collect();
    let mut sorted = tags.clone();
    sorted.sort_unstable();
    assert_eq!(tags, sorted);
    let pos = |t: u16| tags.iter().position(|&x| x == t).unwrap();
    assert!(pos(700) < pos(33432) && pos(33432) < pos(33723));
    let e = find(&entries, 33723).unwrap();
    assert_eq!(e.field_type, 7, "UNDEFINED");
    assert_eq!(e.count, records.len() as u64);
    assert_eq!(&e.data[..records.len()], records.as_slice());
    assert_eq!(
        decode_page(&bytes).unwrap().metadata.iptc.as_deref(),
        Some(records.as_slice())
    );
}

#[test]
fn byte_typed_entry_is_accepted_other_types_are_dropped() {
    let records = iptc_records();
    let img = gray(4, 2).with_metadata(Metadata::new().with_iptc(records.clone()));
    let mut bytes = encode(&img, &EncodeOptions::default()).unwrap();
    // Locate the 12-byte entry for tag 33723 in the (little-endian,
    // classic) IFD and rewrite its type field.
    let hdr = parse_header(&bytes).unwrap();
    let ifd = hdr.first_ifd_offset as usize;
    let n = u16::from_le_bytes([bytes[ifd], bytes[ifd + 1]]) as usize;
    let entry = (0..n)
        .map(|i| ifd + 2 + 12 * i)
        .find(|&o| u16::from_le_bytes([bytes[o], bytes[o + 1]]) == 33723)
        .expect("tag 33723 entry");
    assert_eq!(bytes[entry + 2], 7);
    // BYTE (1): accepted, same bytes.
    bytes[entry + 2] = 1;
    assert_eq!(
        decode(&bytes).unwrap().metadata.iptc.as_deref(),
        Some(records.as_slice())
    );
    // SHORT (3): not an opaque 1-byte element type — total extraction
    // drops the field, the pixels still decode.
    bytes[entry + 2] = 3;
    let back = decode(&bytes).unwrap();
    assert_eq!(back.metadata.iptc, None);
    assert_eq!(back.planes, img.planes);
}

#[test]
fn empty_iptc_is_rejected() {
    let img = gray(2, 2).with_metadata(Metadata::new().with_iptc(Vec::new()));
    assert!(matches!(
        encode(&img, &EncodeOptions::default()),
        Err(TiffError::InvalidData(_))
    ));
}

/// Black-box structural check: an independent TIFF directory lister
/// reports tag 33723 with the UNDEFINED type and the exact byte count
/// we wrote. Skips (with a note) when the tool is not installed.
#[test]
fn tiffdump_lists_tag_33723_as_undefined() {
    use std::process::{Command, Stdio};
    if Command::new("tiffdump")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skipping: `tiffdump` not available");
        return;
    }
    let records = iptc_records();
    let img = gray(6, 2).with_metadata(Metadata::new().with_iptc(records.clone()));
    let bytes = encode(&img, &EncodeOptions::default()).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-iptc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("iptc.tif");
    std::fs::write(&path, &bytes).unwrap();
    let out = Command::new("tiffdump").arg(&path).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|l| l.contains("33723") || l.contains("0x83bb"))
        .unwrap_or_else(|| panic!("tag 33723 listed:\n{text}"));
    assert!(line.contains("UNDEFINED (7)"), "{line}");
    assert!(line.contains(&format!(" {}<", records.len())), "{line}");
    let _ = std::fs::remove_dir_all(&dir);
}
