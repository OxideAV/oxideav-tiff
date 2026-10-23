//! Metadata blobs and multi-page files through the registry path
//! (core 0.1.40 `oxideav_core::blob`): the demuxer puts the first page
//! of each stream's ICC / Exif / XMP / IPTC on `CodecParameters::blobs`,
//! the decoder attaches every page's own blobs to its frame record and
//! returns one frame per page of its stream's geometry, and the encoder
//! writes the union — a frame blob overrides a stream blob of the same
//! kind. Proved with the registry API (`RuntimeContext` factories) and
//! cross-checked against the standalone API.

#![cfg(feature = "registry")]

use std::io::Cursor;

use oxideav_core::{
    BlobKind, CodecId, CodecParameters, CodecRegistry, Decoder, Error, Frame, MetadataBlob, Packet,
    PixelFormat, RuntimeContext, TimeBase, VideoFrame,
};
use oxideav_tiff::container::open_demuxer;
use oxideav_tiff::{
    EncodeOptions, Metadata, Palette, Plane, TiffImage, TiffPixelFormat, decode, decode_all,
    encode, encode_all, merge_blobs, metadata_from_blobs, metadata_to_blobs,
};

fn icc(len: usize) -> Vec<u8> {
    let mut p = vec![0u8; len];
    p[..4].copy_from_slice(&(len as u32).to_be_bytes());
    p[36..40].copy_from_slice(b"acsp");
    for (i, b) in p[128..].iter_mut().enumerate() {
        *b = (i * 13 % 251) as u8;
    }
    p
}

/// An Exif block: little-endian TIFF header, IFD0 → Exif IFD at 26
/// with `DateTimeOriginal` (0x9003, ASCII × 20).
fn exif(stamp: &str) -> Vec<u8> {
    assert_eq!(stamp.len(), 19);
    let mut v = Vec::new();
    v.extend_from_slice(b"II");
    v.extend_from_slice(&42u16.to_le_bytes());
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&34665u16.to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&26u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    // Exif IFD at 26: one entry, value at 44.
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0x9003u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&20u32.to_le_bytes());
    v.extend_from_slice(&44u32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(v.len(), 44);
    v.extend_from_slice(stamp.as_bytes());
    v.push(0);
    v
}

fn iptc(text: &str) -> Vec<u8> {
    let mut v = vec![0x1C, 0x02, 0x05];
    v.extend_from_slice(&(text.len() as u16).to_be_bytes());
    v.extend_from_slice(text.as_bytes());
    v
}

fn image(format: TiffPixelFormat, w: u32, h: u32, seed: u8) -> TiffImage {
    let bpp = format.bytes_per_pixel();
    let data: Vec<u8> = (0..w as usize * h as usize * bpp)
        .map(|i| (i as u8).wrapping_mul(3).wrapping_add(seed))
        .collect();
    let mut img = TiffImage::new(w, h, format, vec![Plane::new(w as usize * bpp, data)]).unwrap();
    if format == TiffPixelFormat::Pal8 {
        img.palette = Some(Palette::new(
            (0..=255u8).map(|i| [i, 255 - i, i / 2, 255]).collect(),
        ));
    }
    img
}

fn frames_of(pages: Vec<TiffImage>) -> Vec<oxideav_tiff::Frame> {
    pages
        .into_iter()
        .enumerate()
        .map(|(i, img)| oxideav_tiff::Frame::new(img, i as u32))
        .collect()
}

fn kinds(blobs: &[MetadataBlob]) -> Vec<&str> {
    blobs.iter().map(|b| b.kind.as_str()).collect()
}

/// Drain a decoder into frames.
fn drain(dec: &mut dyn Decoder) -> Vec<VideoFrame> {
    let mut out = Vec::new();
    loop {
        match dec.receive_frame() {
            Ok(Frame::Video(v)) => out.push(v),
            Ok(_) => panic!("video frames only"),
            Err(Error::NeedMore) | Err(Error::Eof) => return out,
            Err(e) => panic!("{e}"),
        }
    }
}

fn open(bytes: Vec<u8>) -> Box<dyn oxideav_core::Demuxer> {
    let codecs = CodecRegistry::new();
    open_demuxer(Box::new(Cursor::new(bytes)), &codecs).expect("open demuxer")
}

