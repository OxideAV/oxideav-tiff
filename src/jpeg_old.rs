//! TIFF 6.0 §22 "JPEG Compression" (`Compression = 6`, "old-style"
//! JPEG) field parsing and the JPEGInterchangeFormat stream
//! extraction.
//!
//! §22 defines two on-disk layouts for JPEG-compressed data:
//!
//! 1. **Interchange-format layout** — `JPEGInterchangeFormat`
//!    (tag 513) "indicates whether a JPEG interchange format bitstream
//!    is present in the TIFF file. If a JPEG interchange format
//!    bitstream is present, then this Field points to the Start of
//!    Image (SOI) marker code." `JPEGInterchangeFormatLength`
//!    (tag 514) gives its byte length, "useful for extracting the JPEG
//!    interchange format bitstream without parsing the bitstream."
//!    §22 "Strips and Tiles": "Compressed images conforming to the
//!    syntax of the JPEG interchange format can be converted to TIFF
//!    simply by defining a single strip or tile for the entire image
//!    and then concatenating the TIFF image description fields to the
//!    JPEG compressed image data." This layout is a complete,
//!    freestanding ISO JPEG datastream and decodes here.
//!
//! 2. **Tables-form layout** — no interchange stream; instead
//!    `JPEGQTables` / `JPEGDCTables` / `JPEGACTables` (tags 519-521)
//!    point at *raw* table payloads (64-byte zigzag quantization
//!    tables; Huffman tables as "16 BYTES of 'BITS'" + "VALUES") and
//!    each strip / tile "points directly to the start of the entropy
//!    coded data (not to a JPEG marker)". Turning that back into a
//!    decodable stream requires synthesizing ISO 10918-1 marker
//!    segments (DQT / DHT / SOF / SOS) around the raw tables — the
//!    marker byte syntax is defined by ISO 10918-1, not by the TIFF
//!    spec, so this build reports the layout as unsupported with a
//!    precise error instead of guessing. (TIFF Technical Note 2
//!    deprecates the whole §22 design for exactly this reason: "the
//!    TIFF control logic must ... synthesize JPEG markers from the
//!    TIFF fields to feed the codec".)
//!
//! Field-presence rules implemented from the §22 "JPEGProc" table
//! ("The following table specifies the fields that are applicable to
//! each value defined by this Field"): for the baseline DCT process
//! (JPEGProc = 1) the Q/DC/AC table fields are mandatory; for the
//! lossless Huffman process (JPEGProc = 14) `JPEGLosslessPredictors`
//! and `JPEGDCTables` are mandatory and "the JPEGACTables field is
//! not used". `JPEGProc` itself "is mandatory whenever the
//! Compression Field is JPEG (no default)" — but when a complete
//! interchange stream is present the stream itself declares its
//! process in its SOF marker, so a missing `JPEGProc` is tolerated in
//! that layout (TIFF TN2 records that some writers "simply dumped" an
//! interchange datastream into the file without the auxiliary
//! fields).

use crate::error::{Result, TiffError as Error};
use crate::ifd::{find, ByteOrder, Entry};
use crate::types::*;

