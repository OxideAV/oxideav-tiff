//! `PageExtras` write-side round-trip: PageNumber (297) /
//! NewSubfileType bits, auxiliary Exif (34665) / GPS (34853) child
//! IFDs, and the SubIFDs (330) child image tree.
//!
//! The child-IFD *mechanism* under test is plain TIFF 6.0 §2 IFD
//! structure — an entry count, ascending-tag 12-/20-byte entries with
//! inline or out-of-line values, and a zero next-IFD pointer, reached
//! through a LONG / LONG8 offset in the parent. The aux entries are
//! caller-supplied and transported verbatim (this crate interprets no
//! Exif/GPS semantics; the tag numbers are registered-identifier
//! facts). Round-trips walk the written bytes with the crate's own
//! public `parse_header` / `parse_ifd` and compare tag / type / count /
//! value-bytes exactly; SubIFD child images decode through
//! `decode_tiff_at` and must match a standalone encode of the same
//! page. `tiffinfo` (black-box) is not used here — the aux-IFD tests
//! in `encode_imagemagick_validators.rs` stay focused on baseline
//! structures it prints.

use oxideav_tiff::ifd::{find, parse_header, parse_ifd, ByteOrder, Entry, TiffVariant};
use oxideav_tiff::types::{TAG_EXIF_IFD, TAG_GPS_IFD, TAG_NEW_SUBFILE_TYPE, TAG_SUB_IFDS};
use oxideav_tiff::{
    decode_tiff, decode_tiff_at, encode_tiff, encode_tiff_multi, AuxIfdEntry, EncodePage,
    EncodePixelFormat, PageExtras, TiffCompression,
};

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

fn ramp(w: u32, h: u32) -> Vec<u8> {
    (0..w * h).map(|i| (i * 5) as u8).collect()
}

/// Read a child IFD through the crate's own public parser and return
/// its entries.
fn read_child(file: &[u8], offset: u64) -> (Vec<Entry>, u64, ByteOrder, TiffVariant) {
    let hdr = parse_header(file).unwrap();
    let (entries, next) = parse_ifd(file, hdr.byte_order, hdr.variant, offset).unwrap();
    (entries, next, hdr.byte_order, hdr.variant)
}

#[test]
fn page_number_and_subfile_bits_roundtrip() {
    let px = ramp(8, 8);
    let extras = PageExtras {
        page_number: Some((2, 9)),
        multi_page: true,
        reduced_resolution: true,
        ..Default::default()
    };
    let file = encode_tiff(&gray_page(&px, 8, 8, extras)).unwrap();
    let hdr = parse_header(&file).unwrap();
    let (entries, _) = parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let bo = hdr.byte_order;
    let pn = find(&entries, 297).expect("PageNumber written");
    assert_eq!(pn.as_u32_vec(bo).unwrap(), vec![2, 9]);
    let nst = find(&entries, TAG_NEW_SUBFILE_TYPE)
        .unwrap()
        .as_u32(bo)
        .unwrap();
    assert_eq!(nst & 0b11, 0b11, "bits 0 (reduced) + 1 (multi-page) set");
    // The page still decodes normally.
    let img = decode_tiff(&file).unwrap();
    assert_eq!(img.frame.planes[0].data, px);
}

#[test]
fn default_extras_write_no_new_fields() {
    let px = ramp(8, 8);
    let file = encode_tiff(&gray_page(&px, 8, 8, PageExtras::default())).unwrap();
    let hdr = parse_header(&file).unwrap();
    let (entries, _) = parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    for tag in [297, TAG_SUB_IFDS, TAG_EXIF_IFD, TAG_GPS_IFD] {
        assert!(find(&entries, tag).is_none(), "tag {tag} must be absent");
    }
    let nst = find(&entries, TAG_NEW_SUBFILE_TYPE)
        .unwrap()
        .as_u32(hdr.byte_order)
        .unwrap();
    assert_eq!(nst, 0);
}