#[test]
fn bridges_map_the_four_payloads_in_well_known_order() {
    let md = Metadata::new()
        .with_xmp(b"<x:xmpmeta/>".to_vec())
        .with_iptc(iptc("a"))
        .with_icc(icc(140))
        .with_exif(exif("2026:10:17 10:00:00"));
    let blobs = metadata_to_blobs(&md);
    assert_eq!(kinds(&blobs), ["icc", "exif", "xmp", "iptc"]);
    assert!(blobs.iter().all(|b| b.mime.is_none()));
    assert_eq!(blobs[0].data, icc(140));
    assert_eq!(&blobs[1].data[..4], b"II*\0");
    assert_eq!(metadata_from_blobs(&blobs), md);
    // Unknown kinds are left alone; gamma has no blob.
    let mut extra = blobs.clone();
    extra.push(MetadataBlob::new(BlobKind::custom("png-text"), vec![1]));
    extra.push(MetadataBlob::new(BlobKind::COVER_ART, vec![2]).with_mime("image/png"));
    assert_eq!(metadata_from_blobs(&extra), md);
    assert!(metadata_to_blobs(&Metadata::new().with_gamma(2.2)).is_empty());
    // merge: frame first, stream fills the kinds the frame lacks.
    let frame = vec![MetadataBlob::new(BlobKind::XMP, b"frame".to_vec())];
    let stream = vec![
        MetadataBlob::new(BlobKind::XMP, b"stream".to_vec()),
        MetadataBlob::new(BlobKind::ICC, icc(132)),
    ];
    let merged = merge_blobs(&frame, &stream);
    assert_eq!(kinds(&merged), ["xmp", "icc"]);
    assert_eq!(merged[0].data, b"frame");
}

#[test]
fn video_frame_bridge_carries_blobs_and_palette_alpha_both_ways() {
    let img = image(TiffPixelFormat::Rgb24, 3, 2, 1)
        .with_metadata(Metadata::new().with_icc(icc(136)).with_iptc(iptc("bridge")));
    let frame: VideoFrame = img.clone().into();
    assert_eq!(kinds(&frame.blobs()), ["icc", "iptc"]);
    let mut params = CodecParameters::video(CodecId::new("tiff"));
    params.width = Some(3);
    params.height = Some(2);
    params.pixel_format = Some(PixelFormat::Rgb24);
    // Stream blobs fill the kinds the frame lacks.
    params.blobs = vec![
        MetadataBlob::new(BlobKind::ICC, icc(200)),
        MetadataBlob::new(BlobKind::XMP, b"<x/>".to_vec()),
    ];
    let back = TiffImage::from_video_frame(&frame, &params).unwrap();
    assert_eq!(back.metadata.icc, Some(icc(136)), "frame overrides stream");
    assert_eq!(back.metadata.xmp.as_deref(), Some(&b"<x/>"[..]));
    assert_eq!(back.metadata.iptc, Some(iptc("bridge")));
    assert_eq!(back.planes, img.planes);
    // No metadata → no blobs record at all.
    let plain: VideoFrame = image(TiffPixelFormat::Gray8, 2, 2, 0).into();
    assert!(plain.blobs().is_empty());
    assert_eq!(plain.image_planes().len(), 1);

    // A palette with a transparent entry travels on the alpha record
    // and comes back; the standalone encoder then refuses it by type
    // (a TIFF ColorMap holds no alpha).
    let mut pal = image(TiffPixelFormat::Pal8, 2, 1, 0);
    let mut entries = pal.palette.clone().unwrap().entries;
    entries[1][3] = 0;
    pal.palette = Some(Palette::new(entries));
    let f: VideoFrame = pal.clone().into();
    assert_eq!(f.palette_rgba(1).map(|c| c[3]), Some(0));
    assert_eq!(f.palette_rgba(2).map(|c| c[3]), Some(255));
    let mut p8 = CodecParameters::video(CodecId::new("tiff"));
    p8.width = Some(2);
    p8.height = Some(1);
    p8.pixel_format = Some(PixelFormat::Pal8);
    let back = TiffImage::from_video_frame(&f, &p8).unwrap();
    assert_eq!(back, pal);
    assert!(matches!(
        encode(&back, &EncodeOptions::default()),
        Err(oxideav_tiff::TiffError::Unsupported(_))
    ));
}

