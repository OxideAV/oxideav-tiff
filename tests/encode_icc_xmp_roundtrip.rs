//! Encode → decode round-trip of the two registered opaque metadata
//! payloads: the XMP packet (tag 700, `PageExtras::xmp`) and the
//! embedded ICC profile (tag 34675 InterColorProfile,
//! `PageExtras::icc_profile`) — per
//! `docs/image/tiff/tiff-icc-xmp-tags.md`. Both must come back
//! byte-for-byte across every layout the encoder can produce: classic
//! TIFF + BigTIFF, single strip / multi-strip / tiled / planar, and
//! per page over the multi-page chain. Also proves the IFD keeps the
//! §2 ascending-tag order with the new tags interleaved (700 between
//! 532 and 33432; 34675 between 34665 and 34853) and that the
//! encoder's precise ICC integrity rejections fire.

use oxideav_tiff::ifd::{parse_header, parse_ifd};
use oxideav_tiff::{
    decode_tiff, decode_tiff_all_pages, encode_tiff, encode_tiff_multi, AuxIfdEntry, EncodePage,
    EncodePixelFormat, PageExtras, TiffCompression,
};

/// A structurally valid ICC profile: 128-byte header (big-endian size
/// field at +0, `acsp` signature at +36) + zero-entry tag table, with
/// a marker pattern after the header so byte-exact transport is
/// provable.
fn minimal_icc(total_len: usize) -> Vec<u8> {
    assert!(total_len >= 132);
    let mut p = vec![0u8; total_len];
    p[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
    p[36..40].copy_from_slice(b"acsp");
    for (i, b) in p[128..].iter_mut().enumerate() {
        *b = (i % 249) as u8;
    }
    p
}

fn xmp_packet(marker: &str) -> Vec<u8> {
    format!(
        "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF \
         xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
         <rdf:Description rdf:about=\"{marker}\"/></rdf:RDF>\
         </x:xmpmeta><?xpacket end=\"w\"?>"
    )
    .into_bytes()
}

fn gray_page<'a>(pixels: &'a [u8], w: u32, h: u32, extras: PageExtras<'a>) -> EncodePage<'a> {
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

fn assert_both(tiff: &[u8], xmp: &[u8], icc: &[u8]) {
    let d = decode_tiff(tiff).expect("decode");
    assert_eq!(d.metadata.xmp.as_deref(), Some(xmp));
    assert_eq!(d.metadata.icc_profile.as_deref(), Some(icc));
}

#[test]
fn classic_single_strip_round_trip() {
    let px: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let xmp = xmp_packet("classic");
    let icc = minimal_icc(300);
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let tiff = encode_tiff(&gray_page(&px, 8, 8, extras)).expect("encode");
    assert_both(&tiff, &xmp, &icc);
}

#[test]
fn bigtiff_round_trip() {
    let px: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let xmp = xmp_packet("bigtiff");
    let icc = minimal_icc(512);
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let mut page = gray_page(&px, 8, 8, extras);
    page.bigtiff = true;
    let tiff = encode_tiff(&page).expect("encode");
    assert_both(&tiff, &xmp, &icc);
}

#[test]
fn multi_strip_round_trip() {
    let px: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
    let xmp = xmp_packet("strips");
    let icc = minimal_icc(200);
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        rows_per_strip: Some(3),
        ..Default::default()
    };
    let mut page = gray_page(&px, 16, 16, extras);
    page.compression = TiffCompression::Lzw;
    let tiff = encode_tiff(&page).expect("encode");
    assert_both(&tiff, &xmp, &icc);
}

#[test]
fn tiled_round_trip() {
    let px: Vec<u8> = (0..(48 * 48 * 3) as u32).map(|i| i as u8).collect();
    let xmp = xmp_packet("tiled");
    let icc = minimal_icc(400);
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let page = EncodePage {
        width: 48,
        height: 48,
        kind: EncodePixelFormat::Rgb24 { pixels: &px },
        compression: TiffCompression::Deflate,
        predictor: true,
        planar: false,
        tiling: Some((16, 16)),
        bigtiff: false,
        extras,
    };
    let tiff = encode_tiff(&page).expect("encode");
    assert_both(&tiff, &xmp, &icc);
}

#[test]
fn planar_round_trip() {
    let px: Vec<u8> = (0..(8 * 8 * 3) as u32).map(|i| (i * 3) as u8).collect();
    let xmp = xmp_packet("planar");
    let icc = minimal_icc(256);
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Rgb24 { pixels: &px },
        compression: TiffCompression::PackBits,
        predictor: false,
        planar: true,
        tiling: None,
        bigtiff: false,
        extras,
    };
    let tiff = encode_tiff(&page).expect("encode");
    assert_both(&tiff, &xmp, &icc);
}

