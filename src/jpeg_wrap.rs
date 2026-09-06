//! TIFF Technical Note 2 (`Compression = 7`) **segment builder**: turns
//! a page's full-resolution sample raster into the per-strip / per-tile
//! JPEG datastreams (and the optional `JPEGTables` stream) that the
//! IFD writer stores.
//!
//! TN2 rules implemented here (quotes from the staged
//! `docs/image/tiff/technote2-jpeg-in-tiff.html`, "Replacement
//! TIFF/JPEG specification"):
//!
//! * "each image segment contains a complete JPEG datastream … The
//!   datastream shall contain a single JPEG frame storing that segment
//!   of the image." — one [`crate::jpeg_enc::encode_frame`] call per
//!   strip / tile.
//! * "The SOFn marker shall be of type SOF0 for strict baseline JPEG
//!   data, of type SOF1 for non-baseline lossy JPEG data, or of type
//!   SOF3 for lossless JPEG data. … All segments of a JPEG-compressed
//!   TIFF image shall use the same JPEG compression process".
//! * "The data precision field of the SOFn marker shall agree with the
//!   TIFF BitsPerSample field."
//! * Strip geometry: "the SOFn image width shall equal ImageWidth and
//!   the height shall equal RowsPerStrip, except in the last strip;
//!   its SOFn height shall equal the number of rows remaining".
//!   Tiles: "each SOFn shall have width TileWidth and height
//!   TileHeight" — edge tiles carry the §15 replicated padding.
//! * "The number of components in the JPEG datastream shall equal
//!   SamplesPerPixel for PlanarConfiguration=1, and shall be 1 for
//!   PlanarConfiguration=2. The components shall be stored in the same
//!   order as they are described at the TIFF field level."
//! * "In PlanarConfiguration 1, the sampling factors given in SOFn
//!   markers shall agree with the sampling factors defined by the
//!   related TIFF fields" — the TN2 table: `YCbCrSubSampling = [h, v]`
//!   ↔ luma `h`×`v`, chroma `1`×`1`; every other colour space uses
//!   all-ones factors ("Use no subsampling … for color spaces other
//!   than YCbCr").
//! * PlanarConfiguration 2: "the dimensions given in the SOFn of a
//!   subsampled component shall be scaled down by the sampling
//!   factors … In strip TIFF files the computed dimensions may need to
//!   be rounded up to the next integer" and "all SOFn sampling factors
//!   shall be given as 1".
//! * `JPEGTables`: "When a JPEGTables field is used, image segments may
//!   omit tables that have been specified in the JPEGTables field" —
//!   the shared layout writes every table there and abbreviated
//!   segments; the per-segment layout defines all tables in each
//!   segment and writes no `JPEGTables` field.
//! * Chroma decimation (TN2's Section 21 amendment: "pad the source
//!   data to a multiple of the sampling factors by replication of the
//!   last column and/or row, then downsample") uses the same rounded
//!   box mean as the uncompressed §21 writer, so the two YCbCr layouts
//!   agree sample-for-sample before entropy coding.

use crate::error::{Result, TiffError as Error};
use crate::jpeg_enc::{
    encode_frame, gather_stats, scaled_quant_table, HuffSpec, HuffStats, JpegComponent, JpegFrame,
    JpegProcess, JpegTableSet, QUANT_CHROMINANCE_K2, QUANT_LUMINANCE_K1,
};
use crate::types::*;

/// Where the JPEG quantisation / Huffman tables live (TN2 "JPEGTables
/// field").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JpegTablesLayout {
    /// One `JPEGTables` field (tag 347) carries every table as an
    /// "abbreviated table specification" datastream and each strip /
    /// tile is an abbreviated image segment that only references
    /// them. TN2's recommended space-saving layout; the default.
    #[default]
    Shared,
    /// Every strip / tile is a complete interchange datastream that
    /// defines all the tables it uses; no `JPEGTables` field is
    /// written.
    PerSegment,
}

/// JPEG-in-TIFF writer options ([`crate::TiffCompression::Jpeg`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegOptions {
    /// Quantiser quality knob, 1..=100 (T.81 Annex K.1 tables scaled
    /// as documented on [`crate::jpeg_enc::scaled_quant_table`]);
    /// ignored by the lossless process. Default 75.
    pub quality: u8,
    /// Table placement — see [`JpegTablesLayout`]. Default `Shared`.
    pub tables: JpegTablesLayout,
    /// The coding process: sequential DCT (`SOF0` at 8 bits, `SOF1` at
    /// 12 bits) or lossless `SOF3` with a Table H.1 predictor. Default
    /// DCT.
    pub process: JpegProcess,
    /// Derive per-image optimal Huffman tables (T.81 K.2) instead of
    /// the Annex K.3 typical tables. Always on for 12-bit DCT (the
    /// K.3 tables cannot code the extended categories) and for the
    /// lossless process. Default off.
    pub optimize_huffman: bool,
}

