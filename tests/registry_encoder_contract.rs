//! The registry encoder's contract (IMAGE_CRATE_API rounds 475 / 476):
//! it declares every layout it codes on `accepted_pixel_formats`,
//! writes each as itself with a truthful output label, refuses
//! anything else — and every layout × scheme pair the writer cannot
//! code — by type (`Unsupported`) before the first frame, and the
//! framework's advisory `quality` governs JPEG-in-TIFF only, in any
//! option order.

#![cfg(feature = "registry")]

use oxideav_core::{
    CodecId, CodecOptions, CodecParameters, Error, Frame, Packet, PixelFormat, RuntimeContext,
    VideoFrame,
};
use oxideav_tiff::{
    ACCEPTED_PIXEL_FORMATS, EncodeOptions, Palette, Plane, TiffImage, TiffPixelFormat, decode, info,
};

fn ctx() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_tiff::register(&mut ctx);
    ctx
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

fn image(format: TiffPixelFormat, w: u32, h: u32) -> TiffImage {
    let bpp = format.bytes_per_pixel();
    let n = w as usize * h as usize;
    let data: Vec<u8> = if format.is_float() {
        (0..n * bpp / 4)
            .flat_map(|i| ((i as f32) * 0.125 - 1.0).to_le_bytes())
            .collect()
    } else {
        (0..n * bpp).map(|i| (i * 5 % 253) as u8).collect()
    };
    let mut img = TiffImage::new(w, h, format, vec![Plane::new(w as usize * bpp, data)]).unwrap();
    if format == TiffPixelFormat::Pal8 {
        img.palette = Some(Palette::new(
            (0..=255u8)
                .map(|i| [i, i.wrapping_mul(3), 255 - i, 255])
                .collect(),
        ));
    }
    img
}

/// Encode one frame through the registry; the packet and the
/// encoder's output label.
fn encode_one(ctx: &RuntimeContext, p: &CodecParameters, img: &TiffImage) -> (Packet, PixelFormat) {
    let mut enc = ctx.codecs.first_encoder(p).unwrap();
    let mut f: VideoFrame = img.clone().into();
    f.pts = Some(0);
    enc.send_frame(&Frame::Video(f)).unwrap();
    enc.flush().unwrap();
    let pkt = enc.receive_packet().unwrap();
    assert!(matches!(enc.receive_packet(), Err(Error::Eof)));
    (pkt, enc.output_params().pixel_format.unwrap())
}

fn decode_one(ctx: &RuntimeContext, p: &CodecParameters, pkt: &Packet) -> VideoFrame {
    let mut dec = ctx.codecs.first_decoder(p).unwrap();
    dec.send_packet(pkt).unwrap();
    let Frame::Video(v) = dec.receive_frame().unwrap() else {
        panic!("video frame");
    };
    assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
    v
}

#[test]
fn capabilities_declare_the_nine_layouts_in_ladder_order() {
    let ctx = ctx();
    let id = CodecId::new("tiff");
    let imp = &ctx.codecs.implementations(&id)[0];
    assert_eq!(imp.caps.accepted_pixel_formats, ACCEPTED_PIXEL_FORMATS);
    assert_eq!(ACCEPTED_PIXEL_FORMATS.len(), 9);
    // Colour first (closest rung outward), grey last; no duplicates.
    assert_eq!(ACCEPTED_PIXEL_FORMATS[0], PixelFormat::Rgb24);
    assert_eq!(ACCEPTED_PIXEL_FORMATS[8], PixelFormat::GrayF32Le);
    let mut sorted = ACCEPTED_PIXEL_FORMATS.to_vec();
    sorted.sort_by_key(|f| *f as u32);
    sorted.dedup();
    assert_eq!(sorted.len(), 9);
    // Every declared layout has a TIFF layout and back.
    for f in ACCEPTED_PIXEL_FORMATS {
        let t = TiffPixelFormat::try_from(f).unwrap();
        assert_eq!(PixelFormat::from(t), f);
    }
    let schema = ctx.codecs.encoder_options_schema(&id).unwrap();
    assert!(schema.iter().any(|f| f.name == "quality"));
    assert!(schema.iter().any(|f| f.name == "compression"));
}