/// Parsed TIFF 6.0 §22 old-style JPEG fields (tags 512-521).
#[derive(Debug, Clone, Default)]
pub struct OldJpegFields {
    /// `JPEGProc` (512). `None` when the tag is absent — tolerated
    /// only when an interchange stream is present (see module docs).
    pub proc: Option<u16>,
    /// `JPEGInterchangeFormat` (513) — file offset of the SOI marker.
    /// `None` when absent **or zero**: "If this Field is zero or not
    /// present, a JPEG interchange format bitstream is not present."
    pub interchange_offset: Option<u64>,
    /// `JPEGInterchangeFormatLength` (514) — "relevant only if the
    /// JPEGInterchangeFormat Field is present and is non-zero."
    pub interchange_length: Option<u64>,
    /// `JPEGRestartInterval` (515) — MCUs between restart markers;
    /// "If this Field is zero or is not present, the compressed data
    /// does not contain restart markers." Metadata only in the
    /// interchange layout (the stream carries its own DRI marker).
    pub restart_interval: u16,
    /// `JPEGLosslessPredictors` (517) — one §22 selection-value
    /// (1..=7) per component.
    pub lossless_predictors: Option<Vec<u16>>,
    /// `JPEGPointTransforms` (518) — one Pt value per component
    /// ("The default value of this Field is 0 for each component").
    pub point_transforms: Option<Vec<u16>>,
    /// `JPEGQTables` (519) — per-component file offsets of 64-byte
    /// zigzag-order quantization tables.
    pub q_tables: Option<Vec<u64>>,
    /// `JPEGDCTables` (520) — per-component file offsets of raw DC
    /// Huffman tables (16 BITS bytes + up to 17 VALUES bytes).
    pub dc_tables: Option<Vec<u64>>,
    /// `JPEGACTables` (521) — per-component file offsets of raw AC
    /// Huffman tables (16 BITS bytes + up to 256 VALUES bytes).
    pub ac_tables: Option<Vec<u64>>,
}

/// Read one per-component SHORT array field (tags 517 / 518): §22
/// declares `N = SamplesPerPixel` for both.
fn per_component_u16(
    entries: &[Entry],
    bo: ByteOrder,
    tag: u16,
    name: &str,
    samples_per_pixel: u16,
) -> Result<Option<Vec<u16>>> {
    let Some(e) = find(entries, tag) else {
        return Ok(None);
    };
    let v = e.as_u32_vec(bo)?;
    if v.len() != samples_per_pixel as usize {
        return Err(Error::invalid(format!(
            "TIFF/JPEG(§22): {name} has {} values, expected N = SamplesPerPixel = {samples_per_pixel}",
            v.len()
        )));
    }
    let mut out = Vec::with_capacity(v.len());
    for x in v {
        if x > u16::MAX as u32 {
            return Err(Error::invalid(format!(
                "TIFF/JPEG(§22): {name} value {x} out of SHORT range"
            )));
        }
        out.push(x as u16);
    }
    Ok(Some(out))
}

/// Read one per-component LONG offset-array field (tags 519 / 520 /
/// 521): §22 declares `N = SamplesPerPixel` for all three.
fn per_component_u64(
    entries: &[Entry],
    bo: ByteOrder,
    tag: u16,
    name: &str,
    samples_per_pixel: u16,
) -> Result<Option<Vec<u64>>> {
    let Some(e) = find(entries, tag) else {
        return Ok(None);
    };
    let v = e.as_u64_vec(bo)?;
    if v.len() != samples_per_pixel as usize {
        return Err(Error::invalid(format!(
            "TIFF/JPEG(§22): {name} has {} offsets, expected N = SamplesPerPixel = {samples_per_pixel}",
            v.len()
        )));
    }
    Ok(Some(v))
}

