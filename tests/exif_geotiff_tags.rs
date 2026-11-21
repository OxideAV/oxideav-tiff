//! Typed Exif / GPS / Interoperability directories and GeoTIFF
//! georeferencing ([`oxideav_tiff::tags`]).

use std::fs;
use std::process::Command;

use oxideav_tiff::tags::{GeoKeyValue, TagGroup, TagValue, geo_key_name, tag_name};
use oxideav_tiff::{
    AuxIfdEntry, EncodePage, EncodePixelFormat, ExifData, PageExtras, TiffCompression, decode_page,
    encode_page,
};

fn rational(pairs: &[(u32, u32)]) -> Vec<u8> {
    pairs
        .iter()
        .flat_map(|&(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
        .collect()
}

/// One little-endian classic IFD at `base` (entries sorted by the
/// caller), values past four bytes laid out behind it.
fn ifd(entries: &[(u16, u16, u32, Vec<u8>)], base: u32) -> Vec<u8> {
    let size = 2 + 12 * entries.len() as u32 + 4;
    let mut out = (entries.len() as u16).to_le_bytes().to_vec();
    let mut tail: Vec<u8> = Vec::new();
    for (tag, ty, count, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        if value.len() <= 4 {
            let mut slot = [0u8; 4];
            slot[..value.len()].copy_from_slice(value);
            out.extend_from_slice(&slot);
        } else {
            if tail.len() % 2 == 1 {
                tail.push(0);
            }
            out.extend_from_slice(&(base + size + tail.len() as u32).to_le_bytes());
            tail.extend_from_slice(value);
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&tail);
    if out.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn exif_entries() -> Vec<(u16, u16, u32, Vec<u8>)> {
    vec![
        (33434, 5, 1, rational(&[(1, 125)])),
        (33437, 5, 1, rational(&[(28, 10)])),
        (34855, 3, 1, 400u16.to_le_bytes().to_vec()),
        (36864, 7, 4, b"0231".to_vec()),
        (36867, 2, 20, b"2026:11:21 10:30:00\0".to_vec()),
        (37386, 5, 1, rational(&[(50, 1)])),
        (42036, 2, 12, b"Test 50mm f\0".to_vec()),
    ]
}

fn gps_entries() -> Vec<(u16, u16, u32, Vec<u8>)> {
    vec![
        (0, 1, 4, vec![2, 4, 0, 0]),
        (1, 2, 2, b"S\0".to_vec()),
        (2, 5, 3, rational(&[(35, 1), (40, 1), (3042, 100)])),
        (3, 2, 2, b"E\0".to_vec()),
        (4, 5, 3, rational(&[(139, 1), (45, 1), (1234, 100)])),
        (5, 1, 1, vec![1]),
        (6, 5, 1, rational(&[(4012, 100)])),
    ]
}

/// A TIFF-structured Exif block: header, a first directory with the
/// two pointers, then the Exif and GPS directories.
fn exif_block() -> Vec<u8> {
    let ifd0_size = 2 + 12 * 2 + 4;
    let exif_at = 8 + ifd0_size;
    let exif = ifd(&exif_entries(), exif_at);
    let gps_at = exif_at + exif.len() as u32;
    let gps = ifd(&gps_entries(), gps_at);
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    for (tag, at) in [(34665u16, exif_at), (34853, gps_at)] {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&exif);
    out.extend_from_slice(&gps);
    out
}

fn check_typed(exif: &ExifData) {
    assert_eq!(exif.exposure_time(), Some(1.0 / 125.0));
    assert_eq!(exif.f_number(), Some(2.8));
    assert_eq!(exif.photographic_sensitivity(), Some(400));
    assert_eq!(exif.focal_length(), Some(50.0));
    assert_eq!(exif.date_time_original(), Some("2026:11:21 10:30:00"));
    assert_eq!(exif.lens_model(), Some("Test 50mm f"));
    assert_eq!(exif.exif_version().as_deref(), Some("0231"));
    assert_eq!(
        exif.get(TagGroup::Exif, 33434),
        Some(&TagValue::Rational(vec![(1, 125)]))
    );
    let names: Vec<_> = exif.exif.iter().map(|e| e.name).collect();
    assert!(names.contains(&Some("ExposureTime")) && names.contains(&Some("LensModel")));
    assert!(exif.exif.windows(2).all(|w| w[0].tag < w[1].tag));
    let pos = exif.gps_position().expect("gps position");
    // 35° 40' 30.42" S, 139° 45' 12.34" E, 40.12 m below the reference.
    assert!((pos.latitude + (35.0 + 40.0 / 60.0 + 30.42 / 3600.0)).abs() < 1e-9);
    assert!((pos.longitude - (139.0 + 45.0 / 60.0 + 12.34 / 3600.0)).abs() < 1e-9);
    assert_eq!(pos.altitude, Some(-40.12));
    assert_eq!(
        exif.gps.iter().find(|e| e.tag == 2).and_then(|e| e.name),
        Some("GPSLatitude")
    );
}

#[test]
fn exif_block_parses_to_typed_entries() {
    let block = exif_block();
    check_typed(&ExifData::parse(&block).unwrap());
    // The APP1 form (identifier in front) parses too.
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&block);
    check_typed(&ExifData::parse(&app1).unwrap());
    assert!(ExifData::parse(b"not an exif block").is_err());
}

#[test]
fn written_child_directories_come_back_typed() {
    let exif = exif_entries();
    let gps = gps_entries();
    fn aux(v: &[(u16, u16, u32, Vec<u8>)]) -> Vec<AuxIfdEntry<'_>> {
        v.iter()
            .map(|(tag, ty, count, value)| AuxIfdEntry::new(*tag, *ty, *count as u64, value))
            .collect()
    }
    let (exif_aux, gps_aux) = (aux(&exif), aux(&gps));
    let pixels = [10u8, 20, 30, 40];
    let page = EncodePage::new(
        2,
        2,
        EncodePixelFormat::Gray8 { pixels: &pixels },
        TiffCompression::Lzw,
        false,
        false,
        None,
        false,
        PageExtras::default()
            .with_exif_ifd(Some(&exif_aux))
            .with_gps_ifd(Some(&gps_aux)),
    );
    for bytes in [encode_page(&page).unwrap(), {
        let mut big = page.clone();
        big.bigtiff = true;
        encode_page(&big).unwrap()
    }] {
        let decoded = decode_page(&bytes).unwrap();
        check_typed(&decoded.exif);
        // The portable Exif block of the image carries the same fields.
        let blob = decoded.image.metadata.exif.as_ref().expect("exif blob");
        check_typed(&ExifData::parse(blob).unwrap());
        assert!(decoded.geotiff.is_none());
    }
}

#[test]
fn page_without_child_directories_has_empty_exif() {
    let bytes = encode_page(&EncodePage::new(
        1,
        1,
        EncodePixelFormat::Gray8 { pixels: &[7] },
        TiffCompression::None,
        false,
        false,
        None,
        false,
        PageExtras::default(),
    ))
    .unwrap();
    let page = decode_page(&bytes).unwrap();
    assert!(page.exif.is_empty());
    assert!(page.exif.gps_position().is_none());
}

/// A 1 × 1 gray TIFF (either byte order) whose directory carries the
/// GeoTIFF tags of an imaginary UTM page.
fn geotiff_file(big_endian: bool) -> Vec<u8> {
    let u16b = |v: u16| {
        if big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let u32b = |v: u32| {
        if big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let f64b = |v: f64| {
        if big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let doubles = |v: &[f64]| -> Vec<u8> { v.iter().flat_map(|&x| f64b(x)).collect() };
    let shorts = |v: &[u16]| -> Vec<u8> { v.iter().flat_map(|&x| u16b(x)).collect() };
    let ascii = b"WGS 84 / UTM zone 12N|WGS 84|\0".to_vec();
    #[rustfmt::skip]
    let keys: Vec<u16> = vec![
        1, 1, 1, 6,
        1024, 0, 1, 1,
        1025, 0, 1, 1,
        1026, 34737, 22, 0,
        2049, 34737, 7, 22,
        3072, 0, 1, 32612,
        2057, 34736, 1, 1,
    ];
    // (tag, type, count, value bytes)
    let entries: Vec<(u16, u16, u32, Vec<u8>)> = vec![
        (256, 3, 1, shorts(&[1])),
        (257, 3, 1, shorts(&[1])),
        (258, 3, 1, shorts(&[8])),
        (259, 3, 1, shorts(&[1])),
        (262, 3, 1, shorts(&[1])),
        (273, 4, 1, u32b(8).to_vec()),
        (277, 3, 1, shorts(&[1])),
        (278, 3, 1, shorts(&[1])),
        (279, 4, 1, u32b(1).to_vec()),
        (33550, 12, 3, doubles(&[30.0, 30.0, 0.0])),
        (
            33922,
            12,
            6,
            doubles(&[0.0, 0.0, 0.0, 440720.0, 3751320.0, 0.0]),
        ),
        (34735, 3, keys.len() as u32, shorts(&keys)),
        (34736, 12, 2, doubles(&[298.257223563, 6378137.0])),
        (34737, 2, ascii.len() as u32, ascii),
    ];
    let mut out = if big_endian {
        b"MM\0*".to_vec()
    } else {
        b"II*\0".to_vec()
    };
    out.extend_from_slice(&u32b(10));
    out.extend_from_slice(&[0x55, 0]); // the pixel + pad; directory at 10
    let size = 2 + 12 * entries.len() as u32 + 4;
    let mut dir = u16b(entries.len() as u16).to_vec();
    let mut tail: Vec<u8> = Vec::new();
    for (tag, ty, count, value) in &entries {
        dir.extend_from_slice(&u16b(*tag));
        dir.extend_from_slice(&u16b(*ty));
        dir.extend_from_slice(&u32b(*count));
        if value.len() <= 4 {
            let mut slot = [0u8; 4];
            slot[..value.len()].copy_from_slice(value);
            dir.extend_from_slice(&slot);
        } else {
            if tail.len() % 2 == 1 {
                tail.push(0);
            }
            dir.extend_from_slice(&u32b(10 + size + tail.len() as u32));
            tail.extend_from_slice(value);
        }
    }
    dir.extend_from_slice(&u32b(0));
    out.extend_from_slice(&dir);
    out.extend_from_slice(&tail);
    out
}

#[test]
fn geotiff_directory_resolves_every_key_kind() {
    for big_endian in [false, true] {
        let page = decode_page(&geotiff_file(big_endian)).unwrap();
        let geo = page.geotiff.as_ref().expect("georeferencing");
        assert_eq!(geo.version, (1, 1, 1));
        assert_eq!(geo.model_type(), Some(1));
        assert_eq!(geo.raster_type(), Some(1));
        assert_eq!(geo.projected_crs(), Some(32612));
        assert_eq!(geo.geodetic_crs(), None);
        assert_eq!(
            geo.key(1026),
            Some(&GeoKeyValue::Ascii("WGS 84 / UTM zone 12N".into()))
        );
        assert_eq!(geo.key(2049), Some(&GeoKeyValue::Ascii("WGS 84".into())));
        assert_eq!(geo.key(2057), Some(&GeoKeyValue::Double(vec![6378137.0])));
        assert_eq!(geo.keys.len(), 6);
        assert_eq!(geo.keys[2].name, Some("GTCitationGeoKey"));
        assert_eq!(geo.pixel_scale, Some([30.0, 30.0, 0.0]));
        assert_eq!(
            geo.tiepoints,
            vec![[0.0, 0.0, 0.0, 440720.0, 3751320.0, 0.0]]
        );
        // ScaleY positive: model Y decreases as J grows.
        assert_eq!(geo.raster_to_model(10.0, 20.0), Some((441020.0, 3750720.0)));
    }
    assert_eq!(geo_key_name(4096), Some("VerticalGeoKey"));
    assert_eq!(
        tag_name(TagGroup::Interoperability, 1),
        Some("InteroperabilityIndex")
    );
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-version").output().is_ok()
}

/// Outside stream: a JPEG carrying the Exif block as APP1 is converted
/// to TIFF by `sips`, which lays the Exif and GPS directories out its
/// own way; the typed view must read the same fields back.
#[test]
fn exif_directories_written_by_another_tool() {
    if !have("magick") || !have("sips") {
        eprintln!("skip: magick / sips not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("oxideav-tiff-exif-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (jpg, spliced, tif) = (dir.join("a.jpg"), dir.join("b.jpg"), dir.join("c.tif"));
    let ok = Command::new("magick")
        .args(["-size", "32x24", "gradient:red-blue"])
        .arg(&jpg)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skip: magick could not write the JPEG");
        return;
    }
    let base = fs::read(&jpg).unwrap();
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&exif_block());
    let mut out = base[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&(payload.len() as u16 + 2).to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&base[2..]);
    fs::write(&spliced, out).unwrap();
    let ok = Command::new("sips")
        .args(["-s", "format", "tiff"])
        .arg(&spliced)
        .arg("--out")
        .arg(&tif)
        .output()
        .is_ok_and(|o| o.status.success());
    let bytes = fs::read(&tif).unwrap_or_default();
    let _ = fs::remove_dir_all(&dir);
    if !ok || bytes.is_empty() {
        eprintln!("skip: sips could not convert the JPEG");
        return;
    }
    let page = decode_page(&bytes).unwrap();
    let exif = &page.exif;
    assert_eq!(exif.exposure_time(), Some(1.0 / 125.0));
    assert_eq!(exif.f_number(), Some(2.8));
    assert_eq!(exif.focal_length(), Some(50.0));
    assert_eq!(exif.lens_model(), Some("Test 50mm f"));
    assert_eq!(exif.date_time_original(), Some("2026:11:21 10:30:00"));
    let pos = exif.gps_position().expect("gps position");
    assert!((pos.latitude + (35.0 + 40.0 / 60.0 + 30.42 / 3600.0)).abs() < 1e-6);
    assert!((pos.longitude - (139.0 + 45.0 / 60.0 + 12.34 / 3600.0)).abs() < 1e-6);
}
