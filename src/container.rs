//! TIFF container: the demuxer declares one video stream per distinct
//! page geometry (`width × height × layout` after `Orientation`, in
//! first-appearance order on the IFD chain — a single-page or uniform
//! multi-page file is exactly one stream) and hands the whole file to
//! each stream as its single [`Packet`]; the codec decoder returns the
//! pages of its stream's geometry (see [`crate::make_decoder`]). Width
//! / height / pixel-format are pulled from the IFDs up-front so callers
//! that read `StreamInfo` before seeing any frames get accurate,
//! truthful labels for every page.
//!
//! Metadata: each stream's `CodecParameters::blobs` carry the first
//! page of its geometry's ICC profile (tag 34675) / Exif block / XMP
//! packet (tag 700) / IPTC record stream (tag 33723) as framework
//! blobs ([`crate::metadata_to_blobs`]); every decoded page carries
//! its own on the frame record. The pre-blob surfaces stay: the TIFF
//! 6.0 §8 ASCII descriptive fields of the first IFD are flat
//! `metadata()` key/value pairs, and the first IFD's ICC profile / XMP
//! packet are also structured `attachments()` (raw bytes verbatim, per
//! `docs/image/tiff/tiff-icc-xmp-tags.md`).
//!
//! The muxer takes one or more `tiff` streams (the gateway's one per
//! picture geometry) and writes one packet (a complete TIFF file, as
//! the codec encoder produces) verbatim; several packets become the
//! pages of one multi-page file in write order, each page re-encoded
//! under its stream's options (compression / predictor / planar /
//! tiling / BigTIFF — the same option bag the encoder parsed) with its
//! own metadata blobs and descriptive tags carried along; pages may
//! differ in geometry. Identical packet payloads (a multi-stream file
//! remuxed straight from the demuxer) are written once.

use std::io::{Read, SeekFrom, Write};

use oxideav_core::{
    Attachment, CodecId, CodecParameters, CodecResolver, Error, Packet, PixelFormat, Result,
    StreamInfo, TimeBase,
};
use oxideav_core::{
    ContainerRegistry, Demuxer, MAX_PROBE_SCORE, Muxer, ProbeData, ProbeScore, ReadSeek, WriteSeek,
};

use crate::decoder::ifd_chain;
use crate::ifd::parse_header;
use crate::image::Metadata;
use crate::metadata::extract_metadata;
use crate::options::EncodeOptions;
use crate::registry::{RegistryEncodeOptions, metadata_to_blobs, page_geometry};

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
    let chain = ifd_chain(&buf, &header)?;
    let bo = header.byte_order;
    let entries = &chain[0];

    // One stream per distinct page geometry — the layout `decode`
    // hands back, after Orientation — in first-appearance order. The
    // first page must resolve (as `info` requires); a later page whose
    // layout the decoder does not support is left out of every stream.
    let mut streams: Vec<StreamInfo> = Vec::new();
    let mut geometries: Vec<(u32, u32, PixelFormat)> = Vec::new();
    for (i, page) in chain.iter().enumerate() {
        let g = match page_geometry(page, bo) {
            Ok(g) => g,
            Err(e) if i == 0 => return Err(e.into()),
            Err(_) => continue,
        };
        if geometries.contains(&g) {
            continue;
        }
        geometries.push(g);
        let mut params = CodecParameters::video(CodecId::new(crate::CODEC_ID_STR));
        params.width = Some(g.0);
        params.height = Some(g.1);
        params.pixel_format = Some(g.2);
        // The first page of the geometry lends the stream its blobs.
        let md = extract_metadata(page, bo);
        params.blobs = metadata_to_blobs(&Metadata {
            icc: md.icc_profile,
            exif: crate::exif::extract_exif_payload(&buf, &header, page),
            xmp: md.xmp,
            gamma: None,
            iptc: md.iptc,
        });
        streams.push(StreamInfo {
            index: streams.len() as u32,
            params,
            time_base: TimeBase::new(1, 1),
            start_time: Some(0),
            duration: None,
        });
    }

    // Container-level metadata from the first IFD: the §8 ASCII
    // descriptive fields become flat key/value pairs, the ICC / XMP
    // payloads become structured attachments. Extraction is total, so
    // malformed informational tags simply produce fewer entries.
    let md = extract_metadata(entries, bo);
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

    let remaining = (0..streams.len() as u32).collect();
    Ok(Box::new(TiffDemuxer {
        streams,
        data: buf,
        remaining,
        metadata,
        attachments,
    }))
}