#[test]
fn aux_exif_and_gps_ifds_roundtrip_verbatim() {
    // A spread of §2 value shapes: inline SHORT, inline LONG, an
    // out-of-line ASCII string, an out-of-line RATIONAL triple, and
    // UNDEFINED bytes. Tags are arbitrary (transported verbatim);
    // supplied deliberately out of order to prove the writer sorts.
    let ascii = b"OxideAV test string\0";
    let rats: Vec<u8> = [(35u32, 1u32), (57, 1), (30, 1)]
        .iter()
        .flat_map(|(n, d)| {
            let mut v = n.to_le_bytes().to_vec();
            v.extend_from_slice(&d.to_le_bytes());
            v
        })
        .collect();
    let undef = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x01];
    let exif_entries = [
        AuxIfdEntry {
            tag: 40961,
            field_type: 3, // SHORT
            count: 1,
            value: &1u16.to_le_bytes(),
        },
        AuxIfdEntry {
            tag: 36864,
            field_type: 7, // UNDEFINED
            count: 4,
            value: b"0232",
        },
        AuxIfdEntry {
            tag: 37500,
            field_type: 7,
            count: undef.len() as u64,
            value: &undef,
        },
    ];
    let gps_entries = [
        AuxIfdEntry {
            tag: 2,
            field_type: 5, // RATIONAL × 3
            count: 3,
            value: &rats,
        },
        AuxIfdEntry {
            tag: 1,
            field_type: 2, // ASCII
            count: ascii.len() as u64,
            value: ascii,
        },
    ];
    let px = ramp(16, 8);
    let extras = PageExtras {
        exif_ifd: Some(&exif_entries),
        gps_ifd: Some(&gps_entries),
        ..Default::default()
    };
    for bigtiff in [false, true] {
        let page = EncodePage {
            bigtiff,
            ..gray_page(&px, 16, 8, extras.clone())
        };
        let file = encode_tiff(&page).unwrap();
        // The image still decodes.
        assert_eq!(decode_tiff(&file).unwrap().frame.planes[0].data, px);
        let hdr = parse_header(&file).unwrap();
        let (entries, _) =
            parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
        let bo = hdr.byte_order;

        // --- Exif child ---
        let exif_off = find(&entries, TAG_EXIF_IFD)
            .expect("Exif pointer written")
            .as_u64_vec(bo)
            .unwrap()[0];
        let (child, next, ..) = read_child(&file, exif_off);
        assert_eq!(next, 0, "child IFD is not on the next-IFD chain");
        assert_eq!(child.len(), 3);
        // Ascending order: 36864 < 37500 < 40961.
        let tags: Vec<u16> = child.iter().map(|e| e.tag).collect();
        assert_eq!(tags, vec![36864, 37500, 40961]);
        let e = find(&child, 40961).unwrap();
        assert_eq!((e.field_type, e.count), (3, 1));
        assert_eq!(e.as_u32(bo).unwrap(), 1);
        let e = find(&child, 37500).unwrap();
        assert_eq!((e.field_type, e.count as usize), (7, undef.len()));
        assert_eq!(e.data, undef.to_vec(), "UNDEFINED bytes verbatim");
        let e = find(&child, 36864).unwrap();
        assert_eq!(e.data, b"0232".to_vec());

        // --- GPS child ---
        let gps_off = find(&entries, TAG_GPS_IFD)
            .expect("GPS pointer written")
            .as_u64_vec(bo)
            .unwrap()[0];
        let (child, next, ..) = read_child(&file, gps_off);
        assert_eq!(next, 0);
        let tags: Vec<u16> = child.iter().map(|e| e.tag).collect();
        assert_eq!(tags, vec![1, 2], "writer must sort supplied entries");
        let asc = find(&child, 1).unwrap();
        assert_eq!((asc.field_type, asc.count as usize), (2, ascii.len()));
        assert_eq!(asc.data, ascii.to_vec(), "out-of-line ASCII verbatim");
        let rat = find(&child, 2).unwrap();
        assert_eq!((rat.field_type, rat.count), (5, 3));
        assert_eq!(
            rat.as_f64_vec(bo).unwrap(),
            vec![35.0, 57.0, 30.0],
            "RATIONAL values survive"
        );
    }
}

