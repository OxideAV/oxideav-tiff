//! TIFF container: a single TIFF file becomes one [`Packet`] on
//! stream `0`. Width / height / pixel-format are pulled from the
//! first IFD up-front so callers that read `StreamInfo` before
//! seeing any frames get accurate metadata.
//!
//! Container-level metadata surfaces through the core `Demuxer`
//! accessors: the TIFF 6.0 §8 ASCII descriptive fields of the first
//! IFD become flat `metadata()` key/value pairs, and the registered
//! opaque payloads — the embedded ICC profile (tag 34675) and the
//! XMP packet (tag 700), per `docs/image/tiff/tiff-icc-xmp-tags.md` —
//! become structured `attachments()` carrying the raw bytes verbatim.
//!
//! The muxer writes one packet (a complete single-page TIFF file, as
//! the codec encoder produces) verbatim; several packets are
//! re-assembled into one multi-page file through the standalone
//! decode + [`crate::encode_pages`] path.

use std::io::{Read, SeekFrom, Write};

use oxideav_core::{
    Attachment, CodecId, CodecParameters, CodecResolver, Error, Packet, PixelFormat, Result,
    StreamInfo, TimeBase,
};
use oxideav_core::{
    ContainerRegistry, Demuxer, Muxer, ProbeData, ProbeScore, ReadSeek, WriteSeek, MAX_PROBE_SCORE,
};

use crate::ifd::{parse_header, parse_ifd};
use crate::metadata::extract_metadata;

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("tiff", open_demuxer);
    reg.register_muxer("tiff", open_muxer);
    reg.register_extension("tif", "tiff");
    reg.register_extension("tiff", "tiff");
    reg.register_probe("tiff", probe);
}

fn probe(data: &ProbeData) -> ProbeScore {
    // II 4949 + magic 002A (LE) → bytes 49 49 2A 00 (classic).
    // MM 4D4D + magic 002A (BE) → bytes 4D 4D 00 2A (classic).
    // II 4949 + magic 002B (LE) → bytes 49 49 2B 00 (BigTIFF).
    // MM 4D4D + magic 002B (BE) → bytes 4D 4D 00 2B (BigTIFF).
    if crate::probe(data.buf) {
        return MAX_PROBE_SCORE;
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
    let (entries, _next) = parse_ifd(
        &buf,
        header.byte_order,
        header.variant,
        header.first_ifd_offset,
    )?;
    let bo = header.byte_order;

    // The stream parameters are exactly what the standalone `info`
    // reports: the layout `decode` will hand back, after Orientation.
    let info = crate::info(&buf)?;
    let pf: PixelFormat = info.format.into();

    let mut params = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
    params.width = Some(info.width);
    params.height = Some(info.height);
    params.pixel_format = Some(pf);
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, 1),
        start_time: Some(0),
        duration: None,
    };

    // Container-level metadata from the first IFD: the §8 ASCII
    // descriptive fields become flat key/value pairs, the ICC / XMP
    // payloads become structured attachments. Extraction is total, so
    // malformed informational tags simply produce fewer entries.
    let md = extract_metadata(&entries, bo);
    let mut metadata: Vec<(String, String)> = Vec::new();
    for (key, value) in [
        ("document_name", &md.document_name),
        ("description", &md.image_description),
        ("make", &md.make),
        ("model", &md.model),
        ("page_name", &md.page_name),
        ("software", &md.software),
        ("date", &md.date_time),
        ("artist", &md.artist),
        ("host_computer", &md.host_computer),
        ("copyright", &md.copyright),
    ] {
        if let Some(v) = value {
            metadata.push((key.to_string(), v.clone()));
        }
    }
    let mut attachments: Vec<Attachment> = Vec::new();
    if let Some(icc) = md.icc_profile {
        attachments.push(Attachment {
            name: "profile.icc".to_string(),
            // IANA-registered media type for ICC profiles.
            mime: Some("application/vnd.iccprofile".to_string()),
            description: Some("embedded ICC colour profile (TIFF tag 34675)".to_string()),
            data: icc,
        });
    }
    if let Some(xmp) = md.xmp {
        attachments.push(Attachment {
            name: "packet.xmp".to_string(),
            // XMP packets are RDF/XML documents.
            mime: Some("application/rdf+xml".to_string()),
            description: Some("XMP metadata packet (TIFF tag 700)".to_string()),
            data: xmp,
        });
    }

    Ok(Box::new(TiffDemuxer {
        streams: vec![stream],
        data: Some(buf),
        metadata,
        attachments,
    }))
}