struct TiffDemuxer {
    streams: Vec<StreamInfo>,
    /// The whole file — every stream's single packet.
    data: Vec<u8>,
    /// Streams whose packet has not been emitted yet, in order.
    remaining: std::collections::VecDeque<u32>,
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
        let Some(index) = self.remaining.pop_front() else {
            return Err(Error::Eof);
        };
        // The last stream takes the buffer; earlier ones a copy.
        let bytes = if self.remaining.is_empty() {
            std::mem::take(&mut self.data)
        } else {
            self.data.clone()
        };
        let mut pkt = Packet::new(index, TimeBase::new(1, 1), bytes);
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }
    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }
    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }
}

/// Open the TIFF muxer: one or more video streams whose codec is
/// `tiff` (one per picture geometry). Packets are complete TIFF files
/// (what [`crate::make_encoder`] emits); each stream's
/// `params.options` are parsed with the encoder's schema and govern
/// how its pages are re-encoded when several packets are assembled
/// into one multi-page file.
pub fn open_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    if streams.is_empty() {
        return Err(Error::unsupported(
            "TIFF muxer: at least one video stream expected",
        ));
    }
    let mut options: Vec<(u32, EncodeOptions)> = Vec::with_capacity(streams.len());
    for s in streams {
        if s.params.codec_id.as_str() != crate::CODEC_ID_STR {
            return Err(Error::invalid(format!(
                "TIFF muxer: codec_id must be tiff (got {} on stream {})",
                s.params.codec_id, s.index
            )));
        }
        let opts = oxideav_core::parse_options::<RegistryEncodeOptions>(&s.params.options)
            .map_err(|e| Error::invalid(format!("TIFF muxer: stream {} options: {e}", s.index)))?
            .resolve();
        options.push((s.index, opts));
    }
    Ok(Box::new(TiffMuxer {
        output,
        options,
        packets: Vec::new(),
        header_written: false,
        trailer_written: false,
    }))
}

struct TiffMuxer {
    output: Box<dyn WriteSeek>,
    /// Per stream index: the resolved encode options of that stream.
    options: Vec<(u32, EncodeOptions)>,
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
        if !self.options.iter().any(|(i, _)| *i == packet.stream_index) {
            return Err(Error::invalid(format!(
                "TIFF muxer: packet on unknown stream {}",
                packet.stream_index
            )));
        }
        // A multi-stream file remuxed straight from the demuxer hands
        // the same whole file once per stream: keep one copy.
        if self.packets.iter().any(|p| p.data == packet.data) {
            return Ok(());
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
            // Several packets: the pages of each, in write order,
            // re-encoded under their stream's options.
            _ => {
                let bytes = merge_pages(&self.packets, &self.options)?;
                self.output.write_all(&bytes)?;
            }
        }
        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

/// Re-assemble several TIFF packets into one multi-page file: every
/// page of every packet is decoded in its native layout (metadata
/// blobs and descriptive tags included) and re-encoded in write order
/// under its stream's options — compression, predictor, planar,
/// tiling and BigTIFF as the encoder would have written a single
/// page. A lossy JPEG stream is quantised a second time (through the
/// same tables); a lossless scheme reproduces the pixels exactly.
fn merge_pages(packets: &[Packet], options: &[(u32, EncodeOptions)]) -> Result<Vec<u8>> {
    let mut decoded: Vec<(Vec<crate::Page>, &EncodeOptions)> = Vec::with_capacity(packets.len());
    for p in packets {
        let opts = options
            .iter()
            .find(|(i, _)| *i == p.stream_index)
            .map(|(_, o)| o)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "TIFF muxer: packet on unknown stream {}",
                    p.stream_index
                ))
            })?;
        decoded.push((crate::decode_pages(&p.data)?, opts));
    }
    let pages: Vec<(&crate::Page, &EncodeOptions)> = decoded
        .iter()
        .flat_map(|(pages, opts)| pages.iter().map(move |p| (p, *opts)))
        .collect();
    Ok(crate::api::encode_decoded_pages(&pages)?)
}