/// Parse and validate the §22 old-style JPEG fields of one IFD.
pub fn parse_old_jpeg_fields(
    entries: &[Entry],
    bo: ByteOrder,
    samples_per_pixel: u16,
) -> Result<OldJpegFields> {
    let proc = match find(entries, TAG_JPEG_PROC) {
        Some(e) => {
            let p = e.as_u32(bo)?;
            match p as u16 {
                JPEG_PROC_BASELINE | JPEG_PROC_LOSSLESS => Some(p as u16),
                other => {
                    // §22 JPEGProc: "Two values are defined at this
                    // time. ... Values indicating JPEG processes other
                    // than those specified above will be defined in
                    // the future."
                    return Err(Error::Unsupported(format!(
                        "TIFF/JPEG(§22): JPEGProc={other} (only 1 = baseline sequential and \
                         14 = lossless Huffman are defined by TIFF 6.0 §22)"
                    )));
                }
            }
        }
        None => None,
    };

    let interchange_offset = match find(entries, TAG_JPEG_INTERCHANGE_FORMAT) {
        Some(e) => {
            let off = e
                .as_u64_vec(bo)?
                .into_iter()
                .next()
                .ok_or_else(|| Error::invalid("TIFF/JPEG(§22): empty JPEGInterchangeFormat"))?;
            // "If this Field is zero or not present, a JPEG
            // interchange format bitstream is not present."
            if off == 0 {
                None
            } else {
                Some(off)
            }
        }
        None => None,
    };
    let interchange_length = find(entries, TAG_JPEG_INTERCHANGE_FORMAT_LENGTH)
        .map(|e| {
            e.as_u64_vec(bo)?
                .into_iter()
                .next()
                .ok_or_else(|| Error::invalid("TIFF/JPEG(§22): empty JPEGInterchangeFormatLength"))
        })
        .transpose()?;

    let restart_interval = match find(entries, TAG_JPEG_RESTART_INTERVAL) {
        Some(e) => {
            let v = e.as_u32(bo)?;
            if v > u16::MAX as u32 {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG(§22): JPEGRestartInterval={v} out of SHORT range"
                )));
            }
            v as u16
        }
        None => 0,
    };

    let lossless_predictors = per_component_u16(
        entries,
        bo,
        TAG_JPEG_LOSSLESS_PREDICTORS,
        "JPEGLosslessPredictors",
        samples_per_pixel,
    )?;
    if let Some(preds) = &lossless_predictors {
        for &p in preds {
            // §22 JPEGLosslessPredictors: "The allowed predictors are
            // listed in the following table" — selection values 1..=7.
            if !(1..=7).contains(&p) {
                return Err(Error::invalid(format!(
                    "TIFF/JPEG(§22): JPEGLosslessPredictors selection-value {p} \
                     (spec defines 1..=7 only)"
                )));
            }
        }
    }
    let point_transforms = per_component_u16(
        entries,
        bo,
        TAG_JPEG_POINT_TRANSFORMS,
        "JPEGPointTransforms",
        samples_per_pixel,
    )?;
    let q_tables = per_component_u64(
        entries,
        bo,
        TAG_JPEG_Q_TABLES,
        "JPEGQTables",
        samples_per_pixel,
    )?;
    let dc_tables = per_component_u64(
        entries,
        bo,
        TAG_JPEG_DC_TABLES,
        "JPEGDCTables",
        samples_per_pixel,
    )?;
    let ac_tables = per_component_u64(
        entries,
        bo,
        TAG_JPEG_AC_TABLES,
        "JPEGACTables",
        samples_per_pixel,
    )?;

    Ok(OldJpegFields {
        proc,
        interchange_offset,
        interchange_length,
        restart_interval,
        lossless_predictors,
        point_transforms,
        q_tables,
        dc_tables,
        ac_tables,
    })
}

