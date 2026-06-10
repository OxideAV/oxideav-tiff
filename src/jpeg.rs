//! JPEG-in-TIFF (Compression = 7, per TIFF Technical Note 2) decode
//! helpers.
//!
//! TIFF Tech Note 2 (DRAFT 17-Mar-95) replaces the unworkable TIFF 6.0
//! §22 design with a much simpler one: each strip / tile is itself a
//! complete ISO JPEG datastream (SOI..EOI) and an optional auxiliary
//! field, `JPEGTables` (tag 347), carries a JPEG "abbreviated table
//! specification" stream whose `DQT` / `DHT` / `DRI` / `DAC` markers
//! apply by reference to every segment.
//!
//! Spec mechanics relevant to this module:
//!
//! * `JPEGTables` SHALL begin with `SOI` and end with `EOI`. It may
//!   contain `DQT`, `DHT`, `DAC`, `DRI`, `APPn` (ignored), `COM`
//!   (ignored) and nothing else.
//! * Each image segment SHALL contain a valid JPEG datastream. It
//!   "may simply refer to these preloaded tables without defining
//!   them" — so the segment's bytes generally do NOT include the
//!   `DQT` / `DHT` markers from `JPEGTables`.
//! * "An image segment may not redefine any table defined in
//!   `JPEGTables`." The merged stream is therefore well-defined.
//! * For DCT-based JPEG, `RowsPerStrip` / `TileLength` must be a
//!   multiple of `8 * max-vertical-sampling-factor` (i.e. the MCU
//!   height); single-strip images are exempt. We accept whatever the
//!   IFD says — bottom-edge padding rules live in the JPEG codec.
//!
//! The TIFF crate cannot reach `oxideav_mjpeg::decoder::decode_jpeg`
//! directly (it is `pub(crate)`), so the merged JPEG bytes flow
//! through the registered `Decoder` factory via `make_decoder` —
//! send_packet, receive_frame, copy planes out, drop the decoder.
//! This keeps the integration honest about its public API surface.

use crate::error::{Result, TiffError as Error};

use oxideav_core::{frame::VideoFrame, time::TimeBase, CodecId, CodecParameters, Frame, Packet};

/// Result of decoding one JPEG-compressed TIFF segment.
///
/// Each segment is a self-contained JPEG of the (logical) segment
/// dimensions. The TIFF compositor will eventually paste the visible
/// portion of these planes into the full-image buffer; this struct is
/// just the per-segment payload.
#[derive(Debug, Clone)]
pub struct JpegSegment {
    /// JPEG image width as declared in the SOFn marker. Per TN2 this
    /// matches `ImageWidth` for strips and `TileWidth` for tiles.
    pub width: u32,
    /// JPEG image height as declared in the SOFn marker. For strips
    /// this is `RowsPerStrip` (except the last strip, which may be
    /// shorter); for tiles this is `TileLength`.
    pub height: u32,
    /// Per-plane bytes. Layout depends on `pixel_format`.
    pub planes: Vec<Plane>,
    /// Output pixel format produced by the JPEG codec. Determined by
    /// the JPEG's component count + sampling factors — NOT by the
    /// TIFF photometric. The TIFF compositor cross-checks the two
    /// against `PhotometricInterpretation` + `YCbCrSubSampling`.
    pub pixel_format: JpegPixelFormat,
}

/// Pixel formats `oxideav-mjpeg` produces that we know how to splat
/// into a TIFF output buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegPixelFormat {
    Gray8,
    Yuv444P,
    Yuv422P,
    Yuv420P,
    Yuv411P,
    /// 3-component RGB (rare in JPEG-in-TIFF: needs `Adobe APP14`
    /// transform = 0 or a 3-component lossless / SOF3 stream). Used
    /// when `PhotometricInterpretation = RGB (2)` and `Sf` is 1h1v
    /// for every component.
    Rgb24,
    /// 3-component RGB delivered as a single packed plane of
    /// interleaved `R G B` bytes (3 bytes / pixel, stride ≥
    /// `width × 3`). `oxideav-mjpeg` may hand a full-resolution
    /// 3-component frame back either as three planar components
    /// ([`JpegPixelFormat::Rgb24`]) or in this packed layout
    /// depending on its build; both classify to the same TIFF
    /// render target (`PhotometricInterpretation = RGB (2)`), the
    /// compositor just blits rows instead of interleaving planes.
    Rgb24Packed,
    /// 4-component CMYK delivered as a single packed plane of
    /// `C M Y K` bytes (4 bytes / pixel). Used when
    /// `PhotometricInterpretation = CMYK (5)` and `SamplesPerPixel = 4`.
    /// `oxideav-mjpeg` consumes the optional Adobe APP14 marker inside
    /// the JPEG stream to pick the correct sample inversion (plain CMYK
    /// / Adobe-inverted CMYK / YCCK) and emits "regular" CMYK where
    /// `0 = no ink`, per TIFF 6.0 §16's `InkSet = 1` (CMYK) convention.
    /// The TIFF compositor only needs to walk the packed buffer and
    /// apply the additive-RGB conversion `R=(1-C)(1-K)` etc.
    Cmyk8,
}

