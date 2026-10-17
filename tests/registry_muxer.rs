//! The registry muxer (IMAGE_CRATE_API round 475 mixed-geometry
//! ruling): one or more `tiff` streams — the gateway's one per picture
//! geometry — whose packets become the pages of one multi-page file in
//! write order, each page re-encoded under its stream's options with
//! its own metadata blobs; a single packet is written verbatim; a
//! multi-stream file remuxed straight from the demuxer is written once.

#![cfg(feature = "registry")]

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use oxideav_core::{
    BlobKind, CodecId, CodecOptions, CodecParameters, CodecRegistry, Error, Frame, MetadataBlob,
    Packet, PixelFormat, RuntimeContext, StreamInfo, TimeBase, VideoFrame,
};
use oxideav_tiff::container::{open_demuxer, open_muxer};
use oxideav_tiff::{
    decode_all, decode_pages, encode_all, encode_page, EncodeOptions, EncodePage,
    EncodePixelFormat, Metadata, PageExtras, PageResolution, Plane, TiffCompression, TiffImage,
    TiffPixelFormat,
};

/// An in-memory sink the muxer owns while the test keeps a handle.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl Sink {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

impl Seek for Sink {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(pos)
    }
}

fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_tiff::register(&mut ctx);
    ctx
}

fn image(format: TiffPixelFormat, w: u32, h: u32, seed: u8) -> TiffImage {
    let bpp = format.bytes_per_pixel();
    let data: Vec<u8> = (0..w as usize * h as usize * bpp)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect();
    TiffImage::new(w, h, format, vec![Plane::new(w as usize * bpp, data)]).unwrap()
}

fn params(w: u32, h: u32, pf: PixelFormat, opts: &[(&str, &str)]) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new("tiff"));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = Some(pf);
    let mut o = CodecOptions::new();
    for (k, v) in opts {
        o = o.set(*k, *v);
    }
    p.options = o;
    p
}

fn icc(len: usize) -> Vec<u8> {
    let mut p = vec![0u8; len];
    p[..4].copy_from_slice(&(len as u32).to_be_bytes());
    p[36..40].copy_from_slice(b"acsp");
    p
}

/// Encode `images` on one encoder built from `params`; the packets
/// (stream index stamped) and the encoder's output parameters.
fn encode_group(
    ctx: &RuntimeContext,
    params: &CodecParameters,
    stream_index: u32,
    images: &[(&TiffImage, Vec<MetadataBlob>)],
) -> (Vec<Packet>, CodecParameters) {
    let mut enc = ctx.codecs.first_encoder(params).unwrap();
    for (i, (img, blobs)) in images.iter().enumerate() {
        let mut f: VideoFrame = (*img).clone().into();
        f.set_blobs(blobs.clone());
        f.pts = Some(i as i64);
        enc.send_frame(&Frame::Video(f)).unwrap();
    }
    enc.flush().unwrap();
    let mut out = Vec::new();
    while let Ok(mut p) = enc.receive_packet() {
        p.stream_index = stream_index;
        out.push(p);
    }
    (out, enc.output_params().clone())
}

fn stream(index: u32, params: CodecParameters) -> StreamInfo {
    StreamInfo {
        index,
        params,
        time_base: TimeBase::MILLIS,
        start_time: Some(0),
        duration: None,
    }
}

fn mux(ctx: &RuntimeContext, streams: &[StreamInfo], packets: &[&Packet]) -> Vec<u8> {
    let sink = Sink::default();
    let mut m = ctx
        .containers
        .open_muxer("tiff", Box::new(sink.clone()), streams)
        .unwrap();
    m.write_header().unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    sink.bytes()
}

