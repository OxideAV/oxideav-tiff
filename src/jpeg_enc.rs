//! In-crate ITU-T T.81 | ISO/IEC 10918-1 JPEG **encoder** — the
//! datastream writer behind `Compression = 7` (TIFF Technical Note 2)
//! encode.
//!
//! Scope (all sequential, all Huffman — TN2 forbids the progressive /
//! hierarchical processes and discourages arithmetic coding):
//!
//! * **Baseline DCT** (`SOF0`, T.81 Annex F.1.2): 8-bit samples,
//!   Huffman entropy coding with at most two DC and two AC tables.
//! * **Extended sequential DCT** (`SOF1`, T.81 F.1.3 / F.1.5): 12-bit
//!   samples; the DC difference categories extend to `SSSS = 15`
//!   (Table F.6) and the AC amplitude categories to `SSSS = 14`
//!   (Table F.7), so the Annex K.3 "typical" 8-bit tables cannot code
//!   the symbol alphabet and the encoder always derives per-image
//!   optimal tables through the Annex K.2 procedure.
//! * **Lossless** (`SOF3`, T.81 Annex H.1): 2..=16-bit samples, one of
//!   the seven Table H.1 predictors, modulo-2^16 differences coded
//!   with the DC-difference Huffman model extended by the `SSSS = 16`
//!   entry of Table H.2.
//!
//! Everything below is transcribed from the staged
//! `docs/image/jpeg/T-REC-T.81-199209-I.pdf`:
//!
//! * A.1.1 (component dimensions from sampling factors), A.2 (MCU
//!   ordering: non-interleaved when `Ns = 1`, interleaved `Hk × Vk`
//!   data-unit arrays otherwise), A.2.4 (partial-MCU completion by
//!   edge replication), A.3.1 (level shift by `2^(P-1)`), A.3.3 (the
//!   FDCT definition), A.3.4 (uniform quantisation, round to nearest),
//!   A.3.5 (differential DC), A.3.6 / Figure A.6 (zig-zag sequence).
//! * B.1.1.3 Table B.1 (marker codes), B.1.1.4 (segment length
//!   convention), B.1.1.5 (byte stuffing / 1-bit padding), B.2.2
//!   (frame header), B.2.3 (scan header), B.2.4.1 (DQT — `Pq` = 0 for
//!   8-bit `Qk`, 1 for 16-bit; elements in zig-zag order), B.2.4.2
//!   (DHT — `Tc`, `Th`, 16 `Li` counts then the `Vi,j` values).
//! * Annex C (Figures C.1–C.3: HUFFSIZE / HUFFCODE / EHUFCO+EHUFSI
//!   generation from the `BITS` / `HUFFVAL` lists), C.3 (bit order).
//! * F.1.1.5.1 (DC prediction reset to 0 at scan start), F.1.2.1
//!   (DC category coding, Table F.1, additional bits = low-order SSSS
//!   bits of DIFF, or of DIFF − 1 when negative), F.1.2.2 (AC
//!   run/size composite `RRRRSSSS`, ZRL = `X'F0'`, EOB = `X'00'`,
//!   Table F.2), F.1.2.3 (byte stuffing).
//! * H.1.2.1 (prediction: `Ra` on the first line, `Rb` at the start
//!   of every other line, `2^(P-Pt-1)` for the very first sample),
//!   H.1.2.2 / Table H.2 (modulo 2^16 difference categories).
//! * K.1 Tables K.1 / K.2 (quantisation tables, natural order), K.2
//!   Figures K.1–K.4 (optimal `BITS` / `HUFFVAL` from symbol
//!   statistics, 16-bit length limiting), K.3.3 (the `BITS` /
//!   `HUFFVAL` specification lists of Tables K.3–K.6).
//!
//! The numeric tables are cross-checked against the staged CSV
//! transcriptions in `docs/image/jpeg/tables/`.

use crate::error::{Result, TiffError as Error};

// ---------------------------------------------------------------------------
// Marker codes — T.81 Table B.1.
// ---------------------------------------------------------------------------

const SOF0: u8 = 0xC0;
const SOF1: u8 = 0xC1;
const SOF3: u8 = 0xC3;
const DHT: u8 = 0xC4;
const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;

// ---------------------------------------------------------------------------
// Zig-zag sequence — T.81 Figure A.6 (A.3.6). `NATURAL[k]` is the
// natural row-major index (`v * 8 + u`) of the k-th zig-zag position;
// identical to `docs/image/jpeg/tables/natural-scan-order.csv`.
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const NATURAL: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

// ---------------------------------------------------------------------------
// Annex K.1 quantisation tables (natural row-major order, row = v).
// ---------------------------------------------------------------------------

/// T.81 Table K.1 — luminance quantisation table.
#[rustfmt::skip]
pub const QUANT_LUMINANCE_K1: [u16; 64] = [
    16, 11, 10, 16,  24,  40,  51,  61,
    12, 12, 14, 19,  26,  58,  60,  55,
    14, 13, 16, 24,  40,  57,  69,  56,
    14, 17, 22, 29,  51,  87,  80,  62,
    18, 22, 37, 56,  68, 109, 103,  77,
    24, 35, 55, 64,  81, 104, 113,  92,
    49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103,  99,
];

/// T.81 Table K.2 — chrominance quantisation table.
#[rustfmt::skip]
pub const QUANT_CHROMINANCE_K2: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99,
    18, 21, 26, 66, 99, 99, 99, 99,
    24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
];

// ---------------------------------------------------------------------------
// Annex K.3.3 "typical" Huffman table specifications (BITS / HUFFVAL).
// ---------------------------------------------------------------------------

/// Table K.3 (luminance DC) — K.3.3.1 `BITS` list.
const BITS_DC_LUMINANCE: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
/// Table K.3 (luminance DC) — K.3.3.1 `HUFFVAL` list.
const VAL_DC_LUMINANCE: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
/// Table K.4 (chrominance DC) — K.3.3.1 `BITS` list.
const BITS_DC_CHROMINANCE: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
/// Table K.4 (chrominance DC) — K.3.3.1 `HUFFVAL` list.
const VAL_DC_CHROMINANCE: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
/// Table K.5 (luminance AC) — K.3.3.2 `BITS` list.
const BITS_AC_LUMINANCE: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D];
/// Table K.5 (luminance AC) — K.3.3.2 `HUFFVAL` list.
#[rustfmt::skip]
const VAL_AC_LUMINANCE: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];
/// Table K.6 (chrominance AC) — K.3.3.2 `BITS` list.
const BITS_AC_CHROMINANCE: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
/// Table K.6 (chrominance AC) — K.3.3.2 `HUFFVAL` list.
#[rustfmt::skip]
const VAL_AC_CHROMINANCE: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0,
    0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26,
    0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
    0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3,
    0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA,
    0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];

// ---------------------------------------------------------------------------
// Huffman table specification (B.2.4.2 `BITS` + `HUFFVAL`) and the
// Annex C encoder code tables derived from it.
// ---------------------------------------------------------------------------

