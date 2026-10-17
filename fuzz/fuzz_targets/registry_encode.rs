#![no_main]

//! Panic-freedom fuzz target for the `oxideav-tiff` registry seam:
//! arbitrary bytes steer the framework encoder and everything it
//! produces must survive the registry decoder, the demuxer and the
//! muxer's multi-packet merge.
//!
//! What the input controls:
//!
//! * the input layout (one of the nine declared ones), a small
//!   geometry, the compression scheme and the advisory `quality`
//!   (the option bag is built the way the gateway builds it — quality
//!   first, then the caller's options);
//! * the metadata blobs on the stream (`CodecParameters::blobs`) and
//!   on the frame record — the remaining bytes are cut into up to four
//!   payloads of fuzzer-chosen kinds: an **Exif** block the encoder
//!   parses as a TIFF-structured payload into child IFDs (header /
//!   IFD walk / value-type / count arithmetic over hostile bytes), an
//!   **ICC** profile whose big-endian size field it checks, **XMP** /
//!   **IPTC** it copies verbatim, a custom kind it must ignore;
//! * a palette-alpha record on `Pal8` frames (a transparent entry is
//!   a typed refusal, never a panic).
//!
//! When the encoder produces a packet, the packet goes through the
//! demuxer (streams, blobs), the registry decoder (frames, blobs) and
//! the muxer twice: the packet twice (deduplicated: verbatim) and the
//! packet plus the raw input as a second, hostile packet (the merge
//! decodes it — `decode_pages` on arbitrary bytes). Every call must
//! return a `Result`; return values are discarded.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use oxideav_core::{
    BlobKind, CodecId, CodecOptions, CodecParameters, CodecRegistry, Frame, MetadataBlob, Packet,
    PixelFormat, StreamInfo, TimeBase, VideoFrame,
};
use oxideav_tiff::{Palette, Plane, TiffImage, TiffPixelFormat, ACCEPTED_PIXEL_FORMATS};

/// An in-memory sink the muxer owns.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

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

const SCHEMES: [&str; 7] = ["none", "packbits", "lzw", "deflate", "zstd", "webp", "jpeg"];

fn kind_of(b: u8) -> BlobKind {
    match b % 6 {
        0 => BlobKind::ICC,
        1 => BlobKind::EXIF,
        2 => BlobKind::XMP,
        3 => BlobKind::IPTC,
        4 => BlobKind::custom("png-text"),
        _ => BlobKind::EXIF,
    }
}

