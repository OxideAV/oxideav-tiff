//! Black-box interop for the ICC profile (tag 34675) / XMP packet
//! (tag 700) carriage, per workspace policy: independent binaries are
//! invoked as opaque validator processes only.
//!
//! Directions exercised:
//!
//! * ours → theirs: files written by this crate's encoder are read by
//!   `magick` (profile extraction must be byte-exact), structurally
//!   listed by `tiffdump` (tag / type / count), and rewritten by
//!   `tiffcp` (the ICC profile must survive the rewrite byte-exact —
//!   `tiffcp` does not carry tag 700, so no XMP assertion there);
//! * theirs → ours: a file whose profiles were embedded by `magick`
//!   is decoded by this crate and both payloads must match the source
//!   bytes exactly.
//!
//! Every test gates on the binary being available (and, where a real
//! ICC profile is embedded by the external tool, on a system
//! colour-profile file existing) and passes with a skip note
//! otherwise, so CI without the tools stays green.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use oxideav_tiff::{
    decode_tiff, encode_tiff, EncodePage, EncodePixelFormat, PageExtras, TiffCompression,
};

fn binary_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `tiffdump` has no `-version`; probe with `-h`-less bare run (exits
/// non-zero but proves the binary exists and executes).
fn tiffdump_available() -> bool {
    Command::new("tiffdump")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n}-{seq}")
}

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxideav-tiff-iccxmp-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A structurally valid ICC profile (128-byte header with a correct
/// big-endian size field and `acsp` signature + zero-entry tag table).
fn minimal_icc(total_len: usize) -> Vec<u8> {
    assert!(total_len >= 132);
    let mut p = vec![0u8; total_len];
    p[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
    p[36..40].copy_from_slice(b"acsp");
    for (i, b) in p[128..].iter_mut().enumerate() {
        *b = (i % 247) as u8;
    }
    p
}

/// A real ICC profile from the host system, when one exists — used
/// where the *external* tool embeds the profile (it validates profile
/// contents before accepting them, so a synthesized minimal profile
/// is not enough for that direction).
fn system_icc_profile() -> Option<Vec<u8>> {
    let candidates = [
        "/System/Library/ColorSync/Profiles/Display P3.icc",
        "/System/Library/ColorSync/Profiles/AdobeRGB1998.icc",
        "/System/Library/ColorSync/Profiles/Generic RGB Profile.icc",
        "/usr/share/color/icc/sRGB.icc",
        "/usr/share/color/icc/colord/sRGB.icc",
    ];
    for c in candidates {
        if let Ok(bytes) = fs::read(c) {
            if bytes.len() >= 132 {
                return Some(bytes);
            }
        }
    }
    None
}

fn xmp_packet(marker: &str) -> Vec<u8> {
    format!(
        "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF \
         xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
         <rdf:Description rdf:about=\"{marker}\"/></rdf:RDF>\
         </x:xmpmeta><?xpacket end=\"w\"?>"
    )
    .into_bytes()
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::File::create(path).unwrap().write_all(bytes).unwrap();
}

fn encode_gray_with(xmp: Option<&[u8]>, icc: Option<&[u8]>, bigtiff: bool) -> Vec<u8> {
    let px: Vec<u8> = (0..64u32).map(|i| (i * 4) as u8).collect();
    let extras = PageExtras {
        xmp,
        icc_profile: icc,
        ..Default::default()
    };
    encode_tiff(&EncodePage {
        width: 8,
        height: 8,
        kind: EncodePixelFormat::Gray8 { pixels: &px },
        compression: TiffCompression::None,
        predictor: false,
        planar: false,
        tiling: None,
        bigtiff,
        extras,
    })
    .expect("encode")
}

#[test]
fn ours_to_magick_profile_extraction_byte_exact() {
    if !binary_available("magick") {
        eprintln!("skipping: `magick` not available");
        return;
    }
    let xmp = xmp_packet("ours-to-magick");
    // Prefer a real system profile so the reading tool has no excuse
    // to reject the blob; fall back to the synthesized minimal one
    // (extraction is a raw copy either way).
    let icc = system_icc_profile().unwrap_or_else(|| minimal_icc(300));
    for bigtiff in [false, true] {
        let tiff = encode_gray_with(Some(&xmp), Some(&icc), bigtiff);
        let dir = tmp_dir();
        let in_path = dir.join("ours.tif");
        write_file(&in_path, &tiff);
        let icc_out = dir.join("out.icc");
        let st = Command::new("magick")
            .arg(&in_path)
            .arg(format!("icc:{}", icc_out.display()))
            .status()
            .expect("run magick icc");
        assert!(
            st.success(),
            "magick icc extraction failed (bigtiff={bigtiff})"
        );
        assert_eq!(
            fs::read(&icc_out).unwrap(),
            icc,
            "ICC extracted by magick differs (bigtiff={bigtiff})"
        );
        let xmp_out = dir.join("out.xmp");
        let st = Command::new("magick")
            .arg(&in_path)
            .arg(format!("xmp:{}", xmp_out.display()))
            .status()
            .expect("run magick xmp");
        assert!(
            st.success(),
            "magick xmp extraction failed (bigtiff={bigtiff})"
        );
        assert_eq!(
            fs::read(&xmp_out).unwrap(),
            xmp,
            "XMP extracted by magick differs (bigtiff={bigtiff})"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn magick_to_ours_both_payloads_byte_exact() {
    if !binary_available("magick") {
        eprintln!("skipping: `magick` not available");
        return;
    }
    let Some(icc) = system_icc_profile() else {
        eprintln!("skipping: no system ICC profile file found");
        return;
    };
    let xmp = xmp_packet("magick-to-ours");
    let dir = tmp_dir();
    // Base image written by our encoder, no payloads.
    let base = dir.join("base.tif");
    write_file(&base, &encode_gray_with(None, None, false));
    let icc_src = dir.join("src.icc");
    write_file(&icc_src, &icc);
    let xmp_src = dir.join("src.xmp");
    write_file(&xmp_src, &xmp);
    let out = dir.join("theirs.tif");
    let st = Command::new("magick")
        .arg(&base)
        .arg("-profile")
        .arg(&icc_src)
        .arg("-profile")
        .arg(format!("xmp:{}", xmp_src.display()))
        .arg(&out)
        .status()
        .expect("run magick embed");
    assert!(st.success(), "magick profile embedding failed");
    let theirs = fs::read(&out).unwrap();
    let d = decode_tiff(&theirs).expect("decode externally-written file");
    assert_eq!(
        d.metadata.icc_profile.as_deref(),
        Some(icc.as_slice()),
        "ICC decoded from externally-written file differs"
    );
    assert_eq!(
        d.metadata.xmp.as_deref(),
        Some(xmp.as_slice()),
        "XMP decoded from externally-written file differs"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tiffcp_rewrite_preserves_icc() {
    if !binary_available("tiffcp") {
        eprintln!("skipping: `tiffcp` not available");
        return;
    }
    let xmp = xmp_packet("tiffcp");
    let icc = minimal_icc(260);
    let tiff = encode_gray_with(Some(&xmp), Some(&icc), false);
    let dir = tmp_dir();
    let in_path = dir.join("ours.tif");
    write_file(&in_path, &tiff);
    // Rewrite with a compression change so the rewriter fully
    // re-encodes the image rather than block-copying it.
    let out_path = dir.join("copied.tif");
    let st = Command::new("tiffcp")
        .arg("-c")
        .arg("lzw")
        .arg(&in_path)
        .arg(&out_path)
        .status()
        .expect("run tiffcp");
    assert!(st.success(), "tiffcp rewrite failed");
    let copied = fs::read(&out_path).unwrap();
    let d = decode_tiff(&copied).expect("decode tiffcp output");
    assert_eq!(
        d.metadata.icc_profile.as_deref(),
        Some(icc.as_slice()),
        "ICC must survive the external rewrite byte-exact"
    );
    // (tag 700 is not carried over by this rewriter — no XMP claim.)
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tiffdump_reports_registered_tag_shapes() {
    if !tiffdump_available() {
        eprintln!("skipping: `tiffdump` not available");
        return;
    }
    let xmp = xmp_packet("tiffdump");
    let icc = minimal_icc(180);
    let tiff = encode_gray_with(Some(&xmp), Some(&icc), false);
    let dir = tmp_dir();
    let in_path = dir.join("ours.tif");
    write_file(&in_path, &tiff);
    let out = Command::new("tiffdump")
        .arg(&in_path)
        .output()
        .expect("run tiffdump");
    let text = String::from_utf8_lossy(&out.stdout);
    // Independent structural listing: tag number, field type name,
    // and count must all match what we intended to write.
    let icc_line = text
        .lines()
        .find(|l| l.contains("34675"))
        .expect("tag 34675 listed");
    assert!(icc_line.contains("UNDEFINED (7)"), "{icc_line}");
    assert!(icc_line.contains(&format!(" {}<", icc.len())), "{icc_line}");
    let xmp_line = text
        .lines()
        .find(|l| l.contains("(0x2bc)") || l.starts_with("700") || l.contains(" 700 "))
        .expect("tag 700 listed");
    assert!(xmp_line.contains("BYTE (1)"), "{xmp_line}");
    assert!(xmp_line.contains(&format!(" {}<", xmp.len())), "{xmp_line}");
    let _ = fs::remove_dir_all(&dir);
}