/// One plane of segment pixels.
#[derive(Debug, Clone)]
pub struct Plane {
    /// Bytes per row in `data`.
    pub stride: usize,
    /// Plane width in samples.
    pub width: u32,
    /// Plane height in samples.
    pub height: u32,
    pub data: Vec<u8>,
}

/// Synthesise a single ISO JPEG datastream from the optional
/// `JPEGTables` blob (`tables`) and one image-segment blob
/// (`segment`).
///
/// Cases (per TN2):
///
/// 1. No `JPEGTables` and the segment is itself a complete JPEG
///    interchange stream → use the segment bytes verbatim.
/// 2. `JPEGTables` present and the segment is an abbreviated
///    image-only stream (still SOI..EOI but with the table markers
///    omitted) → strip the segment's leading `SOI` (it would otherwise
///    reset the decoder's table state per JPEG D.2.4) and splice the
///    table bytes between the table stream's `SOI` and `EOI` in front
///    of the segment, leaving exactly one `SOI` at the front and one
///    `EOI` at the back.
///
/// In both cases the result is a freestanding JPEG datastream that an
/// ordinary JPEG decoder will accept.
pub fn merge_jpeg_segment(tables: Option<&[u8]>, segment: &[u8]) -> Result<Vec<u8>> {
    if segment.len() < 4 || segment[0] != 0xFF || segment[1] != 0xD8 {
        return Err(Error::invalid(
            "TIFF/JPEG: segment does not begin with SOI (FF D8)",
        ));
    }
    if segment[segment.len() - 2..] != [0xFF, 0xD9] {
        return Err(Error::invalid(
            "TIFF/JPEG: segment does not end with EOI (FF D9)",
        ));
    }

    let Some(tab) = tables else {
        return Ok(segment.to_vec());
    };
    // JPEGTables is supposed to be a complete abbreviated table
    // stream framed by SOI..EOI per TN2. Strip both ends; if the
    // sentinels are missing the file is malformed.
    if tab.len() < 4 || tab[0] != 0xFF || tab[1] != 0xD8 {
        return Err(Error::invalid(
            "TIFF/JPEG: JPEGTables does not begin with SOI (FF D8)",
        ));
    }
    if tab[tab.len() - 2..] != [0xFF, 0xD9] {
        return Err(Error::invalid(
            "TIFF/JPEG: JPEGTables does not end with EOI (FF D9)",
        ));
    }
    let table_body = &tab[2..tab.len() - 2];

    // Splice: SOI (from segment) + table_body + segment_body (without
    // its leading SOI) + EOI (from segment). We preserve exactly one
    // leading SOI so the JPEG codec correctly resets the per-stream
    // state machine; the inner SOI from the tables blob is omitted
    // because TN2 says SOI carries no DAC/DRI state across the merge
    // boundary anyway.
    let mut out = Vec::with_capacity(tab.len() + segment.len());
    out.extend_from_slice(&segment[..2]); // SOI
    out.extend_from_slice(table_body);
    out.extend_from_slice(&segment[2..]); // segment body (incl. its EOI)
    Ok(out)
}

/// Hand a single freestanding JPEG bytestring to `oxideav-mjpeg` via
/// the framework `Decoder` trait surface and pull back the decoded
/// frame.
///
/// The TIFF crate cannot use mjpeg's `decoder::decode_jpeg` directly
/// (it is `pub(crate)`), so we go through the registered factory.
/// This adds one allocation per segment for the `CodecParameters`
/// scaffold but keeps the integration honest about its public API.
fn decode_one_jpeg(jpeg_bytes: Vec<u8>) -> Result<VideoFrame> {
    let params = CodecParameters::video(CodecId::new(oxideav_mjpeg::CODEC_ID_STR));
    let mut dec = oxideav_mjpeg::registry::make_decoder(&params)
        .map_err(|e| Error::invalid(format!("TIFF/JPEG: failed to make mjpeg decoder: {e}")))?;
    let pkt = Packet::new(0, TimeBase::new(1, 1), jpeg_bytes);
    dec.send_packet(&pkt)
        .map_err(|e| Error::invalid(format!("TIFF/JPEG: mjpeg send_packet: {e}")))?;
    match dec.receive_frame() {
        Ok(Frame::Video(vf)) => Ok(vf),
        Ok(other) => Err(Error::invalid(format!(
            "TIFF/JPEG: mjpeg returned non-video frame {other:?}"
        ))),
        Err(e) => Err(Error::invalid(format!(
            "TIFF/JPEG: mjpeg receive_frame: {e}"
        ))),
    }
}