#[test]
fn demuxer_streams_first_page_blobs_and_decoder_attaches_each_pages_own() {
    let pages = vec![
        image(TiffPixelFormat::Gray8, 6, 4, 0).with_metadata(
            Metadata::new()
                .with_icc(icc(140))
                .with_xmp(b"<x:xmpmeta>page0</x:xmpmeta>".to_vec()),
        ),
        image(TiffPixelFormat::Gray8, 6, 4, 50).with_metadata(
            Metadata::new()
                .with_exif(exif("2026:10:17 11:11:11"))
                .with_iptc(iptc("page1")),
        ),
        image(TiffPixelFormat::Gray8, 6, 4, 100),
    ];
    let bytes = encode_all(&frames_of(pages.clone()), &EncodeOptions::default()).unwrap();

    let mut dmx = open(bytes.clone());
    assert_eq!(dmx.streams().len(), 1, "uniform geometry: one stream");
    let params = dmx.streams()[0].params.clone();
    assert_eq!(
        (params.width, params.height, params.pixel_format),
        (Some(6), Some(4), Some(PixelFormat::Gray8))
    );
    assert_eq!(kinds(&params.blobs), ["icc", "xmp"], "first page's blobs");
    assert_eq!(
        params.blob(&BlobKind::ICC).map(|b| &b.data),
        Some(&icc(140))
    );
    // Pre-blob surfaces stay.
    assert_eq!(dmx.attachments().len(), 2);

    let mut ctx = RuntimeContext::new();
    oxideav_tiff::register(&mut ctx);
    let mut dec = ctx.codecs.first_decoder(&params).unwrap();
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.stream_index, 0);
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    dec.send_packet(&pkt).unwrap();
    let frames = drain(dec.as_mut());
    assert_eq!(frames.len(), 3, "one frame per page");
    assert_eq!(frames[0].pts, Some(0));
    assert_eq!(frames[1].pts, Some(1));
    assert_eq!(frames[2].pts, Some(2));
    assert_eq!(kinds(&frames[0].blobs()), ["icc", "xmp"]);
    assert_eq!(kinds(&frames[1].blobs()), ["exif", "iptc"]);
    assert_eq!(
        frames[1].blob(&BlobKind::EXIF).map(|b| b.data),
        Some(exif("2026:10:17 11:11:11"))
    );
    assert_eq!(
        frames[1].blob(&BlobKind::IPTC).map(|b| b.data),
        Some(iptc("page1"))
    );
    assert!(frames[2].blobs().is_empty());
    // Pixels are the pages', in order; same as the standalone decode.
    let standalone = decode_all(&bytes).unwrap();
    for (f, s) in frames.iter().zip(&standalone) {
        assert_eq!(f.image_planes()[0].data, s.image.planes[0].data);
    }
    dec.flush().unwrap();
    assert!(matches!(dec.receive_frame(), Err(Error::Eof)));
}

#[test]
fn mixed_geometry_pages_become_one_stream_per_geometry_with_truthful_labels() {
    let pages = vec![
        image(TiffPixelFormat::Gray8, 4, 4, 0),
        image(TiffPixelFormat::Rgb24, 2, 2, 7),
        image(TiffPixelFormat::Gray8, 4, 4, 9),
        image(TiffPixelFormat::Gray16Le, 3, 1, 11)
            .with_metadata(Metadata::new().with_iptc(iptc("deep"))),
        image(TiffPixelFormat::Rgb24, 2, 2, 13),
    ];
    let bytes = encode_all(&frames_of(pages.clone()), &EncodeOptions::default()).unwrap();
    let mut dmx = open(bytes);
    let streams: Vec<_> = dmx.streams().to_vec();
    assert_eq!(streams.len(), 3);
    let geom = |s: &oxideav_core::StreamInfo| {
        (
            s.params.width.unwrap(),
            s.params.height.unwrap(),
            s.params.pixel_format.unwrap(),
        )
    };
    assert_eq!(geom(&streams[0]), (4, 4, PixelFormat::Gray8));
    assert_eq!(geom(&streams[1]), (2, 2, PixelFormat::Rgb24));
    assert_eq!(geom(&streams[2]), (3, 1, PixelFormat::Gray16Le));
    assert_eq!(kinds(&streams[2].params.blobs), ["iptc"]);
    assert!(streams[0].params.blobs.is_empty());
    for (i, s) in streams.iter().enumerate() {
        assert_eq!(s.index as usize, i);
    }

    let expect_pages: [&[usize]; 3] = [&[0, 2], &[1, 4], &[3]];
    for (si, s) in streams.iter().enumerate() {
        let pkt = dmx.next_packet().unwrap();
        assert_eq!(pkt.stream_index as usize, si);
        let mut dec = oxideav_tiff::make_decoder(&s.params).unwrap();
        dec.send_packet(&pkt).unwrap();
        let frames = drain(dec.as_mut());
        let got: Vec<i64> = frames.iter().map(|f| f.pts.unwrap()).collect();
        let want: Vec<i64> = expect_pages[si].iter().map(|&p| p as i64).collect();
        assert_eq!(got, want, "stream {si} returns its pages");
        let (w, h, pf) = geom(s);
        let bpp = TiffPixelFormat::try_from(pf).unwrap().bytes_per_pixel();
        for (f, &p) in frames.iter().zip(expect_pages[si]) {
            let plane = &f.image_planes()[0];
            assert_eq!(plane.stride, w as usize * bpp);
            assert_eq!(plane.data.len(), (w * h) as usize * bpp);
            assert_eq!(plane.data, pages[p].planes[0].data);
        }
    }
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));

    // A decoder built without a geometry returns every page.
    let bytes = encode_all(&frames_of(pages.clone()), &EncodeOptions::default()).unwrap();
    let bare = CodecParameters::video(CodecId::new("tiff"));
    let mut dec = oxideav_tiff::make_decoder(&bare).unwrap();
    let mut pkt = Packet::new(0, TimeBase::new(1, 1), bytes);
    pkt.pts = Some(10);
    dec.send_packet(&pkt).unwrap();
    let frames = drain(dec.as_mut());
    assert_eq!(frames.len(), 5);
    assert_eq!(frames[4].pts, Some(14), "packet pts + page index");
    // A geometry no page has falls back to every page too.
    let mut odd = bare.clone();
    odd.width = Some(99);
    odd.height = Some(99);
    odd.pixel_format = Some(PixelFormat::Cmyk);
    let mut dec = oxideav_tiff::make_decoder(&odd).unwrap();
    dec.send_packet(&pkt).unwrap();
    assert_eq!(drain(dec.as_mut()).len(), 5);
}

