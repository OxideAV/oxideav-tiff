//! Independently produced ICC/XMP carriage fixtures, decoded and
//! pinned byte-exact — the trace-doc §5 "carriage-level fixture"
//! shape: prove extraction against files this crate did not write.
//!
//! `tests/data/icc_xmp/` holds:
//!
//! * `profile.icc` — a fully synthetic minimal ICC profile (132
//!   bytes: 128-byte header with big-endian size field, version 4.0,
//!   `mntr`/`RGB `/`XYZ ` fields and `acsp` signature, plus a
//!   zero-entry tag table). No third-party profile bytes.
//! * `packet.xmp` — a 251-byte `<?xpacket?>`-wrapped XMP packet.
//! * `icc_xmp_gray16.tif` / `icc_xmp_gray16_big.tif` — a 16×16
//!   grayscale gradient carrying both payloads, written by an
//!   independent black-box image tool (`magick`; classic TIFF and
//!   BigTIFF via its `tiff64:` coder). Generation commands are
//!   recorded in `docs/image/tiff/fixtures/icc-xmp/README.md`.
//!
//! The assertions pin SHA-256 + length of the embedded payload files
//! and require the decoder's extraction to match them byte-for-byte,
//! so any transport corruption (byte-swap, truncation, off-by-one
//! count handling) fails loudly.

use oxideav_tiff::decode_tiff;

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/icc_xmp");

/// Tiny non-cryptographic-context integrity pin (SHA-256, pure Rust,
/// test-local) so the fixture bytes themselves are tamper-evident.
fn sha256_hex(data: &[u8]) -> String {
    // FIPS 180-4 SHA-256, minimal implementation for test pinning.
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

fn load(name: &str) -> Vec<u8> {
    std::fs::read(format!("{FIXTURE_DIR}/{name}")).unwrap_or_else(|e| panic!("read {name}: {e}"))
}

/// (length, sha256) pins recorded at fixture-generation time.
const ICC_LEN: usize = 132;
const ICC_SHA: &str = "8da1376d6ae54564864ba4f1edff90da9f874a03a8f1baf2ba067a8e6f7c60d0";
const XMP_LEN: usize = 251;
const XMP_SHA: &str = "b71c0621fd7f9037c1d13ccf79c6f2d64f20ba7e89ba464f8abcf3a01e30d0e4";

#[test]
fn payload_source_files_match_their_pins() {
    let icc = load("profile.icc");
    assert_eq!(icc.len(), ICC_LEN);
    assert_eq!(sha256_hex(&icc), ICC_SHA);
    let xmp = load("packet.xmp");
    assert_eq!(xmp.len(), XMP_LEN);
    assert_eq!(sha256_hex(&xmp), XMP_SHA);
}

#[test]
fn classic_fixture_extracts_byte_exact() {
    let tiff = load("icc_xmp_gray16.tif");
    let icc = load("profile.icc");
    let xmp = load("packet.xmp");
    let d = decode_tiff(&tiff).expect("decode classic fixture");
    assert_eq!(d.width, 16);
    assert_eq!(d.height, 16);
    let got_icc = d.metadata.icc_profile.expect("icc extracted");
    assert_eq!(got_icc, icc);
    assert_eq!(sha256_hex(&got_icc), ICC_SHA);
    let got_xmp = d.metadata.xmp.expect("xmp extracted");
    assert_eq!(got_xmp, xmp);
    assert_eq!(sha256_hex(&got_xmp), XMP_SHA);
}

#[test]
fn bigtiff_fixture_extracts_byte_exact() {
    let tiff = load("icc_xmp_gray16_big.tif");
    let icc = load("profile.icc");
    let xmp = load("packet.xmp");
    // BigTIFF magic 43.
    assert_eq!(&tiff[2..4], &[0x2B, 0x00]);
    let d = decode_tiff(&tiff).expect("decode BigTIFF fixture");
    assert_eq!(d.width, 16);
    let got_icc = d.metadata.icc_profile.expect("icc extracted");
    assert_eq!(got_icc, icc);
    let got_xmp = d.metadata.xmp.expect("xmp extracted");
    assert_eq!(got_xmp, xmp);
}

#[test]
fn fixture_payloads_re_encode_and_survive() {
    // Full preservation loop over an externally written file: decode
    // the fixture, re-encode the payloads onto a fresh page with our
    // writer, decode again — the pins must still hold after two hops.
    use oxideav_tiff::{encode_tiff, EncodePage, EncodePixelFormat, PageExtras, TiffCompression};
    let tiff = load("icc_xmp_gray16.tif");
    let d = decode_tiff(&tiff).expect("decode fixture");
    let icc = d.metadata.icc_profile.expect("icc");
    let xmp = d.metadata.xmp.expect("xmp");
    let px: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let extras = PageExtras {
        xmp: Some(&xmp),
        icc_profile: Some(&icc),
        ..Default::default()
    };
    let ours = encode_tiff(&EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Gray8 { pixels: &px },
        compression: TiffCompression::Deflate,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff: false,
        extras,
    })
    .expect("re-encode");
    let d2 = decode_tiff(&ours).expect("decode re-encoded");
    assert_eq!(sha256_hex(&d2.metadata.icc_profile.unwrap()), ICC_SHA);
    assert_eq!(sha256_hex(&d2.metadata.xmp.unwrap()), XMP_SHA);
}