/// Decide which TIFF-compositor-known JPEG output layout the codec
/// produced by inspecting the plane count + per-plane dimensions
/// against the segment's declared width / height.
fn classify(vf: &VideoFrame, seg_w: u32, seg_h: u32, photometric: u16) -> Result<JpegPixelFormat> {
    use crate::types::*;
    let np = vf.planes.len();
    match np {
        1 => {
            // Single plane: either grayscale (`stride ≈ width`) or
            // packed CMYK (`stride ≈ width * 4`, photometric must be
            // CMYK). Pick by stride.
            let p = &vf.planes[0];
            let w = seg_w as usize;
            let h = seg_h as usize;
            // CMYK detection: oxideav-mjpeg packs 4 components into
            // one plane with stride == width * 4. The TIFF spec
            // requires `PhotometricInterpretation = CMYK (5)` for that
            // layout per TN2 ("PhotometricInterpretation and related
            // fields shall describe the color space actually stored
            // in the file").
            if p.stride >= w.saturating_mul(4) && p.data.len() >= p.stride * h {
                if photometric != PHOTO_CMYK {
                    return Err(Error::invalid(format!(
                        "TIFF/JPEG: 1-plane packed-4 JPEG but photometric={photometric} (expected 5/CMYK)"
                    )));
                }
                return Ok(JpegPixelFormat::Cmyk8);
            }
            // Packed interleaved RGB: 3 components in one plane with
            // stride == width * 3. `oxideav-mjpeg` may deliver a
            // full-resolution 3-component frame this way (the planar
            // 3-plane layout remains accepted via the 3-plane arm
            // below). Gated on the photometric — only
            // `PhotometricInterpretation = RGB (2)` makes the packed
            // bytes a render-ready `R G B` stream — so a narrow
            // stride-padded gray plane can never be hijacked into
            // this branch.
            if photometric == PHOTO_RGB {
                if p.stride < w.saturating_mul(3) || p.data.len() < p.stride * h {
                    return Err(Error::invalid(format!(
                        "TIFF/JPEG: 1-plane JPEG with photometric=2/RGB but plane is not \
                         packed-3 (stride={} data={} expected w={seg_w} h={seg_h})",
                        p.stride,
                        p.data.len()
                    )));
                }
                return Ok(JpegPixelFormat::Rgb24Packed);
            }
            if photometric != PHOTO_BLACK_IS_ZERO && photometric != PHOTO_WHITE_IS_ZERO {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG: 1-plane JPEG but photometric={photometric} (expected 0 or 1)"
                )));
            }
            if p.stride < w || p.data.len() < p.stride * h {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG: gray plane too small: stride={} data={} expected w={seg_w} h={seg_h}",
                    p.stride,
                    p.data.len()
                )));
            }
            Ok(JpegPixelFormat::Gray8)
        }
        3 => {
            // YUV (chunky) or RGB. mjpeg's render path produces 3
            // planes of equal dimensions for RGB and 3 planes of
            // {full, sub, sub} dimensions for YUV.
            let y_w = vf.planes[0].stride.max(seg_w as usize);
            let c_stride = vf.planes[1].stride;
            // RGB-from-JPEG produces 3 planes each at full
            // resolution; YUV produces chroma at sub-resolution. We
            // distinguish by chroma stride.
            if c_stride == y_w {
                // All three planes are full-res. Treat as RGB iff
                // photometric is RGB; otherwise reject (we don't
                // know how to interpret a 3-plane full-res frame
                // with photometric=YCbCr — would imply 1:1
                // YCbCrSubSampling, fine to treat as Yuv444P).
                if photometric == PHOTO_RGB {
                    Ok(JpegPixelFormat::Rgb24)
                } else if photometric == PHOTO_YCBCR {
                    Ok(JpegPixelFormat::Yuv444P)
                } else {
                    Err(Error::invalid(format!(
                        "TIFF/JPEG: 3-plane full-res JPEG but photometric={photometric}"
                    )))
                }
            } else if c_stride * 2 == y_w {
                // Chroma half-width → 4:2:2 or 4:2:0; rely on chroma
                // height to disambiguate. mjpeg's plane layout for
                // chroma uses stride == ceil(w/sx) per plane.
                let c_h = vf.planes[1].data.len() / c_stride.max(1);
                let seg_h_us = seg_h as usize;
                if c_h >= seg_h_us {
                    Ok(JpegPixelFormat::Yuv422P)
                } else {
                    Ok(JpegPixelFormat::Yuv420P)
                }
            } else if c_stride * 4 == y_w {
                Ok(JpegPixelFormat::Yuv411P)
            } else {
                Err(Error::invalid(format!(
                    "TIFF/JPEG: cannot classify 3-plane JPEG (y_stride≈{y_w} chroma_stride={c_stride})"
                )))
            }
        }
        n => Err(Error::invalid(format!(
            "TIFF/JPEG: unsupported JPEG plane count {n}"
        ))),
    }
}