#[test]
fn aux_ifd_validation_rejects_bad_entries() {
    let px = ramp(4, 4);
    let mk = |entries: &'static [AuxIfdEntry<'static>]| PageExtras {
        exif_ifd: Some(entries),
        ..Default::default()
    };
    // Size mismatch: SHORT count 2 needs 4 bytes.
    static BAD_SIZE: [AuxIfdEntry<'static>; 1] = [AuxIfdEntry {
        tag: 1,
        field_type: 3,
        count: 2,
        value: &[0, 0],
    }];
    assert!(encode_tiff(&gray_page(&px, 4, 4, mk(&BAD_SIZE))).is_err());
    // Unknown field type.
    static BAD_TYPE: [AuxIfdEntry<'static>; 1] = [AuxIfdEntry {
        tag: 1,
        field_type: 99,
        count: 1,
        value: &[0],
    }];
    assert!(encode_tiff(&gray_page(&px, 4, 4, mk(&BAD_TYPE))).is_err());
    // Duplicate tags.
    static DUP: [AuxIfdEntry<'static>; 2] = [
        AuxIfdEntry {
            tag: 7,
            field_type: 3,
            count: 1,
            value: &[1, 0],
        },
        AuxIfdEntry {
            tag: 7,
            field_type: 3,
            count: 1,
            value: &[2, 0],
        },
    ];
    assert!(encode_tiff(&gray_page(&px, 4, 4, mk(&DUP))).is_err());
    // Empty child IFD.
    static EMPTY: [AuxIfdEntry<'static>; 0] = [];
    assert!(encode_tiff(&gray_page(&px, 4, 4, mk(&EMPTY))).is_err());
    // BigTIFF-only type on a classic page.
    static LONG8: [AuxIfdEntry<'static>; 1] = [AuxIfdEntry {
        tag: 1,
        field_type: 16,
        count: 1,
        value: &[0; 8],
    }];
    assert!(encode_tiff(&gray_page(&px, 4, 4, mk(&LONG8))).is_err());
}

#[test]
fn sub_ifds_tree_roundtrips_and_decodes() {
    // Main image + two reduced-resolution SubIFD children (tag 330),
    // one of which nests its own child — exercising the recursive
    // planner, the out-of-line offsets array (2 children × LONG = 8
    // bytes > classic inline 4), and decode_tiff_at.
    let main_px = ramp(32, 32);
    let thumb_px = ramp(8, 8);
    let micro_px = ramp(4, 4);

    let micro = EncodePage {
        extras: PageExtras {
            reduced_resolution: true,
            ..Default::default()
        },
        ..gray_page(&micro_px, 4, 4, PageExtras::default())
    };
    let nested_children = [micro];
    let thumb_a = EncodePage {
        compression: TiffCompression::Lzw,
        extras: PageExtras {
            reduced_resolution: true,
            sub_ifds: &nested_children,
            ..Default::default()
        },
        ..gray_page(&thumb_px, 8, 8, PageExtras::default())
    };
    let thumb_b = EncodePage {
        compression: TiffCompression::Deflate,
        extras: PageExtras {
            reduced_resolution: true,
            ..Default::default()
        },
        ..gray_page(&thumb_px, 8, 8, PageExtras::default())
    };
    let children = [thumb_a, thumb_b];
    let main = gray_page(
        &main_px,
        32,
        32,
        PageExtras {
            sub_ifds: &children,
            ..Default::default()
        },
    );
    let file = encode_tiff(&main).unwrap();

    // The main chain is unaffected: one page, decodes normally.
    assert_eq!(decode_tiff(&file).unwrap().frame.planes[0].data, main_px);
    let hdr = parse_header(&file).unwrap();
    let (entries, next) =
        parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    assert_eq!(next, 0, "children are not chained pages");
    let bo = hdr.byte_order;
    let subs = find(&entries, TAG_SUB_IFDS)
        .expect("SubIFDs written")
        .as_u64_vec(bo)
        .unwrap();
    assert_eq!(subs.len(), 2);

    // Child A: LZW thumb with its own nested child.
    let a = decode_tiff_at(&file, subs[0]).unwrap();
    assert_eq!((a.width, a.height), (8, 8));
    assert_eq!(a.frame.planes[0].data, thumb_px);
    let (a_entries, a_next, ..) = read_child(&file, subs[0]);
    assert_eq!(a_next, 0);
    let nst = find(&a_entries, TAG_NEW_SUBFILE_TYPE)
        .unwrap()
        .as_u32(bo)
        .unwrap();
    assert_eq!(nst & 1, 1, "child marked reduced-resolution");
    let nested = find(&a_entries, TAG_SUB_IFDS)
        .expect("nested SubIFDs")
        .as_u64_vec(bo)
        .unwrap();
    assert_eq!(nested.len(), 1);
    let micro_dec = decode_tiff_at(&file, nested[0]).unwrap();
    assert_eq!(micro_dec.frame.planes[0].data, micro_px);

    // Child B: Deflate thumb.
    let b = decode_tiff_at(&file, subs[1]).unwrap();
    assert_eq!(b.frame.planes[0].data, thumb_px);

    // Every child must byte-decode identically to a standalone encode
    // of the same page (the child planner is the page planner).
    let standalone = encode_tiff(&gray_page(&thumb_px, 8, 8, PageExtras::default())).unwrap();
    let sd = decode_tiff(&standalone).unwrap();
    assert_eq!(sd.frame.planes[0].data, b.frame.planes[0].data);
}

