//! Decode-side extraction of the two registered opaque metadata
//! payloads — the XMP packet (tag 700) and the embedded ICC profile
//! (tag 34675) — per `docs/image/tiff/tiff-icc-xmp-tags.md`:
//!
//! * both field types are accepted for either tag (BYTE 1 and
//!   UNDEFINED 7 — 1-byte opaque elements either way);
//! * the payload is copied verbatim: the file's `II`/`MM` byte order
//!   applies to the IFD entry's integer fields only, never to the
//!   payload bytes (ICC profiles are internally big-endian always;
//!   XMP is UTF-8 text);
//! * `Count` is the exact payload byte length; a `Count <= 4` payload
//!   lives inline in the classic value slot (§4.3);
//! * the ICC profile's own big-endian size field at profile offset +0
//!   must equal the IFD `Count` — a mismatch is malformed and drops
//!   the field without gating the pixel decode (§2 endianness caveat,
//!   §4.5);
//! * extraction is total: a malformed entry leaves that one field
//!   `None`.
//!
//! Files are hand-built byte-by-byte (classic II + MM, BigTIFF, and a
//! two-page chain) so the reader is exercised against layouts this
//! crate's encoder does not produce.

use oxideav_tiff::{decode_tiff, decode_tiff_all_pages};

// Field types (TIFF 6.0 §2).
const TYPE_BYTE: u16 = 1;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_UNDEFINED: u16 = 7;
const TYPE_LONG8: u16 = 16;

const TAG_XMP: u16 = 700;
const TAG_ICC: u16 = 34675;

/// One IFD entry to hand-place: (tag, field type, count, value bytes).
/// `value` is the raw payload; the builder packs it inline when it
/// fits the variant's value slot and spills it out-of-line (even
/// offset) otherwise.
#[derive(Clone)]
struct Spec {
    tag: u16,
    ty: u16,
    count: u64,
    value: Vec<u8>,
}

fn spec(tag: u16, ty: u16, count: u64, value: Vec<u8>) -> Spec {
    Spec {
        tag,
        ty,
        count,
        value,
    }
}

fn w16(le: bool, v: u16) -> [u8; 2] {
    if le {
        v.to_le_bytes()
    } else {
        v.to_be_bytes()
    }
}
fn w32(le: bool, v: u32) -> [u8; 4] {
    if le {
        v.to_le_bytes()
    } else {
        v.to_be_bytes()
    }
}
fn w64(le: bool, v: u64) -> [u8; 8] {
    if le {
        v.to_le_bytes()
    } else {
        v.to_be_bytes()
    }
}

/// Base IFD entries for a 1×1 Gray8 uncompressed image whose single
/// pixel byte lives at `pixel_off`. Integer-field values are built in
/// the file byte order by the page writer, so here they are abstract.
fn base_entries(le: bool, pixel_off: u32) -> Vec<Spec> {
    vec![
        spec(256, TYPE_SHORT, 1, w16(le, 1).to_vec()), // ImageWidth
        spec(257, TYPE_SHORT, 1, w16(le, 1).to_vec()), // ImageLength
        spec(258, TYPE_SHORT, 1, w16(le, 8).to_vec()), // BitsPerSample
        spec(259, TYPE_SHORT, 1, w16(le, 1).to_vec()), // Compression=None
        spec(262, TYPE_SHORT, 1, w16(le, 1).to_vec()), // BlackIsZero
        spec(273, TYPE_LONG, 1, w32(le, pixel_off).to_vec()), // StripOffsets
        spec(277, TYPE_SHORT, 1, w16(le, 1).to_vec()), // SamplesPerPixel
        spec(278, TYPE_LONG, 1, w32(le, 1).to_vec()),  // RowsPerStrip
        spec(279, TYPE_LONG, 1, w32(le, 1).to_vec()),  // StripByteCounts
    ]
}