/// Decode one JPEG-compressed TIFF segment and return its planes.
///
/// `tables` is the optional `JPEGTables` IFD entry (tag 347) data,
/// passed verbatim. `segment` is the raw strip / tile bytes from the
/// file. `(seg_w, seg_h)` are the segment's logical dimensions (for
/// strips: `(ImageWidth, RowsThisStrip)`; for tiles: `(TileWidth,
/// TileLength)`).
///
/// `photometric` is the TIFF `PhotometricInterpretation` value, used
/// only to disambiguate the 3-plane full-res case (RGB vs Yuv444P)
/// and to validate the 1-plane case (grayscale photometrics only).
pub fn decode_segment(
    tables: Option<&[u8]>,
    segment: &[u8],
    seg_w: u32,
    seg_h: u32,
    photometric: u16,
) -> Result<JpegSegment> {
    let merged = merge_jpeg_segment(tables, segment)?;
    let vf = decode_one_jpeg(merged)?;
    let pf = classify(&vf, seg_w, seg_h, photometric)?;

    // Mjpeg's VideoFrame doesn't carry width/height — we trust the
    // segment dims from the IFD because TN2 mandates them to match
    // the JPEG SOFn dims byte-for-byte. The compositor only ever
    // reads the visible prefix anyway.
    let planes = vf
        .planes
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let (pw, ph) = plane_dims(pf, seg_w, seg_h, i);
            Plane {
                stride: p.stride,
                width: pw,
                height: ph,
                data: p.data,
            }
        })
        .collect();

    Ok(JpegSegment {
        width: seg_w,
        height: seg_h,
        planes,
        pixel_format: pf,
    })
}

/// Plane dimensions for component `i` of a `JpegPixelFormat`-format
/// frame whose luma plane is `(seg_w, seg_h)`.
fn plane_dims(pf: JpegPixelFormat, seg_w: u32, seg_h: u32, i: usize) -> (u32, u32) {
    if i == 0 {
        return (seg_w, seg_h);
    }
    match pf {
        JpegPixelFormat::Gray8 => (seg_w, seg_h),
        JpegPixelFormat::Yuv444P | JpegPixelFormat::Rgb24 => (seg_w, seg_h),
        // Packed RGB is a single plane; component index > 0 is never
        // queried (the compositor blits the packed rows directly).
        JpegPixelFormat::Rgb24Packed => (seg_w, seg_h),
        JpegPixelFormat::Yuv422P => (seg_w.div_ceil(2), seg_h),
        JpegPixelFormat::Yuv420P => (seg_w.div_ceil(2), seg_h.div_ceil(2)),
        JpegPixelFormat::Yuv411P => (seg_w.div_ceil(4), seg_h),
        // CMYK is a single packed plane; we never index by component
        // index > 0 for this layout (the compositor walks the packed
        // bytes directly), so the dims are reported as the segment
        // dims for completeness.
        JpegPixelFormat::Cmyk8 => (seg_w, seg_h),
    }
}

// ---------------------------------------------------------------------------
// YCbCr → RGB and other plane → packed conversions used by the compositor.
// ---------------------------------------------------------------------------