#[test]
fn sub_ifds_single_child_stays_inline_and_bigtiff_composes() {
    let main_px = ramp(16, 16);
    let thumb_px = ramp(4, 4);
    for bigtiff in [false, true] {
        let child = EncodePage {
            extras: PageExtras {
                reduced_resolution: true,
                ..Default::default()
            },
            ..gray_page(&thumb_px, 4, 4, PageExtras::default())
        };
        let children = [child];
        let main = EncodePage {
            bigtiff,
            ..gray_page(
                &main_px,
                16,
                16,
                PageExtras {
                    sub_ifds: &children,
                    page_number: Some((0, 1)),
                    ..Default::default()
                },
            )
        };
        let file = encode_tiff(&main).unwrap();
        let hdr = parse_header(&file).unwrap();
        let (entries, _) =
            parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
        let subs = find(&entries, TAG_SUB_IFDS)
            .unwrap()
            .as_u64_vec(hdr.byte_order)
            .unwrap();
        assert_eq!(subs.len(), 1);
        let t = decode_tiff_at(&file, subs[0]).unwrap();
        assert_eq!(t.frame.planes[0].data, thumb_px, "bigtiff={bigtiff}");
    }
}

#[test]
fn sub_ifd_depth_cap_enforced() {
    // Build a nested chain deeper than MAX_SUB_IFD_DEPTH (8) with
    // explicit levels; each level's page borrows the level below.
    let px = ramp(4, 4);
    let leaf = gray_page(&px, 4, 4, PageExtras::default());
    macro_rules! wrap {
        ($inner:ident) => {
            [gray_page(
                &px,
                4,
                4,
                PageExtras {
                    sub_ifds: &$inner,
                    ..Default::default()
                },
            )]
        };
    }
    let l1 = [leaf];
    let l2 = wrap!(l1);
    let l3 = wrap!(l2);
    let l4 = wrap!(l3);
    let l5 = wrap!(l4);
    let l6 = wrap!(l5);
    let l7 = wrap!(l6);
    let l8 = wrap!(l7);
    let l9 = wrap!(l8);
    let l10 = wrap!(l9);
    // Depth 9 nesting below the root: must exceed the cap.
    let root = gray_page(
        &px,
        4,
        4,
        PageExtras {
            sub_ifds: &l10,
            ..Default::default()
        },
    );
    assert!(
        encode_tiff(&root).is_err(),
        "SubIFDs deeper than the cap must be rejected"
    );
    // A shallow tree stays fine.
    let ok_root = gray_page(
        &px,
        4,
        4,
        PageExtras {
            sub_ifds: &l3,
            ..Default::default()
        },
    );
    assert!(encode_tiff(&ok_root).is_ok());
}