/// A Huffman table in its B.2.4.2 specification form: `bits[i]` is the
/// number of codes of length `i + 1` (`L1..L16`), `vals` the symbol
/// values in code order (`HUFFVAL`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuffSpec {
    pub bits: [u8; 16],
    pub vals: Vec<u8>,
}

impl HuffSpec {
    fn new(bits: [u8; 16], vals: &[u8]) -> Self {
        HuffSpec {
            bits,
            vals: vals.to_vec(),
        }
    }

    /// Table K.3 — luminance DC differences.
    pub fn k3_dc_luminance() -> Self {
        Self::new(BITS_DC_LUMINANCE, &VAL_DC_LUMINANCE)
    }
    /// Table K.4 — chrominance DC differences.
    pub fn k4_dc_chrominance() -> Self {
        Self::new(BITS_DC_CHROMINANCE, &VAL_DC_CHROMINANCE)
    }
    /// Table K.5 — luminance AC coefficients.
    pub fn k5_ac_luminance() -> Self {
        Self::new(BITS_AC_LUMINANCE, &VAL_AC_LUMINANCE)
    }
    /// Table K.6 — chrominance AC coefficients.
    pub fn k6_ac_chrominance() -> Self {
        Self::new(BITS_AC_CHROMINANCE, &VAL_AC_CHROMINANCE)
    }

    /// Byte length of this table inside a DHT segment: `17 + m_t`
    /// (Table B.5).
    fn dht_len(&self) -> usize {
        17 + self.vals.len()
    }

    fn validate(&self) -> Result<()> {
        let total: usize = self.bits.iter().map(|&b| b as usize).sum();
        if total != self.vals.len() || total > 256 {
            return Err(Error::invalid(format!(
                "JPEG encode: Huffman table BITS sum {total} does not match {} HUFFVAL \
                 entries",
                self.vals.len()
            )));
        }
        Ok(())
    }
}

/// Encoder code tables `EHUFCO` / `EHUFSI` (Annex C, Figure C.3),
/// indexed by symbol value.
#[derive(Debug, Clone)]
struct HuffCodes {
    code: [u16; 256],
    size: [u8; 256],
}