/// Build a classic TIFF holding one 1×1 Gray8 page per element of
/// `page_extras` (each element = extra entries merged into the base
/// set, sorted ascending). IFDs are chained via next-IFD pointers.
fn build_classic(le: bool, page_extras: &[Vec<Spec>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(if le { b"II" } else { b"MM" });
    out.extend_from_slice(&w16(le, 42));
    let first_ifd_slot = out.len();
    out.extend_from_slice(&w32(le, 0)); // patched below
    let mut prev_next_slot = first_ifd_slot;
    for extras in page_extras {
        // Pixel byte for this page.
        if out.len() % 2 != 0 {
            out.push(0);
        }
        let pixel_off = out.len() as u32;
        out.push(0xAB);
        let mut entries = base_entries(le, pixel_off);
        entries.extend(extras.iter().cloned());
        entries.sort_by_key(|e| e.tag);
        // Out-of-line payloads (> 4 bytes) go before the IFD.
        let mut offsets: Vec<Option<u32>> = Vec::new();
        for e in &entries {
            if e.value.len() > 4 {
                if out.len() % 2 != 0 {
                    out.push(0);
                }
                offsets.push(Some(out.len() as u32));
                out.extend_from_slice(&e.value);
            } else {
                offsets.push(None);
            }
        }
        if out.len() % 2 != 0 {
            out.push(0);
        }
        let ifd_off = out.len() as u32;
        // Patch the previous next-IFD (or header first-IFD) slot.
        let patched = w32(le, ifd_off);
        out[prev_next_slot..prev_next_slot + 4].copy_from_slice(&patched);
        out.extend_from_slice(&w16(le, entries.len() as u16));
        for (e, off) in entries.iter().zip(&offsets) {
            out.extend_from_slice(&w16(le, e.tag));
            out.extend_from_slice(&w16(le, e.ty));
            out.extend_from_slice(&w32(le, e.count as u32));
            match off {
                Some(o) => out.extend_from_slice(&w32(le, *o)),
                None => {
                    let mut slot = [0u8; 4];
                    slot[..e.value.len()].copy_from_slice(&e.value);
                    out.extend_from_slice(&slot);
                }
            }
        }
        prev_next_slot = out.len();
        out.extend_from_slice(&w32(le, 0));
    }
    out
}

/// Build a BigTIFF (magic 43, 20-byte entries, 8-byte offsets)
/// holding one 1×1 Gray8 page with the given extra entries.
fn build_bigtiff(le: bool, extras: &[Spec]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(if le { b"II" } else { b"MM" });
    out.extend_from_slice(&w16(le, 43));
    out.extend_from_slice(&w16(le, 8)); // offset bytesize
    out.extend_from_slice(&w16(le, 0)); // reserved
    let first_ifd_slot = out.len();
    out.extend_from_slice(&w64(le, 0)); // patched below
    let pixel_off = out.len() as u64;
    out.push(0xAB);
    let mut entries = base_entries(le, 0);
    // Rewrite StripOffsets as LONG8 with the real offset.
    for e in &mut entries {
        if e.tag == 273 {
            e.ty = TYPE_LONG8;
            e.value = w64(le, pixel_off).to_vec();
        }
    }
    entries.extend(extras.iter().cloned());
    entries.sort_by_key(|e| e.tag);
    let mut offsets: Vec<Option<u64>> = Vec::new();
    for e in &entries {
        if e.value.len() > 8 {
            if out.len() % 2 != 0 {
                out.push(0);
            }
            offsets.push(Some(out.len() as u64));
            out.extend_from_slice(&e.value);
        } else {
            offsets.push(None);
        }
    }
    if out.len() % 2 != 0 {
        out.push(0);
    }
    let ifd_off = out.len() as u64;
    let patched = w64(le, ifd_off);
    out[first_ifd_slot..first_ifd_slot + 8].copy_from_slice(&patched);
    out.extend_from_slice(&w64(le, entries.len() as u64));
    for (e, off) in entries.iter().zip(&offsets) {
        out.extend_from_slice(&w16(le, e.tag));
        out.extend_from_slice(&w16(le, e.ty));
        out.extend_from_slice(&w64(le, e.count));
        match off {
            Some(o) => out.extend_from_slice(&w64(le, *o)),
            None => {
                let mut slot = [0u8; 8];
                slot[..e.value.len()].copy_from_slice(&e.value);
                out.extend_from_slice(&slot);
            }
        }
    }
    out.extend_from_slice(&w64(le, 0)); // next IFD
    out
}

/// A structurally valid minimal ICC profile: the 128-byte header —
/// big-endian size field at +0 (always big-endian, independent of the
/// enclosing TIFF), `acsp` file signature at +36 — plus a zero-entry
/// tag table (4-byte big-endian count), 132 bytes total.
fn minimal_icc(total_len: usize) -> Vec<u8> {
    assert!(total_len >= 132);
    let mut p = vec![0u8; total_len];
    p[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
    p[36..40].copy_from_slice(b"acsp");
    // Fill the post-header area with a marker pattern so byte-exact
    // extraction is provable.
    for (i, b) in p[128..].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    p
}

fn xmp_packet() -> Vec<u8> {
    b"<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
      <x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>\
      <?xpacket end=\"w\"?>"
        .to_vec()
}

#[test]
fn classic_le_xmp_and_icc_extract_byte_exact() {
    let xmp = xmp_packet();
    let icc = minimal_icc(200);
    let extras = vec![
        spec(TAG_XMP, TYPE_BYTE, xmp.len() as u64, xmp.clone()),
        spec(TAG_ICC, TYPE_UNDEFINED, icc.len() as u64, icc.clone()),
    ];
    let file = build_classic(true, &[extras]);
    let d = decode_tiff(&file).expect("decode");
    assert_eq!(d.metadata.xmp.as_deref(), Some(xmp.as_slice()));
    assert_eq!(d.metadata.icc_profile.as_deref(), Some(icc.as_slice()));
    assert_eq!(d.width, 1);
}

#[test]
fn classic_be_payloads_identical_to_le() {
    // Byte-order independence: the same payload bytes stored in an MM
    // file must come back identical to the II extraction — the
    // II/MM order applies to the IFD entry fields only, never to the
    // opaque payloads.
    let xmp = xmp_packet();
    let icc = minimal_icc(180);
    let extras = vec![
        spec(TAG_XMP, TYPE_BYTE, xmp.len() as u64, xmp.clone()),
        spec(TAG_ICC, TYPE_UNDEFINED, icc.len() as u64, icc.clone()),
    ];
    let le = decode_tiff(&build_classic(true, std::slice::from_ref(&extras))).expect("decode II");
    let be = decode_tiff(&build_classic(false, &[extras])).expect("decode MM");
    assert_eq!(le.metadata.xmp, be.metadata.xmp);
    assert_eq!(le.metadata.icc_profile, be.metadata.icc_profile);
    assert_eq!(be.metadata.xmp.as_deref(), Some(xmp.as_slice()));
    assert_eq!(be.metadata.icc_profile.as_deref(), Some(icc.as_slice()));
}

#[test]
fn type_variants_accepted_both_ways() {
    // §4.2: accept BYTE and UNDEFINED for either tag. Swap the
    // canonical types (XMP normally BYTE, ICC normally UNDEFINED).
    let xmp = xmp_packet();
    let icc = minimal_icc(140);
    let extras = vec![
        spec(TAG_XMP, TYPE_UNDEFINED, xmp.len() as u64, xmp.clone()),
        spec(TAG_ICC, TYPE_BYTE, icc.len() as u64, icc.clone()),
    ];
    let d = decode_tiff(&build_classic(true, &[extras])).expect("decode");
    assert_eq!(d.metadata.xmp.as_deref(), Some(xmp.as_slice()));
    assert_eq!(d.metadata.icc_profile.as_deref(), Some(icc.as_slice()));
}

#[test]
fn wrong_field_type_drops_field_without_gating_decode() {
    // A SHORT-typed tag 700 is not an opaque byte payload; the field
    // is dropped, the pixels still decode.
    let extras = vec![spec(TAG_XMP, TYPE_SHORT, 2, vec![1, 0, 2, 0])];
    let d = decode_tiff(&build_classic(true, &[extras])).expect("decode");
    assert_eq!(d.metadata.xmp, None);
    assert_eq!(d.width, 1);
}

#[test]
fn inline_small_xmp_payload_extracts() {
    // §4.3: only if Count <= 4 is the value inline in the entry —
    // "rare, essentially never for real ICC/XMP", but must work.
    let payload = vec![0x58, 0x4D, 0x50]; // 3 bytes, inline
    let extras = vec![spec(TAG_XMP, TYPE_BYTE, 3, payload.clone())];
    let d = decode_tiff(&build_classic(true, &[extras])).expect("decode");
    assert_eq!(d.metadata.xmp.as_deref(), Some(payload.as_slice()));
}

#[test]
fn icc_size_field_mismatch_is_malformed() {
    // §2: the profile's big-endian size field at +0 must equal the IFD
    // Count; a mismatch drops the field.
    let mut icc = minimal_icc(160);
    icc[0..4].copy_from_slice(&159u32.to_be_bytes()); // lies by one
    let extras = vec![spec(TAG_ICC, TYPE_UNDEFINED, icc.len() as u64, icc)];
    let d = decode_tiff(&build_classic(true, &[extras])).expect("decode");
    assert_eq!(d.metadata.icc_profile, None);
    assert_eq!(d.width, 1); // pixel decode not gated
}

#[test]
fn icc_size_field_swapped_for_le_file_is_malformed() {
    // The size field is big-endian even inside an II file: a profile
    // whose size field was little-endian-swapped must not validate.
    let mut icc = minimal_icc(160);
    let le_size = 160u32.to_le_bytes();
    icc[0..4].copy_from_slice(&le_size);
    let extras = vec![spec(TAG_ICC, TYPE_UNDEFINED, icc.len() as u64, icc)];
    let d = decode_tiff(&build_classic(true, &[extras])).expect("decode");
    assert_eq!(d.metadata.icc_profile, None);
}

#[test]
fn icc_shorter_than_header_is_malformed() {
    // 100 bytes cannot hold the fixed 128-byte ICC header even with a
    // self-consistent size field.
    let mut icc = vec![0u8; 100];
    icc[0..4].copy_from_slice(&100u32.to_be_bytes());
    let extras = vec![spec(TAG_ICC, TYPE_UNDEFINED, icc.len() as u64, icc)];
    let d = decode_tiff(&build_classic(true, &[extras])).expect("decode");
    assert_eq!(d.metadata.icc_profile, None);
}

#[test]
fn empty_payload_is_dropped() {
    let extras = vec![spec(TAG_XMP, TYPE_BYTE, 0, vec![])];
    let d = decode_tiff(&build_classic(true, &[extras])).expect("decode");
    assert_eq!(d.metadata.xmp, None);
}

#[test]
fn xmp_non_utf8_bytes_surface_verbatim() {
    // The payload is opaque: a packet carrying non-UTF-8 octets is
    // still surfaced byte-for-byte (no lossy re-decoding).
    let payload = vec![0xFF, 0xFE, 0x00, 0x80, 0xC3, 0x28, 0x01];
    let extras = vec![spec(
        TAG_XMP,
        TYPE_BYTE,
        payload.len() as u64,
        payload.clone(),
    )];
    let d = decode_tiff(&build_classic(true, &[extras])).expect("decode");
    assert_eq!(d.metadata.xmp.as_deref(), Some(payload.as_slice()));
}

#[test]
fn bigtiff_both_orders_extract() {
    let xmp = xmp_packet();
    let icc = minimal_icc(256);
    for le in [true, false] {
        let extras = vec![
            spec(TAG_XMP, TYPE_BYTE, xmp.len() as u64, xmp.clone()),
            spec(TAG_ICC, TYPE_UNDEFINED, icc.len() as u64, icc.clone()),
        ];
        let file = build_bigtiff(le, &extras);
        let d = decode_tiff(&file).expect("decode BigTIFF");
        assert_eq!(d.metadata.xmp.as_deref(), Some(xmp.as_slice()), "le={le}");
        assert_eq!(
            d.metadata.icc_profile.as_deref(),
            Some(icc.as_slice()),
            "le={le}"
        );
    }
}

#[test]
fn bigtiff_inline_8_byte_payload_extracts() {
    // BigTIFF widens the inline threshold to 8 bytes; an 8-byte XMP
    // payload lives in the value slot itself.
    let payload = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let extras = vec![spec(TAG_XMP, TYPE_BYTE, 8, payload.clone())];
    let d = decode_tiff(&build_bigtiff(true, &extras)).expect("decode");
    assert_eq!(d.metadata.xmp.as_deref(), Some(payload.as_slice()));
}

#[test]
fn multi_page_chain_keeps_per_page_payloads() {
    // Two chained IFDs, each with its own XMP packet; page 2 also
    // carries an ICC profile. decode_tiff_all_pages must keep them
    // separate; decode_tiff sees only IFD0's.
    let xmp1 = b"<x:xmpmeta>page one</x:xmpmeta>".to_vec();
    let xmp2 = b"<x:xmpmeta>page two, different bytes</x:xmpmeta>".to_vec();
    let icc2 = minimal_icc(144);
    let page1 = vec![spec(TAG_XMP, TYPE_BYTE, xmp1.len() as u64, xmp1.clone())];
    let page2 = vec![
        spec(TAG_XMP, TYPE_BYTE, xmp2.len() as u64, xmp2.clone()),
        spec(TAG_ICC, TYPE_UNDEFINED, icc2.len() as u64, icc2.clone()),
    ];
    let file = build_classic(true, &[page1, page2]);
    let pages = decode_tiff_all_pages(&file).expect("decode all");
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].metadata.xmp.as_deref(), Some(xmp1.as_slice()));
    assert_eq!(pages[0].metadata.icc_profile, None);
    assert_eq!(pages[1].metadata.xmp.as_deref(), Some(xmp2.as_slice()));
    assert_eq!(
        pages[1].metadata.icc_profile.as_deref(),
        Some(icc2.as_slice())
    );
    let first = decode_tiff(&file).expect("decode first");
    assert_eq!(first.metadata.xmp.as_deref(), Some(xmp1.as_slice()));
}