impl Default for JpegOptions {
    fn default() -> Self {
        JpegOptions {
            quality: 75,
            tables: JpegTablesLayout::Shared,
            process: JpegProcess::Dct,
            optimize_huffman: false,
        }
    }
}

/// Everything the segment builder needs about the page.
pub(crate) struct JpegPageInput<'a> {
    /// Full-resolution interleaved samples: `bits` = 8 → one byte per
    /// sample, 12 / 16 → little-endian `u16` per sample.
    pub raw: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub spp: usize,
    pub bits: u16,
    pub photometric: u16,
    /// `YCbCrSubSampling` for YCbCr pages (`(1, 1)` otherwise).
    pub subsampling: (usize, usize),
    pub planar: bool,
    /// `Some((tile_w, tile_h))` for tiled pages.
    pub tiling: Option<(usize, usize)>,
    /// Strip height in luma rows (whole image when single-strip).
    pub rows_per_strip: usize,
    pub opts: JpegOptions,
}

/// One planned segment: its component planes at their own resolution.
struct Segment {
    width: u16,
    height: u16,
    comps: Vec<CompPlane>,
}

struct CompPlane {
    samples: Vec<u16>,
    width: usize,
    height: usize,
    h: u8,
    v: u8,
    quant_id: u8,
    huff_id: u8,
}

impl Segment {
    fn components(&self) -> Vec<JpegComponent<'_>> {
        self.comps
            .iter()
            .map(|c| JpegComponent {
                samples: &c.samples,
                width: c.width,
                height: c.height,
                h: c.h,
                v: c.v,
                quant_id: c.quant_id,
                huff_id: c.huff_id,
            })
            .collect()
    }
}

/// Read sample `(x, y, c)` of the interleaved raster.
fn sample_at(inp: &JpegPageInput<'_>, x: usize, y: usize, c: usize) -> u16 {
    let idx = (y * inp.width + x) * inp.spp + c;
    if inp.bits > 8 {
        u16::from_le_bytes([inp.raw[idx * 2], inp.raw[idx * 2 + 1]])
    } else {
        inp.raw[idx] as u16
    }
}

/// Table destinations per component: YCbCr puts the luma on slot 0
/// and both chroma components on slot 1; every other colour space
/// uses slot 0 for all components.
fn table_ids(photometric: u16, c: usize) -> (u8, u8) {
    if photometric == PHOTO_YCBCR && c > 0 {
        (1, 1)
    } else {
        (0, 0)
    }
}

/// Per-component sampling factors at the TIFF field level
/// (`(sh, sv)` for the luma of a subsampled YCbCr page, `(1, 1)`
/// otherwise); chroma components are `(1, 1)` in the frame header but
/// *decimated* by the luma factors.
fn frame_factors(inp: &JpegPageInput<'_>, c: usize) -> (u8, u8) {
    if inp.photometric == PHOTO_YCBCR && c == 0 {
        (inp.subsampling.0 as u8, inp.subsampling.1 as u8)
    } else {
        (1, 1)
    }
}

/// Decimation factors of component `c` relative to the luma grid.
fn decimation(inp: &JpegPageInput<'_>, c: usize) -> (usize, usize) {
    if inp.photometric == PHOTO_YCBCR && c > 0 {
        inp.subsampling
    } else {
        (1, 1)
    }
}

/// Extract component `c` over the luma-grid window
/// `[x0, x0 + win_w) × [y0, y0 + win_h)` (coordinates clamped to the
/// image for §15 edge replication), decimating by `(dh, dv)` with a
/// rounded box mean. Returns the plane and its dimensions
/// (`ceil(win / d)`).
#[allow(clippy::too_many_arguments)]
fn extract_plane(
    inp: &JpegPageInput<'_>,
    c: usize,
    x0: usize,
    y0: usize,
    win_w: usize,
    win_h: usize,
    dh: usize,
    dv: usize,
) -> (Vec<u16>, usize, usize) {
    let pw = win_w.div_ceil(dh);
    let ph = win_h.div_ceil(dv);
    let mut out = vec![0u16; pw * ph];
    for by in 0..ph {
        for bx in 0..pw {
            let mut sum = 0u32;
            let mut n = 0u32;
            for sy in 0..dv {
                for sx in 0..dh {
                    let wx = bx * dh + sx;
                    let wy = by * dv + sy;
                    if wx >= win_w || wy >= win_h {
                        continue;
                    }
                    let x = (x0 + wx).min(inp.width - 1);
                    let y = (y0 + wy).min(inp.height - 1);
                    sum += sample_at(inp, x, y, c) as u32;
                    n += 1;
                }
            }
            out[by * pw + bx] = ((sum + n / 2) / n) as u16;
        }
    }
    (out, pw, ph)
}