#[test]
fn multi_page_distinct_payloads_per_page() {
    // Three pages: page 0 carries XMP+ICC, page 1 different XMP only,
    // page 2 ICC only. Each page's payloads must stay with its IFD —
    // for classic and BigTIFF variants.
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let xmp0 = xmp_packet("page-zero");
    let icc0 = minimal_icc(160);
    let xmp1 = xmp_packet("page-one-with-longer-marker-text");
    let icc2 = minimal_icc(224);
    for bigtiff in [false, true] {
        let e0 = PageExtras {
            xmp: Some(&xmp0),
            icc_profile: Some(&icc0),
            ..Default::default()
        };
        let e1 = PageExtras {
            xmp: Some(&xmp1),
            ..Default::default()
        };
        let e2 = PageExtras {
            icc_profile: Some(&icc2),
            ..Default::default()
        };
        let mut pages = vec![
            gray_page(&px, 4, 4, e0),
            gray_page(&px, 4, 4, e1),
            gray_page(&px, 4, 4, e2),
        ];
        for p in &mut pages {
            p.bigtiff = bigtiff;
        }
        let tiff = encode_tiff_multi(&pages).expect("encode multi");
        let decoded = decode_tiff_all_pages(&tiff).expect("decode all");
        assert_eq!(decoded.len(), 3, "bigtiff={bigtiff}");
        assert_eq!(decoded[0].metadata.xmp.as_deref(), Some(xmp0.as_slice()));
        assert_eq!(
            decoded[0].metadata.icc_profile.as_deref(),
            Some(icc0.as_slice())
        );
        assert_eq!(decoded[1].metadata.xmp.as_deref(), Some(xmp1.as_slice()));
        assert_eq!(decoded[1].metadata.icc_profile, None);
        assert_eq!(decoded[2].metadata.xmp, None);
        assert_eq!(
            decoded[2].metadata.icc_profile.as_deref(),
            Some(icc2.as_slice())
        );
    }
}

#[test]
fn re_encode_preserves_decoded_payloads() {
    // Round-trip preservation on re-encode: decode a file carrying
    // both payloads, feed the decoded bytes back through the encoder,
    // decode again — byte-exact both hops.
    let px: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let xmp = xmp_packet("re-encode");
    let icc = minimal_icc(344);
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let first = encode_tiff(&gray_page(&px, 8, 8, extras)).expect("encode 1");
    let d1 = decode_tiff(&first).expect("decode 1");
    let xmp_back = d1.metadata.xmp.clone().expect("xmp survived hop 1");
    let icc_back = d1.metadata.icc_profile.clone().expect("icc survived hop 1");
    let extras2 = PageExtras {
        xmp: Some(&xmp_back),
        icc_profile: Some(&icc_back),
        ..Default::default()
    };
    let second = encode_tiff(&gray_page(&px, 8, 8, extras2)).expect("encode 2");
    let d2 = decode_tiff(&second).expect("decode 2");
    assert_eq!(d2.metadata.xmp.as_deref(), Some(xmp.as_slice()));
    assert_eq!(d2.metadata.icc_profile.as_deref(), Some(icc.as_slice()));
}

