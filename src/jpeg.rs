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
            // Single plane: grayscale, full-resolution.
            if photometric != PHOTO_BLACK_IS_ZERO && photometric != PHOTO_WHITE_IS_ZERO {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG: 1-plane JPEG but photometric={photometric} (expected 0 or 1)"
                )));
            }
            let p = &vf.planes[0];
            if p.stride < seg_w as usize || p.data.len() < p.stride * seg_h as usize {
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
        JpegPixelFormat::Yuv422P => (seg_w.div_ceil(2), seg_h),
        JpegPixelFormat::Yuv420P => (seg_w.div_ceil(2), seg_h.div_ceil(2)),
        JpegPixelFormat::Yuv411P => (seg_w.div_ceil(4), seg_h),
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
    }
}