/// Convert a planar YUV segment to packed `Rgb24`. Coefficients
/// follow BT.601 with TN2's default `ReferenceBlackWhite =
/// [0, 255, 128, 255, 128, 255]` (JFIF-compatible). The decoder upsamples
/// chroma by nearest-neighbour replication — matching what most JPEG
/// codecs do when expanding subsampled chroma. The function writes
/// into an existing RGB buffer at the supplied `(dst_x, dst_y)`
/// offset, copying only the visible width / height.
#[allow(clippy::too_many_arguments)]
pub fn composite_yuv_to_rgb(
    seg: &JpegSegment,
    visible_w: u32,
    visible_h: u32,
    dst: &mut [u8],
    dst_row_stride: usize,
    dst_x: u32,
    dst_y: u32,
) -> Result<()> {
    let (sh, sv) = match seg.pixel_format {
        JpegPixelFormat::Gray8 => return Err(Error::invalid("composite_yuv_to_rgb on Gray8")),
        JpegPixelFormat::Rgb24 => return Err(Error::invalid("composite_yuv_to_rgb on Rgb24")),
        JpegPixelFormat::Rgb24Packed => {
            return Err(Error::invalid("composite_yuv_to_rgb on Rgb24Packed"))
        }
        JpegPixelFormat::Cmyk8 => return Err(Error::invalid("composite_yuv_to_rgb on Cmyk8")),
        JpegPixelFormat::Yuv444P => (1u32, 1u32),
        JpegPixelFormat::Yuv422P => (2, 1),
        JpegPixelFormat::Yuv420P => (2, 2),
        JpegPixelFormat::Yuv411P => (4, 1),
    };
    let yp = &seg.planes[0];
    let cb = &seg.planes[1];
    let cr = &seg.planes[2];
    for y in 0..visible_h as usize {
        let py = y;
        let cy = y / sv as usize;
        let dy = dst_y as usize + y;
        for x in 0..visible_w as usize {
            let cx = x / sh as usize;
            let y_val = yp.data[py * yp.stride + x] as i32;
            let cb_val = cb.data[cy * cb.stride + cx] as i32;
            let cr_val = cr.data[cy * cr.stride + cx] as i32;
            let (r, g, b) = ycbcr_to_rgb(y_val, cb_val, cr_val);
            let dst_off = dy * dst_row_stride + (dst_x as usize + x) * 3;
            dst[dst_off] = r;
            dst[dst_off + 1] = g;
            dst[dst_off + 2] = b;
        }
    }
    Ok(())
}

/// Composite a Gray8 segment into a Gray8 destination, applying the
/// `WhiteIsZero` polarity inversion when needed.
#[allow(clippy::too_many_arguments)]
pub fn composite_gray(
    seg: &JpegSegment,
    visible_w: u32,
    visible_h: u32,
    dst: &mut [u8],
    dst_row_stride: usize,
    dst_x: u32,
    dst_y: u32,
    invert: bool,
) -> Result<()> {
    if seg.pixel_format != JpegPixelFormat::Gray8 {
        return Err(Error::invalid(
            "composite_gray called with non-Gray8 segment",
        ));
    }
    let p = &seg.planes[0];
    for y in 0..visible_h as usize {
        let dy = dst_y as usize + y;
        let src_row = &p.data[y * p.stride..y * p.stride + visible_w as usize];
        let dst_row = &mut dst[dy * dst_row_stride + dst_x as usize
            ..dy * dst_row_stride + dst_x as usize + visible_w as usize];
        if invert {
            for (d, s) in dst_row.iter_mut().zip(src_row.iter()) {
                *d = 255 - *s;
            }
        } else {
            dst_row.copy_from_slice(src_row);
        }
    }
    Ok(())
}

/// Composite an `Rgb24` JPEG segment (3 planar full-resolution
/// components) into an Rgb24 destination by interleaving the planes.
pub fn composite_rgb_planar(
    seg: &JpegSegment,
    visible_w: u32,
    visible_h: u32,
    dst: &mut [u8],
    dst_row_stride: usize,
    dst_x: u32,
    dst_y: u32,
) -> Result<()> {
    if seg.pixel_format != JpegPixelFormat::Rgb24 && seg.pixel_format != JpegPixelFormat::Yuv444P {
        return Err(Error::invalid(
            "composite_rgb_planar called with non-3-plane-full-res segment",
        ));
    }
    let r = &seg.planes[0];
    let g = &seg.planes[1];
    let b = &seg.planes[2];
    for y in 0..visible_h as usize {
        let dy = dst_y as usize + y;
        for x in 0..visible_w as usize {
            let off = dy * dst_row_stride + (dst_x as usize + x) * 3;
            dst[off] = r.data[y * r.stride + x];
            dst[off + 1] = g.data[y * g.stride + x];
            dst[off + 2] = b.data[y * b.stride + x];
        }
    }
    Ok(())
}

