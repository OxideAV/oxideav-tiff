//! Per-page metadata over a multi-IFD chain
//! ([`oxideav_tiff::decode_tiff_all_pages`]).
//!
//! `decode_tiff_all` returns bare pixels; the pages variant carries
//! each IFD's [`oxideav_tiff::TiffMetadata`] so a caller can read the
//! §8 descriptive fields, page number and structural flags that differ
//! from page to page. Oracle: encode a 3-page chain with distinct
//! per-page PageExtras, then confirm every page reads its own metadata
//! back.

use oxideav_tiff::{
    decode_tiff_all, decode_tiff_all_pages, encode_tiff_multi, EncodePage, EncodePixelFormat,
    PageExtras, PageResolution, ResolutionUnit, TiffCompression,
};

fn page<'a>(w: u32, h: u32, pixels: &'a [u8], extras: PageExtras<'a>) -> EncodePage<'a> {
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
fn three_page_chain_carries_distinct_per_page_metadata() {
    let px: Vec<u8> = vec![0x40; 4];
    let pages = [
        page(
            2,
            2,
            &px,
            PageExtras {
                description: Some("first"),
                page_number: Some((0, 3)),
                multi_page: true,
                ..Default::default()
            },
        ),
        page(
            2,
            2,
            &px,
            PageExtras {
                description: Some("second"),
                page_number: Some((1, 3)),
                multi_page: true,
                software: Some("oxideav"),
                ..Default::default()
            },
        ),
        page(
            2,
            2,
            &px,
            PageExtras {
                description: Some("third"),
                page_number: Some((2, 3)),
                multi_page: true,
                resolution: Some(PageResolution {
                    x: (72, 1),
                    y: (72, 1),
                    unit: 2,
                }),
                ..Default::default()
            },
        ),
    ];
    let tiff = encode_tiff_multi(&pages).expect("encode multi");

    let all = decode_tiff_all_pages(&tiff).expect("decode pages");
    assert_eq!(all.len(), 3);

    assert_eq!(all[0].metadata.image_description.as_deref(), Some("first"));
    assert_eq!(all[0].metadata.page_number, Some((0, 3)));
    assert_eq!(all[0].metadata.software, None);

    assert_eq!(all[1].metadata.image_description.as_deref(), Some("second"));
    assert_eq!(all[1].metadata.page_number, Some((1, 3)));
    assert_eq!(all[1].metadata.software.as_deref(), Some("oxideav"));

    assert_eq!(all[2].metadata.image_description.as_deref(), Some("third"));
    assert_eq!(all[2].metadata.page_number, Some((2, 3)));
    assert_eq!(all[2].metadata.x_resolution, Some((72, 1)));
    assert_eq!(all[2].metadata.resolution_unit, Some(ResolutionUnit::Inch));

    // The pixel content matches the bare multi-page decode page-for-page.
    let bare = decode_tiff_all(&tiff).expect("decode bare");
    assert_eq!(bare.len(), all.len());
    for (b, p) in bare.iter().zip(&all) {
        assert_eq!(b.planes[0].data, p.frame.planes[0].data);
        assert_eq!((b.width, b.height), (p.width, p.height));
    }
}