#[test]
fn ifd_tags_stay_ascending_with_full_extras() {
    // Load an IFD with everything at once — §8 ASCII fields,
    // resolution, page tags, Exif + GPS child IFDs, XMP and ICC — and
    // assert the §2 ascending-tag invariant on the written file. The
    // new tags must land 700 < 33432 (Copyright) < 34665 (Exif) <
    // 34675 (ICC) < 34853 (GPS).
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let xmp = xmp_packet("ordering");
    let icc = minimal_icc(148);
    let exif = [AuxIfdEntry {
        tag: 33434, // ExposureTime slot in the child IFD (opaque here)
        field_type: 5,
        count: 1,
        value: &[1, 0, 0, 0, 250, 0, 0, 0],
    }];
    let gps = [AuxIfdEntry {
        tag: 0,
        field_type: 1,
        count: 4,
        value: &[2, 3, 0, 0],
    }];
    let extras = PageExtras {
        page_number: Some((0, 1)),
        orientation: Some(1),
        description: Some("d"),
        software: Some("s"),
        artist: Some("a"),
        copyright: Some("c"),
        exif_ifd: Some(&exif),
        gps_ifd: Some(&gps),
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let tiff = encode_tiff(&gray_page(&px, 4, 4, extras)).expect("encode");
    let h = parse_header(&tiff).expect("header");
    let (entries, _next) =
        parse_ifd(&tiff, h.byte_order, h.variant, h.first_ifd_offset).expect("ifd");
    let tags: Vec<u16> = entries.iter().map(|e| e.tag).collect();
    let mut sorted = tags.clone();
    sorted.sort_unstable();
    assert_eq!(tags, sorted, "IFD entries must be ascending: {tags:?}");
    assert!(tags.contains(&700));
    assert!(tags.contains(&34675));
    let pos = |t: u16| tags.iter().position(|&x| x == t).unwrap();
    assert!(pos(700) < pos(33432));
    assert!(pos(33432) < pos(34665));
    assert!(pos(34665) < pos(34675));
    assert!(pos(34675) < pos(34853));
    // And the payloads still extract.
    assert_both(&tiff, &xmp, &icc);
}

#[test]
fn written_entry_types_match_the_registered_defaults() {
    // The doc's canonical field types: XMP = BYTE (1), ICC = UNDEFINED
    // (7). Check the on-disk entries directly.
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let xmp = xmp_packet("types");
    let icc = minimal_icc(132);
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let tiff = encode_tiff(&gray_page(&px, 4, 4, extras)).expect("encode");
    let h = parse_header(&tiff).expect("header");
    let (entries, _) = parse_ifd(&tiff, h.byte_order, h.variant, h.first_ifd_offset).expect("ifd");
    let xe = entries.iter().find(|e| e.tag == 700).expect("tag 700");
    assert_eq!(xe.field_type, 1); // BYTE
    assert_eq!(xe.count, xmp.len() as u64);
    let ie = entries.iter().find(|e| e.tag == 34675).expect("tag 34675");
    assert_eq!(ie.field_type, 7); // UNDEFINED
    assert_eq!(ie.count, icc.len() as u64);
    // Payload placement: out-of-line at an even offset, byte-exact.
    let off = h.byte_order.read_u32(&{
        // Re-locate the raw entry to read its offset slot: walk the
        // 12-byte entries at the IFD.
        let ifd = h.first_ifd_offset as usize;
        let n = h.byte_order.read_u16(&tiff[ifd..ifd + 2]) as usize;
        let mut slot = [0u8; 4];
        for i in 0..n {
            let base = ifd + 2 + i * 12;
            if h.byte_order.read_u16(&tiff[base..base + 2]) == 34675 {
                slot.copy_from_slice(&tiff[base + 8..base + 12]);
            }
        }
        slot
    }) as usize;
    assert_eq!(off % 2, 0, "ICC payload must sit at an even offset");
    assert_eq!(&tiff[off..off + icc.len()], icc.as_slice());
}

#[test]
fn icc_rejections_are_precise() {
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    // Shorter than the 128-byte header.
    let short = vec![0u8; 64];
    let extras = PageExtras {
        icc_profile: Some(&short),
        ..Default::default()
    };
    let err = encode_tiff(&gray_page(&px, 4, 4, extras)).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("128-byte ICC header"), "{msg}");
    // Size-field mismatch.
    let mut lying = minimal_icc(200);
    lying[0..4].copy_from_slice(&100u32.to_be_bytes());
    let extras = PageExtras {
        icc_profile: Some(&lying),
        ..Default::default()
    };
    let err = encode_tiff(&gray_page(&px, 4, 4, extras)).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("authoritative"), "{msg}");
    // Little-endian-swapped size field is a mismatch too (the field
    // is big-endian regardless of the file's byte order).
    let mut swapped = minimal_icc(200);
    let le = 200u32.to_le_bytes();
    swapped[0..4].copy_from_slice(&le);
    let extras = PageExtras {
        icc_profile: Some(&swapped),
        ..Default::default()
    };
    assert!(encode_tiff(&gray_page(&px, 4, 4, extras)).is_err());
}

#[test]
fn empty_xmp_rejected() {
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let extras = PageExtras {
        xmp: Some(&[]),
        ..Default::default()
    };
    let err = encode_tiff(&gray_page(&px, 4, 4, extras)).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("non-empty"), "{msg}");
}

#[test]
fn xmp_payload_is_opaque_bytes() {
    // The encoder must not impose the §2 ASCII rules on the packet:
    // arbitrary (non-ASCII, NUL-carrying) bytes transport verbatim.
    let px: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    let payload: Vec<u8> = vec![0xEF, 0xBB, 0xBF, 0x00, 0xFF, 0x80, 0x01];
    let extras = PageExtras {
        xmp: Some(&payload),
        ..Default::default()
    };
    let tiff = encode_tiff(&gray_page(&px, 4, 4, extras)).expect("encode");
    let d = decode_tiff(&tiff).expect("decode");
    assert_eq!(d.metadata.xmp.as_deref(), Some(payload.as_slice()));
}

#[test]
fn payloads_compose_with_ccitt_bilevel_page() {
    // The opaque payloads are layout-independent: attach both to a
    // CCITT T.6 bilevel page (a completely different strip pipeline).
    let row = [0b1010_1010u8];
    let px: Vec<u8> = row.iter().cycle().take(8).copied().collect();
    let xmp = xmp_packet("ccitt");
    let icc = minimal_icc(132);
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let page = EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Bilevel { pixels: &px },
        compression: TiffCompression::CcittT6,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras,
    };
    let tiff = encode_tiff(&page).expect("encode");
    assert_both(&tiff, &xmp, &icc);
}