#[test]
fn a_later_page_the_decoder_does_not_support_is_left_out_not_fatal() {
    let pages = vec![
        image(TiffPixelFormat::Gray8, 4, 2, 0),
        image(TiffPixelFormat::Gray8, 4, 2, 5),
        image(TiffPixelFormat::Gray8, 4, 2, 9),
    ];
    let mut bytes = encode_all(&frames_of(pages), &EncodeOptions::default()).unwrap();
    // Walk the little-endian classic chain to page 1 and rewrite its
    // SamplesPerPixel (277) to 5: BlackIsZero with five 8-bit samples
    // is outside the decode table.
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let ifd0 = u32_at(&bytes, 4) as usize;
    let n0 = u16::from_le_bytes([bytes[ifd0], bytes[ifd0 + 1]]) as usize;
    let ifd1 = u32_at(&bytes, ifd0 + 2 + 12 * n0) as usize;
    let n1 = u16::from_le_bytes([bytes[ifd1], bytes[ifd1 + 1]]) as usize;
    let spp = (0..n1)
        .map(|i| ifd1 + 2 + 12 * i)
        .find(|&o| u16::from_le_bytes([bytes[o], bytes[o + 1]]) == 277)
        .expect("SamplesPerPixel entry on page 1");
    bytes[spp + 8] = 5;
    assert!(
        decode_all(&bytes).is_err(),
        "standalone decode_all is strict"
    );

    let mut dmx = open(bytes);
    assert_eq!(dmx.streams().len(), 1);
    let params = dmx.streams()[0].params.clone();
    let mut dec = oxideav_tiff::make_decoder(&params).unwrap();
    dec.send_packet(&dmx.next_packet().unwrap()).unwrap();
    let frames = drain(dec.as_mut());
    let got: Vec<i64> = frames.iter().map(|f| f.pts.unwrap()).collect();
    assert_eq!(got, [0, 2]);

    // When no page resolves the error is precise and immediate.
    let mut one = encode(
        &image(TiffPixelFormat::Gray8, 2, 2, 1),
        &EncodeOptions::default(),
    )
    .unwrap();
    let ifd0 = u32_at(&one, 4) as usize;
    let n0 = u16::from_le_bytes([one[ifd0], one[ifd0 + 1]]) as usize;
    let spp = (0..n0)
        .map(|i| ifd0 + 2 + 12 * i)
        .find(|&o| u16::from_le_bytes([one[o], one[o + 1]]) == 277)
        .unwrap();
    one[spp + 8] = 5;
    let mut dec = oxideav_tiff::make_decoder(&params).unwrap();
    let pkt = Packet::new(0, TimeBase::new(1, 1), one.clone());
    assert!(matches!(dec.send_packet(&pkt), Err(Error::InvalidData(_))));
    let codecs = CodecRegistry::new();
    assert!(open_demuxer(Box::new(Cursor::new(one)), &codecs).is_err());
}