#[test]
fn every_declared_layout_is_written_as_itself_with_a_truthful_label() {
    let ctx = ctx();
    for scheme in ["none", "lzw", "deflate", "zstd", "packbits"] {
        for f in ACCEPTED_PIXEL_FORMATS {
            let t = TiffPixelFormat::try_from(f).unwrap();
            let img = image(t, 7, 3);
            let p = params(7, 3, f, &[("compression", scheme)]);
            let (pkt, label) = encode_one(&ctx, &p, &img);
            assert_eq!(label, f, "{scheme} {f:?}: output label is the input layout");
            // The file says the same thing.
            let i = info(&pkt.data).unwrap();
            assert_eq!(PixelFormat::from(i.format), f, "{scheme} {f:?}");
            assert_eq!((i.width, i.height), (7, 3));
            // Standalone decode: identical image (planes, palette).
            let back = decode(&pkt.data).unwrap();
            assert_eq!(back, img, "{scheme} {f:?}");
            // Registry decode: identical plane and label.
            let v = decode_one(&ctx, &p, &pkt);
            assert_eq!(
                v.image_planes()[0].data,
                img.planes[0].data,
                "{scheme} {f:?}"
            );
            if f == PixelFormat::Pal8 {
                assert_eq!(v.palette().unwrap().len(), 256 * 3);
                assert!(v.palette_alpha().is_none());
            }
        }
    }
}

#[test]
fn undeclared_layouts_are_refused_by_type_at_the_factory() {
    let ctx = ctx();
    for f in [
        PixelFormat::Yuv420P,
        PixelFormat::Yuv444P,
        PixelFormat::Bgra,
        PixelFormat::Rgba64Le,
    ] {
        assert!(!ACCEPTED_PIXEL_FORMATS.contains(&f));
        let r = ctx.codecs.first_encoder(&params(4, 4, f, &[]));
        assert!(
            matches!(r, Err(Error::Unsupported(_))),
            "{f:?}: {:?}",
            r.err()
        );
    }
}

#[test]
fn layout_scheme_pairs_the_writer_cannot_code_are_refused_up_front() {
    let ctx = ctx();
    let refused = [
        (PixelFormat::Pal8, "jpeg"),
        (PixelFormat::Rgba, "jpeg"),
        (PixelFormat::GrayF32Le, "jpeg"),
        (PixelFormat::RgbF32Le, "jpeg"),
        (PixelFormat::Gray16Le, "jpeg"),
        (PixelFormat::Rgb48Le, "jpeg"),
        (PixelFormat::Gray8, "webp"),
        (PixelFormat::Pal8, "webp"),
        (PixelFormat::Cmyk, "webp"),
        (PixelFormat::Rgb48Le, "webp"),
    ];
    for (f, scheme) in refused {
        let r = ctx
            .codecs
            .first_encoder(&params(4, 4, f, &[("compression", scheme)]));
        assert!(
            matches!(r, Err(Error::Unsupported(_))),
            "{f:?} + {scheme}: {:?}",
            r.err()
        );
    }
    // 16-bit input pairs with the lossless process (quality 100).
    for f in [PixelFormat::Gray16Le, PixelFormat::Rgb48Le] {
        let t = TiffPixelFormat::try_from(f).unwrap();
        let img = image(t, 8, 8);
        let p = params(8, 8, f, &[("compression", "jpeg"), ("quality", "100")]);
        let (pkt, label) = encode_one(&ctx, &p, &img);
        assert_eq!(label, f);
        let i = info(&pkt.data).unwrap();
        assert_eq!(i.compression, 7);
        assert_eq!(decode(&pkt.data).unwrap(), img, "{f:?} lossless JPEG");
    }
    // The accepted JPEG / WebP pairs go through.
    for (f, scheme) in [
        (PixelFormat::Gray8, "jpeg"),
        (PixelFormat::Rgb24, "jpeg"),
        (PixelFormat::Cmyk, "jpeg"),
        (PixelFormat::Rgb24, "webp"),
        (PixelFormat::Rgba, "webp"),
    ] {
        let t = TiffPixelFormat::try_from(f).unwrap();
        let img = image(t, 16, 8);
        let p = params(16, 8, f, &[("compression", scheme)]);
        let (pkt, label) = encode_one(&ctx, &p, &img);
        assert_eq!(label, f);
        let i = info(&pkt.data).unwrap();
        assert_eq!(i.compression, if scheme == "jpeg" { 7 } else { 50001 });
        assert_eq!(PixelFormat::from(i.format), f);
    }
}