#[test]
fn mixed_geometry_streams_become_pages_in_write_order_under_each_streams_options() {
    let ctx = ctx();
    let a0 = image(TiffPixelFormat::Gray8, 4, 4, 0);
    let b = image(TiffPixelFormat::Rgb24, 2, 3, 9);
    let a1 = image(TiffPixelFormat::Gray8, 4, 4, 77);
    let (pa, out_a) = encode_group(
        &ctx,
        &params(
            4,
            4,
            PixelFormat::Gray8,
            &[("compression", "lzw"), ("predictor", "true")],
        ),
        0,
        &[
            (
                &a0,
                vec![MetadataBlob::new(BlobKind::XMP, b"<x>a0</x>".to_vec())],
            ),
            (&a1, vec![]),
        ],
    );
    let (pb, out_b) = encode_group(
        &ctx,
        &params(
            2,
            3,
            PixelFormat::Rgb24,
            &[("compression", "deflate"), ("tile", "16")],
        ),
        1,
        &[(&b, vec![MetadataBlob::new(BlobKind::ICC, icc(132))])],
    );
    assert_eq!((pa.len(), pb.len()), (2, 1));
    // Picture order a0, b, a1 — interleaved across the two streams.
    let streams = [stream(0, out_a), stream(1, out_b)];
    let bytes = mux(&ctx, &streams, &[&pa[0], &pb[0], &pa[1]]);

    let pages = decode_pages(&bytes).unwrap();
    assert_eq!(pages.len(), 3);
    assert_eq!(
        pages[0].image,
        a0.clone()
            .with_metadata(Metadata::new().with_xmp(b"<x>a0</x>".to_vec()))
    );
    assert_eq!(
        pages[1].image,
        b.clone().with_metadata(Metadata::new().with_icc(icc(132)))
    );
    assert_eq!(pages[2].image, a1);
    // Each page carries its stream's scheme.
    assert_eq!(pages[0].layout.compression, Some(5));
    assert_eq!(pages[0].layout.predictor, Some(2));
    assert_eq!(pages[1].layout.compression, Some(8));
    assert!(pages[1].layout.tiled);
    assert_eq!(pages[1].layout.tile_size, Some((16, 16)));
    assert_eq!(pages[2].layout.compression, Some(5));
    for (i, p) in pages.iter().enumerate() {
        assert_eq!(p.metadata.page_number, Some((i as u16, 3)));
        assert_eq!(p.metadata.new_subfile_type, Some(2));
    }

    // Through the demuxer: two geometry streams again, pages on the
    // right ones.
    let codecs = CodecRegistry::new();
    let mut dmx = open_demuxer(Box::new(Cursor::new(bytes)), &codecs).unwrap();
    assert_eq!(dmx.streams().len(), 2);
    assert_eq!(
        dmx.streams()[0].params.pixel_format,
        Some(PixelFormat::Gray8)
    );
    assert_eq!(
        dmx.streams()[1].params.pixel_format,
        Some(PixelFormat::Rgb24)
    );
    assert_eq!(
        dmx.streams()[1]
            .params
            .blob(&BlobKind::ICC)
            .map(|b| &b.data),
        Some(&icc(132))
    );
    let s0 = dmx.streams()[0].params.clone();
    let mut dec = ctx.codecs.first_decoder(&s0).unwrap();
    dec.send_packet(&dmx.next_packet().unwrap()).unwrap();
    let mut n = 0;
    while let Ok(Frame::Video(v)) = dec.receive_frame() {
        assert_eq!(v.image_planes()[0].data.len(), 16);
        n += 1;
    }
    assert_eq!(n, 2);
}