impl OldJpegFields {
    /// Slice the JPEG interchange-format bitstream out of the file, if
    /// one is present.
    ///
    /// Per §22, tag 513 "points to the Start of Image (SOI) marker
    /// code" and tag 514 "indicates the length in bytes of the JPEG
    /// interchange format bitstream". When the length field is absent
    /// (it is "relevant only if the JPEGInterchangeFormat Field is
    /// present", not itself mandatory) the stream is taken to run to
    /// the end of the last EOI marker found before end-of-file; when a
    /// writer's declared length includes trailing padding after the
    /// EOI, the same trim applies.
    pub fn interchange_stream<'a>(&self, input: &'a [u8]) -> Result<Option<&'a [u8]>> {
        let Some(off) = self.interchange_offset else {
            return Ok(None);
        };
        let start = usize::try_from(off)
            .map_err(|_| Error::invalid("TIFF/JPEG(§22): JPEGInterchangeFormat offset overflow"))?;
        if start >= input.len() {
            return Err(Error::invalid(
                "TIFF/JPEG(§22): JPEGInterchangeFormat offset past EOF",
            ));
        }
        let end = match self.interchange_length {
            Some(len) => {
                let len = usize::try_from(len).map_err(|_| {
                    Error::invalid("TIFF/JPEG(§22): JPEGInterchangeFormatLength overflow")
                })?;
                let end = start.checked_add(len).ok_or_else(|| {
                    Error::invalid("TIFF/JPEG(§22): JPEGInterchangeFormat range overflow")
                })?;
                if end > input.len() {
                    return Err(Error::invalid(
                        "TIFF/JPEG(§22): JPEGInterchangeFormat + Length extends past EOF",
                    ));
                }
                end
            }
            None => input.len(),
        };
        let mut stream = &input[start..end];
        if stream.len() < 4 || stream[0] != 0xFF || stream[1] != 0xD8 {
            return Err(Error::invalid(
                "TIFF/JPEG(§22): JPEGInterchangeFormat does not point at an SOI marker (FF D8)",
            ));
        }
        if stream[stream.len() - 2..] != [0xFF, 0xD9] {
            // Trailing padding (or an absent length field): trim to
            // the last EOI marker in range.
            match stream.windows(2).rposition(|w| w == [0xFF, 0xD9]) {
                Some(i) => stream = &stream[..i + 2],
                None => {
                    return Err(Error::invalid(
                        "TIFF/JPEG(§22): JPEG interchange bitstream has no EOI marker (FF D9)",
                    ));
                }
            }
        }
        Ok(Some(stream))
    }

    /// Build the precise error for an IFD that carries no interchange
    /// stream (the §22 tables-form layout — see the module docs).
    ///
    /// Distinguishes a *malformed* tables-form IFD (mandatory fields
    /// missing per the §22 JPEGProc applicability table → invalid
    /// data) from a *well-formed but unsupported* one (raw-table +
    /// entropy-strip reconstruction needs ISO 10918-1 marker
    /// synthesis → unsupported).
    pub fn tables_form_error(&self) -> Error {
        match self.proc {
            None => Error::invalid(
                "TIFF/JPEG(§22): Compression=6 without JPEGProc (tag 512 is mandatory, \
                 no default) and without a JPEGInterchangeFormat bitstream",
            ),
            Some(JPEG_PROC_BASELINE) => {
                let mut missing = Vec::new();
                if self.q_tables.is_none() {
                    missing.push("JPEGQTables");
                }
                if self.dc_tables.is_none() {
                    missing.push("JPEGDCTables");
                }
                if self.ac_tables.is_none() {
                    missing.push("JPEGACTables");
                }
                if missing.is_empty() {
                    Error::Unsupported(
                        "TIFF/JPEG(§22): old-style tables-form layout (raw JPEGQTables/\
                         JPEGDCTables/JPEGACTables + entropy-coded strips) is not supported \
                         in this build; only the JPEGInterchangeFormat (tag 513) layout \
                         decodes"
                            .into(),
                    )
                } else {
                    Error::invalid(format!(
                        "TIFF/JPEG(§22): JPEGProc=1 (baseline DCT) without {} \
                         (mandatory whenever JPEGProc specifies a DCT-based process) and \
                         without a JPEGInterchangeFormat bitstream",
                        missing.join("/")
                    ))
                }
            }
            Some(_) => {
                // JPEGProc = 14 (lossless Huffman): per the §22
                // applicability table, JPEGLosslessPredictors and
                // JPEGDCTables are mandatory; "the JPEGACTables field
                // is not used."
                let mut missing = Vec::new();
                if self.lossless_predictors.is_none() {
                    missing.push("JPEGLosslessPredictors");
                }
                if self.dc_tables.is_none() {
                    missing.push("JPEGDCTables");
                }
                if missing.is_empty() {
                    Error::Unsupported(
                        "TIFF/JPEG(§22): old-style lossless (JPEGProc=14) tables-form \
                         layout is not supported in this build; only the \
                         JPEGInterchangeFormat (tag 513) layout decodes"
                            .into(),
                    )
                } else {
                    Error::invalid(format!(
                        "TIFF/JPEG(§22): JPEGProc=14 (lossless) without {} \
                         (mandatory whenever JPEGProc specifies one of the lossless \
                         processes) and without a JPEGInterchangeFormat bitstream",
                        missing.join("/")
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ifd::Entry;

    fn short_entry(tag: u16, vals: &[u16]) -> Entry {
        let mut data = Vec::new();
        for v in vals {
            data.extend_from_slice(&v.to_le_bytes());
        }
        Entry {
            tag,
            field_type: TYPE_SHORT,
            count: vals.len() as u64,
            data,
        }
    }

    fn long_entry(tag: u16, vals: &[u32]) -> Entry {
        let mut data = Vec::new();
        for v in vals {
            data.extend_from_slice(&v.to_le_bytes());
        }
        Entry {
            tag,
            field_type: TYPE_LONG,
            count: vals.len() as u64,
            data,
        }
    }

    #[test]
    fn parse_minimal_interchange_layout() {
        let entries = vec![
            short_entry(TAG_JPEG_PROC, &[1]),
            long_entry(TAG_JPEG_INTERCHANGE_FORMAT, &[128]),
            long_entry(TAG_JPEG_INTERCHANGE_FORMAT_LENGTH, &[1000]),
        ];
        let f = parse_old_jpeg_fields(&entries, ByteOrder::Little, 3).unwrap();
        assert_eq!(f.proc, Some(JPEG_PROC_BASELINE));
        assert_eq!(f.interchange_offset, Some(128));
        assert_eq!(f.interchange_length, Some(1000));
        assert_eq!(f.restart_interval, 0);
    }

    #[test]
    fn interchange_offset_zero_means_absent() {
        // §22: "If this Field is zero or not present, a JPEG
        // interchange format bitstream is not present."
        let entries = vec![
            short_entry(TAG_JPEG_PROC, &[1]),
            long_entry(TAG_JPEG_INTERCHANGE_FORMAT, &[0]),
        ];
        let f = parse_old_jpeg_fields(&entries, ByteOrder::Little, 1).unwrap();
        assert_eq!(f.interchange_offset, None);
    }

    #[test]
    fn unknown_proc_rejected_as_unsupported() {
        let entries = vec![short_entry(TAG_JPEG_PROC, &[2])];
        let e = parse_old_jpeg_fields(&entries, ByteOrder::Little, 1).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }

    #[test]
    fn per_component_count_mismatch_rejected() {
        // JPEGQTables must carry N = SamplesPerPixel offsets.
        let entries = vec![
            short_entry(TAG_JPEG_PROC, &[1]),
            long_entry(TAG_JPEG_Q_TABLES, &[100, 200]),
        ];
        let e = parse_old_jpeg_fields(&entries, ByteOrder::Little, 3).unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("JPEGQTables"), "{msg}");
    }

    #[test]
    fn lossless_predictor_out_of_range_rejected() {
        let entries = vec![
            short_entry(TAG_JPEG_PROC, &[14]),
            short_entry(TAG_JPEG_LOSSLESS_PREDICTORS, &[8]),
        ];
        let e = parse_old_jpeg_fields(&entries, ByteOrder::Little, 1).unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("selection-value"), "{msg}");
    }

    #[test]
    fn interchange_stream_slices_soi_to_eoi() {
        let mut file = vec![0u8; 10];
        file.extend_from_slice(&[0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
        let f = OldJpegFields {
            interchange_offset: Some(10),
            interchange_length: Some(6),
            ..Default::default()
        };
        let s = f.interchange_stream(&file).unwrap().unwrap();
        assert_eq!(s, &[0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
    }

    #[test]
    fn interchange_stream_without_length_trims_to_eoi() {
        // No JPEGInterchangeFormatLength: stream runs to the last EOI
        // before end-of-file (trailing pad bytes dropped).
        let mut file = vec![0u8; 4];
        file.extend_from_slice(&[0xFF, 0xD8, 0xAA, 0xFF, 0xD9, 0x00, 0x00]);
        let f = OldJpegFields {
            interchange_offset: Some(4),
            interchange_length: None,
            ..Default::default()
        };
        let s = f.interchange_stream(&file).unwrap().unwrap();
        assert_eq!(s, &[0xFF, 0xD8, 0xAA, 0xFF, 0xD9]);
    }

    #[test]
    fn interchange_stream_padded_length_trims_to_eoi() {
        // Writer declared a length that includes padding after EOI.
        let mut file = vec![0u8; 4];
        file.extend_from_slice(&[0xFF, 0xD8, 0xAA, 0xFF, 0xD9, 0x00, 0x00, 0x00]);
        let f = OldJpegFields {
            interchange_offset: Some(4),
            interchange_length: Some(8),
            ..Default::default()
        };
        let s = f.interchange_stream(&file).unwrap().unwrap();
        assert_eq!(s, &[0xFF, 0xD8, 0xAA, 0xFF, 0xD9]);
    }

    #[test]
    fn interchange_stream_bounds_errors() {
        let file = vec![0u8; 16];
        // Offset past EOF.
        let f = OldJpegFields {
            interchange_offset: Some(100),
            ..Default::default()
        };
        assert!(f.interchange_stream(&file).is_err());
        // Offset + length past EOF.
        let f = OldJpegFields {
            interchange_offset: Some(8),
            interchange_length: Some(100),
            ..Default::default()
        };
        assert!(f.interchange_stream(&file).is_err());
        // In-bounds but not an SOI marker.
        let f = OldJpegFields {
            interchange_offset: Some(4),
            interchange_length: Some(8),
            ..Default::default()
        };
        assert!(f.interchange_stream(&file).is_err());
        // SOI but no EOI anywhere in range.
        let mut file2 = vec![0u8; 4];
        file2.extend_from_slice(&[0xFF, 0xD8, 0x00, 0x00, 0x00, 0x00]);
        let f = OldJpegFields {
            interchange_offset: Some(4),
            interchange_length: None,
            ..Default::default()
        };
        assert!(f.interchange_stream(&file2).is_err());
    }

    #[test]
    fn tables_form_errors_are_precise() {
        // No proc, no interchange → invalid mentioning JPEGProc.
        let f = OldJpegFields::default();
        let msg = format!("{:?}", f.tables_form_error());
        assert!(msg.contains("JPEGProc"), "{msg}");
        // Baseline with all tables present → Unsupported tables-form.
        let f = OldJpegFields {
            proc: Some(JPEG_PROC_BASELINE),
            q_tables: Some(vec![1]),
            dc_tables: Some(vec![2]),
            ac_tables: Some(vec![3]),
            ..Default::default()
        };
        assert!(matches!(f.tables_form_error(), Error::Unsupported(_)));
        // Baseline missing AC tables → invalid naming the gap.
        let f = OldJpegFields {
            proc: Some(JPEG_PROC_BASELINE),
            q_tables: Some(vec![1]),
            dc_tables: Some(vec![2]),
            ..Default::default()
        };
        let msg = format!("{:?}", f.tables_form_error());
        assert!(msg.contains("JPEGACTables"), "{msg}");
        // Lossless with predictors + DC tables → Unsupported.
        let f = OldJpegFields {
            proc: Some(JPEG_PROC_LOSSLESS),
            lossless_predictors: Some(vec![1]),
            dc_tables: Some(vec![2]),
            ..Default::default()
        };
        assert!(matches!(f.tables_form_error(), Error::Unsupported(_)));
        // Lossless missing predictors → invalid naming the gap.
        let f = OldJpegFields {
            proc: Some(JPEG_PROC_LOSSLESS),
            dc_tables: Some(vec![2]),
            ..Default::default()
        };
        let msg = format!("{:?}", f.tables_form_error());
        assert!(msg.contains("JPEGLosslessPredictors"), "{msg}");
    }
}