#[test]
fn quality_governs_jpeg_only_in_any_option_order() {
    let ctx = ctx();
    let img = image(TiffPixelFormat::Rgb24, 32, 16);
    // Alone: no scheme switch — bytes identical to the plain encode.
    let plain = encode_one(&ctx, &params(32, 16, PixelFormat::Rgb24, &[]), &img).0;
    let q = encode_one(
        &ctx,
        &params(32, 16, PixelFormat::Rgb24, &[("quality", "50")]),
        &img,
    )
    .0;
    assert_eq!(plain.data, q.data);
    assert_eq!(info(&plain.data).unwrap().compression, 1);
    // With a lossless scheme: still ignored.
    let lzw = encode_one(
        &ctx,
        &params(32, 16, PixelFormat::Rgb24, &[("compression", "lzw")]),
        &img,
    )
    .0;
    let lzw_q = encode_one(
        &ctx,
        &params(
            32,
            16,
            PixelFormat::Rgb24,
            &[("quality", "10"), ("compression", "lzw")],
        ),
        &img,
    )
    .0;
    assert_eq!(lzw.data, lzw_q.data);
    assert_eq!(info(&lzw.data).unwrap().compression, 5);
    assert_eq!(decode(&lzw_q.data).unwrap(), img);

    // JPEG: quality before or after `compression` resolves alike, and
    // 50 / 90 / 100 produce different files.
    let jpeg = |opts: &[(&str, &str)]| {
        encode_one(&ctx, &params(32, 16, PixelFormat::Rgb24, opts), &img)
            .0
            .data
    };
    let q50a = jpeg(&[("quality", "50"), ("compression", "jpeg")]);
    let q50b = jpeg(&[("compression", "jpeg"), ("quality", "50")]);
    assert_eq!(q50a, q50b, "option order");
    let q90 = jpeg(&[("compression", "jpeg"), ("quality", "90")]);
    let q100 = jpeg(&[("quality", "100"), ("compression", "jpeg")]);
    let q75 = jpeg(&[("compression", "jpeg")]);
    assert_ne!(q50a, q90);
    assert_ne!(q90, q100);
    assert_ne!(q50a, q100);
    assert_ne!(q75, q90);
    for b in [&q50a, &q90, &q100, &q75] {
        assert_eq!(info(b).unwrap().compression, 7);
        assert_eq!(
            PixelFormat::from(info(b).unwrap().format),
            PixelFormat::Rgb24
        );
    }
    // 100 is the lossless process: the pixels come back exactly; 90
    // does not.
    assert_eq!(decode(&q100).unwrap(), img);
    assert_ne!(decode(&q90).unwrap().planes, img.planes);
    // 0 is the lowest DCT quality, not an error; 101 is.
    let q0 = jpeg(&[("compression", "jpeg"), ("quality", "0")]);
    assert_eq!(info(&q0).unwrap().compression, 7);
    assert!(q0.len() <= q50a.len());
    assert!(
        ctx.codecs
            .first_encoder(&params(
                32,
                16,
                PixelFormat::Rgb24,
                &[("compression", "jpeg"), ("quality", "101")]
            ))
            .is_err()
    );
}

#[test]
fn palette_with_transparency_is_refused_not_flattened() {
    let ctx = ctx();
    let img = image(TiffPixelFormat::Pal8, 4, 4);
    let mut f: VideoFrame = img.into();
    f.set_palette_alpha(vec![255, 0]);
    let mut enc = ctx
        .codecs
        .first_encoder(&params(4, 4, PixelFormat::Pal8, &[]))
        .unwrap();
    let r = enc.send_frame(&Frame::Video(f.clone()));
    assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
    // An all-opaque alpha record is fine.
    f.set_palette_alpha(vec![255, 255, 255]);
    enc.send_frame(&Frame::Video(f)).unwrap();
    enc.flush().unwrap();
    assert_eq!(
        decode(&enc.receive_packet().unwrap().data).unwrap().format,
        TiffPixelFormat::Pal8
    );
}

#[test]
fn encode_options_schema_quality_never_switches_the_scheme() {
    use oxideav_core::options::parse_options_json;
    use oxideav_tiff::TiffCompression;
    let o: EncodeOptions = parse_options_json(r#"{"quality":"40"}"#).unwrap();
    assert!(matches!(o.compression, TiffCompression::None));
    let o: EncodeOptions = parse_options_json(r#"{"compression":"jpeg","quality":"40"}"#).unwrap();
    assert!(matches!(o.compression, TiffCompression::Jpeg(j) if j.quality == 40));
    let o: EncodeOptions = parse_options_json(r#"{"compression":"jpeg","quality":"100"}"#).unwrap();
    assert!(matches!(
        o.compression,
        TiffCompression::Jpeg(j) if matches!(j.process, oxideav_tiff::JpegProcess::Lossless { predictor: 1 })
    ));
}