#[test]
fn one_packet_is_written_verbatim_and_a_remux_keeps_the_file() {
    let ctx = ctx();
    let img = image(TiffPixelFormat::Rgb24, 5, 5, 3);
    let (pk, out) = encode_group(
        &ctx,
        &params(5, 5, PixelFormat::Rgb24, &[("compression", "zstd")]),
        0,
        &[(&img, vec![])],
    );
    let bytes = mux(&ctx, &[stream(0, out)], &[&pk[0]]);
    assert_eq!(bytes, pk[0].data);

    // A mixed-geometry file demuxed (one whole-file packet per
    // stream) and muxed back is written once, byte for byte.
    let pages = vec![
        oxideav_tiff::Frame::new(image(TiffPixelFormat::Gray8, 3, 3, 1), 0),
        oxideav_tiff::Frame::new(image(TiffPixelFormat::Rgb24, 2, 2, 2), 1),
        oxideav_tiff::Frame::new(image(TiffPixelFormat::Gray8, 3, 3, 3), 2),
    ];
    let file = encode_all(
        &pages,
        &EncodeOptions::default().with_compression(TiffCompression::Lzw),
    )
    .unwrap();
    let codecs = CodecRegistry::new();
    let mut dmx = open_demuxer(Box::new(Cursor::new(file.clone())), &codecs).unwrap();
    let streams: Vec<StreamInfo> = dmx.streams().to_vec();
    assert_eq!(streams.len(), 2);
    let mut packets = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        packets.push(p);
    }
    assert_eq!(packets.len(), 2);
    let refs: Vec<&Packet> = packets.iter().collect();
    let remuxed = mux(&ctx, &streams, &refs);
    assert_eq!(remuxed, file);
}

#[test]
fn pages_keep_their_blobs_and_descriptive_tags_through_the_merge() {
    let ctx = ctx();
    // A foreign single-page packet with §8 strings and a resolution
    // (what a remuxed file from another writer could carry).
    let px: Vec<u8> = (0..36u32).map(|i| i as u8).collect();
    let foreign = encode_page(&EncodePage {
        width: 6,
        height: 6,
        kind: EncodePixelFormat::Gray8 { pixels: &px },
        compression: TiffCompression::PackBits,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras: PageExtras {
            software: Some("elsewhere 1.0"),
            artist: Some("Karpeles Lab"),
            description: Some("page from another writer"),
            resolution: Some(PageResolution {
                x: (300, 1),
                y: (300, 1),
                unit: 2,
            }),
            iptc: Some(&[0x1C, 0x02, 0x05, 0x00, 0x01, b'z']),
            ..PageExtras::default()
        },
    })
    .unwrap();
    let mut p0 = Packet::new(0, TimeBase::MILLIS, foreign);
    p0.pts = Some(0);
    let img = image(TiffPixelFormat::Gray8, 6, 6, 40);
    let (mut p1, out) = encode_group(
        &ctx,
        &params(6, 6, PixelFormat::Gray8, &[("compression", "lzw")]),
        0,
        &[(
            &img,
            vec![MetadataBlob::new(BlobKind::XMP, b"<x>1</x>".to_vec())],
        )],
    );
    let p1 = p1.remove(0);
    let bytes = mux(&ctx, &[stream(0, out)], &[&p0, &p1]);
    let pages = decode_pages(&bytes).unwrap();
    assert_eq!(pages.len(), 2);
    // Page 0: the foreign page's strings, resolution and IPTC survive;
    // its scheme is now the stream's (LZW).
    let m0 = &pages[0].metadata;
    assert_eq!(m0.software.as_deref(), Some("elsewhere 1.0"));
    assert_eq!(m0.artist.as_deref(), Some("Karpeles Lab"));
    assert_eq!(
        m0.image_description.as_deref(),
        Some("page from another writer")
    );
    assert_eq!(m0.x_resolution, Some((300, 1)));
    assert_eq!(m0.resolution_unit, Some(oxideav_tiff::ResolutionUnit::Inch));
    assert_eq!(
        pages[0].image.metadata.iptc.as_deref(),
        Some(&[0x1C, 0x02, 0x05, 0x00, 0x01, b'z'][..])
    );
    assert_eq!(pages[0].layout.compression, Some(5));
    assert_eq!(pages[0].image.planes[0].data, px);
    // Page 1: its own XMP, no strings.
    assert_eq!(
        pages[1].image.metadata.xmp.as_deref(),
        Some(&b"<x>1</x>"[..])
    );
    assert_eq!(pages[1].metadata.software, None);
    assert_eq!(pages[1].image.planes, img.planes);
}