struct TiffDemuxer {
    streams: Vec<StreamInfo>,
    /// `None` once the sole packet has been emitted.
    data: Option<Vec<u8>>,
    /// Flat §8 descriptive metadata from the first IFD.
    metadata: Vec<(String, String)>,
    /// ICC profile / XMP packet payloads from the first IFD.
    attachments: Vec<Attachment>,
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
    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }
    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }
}

/// Open the TIFF muxer: exactly one video stream whose codec is
/// `tiff`. Packets are complete single-page TIFF files (what
/// [`crate::make_encoder`] emits).
pub fn open_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    if streams.len() != 1 {
        return Err(Error::unsupported(
            "TIFF muxer: exactly one video stream expected",
        ));
    }
    let s = &streams[0];
    if s.params.codec_id.as_str() != crate::CODEC_ID_STR {
        return Err(Error::invalid(format!(
            "TIFF muxer: codec_id must be tiff (got {})",
            s.params.codec_id
        )));
    }
    Ok(Box::new(TiffMuxer {
        output,
        packets: Vec::new(),
        header_written: false,
        trailer_written: false,
    }))
}

struct TiffMuxer {
    output: Box<dyn WriteSeek>,
    packets: Vec<Packet>,
    header_written: bool,
    trailer_written: bool,
}

impl Muxer for TiffMuxer {
    fn format_name(&self) -> &str {
        "tiff"
    }

    fn write_header(&mut self) -> Result<()> {
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("TIFF muxer: write_header not called"));
        }
        self.packets.push(packet.clone());
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        match self.packets.len() {
            0 => return Err(Error::invalid("TIFF muxer: no packets written")),
            // One packet is already a complete TIFF file.
            1 => self.output.write_all(&self.packets[0].data)?,
            // Several packets: decode each (native layout, lossless)
            // and write them back as the pages of one multi-page file.
            _ => {
                let bytes = merge_pages(&self.packets)?;
                self.output.write_all(&bytes)?;
            }
        }
        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

/// Re-assemble several single-page TIFF packets into one multi-page
/// file: every page of every packet is decoded in its native layout
/// and re-encoded (losslessly, uncompressed) in order.
fn merge_pages(packets: &[Packet]) -> Result<Vec<u8>> {
    use crate::encoder::{EncodePage, EncodePixelFormat, ExtraSampleKind, PageExtras};
    use crate::{RgbColor, TiffCompression, TiffImage, TiffPixelFormat};

    let mut images: Vec<TiffImage> = Vec::new();
    for p in packets {
        for page in crate::decode_pages(&p.data)? {
            images.push(page.image);
        }
    }
    let palettes: Vec<Vec<RgbColor>> = images
        .iter()
        .map(|img| {
            img.palette
                .as_ref()
                .map(|p| p.to_rgb_triples())
                .unwrap_or_default()
        })
        .collect();
    let total = images.len() as u16;
    let mut pages: Vec<EncodePage<'_>> = Vec::with_capacity(images.len());
    for (i, img) in images.iter().enumerate() {
        let pixels: &[u8] = img.as_bytes().unwrap_or(&[]);
        let kind = match img.format {
            TiffPixelFormat::Gray8 => EncodePixelFormat::Gray8 { pixels },
            TiffPixelFormat::Gray16Le => EncodePixelFormat::Gray16Le { pixels },
            TiffPixelFormat::Rgb24 => EncodePixelFormat::Rgb24 { pixels },
            TiffPixelFormat::Rgb48Le => EncodePixelFormat::Rgb48 { pixels },
            TiffPixelFormat::Rgba => EncodePixelFormat::Rgba32 {
                pixels,
                kind: ExtraSampleKind::UnassociatedAlpha,
            },
            TiffPixelFormat::Pal8 => EncodePixelFormat::Palette8 {
                indices: pixels,
                palette: &palettes[i],
            },
            TiffPixelFormat::Cmyk => EncodePixelFormat::Cmyk32 { pixels },
        };
        pages.push(EncodePage {
            width: img.width,
            height: img.height,
            kind,
            compression: TiffCompression::None,
            predictor: false,
            planar: false,
            tiling: None,
            bigtiff: false,
            extras: PageExtras {
                page_number: Some((i as u16, total)),
                multi_page: true,
                icc_profile: img.metadata.icc.as_deref(),
                xmp: img.metadata.xmp.as_deref(),
                ..PageExtras::default()
            },
        });
    }
    Ok(crate::encode_pages(&pages)?)
}