/// Cut `rest` into up to four blobs: each takes a kind byte, a length
/// byte and that many payload bytes (the last one takes the tail).
fn blobs_from(mut rest: &[u8]) -> Vec<MetadataBlob> {
    let mut out = Vec::new();
    for i in 0..4 {
        let Some((&k, tail)) = rest.split_first() else {
            break;
        };
        let Some((&n, tail)) = tail.split_first() else {
            break;
        };
        let take = if i == 3 {
            tail.len()
        } else {
            (n as usize).min(tail.len())
        };
        let (payload, tail) = tail.split_at(take);
        out.push(MetadataBlob::new(kind_of(k), payload.to_vec()));
        rest = tail;
    }
    out
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let fmt = ACCEPTED_PIXEL_FORMATS[(data[0] % 9) as usize];
    let w = (data[1] % 17) as u32 + 1;
    let h = (data[2] % 9) as u32 + 1;
    let scheme = SCHEMES[(data[3] % 7) as usize];
    let quality = data[4];
    let extra = data[5];
    let split = data[6] as usize;
    let rest = &data[7..];
    let (stream_bytes, frame_bytes) = rest.split_at(split.min(rest.len()));

    // The picture: a byte ramp in the chosen layout (float layouts get
    // whatever bit patterns the ramp spells — NaN / Inf included).
    let tiff_fmt = TiffPixelFormat::try_from(fmt).expect("declared layouts bridge");
    let bpp = tiff_fmt.bytes_per_pixel();
    let n = w as usize * h as usize * bpp;
    let pixels: Vec<u8> = (0..n).map(|i| (i as u8).wrapping_mul(extra | 1)).collect();
    let Ok(mut img) = TiffImage::new(w, h, tiff_fmt, vec![Plane::new(w as usize * bpp, pixels)])
    else {
        return;
    };
    if tiff_fmt == TiffPixelFormat::Pal8 {
        let entries = (extra as usize % 256) + 1;
        img.palette = Some(Palette::new(
            (0..entries)
                .map(|i| [i as u8, 255 - i as u8, extra, 255])
                .collect(),
        ));
    }

    // The option bag, the gateway's way: quality first, then the
    // caller's options; a second quality to exercise re-application.
    let mut options = CodecOptions::new();
    if extra & 1 == 1 {
        options = options.set("quality", quality.to_string());
    }
    options = options.set("compression", scheme);
    if extra & 2 == 2 {
        options = options.set("predictor", "true");
    }
    if extra & 4 == 4 {
        options = options.set("tile", "16");
    }
    if extra & 8 == 8 {
        options = options.set("bigtiff", "true");
    }
    if extra & 16 == 16 {
        options = options.set("planar", "true");
    }
    if extra & 32 == 32 {
        options = options.set("quality", (quality / 2).to_string());
    }
    let mut params = CodecParameters::video(CodecId::new("tiff"));
    params.width = Some(w);
    params.height = Some(h);
    params.pixel_format = Some(fmt);
    params.options = options;
    params.blobs = blobs_from(stream_bytes);

    let Ok(mut enc) = oxideav_tiff::make_encoder(&params) else {
        return;
    };
    let mut frame: VideoFrame = img.into();
    frame.set_blobs(blobs_from(frame_bytes));
    if tiff_fmt == TiffPixelFormat::Pal8 && extra & 64 == 64 {
        // A palette-alpha record: transparent entries are refused by
        // type, opaque ones accepted; a record longer than the palette
        // reads as none.
        frame.set_palette_alpha(frame_bytes.iter().take(300).copied().collect());
    }
    frame.pts = Some(0);
    if enc.send_frame(&Frame::Video(frame)).is_err() {
        return;
    }
    let _ = enc.flush();
    let Ok(pkt) = enc.receive_packet() else {
        return;
    };
    let out_params = enc.output_params().clone();

    // Whatever was written decodes: standalone, registry decoder,
    // demuxer.
    let decoded = oxideav_tiff::decode(&pkt.data).expect("the encoder's own file decodes");
    assert_eq!(PixelFormat::from(decoded.format), fmt, "label is truthful");
    let mut dec = oxideav_tiff::make_decoder(&out_params).expect("decoder");
    dec.send_packet(&pkt).expect("registry decode");
    let Ok(Frame::Video(v)) = dec.receive_frame() else {
        panic!("one frame");
    };
    let _ = v.blobs();
    let codecs = CodecRegistry::new();
    let mut dmx =
        oxideav_tiff::container::open_demuxer(Box::new(Cursor::new(pkt.data.clone())), &codecs)
            .expect("the encoder's own file demuxes");
    assert_eq!(dmx.streams().len(), 1);
    let _ = dmx.next_packet();

    // The muxer: the same packet twice is written once (verbatim); the
    // packet plus the raw input as a hostile second packet drives the
    // merge's `decode_pages` over arbitrary bytes.
    let stream = StreamInfo {
        index: 0,
        params: out_params,
        time_base: TimeBase::new(1, 1),
        start_time: Some(0),
        duration: None,
    };
    for hostile in [false, true] {
        let Ok(mut m) = oxideav_tiff::container::open_muxer(
            Box::new(Sink::default()),
            std::slice::from_ref(&stream),
        ) else {
            return;
        };
        let _ = m.write_header();
        let _ = m.write_packet(&pkt);
        let second = if hostile {
            Packet::new(0, TimeBase::new(1, 1), data.to_vec())
        } else {
            pkt.clone()
        };
        let _ = m.write_packet(&second);
        let r = m.write_trailer();
        if !hostile {
            r.expect("two identical packets: verbatim");
        }
    }
});