/// Composite an `Rgb24Packed` JPEG segment (one plane of interleaved
/// `R G B` bytes, stride ≥ `width × 3`) into an Rgb24 destination by
/// blitting the visible prefix of each packed row.
pub fn composite_rgb_packed(
    seg: &JpegSegment,
    visible_w: u32,
    visible_h: u32,
    dst: &mut [u8],
    dst_row_stride: usize,
    dst_x: u32,
    dst_y: u32,
) -> Result<()> {
    if seg.pixel_format != JpegPixelFormat::Rgb24Packed {
        return Err(Error::invalid(
            "composite_rgb_packed called with non-packed-RGB segment",
        ));
    }
    let p = &seg.planes[0];
    let row_bytes = visible_w as usize * 3;
    for y in 0..visible_h as usize {
        let dy = dst_y as usize + y;
        let src_row = &p.data[y * p.stride..y * p.stride + row_bytes];
        let dst_off = dy * dst_row_stride + dst_x as usize * 3;
        dst[dst_off..dst_off + row_bytes].copy_from_slice(src_row);
    }
    Ok(())
}

/// Composite a packed-CMYK JPEG segment into an `Rgb24` destination.
///
/// The single segment plane is the `C M Y K` byte stream produced by
/// `oxideav-mjpeg` for a 4-component JPEG. The conversion is the same
/// additive-RGB formula the uncompressed CMYK path
/// (`build_rgb24_from_cmyk` in `decoder.rs`) uses, matching TIFF 6.0
/// §16 (`InkSet = 1`) and what `tiffinfo` / `magick` reference
/// rendering produces:
///
/// * `R = (255 − C) × (255 − K) / 255`
/// * `G = (255 − M) × (255 − K) / 255`
/// * `B = (255 − Y) × (255 − K) / 255`
///
/// (Per the spec, stored CMYK values are the *amount of dye*: larger
/// = darker. mjpeg has already consumed any Adobe APP14 transform
/// marker and emits "regular" CMYK where `0 = no ink`, so no further
/// per-sample inversion is needed here.)
pub fn composite_cmyk_to_rgb(
    seg: &JpegSegment,
    visible_w: u32,
    visible_h: u32,
    dst: &mut [u8],
    dst_row_stride: usize,
    dst_x: u32,
    dst_y: u32,
) -> Result<()> {
    if seg.pixel_format != JpegPixelFormat::Cmyk8 {
        return Err(Error::invalid(
            "composite_cmyk_to_rgb called with non-Cmyk8 segment",
        ));
    }
    let p = &seg.planes[0];
    for y in 0..visible_h as usize {
        let dy = dst_y as usize + y;
        let src_off = y * p.stride;
        for x in 0..visible_w as usize {
            let s = src_off + x * 4;
            let c = p.data[s] as u32;
            let m = p.data[s + 1] as u32;
            let yy = p.data[s + 2] as u32;
            let k = p.data[s + 3] as u32;
            let r = ((255 - c) * (255 - k) / 255) as u8;
            let g = ((255 - m) * (255 - k) / 255) as u8;
            let b = ((255 - yy) * (255 - k) / 255) as u8;
            let off = dy * dst_row_stride + (dst_x as usize + x) * 3;
            dst[off] = r;
            dst[off + 1] = g;
            dst[off + 2] = b;
        }
    }
    Ok(())
}