fn dim16(v: usize, what: &str) -> Result<u16> {
    u16::try_from(v).map_err(|_| {
        Error::invalid(format!(
            "TIFF encode/JPEG: {what} {v} exceeds the 65535 limit of the SOFn dimension \
             fields (TN2: split the image into smaller strips or tiles)"
        ))
    })
}

/// A chunky (PlanarConfiguration = 1) segment over one luma window.
fn chunky_segment(
    inp: &JpegPageInput<'_>,
    x0: usize,
    y0: usize,
    win_w: usize,
    win_h: usize,
) -> Result<Segment> {
    let mut comps = Vec::with_capacity(inp.spp);
    for c in 0..inp.spp {
        let (dh, dv) = decimation(inp, c);
        let (samples, width, height) = extract_plane(inp, c, x0, y0, win_w, win_h, dh, dv);
        let (h, v) = frame_factors(inp, c);
        let (quant_id, huff_id) = table_ids(inp.photometric, c);
        comps.push(CompPlane {
            samples,
            width,
            height,
            h,
            v,
            quant_id,
            huff_id,
        });
    }
    Ok(Segment {
        width: dim16(win_w, "segment width")?,
        height: dim16(win_h, "segment height")?,
        comps,
    })
}

/// The segments of a page plus the optional `JPEGTables` stream.
pub(crate) struct JpegSegments {
    /// Per-segment datastreams in TIFF storage order.
    pub segments: Vec<Vec<u8>>,
    /// The `JPEGTables` (tag 347) payload under the shared layout.
    pub tables: Option<Vec<u8>>,
}

/// A planar (PlanarConfiguration = 2) single-component segment: the
/// component-`c` plane over the *plane-grid* window
/// `[px0, px0 + pw) × [py0, py0 + ph)`; the plane itself is the
/// full-image component decimated by its factors (TN2: "the
/// dimensions … scaled down by the sampling factors").
#[allow(clippy::too_many_arguments)]
fn planar_segment(
    inp: &JpegPageInput<'_>,
    plane: &[u16],
    plane_w: usize,
    plane_h: usize,
    c: usize,
    px0: usize,
    py0: usize,
    pw: usize,
    ph: usize,
) -> Result<Segment> {
    let mut samples = Vec::with_capacity(pw * ph);
    for y in 0..ph {
        let sy = (py0 + y).min(plane_h - 1);
        for x in 0..pw {
            let sx = (px0 + x).min(plane_w - 1);
            samples.push(plane[sy * plane_w + sx]);
        }
    }
    let (quant_id, huff_id) = table_ids(inp.photometric, c);
    Ok(Segment {
        width: dim16(pw, "plane segment width")?,
        height: dim16(ph, "plane segment height")?,
        comps: vec![CompPlane {
            samples,
            width: pw,
            height: ph,
            h: 1,
            v: 1,
            quant_id,
            huff_id,
        }],
    })
}

/// Plan every segment of the page in TIFF storage order (strips
/// top-to-bottom; tiles row-major; planar layouts plane-major).
fn plan_segments(inp: &JpegPageInput<'_>) -> Result<Vec<Segment>> {
    let mut segs = Vec::new();
    if !inp.planar {
        match inp.tiling {
            Some((tw, th)) => {
                let across = inp.width.div_ceil(tw);
                let down = inp.height.div_ceil(th);
                for ty in 0..down {
                    for tx in 0..across {
                        segs.push(chunky_segment(inp, tx * tw, ty * th, tw, th)?);
                    }
                }
            }
            None => {
                let mut done = 0usize;
                while done < inp.height {
                    let rows = inp.rows_per_strip.min(inp.height - done);
                    segs.push(chunky_segment(inp, 0, done, inp.width, rows)?);
                    done += rows;
                }
            }
        }
        return Ok(segs);
    }
    for c in 0..inp.spp {
        let (dh, dv) = decimation(inp, c);
        let (plane, plane_w, plane_h) = extract_plane(inp, c, 0, 0, inp.width, inp.height, dh, dv);
        match inp.tiling {
            Some((tw, th)) => {
                // TN2 tiled PlanarConfiguration 2: the tile-size
                // restrictions make the scaled tile dimensions exact.
                let (ptw, pth) = (tw / dh, th / dv);
                let across = inp.width.div_ceil(tw);
                let down = inp.height.div_ceil(th);
                for ty in 0..down {
                    for tx in 0..across {
                        segs.push(planar_segment(
                            inp,
                            &plane,
                            plane_w,
                            plane_h,
                            c,
                            tx * ptw,
                            ty * pth,
                            ptw,
                            pth,
                        )?);
                    }
                }
            }
            None => {
                // Strips: the plane's strip covers the same image area
                // as the luma strip; its height is the luma row count
                // scaled down and rounded up (TN2).
                let mut done = 0usize;
                while done < inp.height {
                    let luma_rows = inp.rows_per_strip.min(inp.height - done);
                    let seg_h = luma_rows.div_ceil(dv);
                    let py0 = done / dv;
                    segs.push(planar_segment(
                        inp, &plane, plane_w, plane_h, c, 0, py0, plane_w, seg_h,
                    )?);
                    done += luma_rows;
                }
            }
        }
    }
    Ok(segs)
}