impl HuffCodes {
    /// Annex C.2: Figures C.1 (HUFFSIZE), C.2 (HUFFCODE), C.3 (order by
    /// symbol value).
    fn from_spec(spec: &HuffSpec) -> Result<Self> {
        spec.validate()?;
        // Figure C.1 — Generate_size_table.
        let mut huffsize: Vec<u8> = Vec::with_capacity(spec.vals.len() + 1);
        for (i, &count) in spec.bits.iter().enumerate() {
            for _ in 0..count {
                huffsize.push(i as u8 + 1);
            }
        }
        // Figure C.2 — Generate_code_table.
        let mut huffcode: Vec<u16> = Vec::with_capacity(huffsize.len());
        let mut code: u32 = 0;
        let mut si = huffsize.first().copied().unwrap_or(0);
        for &size in &huffsize {
            while size != si {
                code <<= 1;
                si += 1;
            }
            if code > u16::MAX as u32 {
                return Err(Error::invalid(
                    "JPEG encode: Huffman BITS list over-subscribes the code space",
                ));
            }
            huffcode.push(code as u16);
            code += 1;
        }
        // Figure C.3 — Order_codes.
        let mut out = HuffCodes {
            code: [0; 256],
            size: [0; 256],
        };
        for (k, &v) in spec.vals.iter().enumerate() {
            if out.size[v as usize] != 0 {
                return Err(Error::invalid(format!(
                    "JPEG encode: Huffman HUFFVAL lists symbol {v:#04x} twice"
                )));
            }
            out.code[v as usize] = huffcode[k];
            out.size[v as usize] = huffsize[k];
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Annex K.2 — optimal BITS / HUFFVAL from symbol statistics.
// ---------------------------------------------------------------------------

/// Symbol frequency counter for one Huffman table destination (K.2:
/// `FREQ(V)` for `V = 0..=255`; `FREQ(256)` is the reserved code point
/// that guarantees no all-ones code word).
#[derive(Debug, Clone)]
pub struct HuffStats {
    freq: [u32; 257],
}

impl Default for HuffStats {
    fn default() -> Self {
        HuffStats { freq: [0; 257] }
    }
}

/// K.2 tie-break helper: `v` displaces the current candidate when its
/// frequency is less than or equal (so the largest index wins ties).
fn least_so_far(best: Option<usize>, freq: &[u64; 257], v: usize) -> bool {
    match best {
        None => true,
        Some(b) => freq[v] <= freq[b],
    }
}

impl HuffStats {
    fn count(&mut self, symbol: u8) {
        self.freq[symbol as usize] = self.freq[symbol as usize].saturating_add(1);
    }

    /// True when no symbol was ever counted.
    pub fn is_empty(&self) -> bool {
        self.freq[..256].iter().all(|&f| f == 0)
    }

    /// K.2 Figures K.1–K.4: derive a table specification whose code
    /// lengths are optimal for the collected statistics, limited to
    /// 16 bits.
    pub fn to_spec(&self) -> HuffSpec {
        let mut freq: [u64; 257] = [0; 257];
        for (d, &s) in freq.iter_mut().zip(self.freq.iter()) {
            *d = s as u64;
        }
        // Reserve one code point (K.2: "FREQ value for V = 256 is set to 1").
        freq[256] = 1;
        let mut codesize: [u32; 257] = [0; 257];
        let mut others: [i32; 257] = [-1; 257];

        // Figure K.1 — Code_size. "Find V1 for least value of FREQ(V1) > 0"
        // selects the largest V on ties.
        loop {
            let mut v1: Option<usize> = None;
            for v in 0..257 {
                if freq[v] > 0 && least_so_far(v1, &freq, v) {
                    v1 = Some(v);
                }
            }
            let Some(v1) = v1 else { break };
            let mut v2: Option<usize> = None;
            for v in 0..257 {
                if v != v1 && freq[v] > 0 && least_so_far(v2, &freq, v) {
                    v2 = Some(v);
                }
            }
            let Some(v2) = v2 else { break };
            freq[v1] += freq[v2];
            freq[v2] = 0;
            let mut a = v1;
            loop {
                codesize[a] += 1;
                if others[a] == -1 {
                    break;
                }
                a = others[a] as usize;
            }
            others[a] = v2 as i32;
            let mut b = v2;
            loop {
                codesize[b] += 1;
                if others[b] == -1 {
                    break;
                }
                b = others[b] as usize;
            }
        }

        // Figure K.2 — Count_BITS (lengths up to 32 before adjustment).
        let mut bits: [i32; 33] = [0; 33];
        for &cs in codesize.iter() {
            if cs != 0 {
                bits[cs.min(32) as usize] += 1;
            }
        }

        // Figure K.3 — Adjust_BITS: no code longer than 16 bits, then
        // drop the reserved code point from the longest length.
        let mut i = 32usize;
        while i > 16 {
            if bits[i] > 0 {
                // Figure K.3: J starts at I − 1 and is decremented
                // *before* each BITS(J) > 0 test, so the search begins
                // at I − 2 — the prefix one bit shorter than I is the
                // one being created, not searched.
                let mut j = i - 1;
                loop {
                    j -= 1;
                    if bits[j] > 0 {
                        break;
                    }
                }
                bits[i] -= 2;
                bits[i - 1] += 1;
                bits[j + 1] += 2;
                bits[j] -= 1;
            } else {
                i -= 1;
            }
        }
        while bits[i] == 0 {
            i -= 1;
        }
        bits[i] -= 1;

        // Figure K.4 — Sort_input (symbols 0..=255 by code size).
        let mut vals: Vec<u8> = Vec::new();
        for size in 1..=32u32 {
            for (j, &cs) in codesize.iter().enumerate().take(256) {
                if cs == size {
                    vals.push(j as u8);
                }
            }
        }
        let mut out_bits = [0u8; 16];
        for (k, slot) in out_bits.iter_mut().enumerate() {
            *slot = bits[k + 1].max(0) as u8;
        }
        HuffSpec {
            bits: out_bits,
            vals,
        }
    }
}

// ---------------------------------------------------------------------------
// Quantisation tables and the quality knob.
// ---------------------------------------------------------------------------

/// Scale a K.1 / K.2 table by the `quality` knob (1..=100).
///
/// The anchor points follow the Annex K.1 guidance: `quality = 50`
/// leaves the printed table unchanged, `quality = 75` halves every
/// step ("If these quantization values are divided by 2, the resulting
/// reconstructed image is usually nearly indistinguishable from the
/// source image"), and `quality = 100` collapses every step to 1. The
/// mapping is `scale = 50 / q` below 50 and `scale = (100 − q) / 50`
/// at or above 50, applied as `max(1, round(Qk × scale))`. For 12-bit
/// sample precision every step is additionally multiplied by 16 so the
/// quantiser keeps the same *relative* coarseness the tables were
/// designed for on 8-bit data (F.1.1.4: 12-bit coefficients carry four
/// more bits); the resulting entries exceed 255 and are written with
/// `Pq = 1` (16-bit `Qk`, B.2.4.1), which the extended process permits.
pub fn scaled_quant_table(base: &[u16; 64], quality: u8, precision: u8) -> [u16; 64] {
    let q = quality.clamp(1, 100) as u32;
    // Work in fixed point: scale_pct = percentage of the base step.
    let scale_pct: u32 = if q < 50 { 5000 / q } else { 200 - 2 * q };
    let mut out = [0u16; 64];
    let max = if precision > 8 { 65535u32 } else { 255u32 };
    let mul = if precision > 8 { 16u32 } else { 1u32 };
    for (o, &b) in out.iter_mut().zip(base.iter()) {
        let v = ((b as u32 * scale_pct + 50) / 100).max(1) * mul;
        *o = v.min(max) as u16;
    }
    out
}

// ---------------------------------------------------------------------------
// Table set handed to the frame writer.
// ---------------------------------------------------------------------------

/// The quantisation + Huffman table destinations a frame references.
/// Slot `i` of each array is destination `i` (`Tq` / `Th` = i);
/// unused slots stay `None` and are neither written nor referenced.
#[derive(Debug, Clone, Default)]
pub struct JpegTableSet {
    /// Quantisation tables, natural (row-major) order.
    pub quant: [Option<[u16; 64]>; 4],
    /// DC (or lossless) Huffman tables — `Tc = 0`.
    pub dc: [Option<HuffSpec>; 4],
    /// AC Huffman tables — `Tc = 1`.
    pub ac: [Option<HuffSpec>; 4],
}

impl JpegTableSet {
    /// Serialise the DQT / DHT marker segments for every populated
    /// slot (B.2.4.1 / B.2.4.2). Quantisation tables go first, one
    /// table per DQT segment; then the DC tables, then the AC tables,
    /// one table per DHT segment. `dct` = false (lossless) suppresses
    /// the quantisation tables and the AC tables, neither of which the
    /// lossless process references.
    pub fn write_tables(&self, out: &mut Vec<u8>, dct: bool) {
        if dct {
            for (tq, table) in self.quant.iter().enumerate() {
                if let Some(q) = table {
                    let pq: u8 = if q.iter().any(|&v| v > 255) { 1 } else { 0 };
                    let len = 2 + 1 + 64 * (1 + pq as usize);
                    out.extend_from_slice(&[0xFF, DQT]);
                    out.extend_from_slice(&(len as u16).to_be_bytes());
                    out.push((pq << 4) | tq as u8);
                    for k in 0..64 {
                        let v = q[NATURAL[k]];
                        if pq == 1 {
                            out.extend_from_slice(&v.to_be_bytes());
                        } else {
                            out.push(v as u8);
                        }
                    }
                }
            }
        }
        for (class, tables) in [(0u8, &self.dc), (1u8, &self.ac)] {
            if class == 1 && !dct {
                continue;
            }
            for (th, table) in tables.iter().enumerate() {
                if let Some(h) = table {
                    let len = 2 + h.dht_len();
                    out.extend_from_slice(&[0xFF, DHT]);
                    out.extend_from_slice(&(len as u16).to_be_bytes());
                    out.push((class << 4) | th as u8);
                    out.extend_from_slice(&h.bits);
                    out.extend_from_slice(&h.vals);
                }
            }
        }
    }

    /// A complete TN2 `JPEGTables` "abbreviated table specification"
    /// datastream: `SOI`, the table segments, `EOI`.
    pub fn tables_stream(&self, dct: bool) -> Vec<u8> {
        let mut out = vec![0xFF, SOI];
        self.write_tables(&mut out, dct);
        out.extend_from_slice(&[0xFF, EOI]);
        out
    }
}

// ---------------------------------------------------------------------------
// Frame description.
// ---------------------------------------------------------------------------

/// One frame component: its samples at the component's own
/// resolution (A.1.1: `xi = ceil(X × Hi / Hmax)`,
/// `yi = ceil(Y × Vi / Vmax)`), the sampling factors, and the table
/// destinations it selects.
#[derive(Debug, Clone)]
pub struct JpegComponent<'a> {
    /// Row-major samples, `width × height`, each `< 2^precision`.
    pub samples: &'a [u16],
    pub width: usize,
    pub height: usize,
    /// Horizontal sampling factor `Hi` (1..=4).
    pub h: u8,
    /// Vertical sampling factor `Vi` (1..=4).
    pub v: u8,
    /// Quantisation table destination `Tqi` (DCT processes).
    pub quant_id: u8,
    /// Entropy table destination (`Tdj` and `Taj` — the same slot is
    /// used for both classes).
    pub huff_id: u8,
}

/// The coding process of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegProcess {
    /// Sequential DCT, Huffman: `SOF0` at 8 bits, `SOF1` at 12 bits.
    Dct,
    /// Lossless, Huffman (`SOF3`) with the Table H.1 predictor
    /// selection value (1..=7) and point transform `Pt = 0`.
    Lossless { predictor: u8 },
}

/// Frame-level parameters.
#[derive(Debug, Clone, Copy)]
pub struct JpegFrame {
    /// Frame width `X` (samples per line of the highest-resolution
    /// component).
    pub width: u16,
    /// Frame height `Y`.
    pub height: u16,
    /// Sample precision `P`: 8 or 12 for [`JpegProcess::Dct`], 2..=16
    /// for [`JpegProcess::Lossless`].
    pub precision: u8,
    pub process: JpegProcess,
}

// ---------------------------------------------------------------------------
// Entropy-coded segment sink: either emits bits or counts symbols.
// ---------------------------------------------------------------------------

/// Bit writer implementing C.3 (MSB-first) and F.1.2.3 (byte
/// stuffing, 1-bit padding).
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            out: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }

    fn put(&mut self, code: u32, size: u32) {
        if size == 0 {
            return;
        }
        debug_assert!(size <= 24);
        self.acc = (self.acc << size) | (code & ((1u32 << size) - 1));
        self.nbits += size;
        while self.nbits >= 8 {
            let byte = ((self.acc >> (self.nbits - 8)) & 0xFF) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00);
            }
            self.nbits -= 8;
        }
        self.acc &= (1u32 << self.nbits).wrapping_sub(1);
    }

    /// F.1.2.3: pad the final byte with 1-bits (stuffing a zero after
    /// an `X'FF'` produced by the padding).
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            let pad = 8 - self.nbits;
            self.put((1u32 << pad) - 1, pad);
        }
        self.out
    }
}