fn ycbcr_to_rgb(y: i32, cb: i32, cr: i32) -> (u8, u8, u8) {
    let cb = cb - 128;
    let cr = cr - 128;
    // BT.601 Q16 coefficients matching the TN2 default
    // ReferenceBlackWhite of [0,255,128,255,128,255].
    let r = y + ((91881 * cr + 32768) >> 16);
    let g = y - ((22554 * cb + 46802 * cr + 32768) >> 16);
    let b = y + ((116130 * cb + 32768) >> 16);
    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest possible "tables" stream: SOI + EOI, nothing in
    /// between. The merge should produce exactly the input segment.
    #[test]
    fn merge_empty_tables_passes_segment_through() {
        let seg = vec![0xFF, 0xD8, 0xAB, 0xCD, 0xFF, 0xD9];
        let tables = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let merged = merge_jpeg_segment(Some(&tables), &seg).unwrap();
        // table_body is empty; merged = SOI + segment_body
        assert_eq!(merged, vec![0xFF, 0xD8, 0xAB, 0xCD, 0xFF, 0xD9]);
    }

    /// JPEGTables with one byte of "payload" between SOI and EOI: the
    /// merge should interpose exactly that one byte right after the
    /// segment's SOI.
    #[test]
    fn merge_with_table_payload() {
        let seg = vec![0xFF, 0xD8, 0x11, 0x22, 0xFF, 0xD9];
        let tables = vec![0xFF, 0xD8, 0xEE, 0xFF, 0xD9];
        let merged = merge_jpeg_segment(Some(&tables), &seg).unwrap();
        assert_eq!(merged, vec![0xFF, 0xD8, 0xEE, 0x11, 0x22, 0xFF, 0xD9]);
    }

    /// No JPEGTables → segment bytes are returned verbatim.
    #[test]
    fn merge_without_tables_returns_segment_clone() {
        let seg = vec![0xFF, 0xD8, 0x42, 0xFF, 0xD9];
        let merged = merge_jpeg_segment(None, &seg).unwrap();
        assert_eq!(merged, seg);
    }

    #[test]
    fn merge_rejects_segment_without_soi() {
        let seg = vec![0xFF, 0xE0, 0xFF, 0xD9];
        assert!(merge_jpeg_segment(None, &seg).is_err());
    }

    #[test]
    fn merge_rejects_segment_without_eoi() {
        let seg = vec![0xFF, 0xD8, 0x00, 0x00];
        assert!(merge_jpeg_segment(None, &seg).is_err());
    }

    #[test]
    fn merge_rejects_tables_without_soi() {
        let seg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let bad_tab = vec![0xFF, 0xE0, 0xFF, 0xD9];
        assert!(merge_jpeg_segment(Some(&bad_tab), &seg).is_err());
    }

    #[test]
    fn merge_rejects_tables_without_eoi() {
        let seg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let bad_tab = vec![0xFF, 0xD8, 0x00, 0x00];
        assert!(merge_jpeg_segment(Some(&bad_tab), &seg).is_err());
    }

    #[test]
    fn jpeg_pixel_format_variants_distinct() {
        // Mostly a smoke test that the enum compiles + Eq is wired.
        assert_ne!(JpegPixelFormat::Gray8, JpegPixelFormat::Yuv420P);
        assert_ne!(JpegPixelFormat::Yuv422P, JpegPixelFormat::Yuv444P);
        assert_ne!(JpegPixelFormat::Cmyk8, JpegPixelFormat::Gray8);
        assert_ne!(JpegPixelFormat::Cmyk8, JpegPixelFormat::Rgb24);
    }

    /// `composite_cmyk_to_rgb` should apply the additive-RGB inverse
    /// of `R = (1-C)(1-K)`, etc., so:
    ///
    /// - (C=0, M=0, Y=0, K=0)     → (255, 255, 255)  pure white
    /// - (C=255, M=0, Y=0, K=0)   → (  0, 255, 255)  pure cyan
    /// - (C=0, M=255, Y=0, K=0)   → (255,   0, 255)  pure magenta
    /// - (C=0, M=0, Y=255, K=0)   → (255, 255,   0)  pure yellow
    /// - (C=0, M=0, Y=0, K=255)   → (  0,   0,   0)  pure black
    /// - (C=128, M=128, Y=128, K=128) → mid-gray-ish
    ///
    /// Tests both interior and edge pixels; checks the segment stride
    /// path is honoured (stride > width*4).
    #[test]
    fn composite_cmyk_to_rgb_known_values() {
        // 4×1 segment with one of each spec pixel.
        let plane_w = 4u32;
        let plane_h = 1u32;
        let stride = plane_w as usize * 4 + 3; // extra padding to exercise stride
        let mut data = vec![0xAAu8; stride * plane_h as usize];
        // pixel 0: white
        data[0] = 0;
        data[1] = 0;
        data[2] = 0;
        data[3] = 0;
        // pixel 1: cyan
        data[4] = 255;
        data[5] = 0;
        data[6] = 0;
        data[7] = 0;
        // pixel 2: yellow
        data[8] = 0;
        data[9] = 0;
        data[10] = 255;
        data[11] = 0;
        // pixel 3: pure-K black
        data[12] = 0;
        data[13] = 0;
        data[14] = 0;
        data[15] = 255;

        let seg = JpegSegment {
            width: plane_w,
            height: plane_h,
            planes: vec![Plane {
                stride,
                width: plane_w,
                height: plane_h,
                data,
            }],
            pixel_format: JpegPixelFormat::Cmyk8,
        };
        let dst_stride = plane_w as usize * 3;
        let mut dst = vec![0u8; dst_stride * plane_h as usize];
        composite_cmyk_to_rgb(&seg, plane_w, plane_h, &mut dst, dst_stride, 0, 0).unwrap();
        assert_eq!(&dst[0..3], &[255, 255, 255], "white pixel");
        assert_eq!(&dst[3..6], &[0, 255, 255], "cyan pixel");
        assert_eq!(&dst[6..9], &[255, 255, 0], "yellow pixel");
        assert_eq!(&dst[9..12], &[0, 0, 0], "pure-K black pixel");
    }

    /// A full-resolution 3-component frame delivered as one packed
    /// interleaved plane (stride == width * 3) must classify as
    /// `Rgb24Packed` when the TIFF photometric is RGB (2) — and the
    /// classic 3-planar delivery must keep classifying as `Rgb24`,
    /// since both `oxideav-mjpeg` output shapes are in circulation.
    #[test]
    fn classify_accepts_packed_and_planar_rgb() {
        use oxideav_core::frame::{VideoFrame, VideoPlane};
        let w = 4u32;
        let h = 2u32;
        let packed = VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: w as usize * 3,
                data: vec![0u8; w as usize * 3 * h as usize],
            }],
        };
        assert_eq!(
            classify(&packed, w, h, crate::types::PHOTO_RGB).unwrap(),
            JpegPixelFormat::Rgb24Packed
        );
        let planar = VideoFrame {
            pts: None,
            planes: (0..3)
                .map(|_| VideoPlane {
                    stride: w as usize,
                    data: vec![0u8; w as usize * h as usize],
                })
                .collect(),
        };
        assert_eq!(
            classify(&planar, w, h, crate::types::PHOTO_RGB).unwrap(),
            JpegPixelFormat::Rgb24
        );
    }

    /// A 1-plane frame whose stride cannot hold packed `R G B` rows
    /// must NOT silently classify under photometric=RGB.
    #[test]
    fn classify_rejects_underweight_plane_for_rgb_photometric() {
        use oxideav_core::frame::{VideoFrame, VideoPlane};
        let vf = VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 4, // gray-shaped: one byte per pixel
                data: vec![0u8; 8],
            }],
        };
        assert!(classify(&vf, 4, 2, crate::types::PHOTO_RGB).is_err());
    }

    /// `composite_rgb_packed` blits the visible prefix of each packed
    /// row, honouring a source stride wider than `width * 3` and a
    /// non-zero destination offset.
    #[test]
    fn composite_rgb_packed_blits_rows() {
        let w = 2u32;
        let h = 2u32;
        let stride = w as usize * 3 + 2; // padded source rows
        let mut data = vec![0xEEu8; stride * h as usize];
        // Row 0: (1,2,3) (4,5,6); row 1: (7,8,9) (10,11,12).
        data[0..6].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        data[stride..stride + 6].copy_from_slice(&[7, 8, 9, 10, 11, 12]);
        let seg = JpegSegment {
            width: w,
            height: h,
            planes: vec![Plane {
                stride,
                width: w,
                height: h,
                data,
            }],
            pixel_format: JpegPixelFormat::Rgb24Packed,
        };
        // Destination is 3x2 RGB; paste at dst_x = 1 so the offset
        // arithmetic is exercised.
        let dst_stride = 3usize * 3;
        let mut dst = vec![0u8; dst_stride * 2];
        composite_rgb_packed(&seg, w, h, &mut dst, dst_stride, 1, 0).unwrap();
        assert_eq!(&dst[3..9], &[1, 2, 3, 4, 5, 6], "row 0 pasted at x=1");
        assert_eq!(
            &dst[dst_stride + 3..dst_stride + 9],
            &[7, 8, 9, 10, 11, 12],
            "row 1 pasted at x=1"
        );
        assert_eq!(&dst[0..3], &[0, 0, 0], "pixel left of paste untouched");
    }

    /// Packed-RGB composite on a non-packed segment must error.
    #[test]
    fn composite_rgb_packed_rejects_non_packed_segment() {
        let seg = JpegSegment {
            width: 1,
            height: 1,
            planes: vec![Plane {
                stride: 1,
                width: 1,
                height: 1,
                data: vec![0u8],
            }],
            pixel_format: JpegPixelFormat::Gray8,
        };
        let mut dst = vec![0u8; 3];
        assert!(composite_rgb_packed(&seg, 1, 1, &mut dst, 3, 0, 0).is_err());
    }

    /// CMYK composite to a non-Cmyk segment must error.
    #[test]
    fn composite_cmyk_rejects_non_cmyk_segment() {
        let seg = JpegSegment {
            width: 1,
            height: 1,
            planes: vec![Plane {
                stride: 1,
                width: 1,
                height: 1,
                data: vec![0u8],
            }],
            pixel_format: JpegPixelFormat::Gray8,
        };
        let mut dst = vec![0u8; 3];
        let r = composite_cmyk_to_rgb(&seg, 1, 1, &mut dst, 3, 0, 0);
        assert!(r.is_err());
    }
}