#[test]
fn jpeg_streams_merge_under_jpeg_and_lossless_schemes_reproduce_pixels() {
    let ctx = ctx();
    let a = image(TiffPixelFormat::Rgb24, 16, 16, 1);
    let b = image(TiffPixelFormat::Rgb24, 16, 16, 90);
    // Lossless JPEG (quality 100): the merge reproduces the pixels.
    let (pk, out) = encode_group(
        &ctx,
        &params(
            16,
            16,
            PixelFormat::Rgb24,
            &[("compression", "jpeg"), ("quality", "100")],
        ),
        0,
        &[(&a, vec![]), (&b, vec![])],
    );
    let bytes = mux(&ctx, &[stream(0, out)], &[&pk[0], &pk[1]]);
    let pages = decode_pages(&bytes).unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].layout.compression, Some(7));
    assert_eq!(pages[0].image.planes, a.planes);
    assert_eq!(pages[1].image.planes, b.planes);
    // Lossy JPEG: both pages stay JPEG-coded and decode.
    let (pk, out) = encode_group(
        &ctx,
        &params(
            16,
            16,
            PixelFormat::Rgb24,
            &[("compression", "jpeg"), ("quality", "80")],
        ),
        0,
        &[(&a, vec![]), (&b, vec![])],
    );
    let bytes = mux(&ctx, &[stream(0, out)], &[&pk[0], &pk[1]]);
    let frames = decode_all(&bytes).unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(oxideav_tiff::info(&bytes).unwrap().compression, 7);
    assert_eq!(frames[0].image.format, TiffPixelFormat::Rgb24);
}

#[test]
fn refusals_are_typed() {
    let ctx = ctx();
    let sink = Sink::default();
    // No streams.
    assert!(matches!(
        open_muxer(Box::new(sink.clone()), &[]),
        Err(Error::Unsupported(_))
    ));
    // Another codec on a stream.
    let mut foreign = params(2, 2, PixelFormat::Rgb24, &[]);
    foreign.codec_id = CodecId::new("png");
    assert!(matches!(
        open_muxer(Box::new(sink.clone()), &[stream(0, foreign)]),
        Err(Error::InvalidData(_))
    ));
    // Options the encoder would refuse.
    assert!(open_muxer(
        Box::new(sink.clone()),
        &[stream(
            0,
            params(2, 2, PixelFormat::Rgb24, &[("compression", "rar")])
        )]
    )
    .is_err());
    // A packet on an undeclared stream.
    let img = image(TiffPixelFormat::Gray8, 2, 2, 0);
    let (pk, out) = encode_group(
        &ctx,
        &params(2, 2, PixelFormat::Gray8, &[]),
        0,
        &[(&img, vec![])],
    );
    let mut m = ctx
        .containers
        .open_muxer("tiff", Box::new(sink.clone()), &[stream(0, out.clone())])
        .unwrap();
    m.write_header().unwrap();
    let mut stray = pk[0].clone();
    stray.stream_index = 7;
    assert!(matches!(m.write_packet(&stray), Err(Error::InvalidData(_))));
    // Streams disagreeing on the BigTIFF variant cannot share a file.
    let (pk2, out2) = encode_group(
        &ctx,
        &params(2, 2, PixelFormat::Gray8, &[("bigtiff", "true")]),
        1,
        &[(&image(TiffPixelFormat::Gray8, 2, 2, 5), vec![])],
    );
    let sink2 = Sink::default();
    let mut m = ctx
        .containers
        .open_muxer("tiff", Box::new(sink2), &[stream(0, out), stream(1, out2)])
        .unwrap();
    m.write_header().unwrap();
    m.write_packet(&pk[0]).unwrap();
    m.write_packet(&pk2[0]).unwrap();
    assert!(m.write_trailer().is_err());
}
