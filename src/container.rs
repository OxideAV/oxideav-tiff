//! TIFF container: a single TIFF file becomes one [`Packet`] on
//! stream `0`. Width / height / pixel-format are pulled from the
//! first IFD up-front so callers that read `StreamInfo` before
//! seeing any frames get accurate metadata.

use std::io::{Read, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Error, Packet, PixelFormat, Result, StreamInfo,
    TimeBase,
};
use oxideav_core::{ContainerRegistry, Demuxer, ProbeData, ProbeScore, ReadSeek, MAX_PROBE_SCORE};

use crate::ifd::{find, parse_header, parse_ifd};
use crate::types::*;

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("tiff", open_demuxer);
    reg.register_extension("tif", "tiff");
    reg.register_extension("tiff", "tiff");
    reg.register_probe("tiff", probe);
}

fn probe(data: &ProbeData) -> ProbeScore {
    if data.buf.len() >= 4 {
        // II 4949 + magic 002A (LE) → bytes 49 49 2A 00.
        // MM 4D4D + magic 002A (BE) → bytes 4D 4D 00 2A.
        let h = &data.buf[..4];
        if h == [b'I', b'I', 0x2A, 0x00] || h == [b'M', b'M', 0x00, 0x2A] {
            return MAX_PROBE_SCORE;
        }
    }
    if matches!(data.ext, Some("tif") | Some("tiff")) {
        oxideav_core::PROBE_SCORE_EXTENSION
    } else {
        0
    }
}

pub fn open_demuxer(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    input.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    input.read_to_end(&mut buf)?;
    let header = parse_header(&buf)?;
    let (entries, _next) = parse_ifd(&buf, header.byte_order, header.first_ifd_offset)?;
    let bo = header.byte_order;

    let width = find(&entries, TAG_IMAGE_WIDTH)
        .ok_or_else(|| Error::invalid("TIFF demux: missing ImageWidth"))?
        .as_u32(bo)?;
    let height = find(&entries, TAG_IMAGE_LENGTH)
        .ok_or_else(|| Error::invalid("TIFF demux: missing ImageLength"))?
        .as_u32(bo)?;
    let photo = find(&entries, TAG_PHOTOMETRIC_INTERPRETATION)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(PHOTO_BLACK_IS_ZERO as u32) as u16;
    let spp = find(&entries, TAG_SAMPLES_PER_PIXEL)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(1) as u16;
    let bps = find(&entries, TAG_BITS_PER_SAMPLE)
        .map(|e| e.as_u32(bo))
        .transpose()?
        .unwrap_or(1) as u16;

    // Best-effort PixelFormat advertisement.
    let pf = match (photo, spp, bps) {
        (PHOTO_RGB, 3, 16) => PixelFormat::Rgb48Le,
        (PHOTO_RGB, _, _) | (PHOTO_PALETTE, _, _) => PixelFormat::Rgb24,
        (_, 1, 16) => PixelFormat::Gray16Le,
        _ => PixelFormat::Gray8,
    };

    let mut params = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
    params.width = Some(width);
    params.height = Some(height);
    params.pixel_format = Some(pf);
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, 1),
        start_time: Some(0),
        duration: None,
    };
    Ok(Box::new(TiffDemuxer {
        streams: vec![stream],
        data: Some(buf),
    }))
}

struct TiffDemuxer {
    streams: Vec<StreamInfo>,
    /// `None` once the sole packet has been emitted.
    data: Option<Vec<u8>>,
}

impl Demuxer for TiffDemuxer {
    fn format_name(&self) -> &str {
        "tiff"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn next_packet(&mut self) -> Result<Packet> {
        match self.data.take() {
            Some(bytes) => {
                let mut pkt = Packet::new(0, TimeBase::new(1, 1), bytes);
                pkt.pts = Some(0);
                pkt.dts = Some(0);
                pkt.flags.keyframe = true;
                Ok(pkt)
            }
            None => Err(Error::Eof),
        }
    }
}