#[test]
fn multi_page_chain_with_extras_and_children() {
    // Two chained pages, each carrying a PageNumber and page 0 carrying
    // an Exif child — the chain, the child, and both rasters must all
    // survive together.
    let p0 = ramp(8, 8);
    let p1 = ramp(8, 8);
    static EXIF: [AuxIfdEntry<'static>; 1] = [AuxIfdEntry {
        tag: 40961,
        field_type: 3,
        count: 1,
        value: &[1, 0],
    }];
    let pages = [
        gray_page(
            &p0,
            8,
            8,
            PageExtras {
                page_number: Some((0, 2)),
                multi_page: true,
                exif_ifd: Some(&EXIF),
                ..Default::default()
            },
        ),
        gray_page(
            &p1,
            8,
            8,
            PageExtras {
                page_number: Some((1, 2)),
                multi_page: true,
                ..Default::default()
            },
        ),
    ];
    let file = encode_tiff_multi(&pages).unwrap();
    let all = oxideav_tiff::decode_tiff_all(&file).unwrap();
    assert_eq!(all.len(), 2, "Exif child must not join the page chain");
    let hdr = parse_header(&file).unwrap();
    let (e0, next0) = parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    assert_ne!(next0, 0);
    let (e1, next1) = parse_ifd(&file, hdr.byte_order, hdr.variant, next0).unwrap();
    assert_eq!(next1, 0);
    let bo = hdr.byte_order;
    assert_eq!(find(&e0, 297).unwrap().as_u32_vec(bo).unwrap(), vec![0, 2]);
    assert_eq!(find(&e1, 297).unwrap().as_u32_vec(bo).unwrap(), vec![1, 2]);
    let exif_off = find(&e0, TAG_EXIF_IFD).unwrap().as_u64_vec(bo).unwrap()[0];
    let (child, ..) = read_child(&file, exif_off);
    assert_eq!(child.len(), 1);
    assert_eq!(child[0].tag, 40961);
}

// ---------------------------------------------------------------------------
// Foreign-writer pointer types + BigTIFF array spill
// ---------------------------------------------------------------------------