/// Per-scan entropy tables (up to four destinations of each class).
struct ScanTables {
    dc: [Option<HuffCodes>; 4],
    ac: [Option<HuffCodes>; 4],
}

enum Sink<'a> {
    Emit {
        writer: &'a mut BitWriter,
        tables: &'a ScanTables,
    },
    Count {
        dc: &'a mut [HuffStats; 4],
        ac: &'a mut [HuffStats; 4],
    },
}

impl Sink<'_> {
    /// Code one DC-class symbol (`SSSS` category for DC differences and
    /// lossless differences) followed by `extra_bits` additional bits.
    fn dc_symbol(&mut self, table: u8, symbol: u8, extra: u32, extra_bits: u32) -> Result<()> {
        match self {
            Sink::Emit { writer, tables } => {
                let codes = tables.dc[table as usize].as_ref().ok_or_else(|| {
                    Error::invalid(format!("JPEG encode: DC table {table} not defined"))
                })?;
                let size = codes.size[symbol as usize];
                if size == 0 {
                    return Err(Error::invalid(format!(
                        "JPEG encode: DC table {table} has no code for category {symbol}"
                    )));
                }
                writer.put(codes.code[symbol as usize] as u32, size as u32);
                writer.put(extra, extra_bits);
            }
            Sink::Count { dc, .. } => dc[table as usize].count(symbol),
        }
        Ok(())
    }

    /// Code one AC-class composite symbol (`RRRRSSSS`) plus additional
    /// bits.
    fn ac_symbol(&mut self, table: u8, symbol: u8, extra: u32, extra_bits: u32) -> Result<()> {
        match self {
            Sink::Emit { writer, tables } => {
                let codes = tables.ac[table as usize].as_ref().ok_or_else(|| {
                    Error::invalid(format!("JPEG encode: AC table {table} not defined"))
                })?;
                let size = codes.size[symbol as usize];
                if size == 0 {
                    return Err(Error::invalid(format!(
                        "JPEG encode: AC table {table} has no code for run/size {symbol:#04x}"
                    )));
                }
                writer.put(codes.code[symbol as usize] as u32, size as u32);
                writer.put(extra, extra_bits);
            }
            Sink::Count { ac, .. } => ac[table as usize].count(symbol),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Magnitude categories and additional bits (F.1.2.1.1 / F.1.2.2.1).
// ---------------------------------------------------------------------------

/// `SSSS` for a two's-complement value: the number of bits needed for
/// its magnitude (Tables F.1 / F.2 / F.6 / F.7 / H.2).
fn category(v: i32) -> u32 {
    let m = v.unsigned_abs();
    32 - m.leading_zeros()
}

/// The additional bits: "When DIFF is positive, the SSSS low order
/// bits of DIFF are appended. When DIFF is negative, the SSSS low
/// order bits of (DIFF – 1) are appended."
fn extra_bits(v: i32, ssss: u32) -> u32 {
    if ssss == 0 {
        return 0;
    }
    let raw = if v < 0 { v - 1 } else { v };
    (raw as u32) & ((1u32 << ssss) - 1)
}

// ---------------------------------------------------------------------------
// Forward DCT (A.3.3) + quantisation (A.3.4).
// ---------------------------------------------------------------------------

struct Fdct {
    /// `cos_table[x][u] = C(u) / 2 × cos((2x + 1) u π / 16)` so that the
    /// separable 1-D transform is `T[u] = Σx s[x] × cos_table[x][u]` and
    /// the 2-D result is `S[v][u] = Σy Σx s[y][x] cos_table[y][v]
    /// cos_table[x][u]` = the A.3.3 definition (`1/4 × C(u) C(v)`).
    cos_table: [[f64; 8]; 8],
}

impl Fdct {
    fn new() -> Self {
        let mut cos_table = [[0f64; 8]; 8];
        for (x, row) in cos_table.iter_mut().enumerate() {
            for (u, cell) in row.iter_mut().enumerate() {
                let cu = if u == 0 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                let angle = (2.0 * x as f64 + 1.0) * u as f64 * std::f64::consts::PI / 16.0;
                *cell = cu / 2.0 * angle.cos();
            }
        }
        Fdct { cos_table }
    }

    /// `block` is level-shifted samples in row-major order (`s[y][x]`);
    /// returns `S[v][u]` in row-major order.
    fn transform(&self, block: &[f64; 64]) -> [f64; 64] {
        // Rows first: tmp[y][u] = Σx s[y][x] cos_table[x][u].
        let mut tmp = [0f64; 64];
        for y in 0..8 {
            for u in 0..8 {
                let mut acc = 0f64;
                for x in 0..8 {
                    acc += block[y * 8 + x] * self.cos_table[x][u];
                }
                tmp[y * 8 + u] = acc;
            }
        }
        // Columns: S[v][u] = Σy tmp[y][u] cos_table[y][v].
        let mut out = [0f64; 64];
        for v in 0..8 {
            for u in 0..8 {
                let mut acc = 0f64;
                for y in 0..8 {
                    acc += tmp[y * 8 + u] * self.cos_table[y][v];
                }
                out[v * 8 + u] = acc;
            }
        }
        out
    }
}

/// A.3.4 uniform quantiser: `Sq = round(S / Q)`, rounding half away
/// from zero.
fn quantize(coefs: &[f64; 64], quant: &[u16; 64]) -> [i32; 64] {
    let mut out = [0i32; 64];
    for k in 0..64 {
        let q = quant[k].max(1) as f64;
        out[k] = (coefs[k] / q).round() as i32;
    }
    out
}

// ---------------------------------------------------------------------------
// Frame writer.
// ---------------------------------------------------------------------------

/// Validate the component geometry against the frame per A.1.1 and the
/// interleave rule of B.2.3 (`Σ Hj × Vj ≤ 10` when `Ns > 1`).
fn validate(frame: &JpegFrame, comps: &[JpegComponent<'_>]) -> Result<(u8, u8)> {
    if comps.is_empty() || comps.len() > 4 {
        return Err(Error::invalid(format!(
            "JPEG encode: {} components (a single interleaved scan carries 1..=4)",
            comps.len()
        )));
    }
    if frame.width == 0 || frame.height == 0 {
        return Err(Error::invalid(
            "JPEG encode: frame dimensions must be non-zero (B.2.2: X ≥ 1)",
        ));
    }
    match frame.process {
        JpegProcess::Dct => {
            if frame.precision != 8 && frame.precision != 12 {
                return Err(Error::invalid(format!(
                    "JPEG encode: DCT sample precision {} (Table B.2 allows 8 or 12)",
                    frame.precision
                )));
            }
        }
        JpegProcess::Lossless { predictor } => {
            if !(2..=16).contains(&frame.precision) {
                return Err(Error::invalid(format!(
                    "JPEG encode: lossless sample precision {} (Table B.2 allows 2..=16)",
                    frame.precision
                )));
            }
            if !(1..=7).contains(&predictor) {
                return Err(Error::invalid(format!(
                    "JPEG encode: lossless predictor selection {predictor} (Table H.1 defines \
                     1..=7 for non-differential frames)"
                )));
            }
        }
    }
    let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
    let mut hv_sum = 0u32;
    for (i, c) in comps.iter().enumerate() {
        if !(1..=4).contains(&c.h) || !(1..=4).contains(&c.v) {
            return Err(Error::invalid(format!(
                "JPEG encode: component {i} sampling factors {}x{} (Table B.2 allows 1..=4)",
                c.h, c.v
            )));
        }
        if c.quant_id > 3 || c.huff_id > 3 {
            return Err(Error::invalid(format!(
                "JPEG encode: component {i} table destination out of range (0..=3)"
            )));
        }
        if frame.precision == 8
            && matches!(frame.process, JpegProcess::Dct)
            && (c.quant_id > 1 || c.huff_id > 1)
        {
            return Err(Error::invalid(format!(
                "JPEG encode: component {i} selects table destination > 1; the baseline \
                 process (Table B.2 / B.3) allows only 0 and 1"
            )));
        }
        let want_w = (frame.width as usize * c.h as usize).div_ceil(hmax as usize);
        let want_h = (frame.height as usize * c.v as usize).div_ceil(vmax as usize);
        if c.width != want_w || c.height != want_h {
            return Err(Error::invalid(format!(
                "JPEG encode: component {i} is {}x{} but A.1.1 requires {want_w}x{want_h} \
                 (X={} Y={} H={} V={} Hmax={hmax} Vmax={vmax})",
                c.width, c.height, frame.width, frame.height, c.h, c.v
            )));
        }
        if c.samples.len() != c.width * c.height {
            return Err(Error::invalid(format!(
                "JPEG encode: component {i} carries {} samples for {}x{}",
                c.samples.len(),
                c.width,
                c.height
            )));
        }
        let limit = 1u32 << frame.precision;
        if c.samples.iter().any(|&s| (s as u32) >= limit) {
            return Err(Error::invalid(format!(
                "JPEG encode: component {i} has a sample outside 0..2^{}",
                frame.precision
            )));
        }
        hv_sum += c.h as u32 * c.v as u32;
    }
    if comps.len() > 1 && hv_sum > 10 {
        return Err(Error::invalid(format!(
            "JPEG encode: Σ Hj × Vj = {hv_sum} exceeds the B.2.3 interleave limit of 10"
        )));
    }
    if comps.len() == 1 && (comps[0].h != 1 || comps[0].v != 1) {
        return Err(Error::invalid(
            "JPEG encode: a single-component frame must use sampling factors 1x1 \
             (A.2.2 orders its data units independently of H/V; TN2 requires 1 for \
             PlanarConfiguration = 2 segments)",
        ));
    }
    Ok((hmax, vmax))
}

/// Write the frame header (B.2.2, Figure B.3).
fn write_sof(out: &mut Vec<u8>, frame: &JpegFrame, comps: &[JpegComponent<'_>]) {
    let marker = match frame.process {
        JpegProcess::Dct if frame.precision == 8 => SOF0,
        JpegProcess::Dct => SOF1,
        JpegProcess::Lossless { .. } => SOF3,
    };
    out.extend_from_slice(&[0xFF, marker]);
    let lf = 8 + 3 * comps.len();
    out.extend_from_slice(&(lf as u16).to_be_bytes());
    out.push(frame.precision);
    out.extend_from_slice(&frame.height.to_be_bytes());
    out.extend_from_slice(&frame.width.to_be_bytes());
    out.push(comps.len() as u8);
    for (i, c) in comps.iter().enumerate() {
        // Component identifiers 1..=Nf: TN2 only asks that they be
        // distinct and identical across every segment of the image.
        out.push(i as u8 + 1);
        out.push((c.h << 4) | c.v);
        out.push(match frame.process {
            JpegProcess::Dct => c.quant_id,
            JpegProcess::Lossless { .. } => 0,
        });
    }
}

/// Write the scan header (B.2.3, Figure B.4).
fn write_sos(out: &mut Vec<u8>, frame: &JpegFrame, comps: &[JpegComponent<'_>]) {
    out.extend_from_slice(&[0xFF, SOS]);
    let ls = 6 + 2 * comps.len();
    out.extend_from_slice(&(ls as u16).to_be_bytes());
    out.push(comps.len() as u8);
    for (i, c) in comps.iter().enumerate() {
        out.push(i as u8 + 1);
        let ta = match frame.process {
            JpegProcess::Dct => c.huff_id,
            JpegProcess::Lossless { .. } => 0,
        };
        out.push((c.huff_id << 4) | ta);
    }
    match frame.process {
        // Ss = 0, Se = 63, Ah = Al = 0 for the sequential DCT processes.
        JpegProcess::Dct => out.extend_from_slice(&[0, 63, 0]),
        // Ss = predictor, Se = 0, Ah = 0, Al = Pt = 0 for lossless.
        JpegProcess::Lossless { predictor } => out.extend_from_slice(&[predictor, 0, 0]),
    }
}

/// Drive the entropy coder over the whole scan, feeding `sink`.
fn code_scan(
    frame: &JpegFrame,
    comps: &[JpegComponent<'_>],
    hmax: u8,
    vmax: u8,
    tables: &JpegTableSet,
    sink: &mut Sink<'_>,
) -> Result<()> {
    match frame.process {
        JpegProcess::Dct => code_scan_dct(frame, comps, hmax, vmax, tables, sink),
        JpegProcess::Lossless { predictor } => {
            code_scan_lossless(frame, comps, hmax, vmax, predictor, sink)
        }
    }
}

fn code_scan_dct(
    frame: &JpegFrame,
    comps: &[JpegComponent<'_>],
    hmax: u8,
    vmax: u8,
    tables: &JpegTableSet,
    sink: &mut Sink<'_>,
) -> Result<()> {
    let fdct = Fdct::new();
    let level = 1i32 << (frame.precision - 1);
    let interleaved = comps.len() > 1;
    // A.2.2 / A.2.3: the MCU grid.
    let (mcus_x, mcus_y) = if interleaved {
        (
            (frame.width as usize).div_ceil(8 * hmax as usize),
            (frame.height as usize).div_ceil(8 * vmax as usize),
        )
    } else {
        (comps[0].width.div_ceil(8), comps[0].height.div_ceil(8))
    };
    let quant_for: Vec<[u16; 64]> = comps
        .iter()
        .map(|c| {
            tables.quant[c.quant_id as usize].ok_or_else(|| {
                Error::invalid(format!(
                    "JPEG encode: quantisation table {} not defined",
                    c.quant_id
                ))
            })
        })
        .collect::<Result<_>>()?;
    let mut pred: Vec<i32> = vec![0; comps.len()];
    let mut block = [0f64; 64];
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            for (ci, c) in comps.iter().enumerate() {
                let (bh, bv) = if interleaved {
                    (c.h as usize, c.v as usize)
                } else {
                    (1, 1)
                };
                for v in 0..bv {
                    for h in 0..bh {
                        let bx = (mx * bh + h) * 8;
                        let by = (my * bv + v) * 8;
                        // A.2.4: complete partial blocks by replicating
                        // the right-most column / bottom line.
                        for y in 0..8 {
                            let sy = (by + y).min(c.height - 1);
                            for x in 0..8 {
                                let sx = (bx + x).min(c.width - 1);
                                block[y * 8 + x] =
                                    (c.samples[sy * c.width + sx] as i32 - level) as f64;
                            }
                        }
                        let coefs = fdct.transform(&block);
                        let q = quantize(&coefs, &quant_for[ci]);
                        code_block(&q, &mut pred[ci], c.huff_id, sink)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// F.1.2.1.3 + F.1.2.2.3 (Figures F.2 / F.3): code one quantised block.
fn code_block(q: &[i32; 64], pred: &mut i32, table: u8, sink: &mut Sink<'_>) -> Result<()> {
    // DC: DIFF = ZZ(0) − PRED (F.1.1.5.1).
    let dc = q[0];
    let diff = dc - *pred;
    *pred = dc;
    let ssss = category(diff);
    sink.dc_symbol(table, ssss as u8, extra_bits(diff, ssss), ssss)?;
    // AC: run/size composites in zig-zag order.
    let mut run: u32 = 0;
    for k in 1..64 {
        let coef = q[NATURAL[k]];
        if coef == 0 {
            run += 1;
            continue;
        }
        while run > 15 {
            // ZRL: 16 zero coefficients.
            sink.ac_symbol(table, 0xF0, 0, 0)?;
            run -= 16;
        }
        let ssss = category(coef);
        let rs = ((run << 4) | ssss) as u8;
        sink.ac_symbol(table, rs, extra_bits(coef, ssss), ssss)?;
        run = 0;
    }
    if run > 0 {
        // EOB.
        sink.ac_symbol(table, 0x00, 0, 0)?;
    }
    Ok(())
}

fn code_scan_lossless(
    frame: &JpegFrame,
    comps: &[JpegComponent<'_>],
    hmax: u8,
    vmax: u8,
    predictor: u8,
    sink: &mut Sink<'_>,
) -> Result<()> {
    let interleaved = comps.len() > 1;
    // H.1.1: the data unit is one sample; A.2.3 arranges interleaved
    // samples in Hk × Vk arrays per MCU.
    let (mcus_x, mcus_y) = if interleaved {
        (
            (frame.width as usize).div_ceil(hmax as usize),
            (frame.height as usize).div_ceil(vmax as usize),
        )
    } else {
        (comps[0].width, comps[0].height)
    };
    let initial = 1i32 << (frame.precision - 1);
    for my in 0..mcus_y {
        for mx in 0..mcus_x {
            for c in comps.iter() {
                let (bh, bv) = if interleaved {
                    (c.h as usize, c.v as usize)
                } else {
                    (1, 1)
                };
                for v in 0..bv {
                    for h in 0..bh {
                        let x = mx * bh + h;
                        let y = my * bv + v;
                        // A.2.4: samples appended to complete a partial
                        // MCU replicate the edge sample.
                        let sample = |xx: usize, yy: usize| -> i32 {
                            let sx = xx.min(c.width - 1);
                            let sy = yy.min(c.height - 1);
                            c.samples[sy * c.width + sx] as i32
                        };
                        let cur = sample(x, y);
                        // H.1.2.1 prediction.
                        let px = if y == 0 {
                            if x == 0 {
                                initial
                            } else {
                                sample(x - 1, y)
                            }
                        } else if x == 0 {
                            sample(x, y - 1)
                        } else {
                            let ra = sample(x - 1, y);
                            let rb = sample(x, y - 1);
                            let rc = sample(x - 1, y - 1);
                            match predictor {
                                1 => ra,
                                2 => rb,
                                3 => rc,
                                4 => ra + rb - rc,
                                5 => ra + ((rb - rc) >> 1),
                                6 => rb + ((ra - rc) >> 1),
                                _ => (ra + rb) >> 1,
                            }
                        };
                        // Modulo 2^16 difference (H.1.2.1), coded per
                        // Table H.2 — SSSS = 16 carries no extra bits.
                        let diff = ((cur - px) as i64).rem_euclid(65536) as i32;
                        let diff = if diff >= 32768 { diff - 65536 } else { diff };
                        if diff == -32768 {
                            sink.dc_symbol(c.huff_id, 16, 0, 0)?;
                        } else {
                            let ssss = category(diff);
                            sink.dc_symbol(c.huff_id, ssss as u8, extra_bits(diff, ssss), ssss)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Gather the DC / AC symbol statistics of one frame (K.2 input) into
/// `dc_stats` / `ac_stats` (indexed by table destination), so that
/// optimal tables can be derived — possibly across many frames sharing
/// one `JPEGTables` field.
pub fn gather_stats(
    frame: &JpegFrame,
    comps: &[JpegComponent<'_>],
    quant: &JpegTableSet,
    dc_stats: &mut [HuffStats; 4],
    ac_stats: &mut [HuffStats; 4],
) -> Result<()> {
    let (hmax, vmax) = validate(frame, comps)?;
    let mut sink = Sink::Count {
        dc: dc_stats,
        ac: ac_stats,
    };
    code_scan(frame, comps, hmax, vmax, quant, &mut sink)
}

/// Encode one complete frame as an ISO JPEG datastream: `SOI`, the
/// table segments (only when `emit_tables`), `SOFn`, `SOS`, the
/// entropy-coded segment, `EOI`. With `emit_tables = false` the
/// stream is a TN2 "abbreviated" image segment that relies on the
/// `JPEGTables` field having installed the same [`JpegTableSet`].
pub fn encode_frame(
    frame: &JpegFrame,
    comps: &[JpegComponent<'_>],
    tables: &JpegTableSet,
    emit_tables: bool,
) -> Result<Vec<u8>> {
    let (hmax, vmax) = validate(frame, comps)?;
    let dct = matches!(frame.process, JpegProcess::Dct);
    let mut scan_tables = ScanTables {
        dc: [None, None, None, None],
        ac: [None, None, None, None],
    };
    for c in comps {
        let id = c.huff_id as usize;
        if scan_tables.dc[id].is_none() {
            let spec = tables.dc[id].as_ref().ok_or_else(|| {
                Error::invalid(format!("JPEG encode: DC Huffman table {id} not defined"))
            })?;
            scan_tables.dc[id] = Some(HuffCodes::from_spec(spec)?);
        }
        if dct && scan_tables.ac[id].is_none() {
            let spec = tables.ac[id].as_ref().ok_or_else(|| {
                Error::invalid(format!("JPEG encode: AC Huffman table {id} not defined"))
            })?;
            scan_tables.ac[id] = Some(HuffCodes::from_spec(spec)?);
        }
    }

    let mut out = vec![0xFF, SOI];
    if emit_tables {
        tables.write_tables(&mut out, dct);
    }
    write_sof(&mut out, frame, comps);
    write_sos(&mut out, frame, comps);
    let mut writer = BitWriter::new();
    {
        let mut sink = Sink::Emit {
            writer: &mut writer,
            tables: &scan_tables,
        };
        code_scan(frame, comps, hmax, vmax, tables, &mut sink)?;
    }
    out.extend_from_slice(&writer.finish());
    out.extend_from_slice(&[0xFF, EOI]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests: table transcription cross-checks + structural properties.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k3_k6_bits_sum_matches_huffval_length() {
        for spec in [
            HuffSpec::k3_dc_luminance(),
            HuffSpec::k4_dc_chrominance(),
            HuffSpec::k5_ac_luminance(),
            HuffSpec::k6_ac_chrominance(),
        ] {
            spec.validate().unwrap();
            HuffCodes::from_spec(&spec).unwrap();
        }
        assert_eq!(VAL_AC_LUMINANCE.len(), 162);
        assert_eq!(VAL_AC_CHROMINANCE.len(), 162);
    }

    /// Table K.3 prints the luminance DC code words: category 0 =
    /// `00` (2 bits), 1..=5 = `010`..`110` (3 bits), 6 = `1110`, …,
    /// 11 = `111111110` (9 bits).
    #[test]
    fn k3_code_words_match_the_printed_table() {
        let codes = HuffCodes::from_spec(&HuffSpec::k3_dc_luminance()).unwrap();
        assert_eq!((codes.code[0], codes.size[0]), (0b00, 2));
        assert_eq!((codes.code[1], codes.size[1]), (0b010, 3));
        assert_eq!((codes.code[5], codes.size[5]), (0b110, 3));
        assert_eq!((codes.code[6], codes.size[6]), (0b1110, 4));
        assert_eq!((codes.code[11], codes.size[11]), (0b111111110, 9));
        // Table K.4: chrominance DC category 0 = `00`, 1 = `01`,
        // 2 = `10`, 3 = `110`, 11 = `11111111110`.
        let codes = HuffCodes::from_spec(&HuffSpec::k4_dc_chrominance()).unwrap();
        assert_eq!((codes.code[0], codes.size[0]), (0b00, 2));
        assert_eq!((codes.code[2], codes.size[2]), (0b10, 2));
        assert_eq!((codes.code[3], codes.size[3]), (0b110, 3));
        assert_eq!((codes.code[11], codes.size[11]), (0b11111111110, 11));
    }

    /// No code word may be all ones (F.1.2.1.1 / F.1.2.2.1).
    #[test]
    fn standard_tables_have_no_all_ones_code() {
        for spec in [
            HuffSpec::k3_dc_luminance(),
            HuffSpec::k4_dc_chrominance(),
            HuffSpec::k5_ac_luminance(),
            HuffSpec::k6_ac_chrominance(),
        ] {
            let codes = HuffCodes::from_spec(&spec).unwrap();
            for v in 0..256 {
                let s = codes.size[v] as u32;
                if s > 0 {
                    assert_ne!(codes.code[v] as u32, (1u32 << s) - 1, "symbol {v}");
                }
            }
        }
    }

    #[test]
    fn zigzag_is_a_permutation() {
        let mut seen = [false; 64];
        for &n in NATURAL.iter() {
            assert!(!seen[n]);
            seen[n] = true;
        }
        assert_eq!(NATURAL[1], 1);
        assert_eq!(NATURAL[2], 8);
        assert_eq!(NATURAL[63], 63);
    }

    #[test]
    fn quality_scaling_anchors() {
        let q50 = scaled_quant_table(&QUANT_LUMINANCE_K1, 50, 8);
        assert_eq!(q50, QUANT_LUMINANCE_K1);
        let q75 = scaled_quant_table(&QUANT_LUMINANCE_K1, 75, 8);
        assert_eq!(q75[0], 8);
        assert_eq!(q75[63], 50);
        let q100 = scaled_quant_table(&QUANT_LUMINANCE_K1, 100, 8);
        assert!(q100.iter().all(|&v| v == 1));
        let q1 = scaled_quant_table(&QUANT_LUMINANCE_K1, 1, 8);
        assert!(q1.iter().all(|&v| v == 255));
        let q12 = scaled_quant_table(&QUANT_LUMINANCE_K1, 50, 12);
        assert_eq!(q12[0], 16 * 16);
    }

    #[test]
    fn categories_and_extra_bits() {
        assert_eq!(category(0), 0);
        assert_eq!(category(1), 1);
        assert_eq!(category(-1), 1);
        assert_eq!(category(3), 2);
        assert_eq!(category(-4), 3);
        assert_eq!(category(2047), 11);
        assert_eq!(category(-32767), 15);
        assert_eq!(extra_bits(5, 3), 0b101);
        assert_eq!(extra_bits(-5, 3), 0b010);
        assert_eq!(extra_bits(-1, 1), 0);
        assert_eq!(extra_bits(1, 1), 1);
    }

    #[test]
    fn bit_writer_stuffs_ff_and_pads_with_ones() {
        let mut w = BitWriter::new();
        w.put(0xFF, 8);
        w.put(0b101, 3);
        let out = w.finish();
        assert_eq!(out, vec![0xFF, 0x00, 0b1011_1111]);
        let mut w = BitWriter::new();
        w.put(0b111_1111, 7);
        let out = w.finish();
        assert_eq!(out, vec![0xFF, 0x00]);
    }

    #[test]
    fn optimal_tables_cover_every_counted_symbol_and_limit_lengths() {
        let mut st = HuffStats::default();
        for v in 0..=255u32 {
            for _ in 0..(1 << (v % 12)) {
                st.count(v as u8);
            }
        }
        let spec = st.to_spec();
        spec.validate().unwrap();
        assert_eq!(spec.vals.len(), 256);
        let codes = HuffCodes::from_spec(&spec).unwrap();
        for v in 0..256 {
            assert!(codes.size[v] >= 1 && codes.size[v] <= 16);
            let s = codes.size[v] as u32;
            assert_ne!(codes.code[v] as u32, (1u32 << s) - 1);
        }
        // Kraft: a complete-or-under prefix code.
        let kraft: f64 = (0..256).map(|v| 2f64.powi(-(codes.size[v] as i32))).sum();
        assert!(kraft <= 1.0 + 1e-9);
        // Single-symbol alphabet degenerates to one 1-bit code.
        let mut st = HuffStats::default();
        st.count(0);
        let spec = st.to_spec();
        assert_eq!(spec.bits[0], 1);
        assert_eq!(spec.vals, vec![0]);
    }

    #[test]
    fn dct_of_flat_block_is_dc_only() {
        let f = Fdct::new();
        let block = [10f64; 64];
        let out = f.transform(&block);
        assert!((out[0] - 80.0).abs() < 1e-9);
        assert!(out[1..].iter().all(|v| v.abs() < 1e-9));
    }

    fn gray_frame(w: u16, h: u16) -> (Vec<u16>, JpegFrame) {
        let mut s = Vec::with_capacity(w as usize * h as usize);
        for y in 0..h {
            for x in 0..w {
                s.push((x * 3 + y * 5) % 256);
            }
        }
        (
            s,
            JpegFrame {
                width: w,
                height: h,
                precision: 8,
                process: JpegProcess::Dct,
            },
        )
    }

    fn baseline_tables() -> JpegTableSet {
        let mut t = JpegTableSet::default();
        t.quant[0] = Some(scaled_quant_table(&QUANT_LUMINANCE_K1, 75, 8));
        t.quant[1] = Some(scaled_quant_table(&QUANT_CHROMINANCE_K2, 75, 8));
        t.dc[0] = Some(HuffSpec::k3_dc_luminance());
        t.dc[1] = Some(HuffSpec::k4_dc_chrominance());
        t.ac[0] = Some(HuffSpec::k5_ac_luminance());
        t.ac[1] = Some(HuffSpec::k6_ac_chrominance());
        t
    }

    #[test]
    fn frame_stream_has_the_b2_marker_skeleton() {
        let (s, frame) = gray_frame(13, 9);
        let comp = JpegComponent {
            samples: &s,
            width: 13,
            height: 9,
            h: 1,
            v: 1,
            quant_id: 0,
            huff_id: 0,
        };
        let out = encode_frame(
            &frame,
            std::slice::from_ref(&comp),
            &baseline_tables(),
            true,
        )
        .unwrap();
        assert_eq!(&out[..2], &[0xFF, SOI]);
        assert_eq!(&out[out.len() - 2..], &[0xFF, EOI]);
        // SOI, DQT (only slot 0 referenced? no — every populated slot),
        // DHT ×4, SOF0, SOS.
        let mut i = 2;
        let mut markers = Vec::new();
        while i + 4 <= out.len() {
            assert_eq!(out[i], 0xFF);
            let m = out[i + 1];
            markers.push(m);
            if m == SOS {
                break;
            }
            let len = u16::from_be_bytes([out[i + 2], out[i + 3]]) as usize;
            i += 2 + len;
        }
        assert_eq!(markers, vec![DQT, DQT, DHT, DHT, DHT, DHT, SOF0, SOS]);
        // Abbreviated form drops the table segments.
        let abbr = encode_frame(&frame, &[comp], &baseline_tables(), false).unwrap();
        assert_eq!(abbr[2], 0xFF);
        assert_eq!(abbr[3], SOF0);
        assert!(abbr.len() < out.len());
        // No unstuffed FF inside the entropy segment except the EOI.
        let sos_at = out.windows(2).position(|w| w == [0xFF, SOS]).unwrap();
        let body = &out[sos_at + 2 + 6 + 2..out.len() - 2];
        for w in body.windows(2) {
            if w[0] == 0xFF {
                assert_eq!(w[1], 0x00);
            }
        }
    }

    #[test]
    fn geometry_validation_rejects_a1_1_mismatch() {
        let (s, frame) = gray_frame(16, 16);
        let comp = JpegComponent {
            samples: &s[..15 * 16],
            width: 15,
            height: 16,
            h: 1,
            v: 1,
            quant_id: 0,
            huff_id: 0,
        };
        assert!(encode_frame(&frame, &[comp], &baseline_tables(), true).is_err());
    }

    #[test]
    fn stats_then_optimal_tables_encode_the_same_frame() {
        let (s, mut frame) = gray_frame(24, 17);
        let s12: Vec<u16> = s.iter().map(|&v| v * 16).collect();
        frame.precision = 12;
        let comp = JpegComponent {
            samples: &s12,
            width: 24,
            height: 17,
            h: 1,
            v: 1,
            quant_id: 0,
            huff_id: 0,
        };
        let mut t = JpegTableSet::default();
        t.quant[0] = Some(scaled_quant_table(&QUANT_LUMINANCE_K1, 90, 12));
        let mut dc = [
            HuffStats::default(),
            HuffStats::default(),
            HuffStats::default(),
            HuffStats::default(),
        ];
        let mut ac = dc.clone();
        gather_stats(&frame, std::slice::from_ref(&comp), &t, &mut dc, &mut ac).unwrap();
        assert!(!dc[0].is_empty() && !ac[0].is_empty());
        t.dc[0] = Some(dc[0].to_spec());
        t.ac[0] = Some(ac[0].to_spec());
        let out = encode_frame(&frame, &[comp], &t, true).unwrap();
        assert!(out.windows(2).any(|w| w == [0xFF, SOF1]));
        // DQT carries Pq = 1 (16-bit entries) at 12-bit precision.
        let dqt = out.windows(2).position(|w| w == [0xFF, DQT]).unwrap();
        assert_eq!(out[dqt + 4] >> 4, 1);
    }

    #[test]
    fn lossless_stream_uses_sof3_and_predictor_in_sos() {
        let (s, mut frame) = gray_frame(7, 5);
        frame.process = JpegProcess::Lossless { predictor: 4 };
        let comp = JpegComponent {
            samples: &s,
            width: 7,
            height: 5,
            h: 1,
            v: 1,
            quant_id: 0,
            huff_id: 0,
        };
        let mut t = JpegTableSet::default();
        let mut dc = [
            HuffStats::default(),
            HuffStats::default(),
            HuffStats::default(),
            HuffStats::default(),
        ];
        let mut ac = dc.clone();
        gather_stats(&frame, std::slice::from_ref(&comp), &t, &mut dc, &mut ac).unwrap();
        assert!(ac[0].is_empty());
        t.dc[0] = Some(dc[0].to_spec());
        let out = encode_frame(&frame, &[comp], &t, true).unwrap();
        let sof = out.windows(2).position(|w| w == [0xFF, SOF3]).unwrap();
        assert_eq!(out[sof + 4], 8);
        let sos = out.windows(2).position(|w| w == [0xFF, SOS]).unwrap();
        // Ls = 8 for one component; Ss (predictor) follows the
        // component spec.
        // FF DA | Ls (2) | Ns (1) | Cs1 Td/Ta (2) | Ss | Se | Ah/Al.
        assert_eq!(out[sos + 7], 4);
        assert_eq!(out[sos + 8], 0);
        // No DQT / AC DHT in a lossless stream.
        assert!(!out.windows(2).any(|w| w == [0xFF, DQT]));
    }
}