/// The quantisation half of the table set (slot 0 luma / general,
/// slot 1 chroma — only when a YCbCr component references it).
fn quant_tables(inp: &JpegPageInput<'_>) -> JpegTableSet {
    let mut t = JpegTableSet::default();
    let precision = inp.bits as u8;
    t.quant[0] = Some(scaled_quant_table(
        &QUANT_LUMINANCE_K1,
        inp.opts.quality,
        precision,
    ));
    if inp.photometric == PHOTO_YCBCR && inp.spp > 1 {
        t.quant[1] = Some(scaled_quant_table(
            &QUANT_CHROMINANCE_K2,
            inp.opts.quality,
            precision,
        ));
    }
    t
}

/// Whether the Annex K.3 typical tables can be used as-is.
fn use_typical_tables(inp: &JpegPageInput<'_>) -> bool {
    inp.bits == 8 && matches!(inp.opts.process, JpegProcess::Dct) && !inp.opts.optimize_huffman
}

fn install_typical(t: &mut JpegTableSet, inp: &JpegPageInput<'_>) {
    t.dc[0] = Some(HuffSpec::k3_dc_luminance());
    t.ac[0] = Some(HuffSpec::k5_ac_luminance());
    if inp.photometric == PHOTO_YCBCR && inp.spp > 1 {
        t.dc[1] = Some(HuffSpec::k4_dc_chrominance());
        t.ac[1] = Some(HuffSpec::k6_ac_chrominance());
    }
}

/// K.2 optimal tables from the statistics of `segs` (one or many).
fn install_optimal(
    t: &mut JpegTableSet,
    frame_of: &dyn Fn(&Segment) -> JpegFrame,
    segs: &[&Segment],
) -> Result<()> {
    let mut dc: [HuffStats; 4] = Default::default();
    let mut ac: [HuffStats; 4] = Default::default();
    for s in segs {
        let frame = frame_of(s);
        gather_stats(&frame, &s.components(), t, &mut dc, &mut ac)?;
    }
    for i in 0..4 {
        t.dc[i] = if dc[i].is_empty() {
            None
        } else {
            Some(dc[i].to_spec())
        };
        t.ac[i] = if ac[i].is_empty() {
            None
        } else {
            Some(ac[i].to_spec())
        };
    }
    Ok(())
}

/// Build the page's JPEG segments. Returns the per-segment
/// datastreams in TIFF storage order plus the `JPEGTables` stream when
/// the shared layout is in use.
pub(crate) fn build_jpeg_segments(inp: &JpegPageInput<'_>) -> Result<JpegSegments> {
    let segs = plan_segments(inp)?;
    let frame_of = |s: &Segment| JpegFrame {
        width: s.width,
        height: s.height,
        precision: inp.bits as u8,
        process: inp.opts.process,
    };
    let dct = matches!(inp.opts.process, JpegProcess::Dct);
    match inp.opts.tables {
        JpegTablesLayout::Shared => {
            let mut tables = quant_tables(inp);
            if use_typical_tables(inp) {
                install_typical(&mut tables, inp);
            } else {
                let refs: Vec<&Segment> = segs.iter().collect();
                install_optimal(&mut tables, &frame_of, &refs)?;
            }
            let mut out = Vec::with_capacity(segs.len());
            for s in &segs {
                out.push(encode_frame(&frame_of(s), &s.components(), &tables, false)?);
            }
            Ok(JpegSegments {
                segments: out,
                tables: Some(tables.tables_stream(dct)),
            })
        }
        JpegTablesLayout::PerSegment => {
            let mut out = Vec::with_capacity(segs.len());
            let typical = use_typical_tables(inp);
            let mut shared = quant_tables(inp);
            if typical {
                install_typical(&mut shared, inp);
            }
            for s in &segs {
                let stream = if typical {
                    encode_frame(&frame_of(s), &s.components(), &shared, true)?
                } else {
                    let mut tables = quant_tables(inp);
                    install_optimal(&mut tables, &frame_of, &[s])?;
                    encode_frame(&frame_of(s), &s.components(), &tables, true)?
                };
                out.push(stream);
            }
            Ok(JpegSegments {
                segments: out,
                tables: None,
            })
        }
    }
}