/// Hand-build a classic-II file whose SubIFDs entry uses field type 13
/// ("IFD" — the registered 4-byte child-IFD offset code some writers
/// use instead of LONG). The reader must accept it exactly like LONG.
#[test]
fn foreign_subifd_pointer_typed_ifd13_reads() {
    let mut f: Vec<u8> = Vec::new();
    f.extend_from_slice(b"II");
    f.extend_from_slice(&42u16.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes()); // patched below
                                              // Main 2x2 gray pixels + child 1x1 gray pixel.
    let main_px = [10u8, 20, 30, 40];
    let child_px = [200u8];
    let main_px_off = f.len() as u32;
    f.extend_from_slice(&main_px);
    let child_px_off = f.len() as u32;
    f.extend_from_slice(&child_px);
    f.push(0); // pad to even
               // Child IFD (offset known before main IFD since it comes first).
    let child_ifd_off = f.len() as u32;
    let child_entries: [(u16, u16, u32, u32); 8] = [
        (256, 3, 1, 1),            // width 1
        (257, 3, 1, 1),            // height 1
        (258, 3, 1, 8),            // bits 8
        (259, 3, 1, 1),            // no compression
        (262, 3, 1, 1),            // BlackIsZero
        (273, 4, 1, child_px_off), // strip offset
        (278, 3, 1, 1),            // rows/strip
        (279, 4, 1, child_px.len() as u32),
    ];
    f.extend_from_slice(&(child_entries.len() as u16).to_le_bytes());
    for (tag, typ, count, value) in child_entries {
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&typ.to_le_bytes());
        f.extend_from_slice(&count.to_le_bytes());
        if typ == 3 {
            f.extend_from_slice(&(value as u16).to_le_bytes());
            f.extend_from_slice(&0u16.to_le_bytes());
        } else {
            f.extend_from_slice(&value.to_le_bytes());
        }
    }
    f.extend_from_slice(&0u32.to_le_bytes()); // child next IFD
                                              // Main IFD with tag 330 typed 13 (IFD).
    let main_ifd_off = f.len() as u32;
    let main_entries: [(u16, u16, u32, u32); 9] = [
        (256, 3, 1, 2),
        (257, 3, 1, 2),
        (258, 3, 1, 8),
        (259, 3, 1, 1),
        (262, 3, 1, 1),
        (273, 4, 1, main_px_off),
        (278, 3, 1, 2),
        (279, 4, 1, main_px.len() as u32),
        (330, 13, 1, child_ifd_off), // SubIFDs, field type IFD (13)
    ];
    f.extend_from_slice(&(main_entries.len() as u16).to_le_bytes());
    for (tag, typ, count, value) in main_entries {
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&typ.to_le_bytes());
        f.extend_from_slice(&count.to_le_bytes());
        if typ == 3 {
            f.extend_from_slice(&(value as u16).to_le_bytes());
            f.extend_from_slice(&0u16.to_le_bytes());
        } else {
            f.extend_from_slice(&value.to_le_bytes());
        }
    }
    f.extend_from_slice(&0u32.to_le_bytes());
    f[4..8].copy_from_slice(&main_ifd_off.to_le_bytes());

    let main = decode_tiff(&f).unwrap();
    assert_eq!(main.frame.planes[0].data, main_px.to_vec());
    let hdr = parse_header(&f).unwrap();
    let (entries, _) = parse_ifd(&f, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let sub = find(&entries, TAG_SUB_IFDS).expect("tag 330 present");
    assert_eq!(sub.field_type, 13, "field type IFD");
    let offs = sub
        .as_u64_vec(hdr.byte_order)
        .expect("type 13 reads as offset");
    assert_eq!(offs, vec![child_ifd_off as u64]);
    let child = decode_tiff_at(&f, offs[0]).unwrap();
    assert_eq!(child.frame.planes[0].data, child_px.to_vec());
}

#[test]
fn bigtiff_two_sub_ifds_spill_out_of_line() {
    // Two children × LONG8 = 16 bytes > the BigTIFF 8-byte value slot,
    // so tag 330 must reference an out-of-line offsets array.
    let main_px = ramp(16, 16);
    let t1 = ramp(8, 8);
    let t2: Vec<u8> = ramp(8, 8).iter().map(|b| b.wrapping_add(7)).collect();
    let c1 = EncodePage {
        bigtiff: true,
        ..gray_page(&t1, 8, 8, PageExtras::default())
    };
    let c2 = EncodePage {
        bigtiff: true,
        compression: TiffCompression::Zstd,
        ..gray_page(&t2, 8, 8, PageExtras::default())
    };
    let children = [c1, c2];
    let main = EncodePage {
        bigtiff: true,
        ..gray_page(
            &main_px,
            16,
            16,
            PageExtras {
                sub_ifds: &children,
                ..Default::default()
            },
        )
    };
    let file = encode_tiff(&main).unwrap();
    assert_eq!(decode_tiff(&file).unwrap().frame.planes[0].data, main_px);
    let hdr = parse_header(&file).unwrap();
    let (entries, _) = parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
    let sub = find(&entries, TAG_SUB_IFDS).unwrap();
    assert_eq!(sub.count, 2);
    let offs = sub.as_u64_vec(hdr.byte_order).unwrap();
    assert_eq!(offs.len(), 2);
    assert_eq!(
        decode_tiff_at(&file, offs[0]).unwrap().frame.planes[0].data,
        t1
    );
    assert_eq!(
        decode_tiff_at(&file, offs[1]).unwrap().frame.planes[0].data,
        t2
    );
}

// ---------------------------------------------------------------------------
// Resolution + §8 ASCII metadata
// ---------------------------------------------------------------------------