#[test]
fn encoder_writes_stream_blobs_and_frame_blobs_override_per_kind() {
    let mut ctx = RuntimeContext::new();
    oxideav_tiff::register(&mut ctx);
    let mut params = CodecParameters::video(CodecId::new("tiff"));
    params.width = Some(5);
    params.height = Some(3);
    params.pixel_format = Some(PixelFormat::Rgb24);
    params.blobs = vec![
        MetadataBlob::new(BlobKind::ICC, icc(144)),
        MetadataBlob::new(BlobKind::XMP, b"<x:xmpmeta>stream</x:xmpmeta>".to_vec()),
        MetadataBlob::new(BlobKind::custom("png-text"), b"ignored".to_vec()),
    ];
    let mut enc = ctx.codecs.first_encoder(&params).unwrap();
    assert!(enc.output_params().blobs.len() == 3);

    // Frame 0: stream blobs only. Frame 1: its own XMP + Exif + IPTC.
    let img0 = image(TiffPixelFormat::Rgb24, 5, 3, 2);
    let img1 = image(TiffPixelFormat::Rgb24, 5, 3, 40);
    let f0: VideoFrame = img0.clone().into();
    let f1 = VideoFrame::from(img1.clone()).with_blobs(vec![
        MetadataBlob::new(BlobKind::XMP, b"<x:xmpmeta>frame</x:xmpmeta>".to_vec()),
        MetadataBlob::new(BlobKind::EXIF, exif("2026:10:17 12:00:00")),
        MetadataBlob::new(BlobKind::IPTC, iptc("frame")),
    ]);
    enc.send_frame(&Frame::Video(f0)).unwrap();
    enc.send_frame(&Frame::Video(f1)).unwrap();
    enc.flush().unwrap();
    let p0 = enc.receive_packet().unwrap();
    let p1 = enc.receive_packet().unwrap();
    assert!(matches!(enc.receive_packet(), Err(Error::Eof)));

    let d0 = decode(&p0.data).unwrap();
    assert_eq!(d0.metadata.icc, Some(icc(144)));
    assert_eq!(
        d0.metadata.xmp.as_deref(),
        Some(&b"<x:xmpmeta>stream</x:xmpmeta>"[..])
    );
    assert_eq!(d0.metadata.exif, None);
    assert_eq!(d0.metadata.iptc, None);
    assert_eq!(d0.planes, img0.planes);
    let d1 = decode(&p1.data).unwrap();
    assert_eq!(d1.metadata.icc, Some(icc(144)), "stream ICC kept");
    assert_eq!(
        d1.metadata.xmp.as_deref(),
        Some(&b"<x:xmpmeta>frame</x:xmpmeta>"[..]),
        "frame XMP overrides the stream's"
    );
    assert_eq!(d1.metadata.exif, Some(exif("2026:10:17 12:00:00")));
    assert_eq!(d1.metadata.iptc, Some(iptc("frame")));
    assert_eq!(d1.planes, img1.planes);

    // Round trip through the demuxer + decoder: the stream carries the
    // page's blobs, the frame record the same four.
    let mut dmx = open(p1.data.clone());
    assert_eq!(
        kinds(&dmx.streams()[0].params.blobs),
        ["icc", "exif", "xmp", "iptc"]
    );
    let mut dec = ctx.codecs.first_decoder(&dmx.streams()[0].params).unwrap();
    dec.send_packet(&dmx.next_packet().unwrap()).unwrap();
    let frames = drain(dec.as_mut());
    assert_eq!(kinds(&frames[0].blobs()), ["icc", "exif", "xmp", "iptc"]);

    // Registry and standalone paths write identical bytes for the
    // same request.
    let same = encode(
        &img1.clone().with_metadata(
            Metadata::new()
                .with_icc(icc(144))
                .with_exif(exif("2026:10:17 12:00:00"))
                .with_xmp(b"<x:xmpmeta>frame</x:xmpmeta>".to_vec())
                .with_iptc(iptc("frame")),
        ),
        &EncodeOptions::default(),
    )
    .unwrap();
    assert_eq!(p1.data, same);

    // A malformed stream blob is a typed refusal at encode time, not a
    // silent drop: an ICC profile whose size field disagrees.
    let mut bad = params.clone();
    bad.blobs = vec![MetadataBlob::new(BlobKind::ICC, vec![0u8; 140])];
    let mut enc = ctx.codecs.first_encoder(&bad).unwrap();
    let r = enc.send_frame(&Frame::Video(img0.into()));
    assert!(matches!(r, Err(Error::InvalidData(_))), "{r:?}");
}