#[test]
fn resolution_and_ascii_metadata_roundtrip() {
    use oxideav_tiff::PageResolution;
    let px = ramp(8, 8);
    let extras = PageExtras {
        resolution: Some(PageResolution {
            x: (300, 1),
            y: (600, 2),
            unit: 2,
        }),
        description: Some("An eight by eight ramp"),
        software: Some("oxideav-tiff test"),
        date_time: Some("2026:07:04 12:34:56"),
        artist: Some("OxideAV"),
        copyright: Some("Copyright test notice"),
        ..Default::default()
    };
    for bigtiff in [false, true] {
        let page = EncodePage {
            bigtiff,
            ..gray_page(&px, 8, 8, extras.clone())
        };
        let file = encode_tiff(&page).unwrap();
        assert_eq!(decode_tiff(&file).unwrap().frame.planes[0].data, px);
        let hdr = parse_header(&file).unwrap();
        let (entries, _) =
            parse_ifd(&file, hdr.byte_order, hdr.variant, hdr.first_ifd_offset).unwrap();
        let bo = hdr.byte_order;
        // Ascending tag order held with the new insertions.
        assert!(entries.windows(2).all(|w| w[0].tag < w[1].tag));
        let xr = find(&entries, 282).unwrap();
        assert_eq!((xr.field_type, xr.count), (5, 1));
        assert_eq!(xr.as_f64_vec(bo).unwrap(), vec![300.0]);
        let yr = find(&entries, 283).unwrap();
        assert_eq!(yr.as_f64_vec(bo).unwrap(), vec![300.0], "600/2 = 300");
        assert_eq!(find(&entries, 296).unwrap().as_u32(bo).unwrap(), 2);
        for (tag, text) in [
            (270u16, "An eight by eight ramp"),
            (305, "oxideav-tiff test"),
            (306, "2026:07:04 12:34:56"),
            (315, "OxideAV"),
            (33432, "Copyright test notice"),
        ] {
            let e = find(&entries, tag).unwrap_or_else(|| panic!("tag {tag} missing"));
            assert_eq!(e.field_type, 2, "tag {tag} is ASCII");
            assert_eq!(
                e.count as usize,
                text.len() + 1,
                "tag {tag} count = len + NUL"
            );
            let mut want = text.as_bytes().to_vec();
            want.push(0);
            assert_eq!(e.data, want, "tag {tag} NUL-terminated value");
        }
    }
}

#[test]
fn metadata_validation_rejects_bad_values() {
    use oxideav_tiff::PageResolution;
    let px = ramp(4, 4);
    // Bad resolution unit.
    let bad_unit = PageExtras {
        resolution: Some(PageResolution {
            x: (72, 1),
            y: (72, 1),
            unit: 4,
        }),
        ..Default::default()
    };
    assert!(encode_tiff(&gray_page(&px, 4, 4, bad_unit)).is_err());
    // Zero denominator.
    let bad_den = PageExtras {
        resolution: Some(PageResolution {
            x: (72, 0),
            y: (72, 1),
            unit: 2,
        }),
        ..Default::default()
    };
    assert!(encode_tiff(&gray_page(&px, 4, 4, bad_den)).is_err());
    // Malformed DateTime shapes.
    for dt in ["2026-07-04 12:34:56", "2026:07:04", "2026:07:04 12:34:5"] {
        let bad_dt = PageExtras {
            date_time: Some(dt),
            ..Default::default()
        };
        assert!(
            encode_tiff(&gray_page(&px, 4, 4, bad_dt)).is_err(),
            "DateTime {dt:?} must reject"
        );
    }
    // Non-ASCII and embedded-NUL strings.
    let non_ascii = PageExtras {
        artist: Some("Karpelès"),
        ..Default::default()
    };
    assert!(encode_tiff(&gray_page(&px, 4, 4, non_ascii)).is_err());
    let embedded_nul = PageExtras {
        software: Some("abc\0def"),
        ..Default::default()
    };
    assert!(encode_tiff(&gray_page(&px, 4, 4, embedded_nul)).is_err());
}
