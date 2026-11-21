//! Tag catalogues layered on the TIFF directory: the Exif IFD, the GPS
//! Info IFD and the Interoperability IFD (CIPA DC-008 / JEITA CP-3451
//! "Exif 3.1", §4.6.6 – §4.6.8) and the GeoTIFF GeoKey directory (OGC
//! 19-008r4 "GeoTIFF 1.1", §7.1).
//!
//! [`ExifData`] is the typed view of an Exif block: every field of the
//! child directories as a [`TagEntry`] (tag number, catalogue name,
//! decoded [`TagValue`]) plus a few derived accessors (GPS position in
//! signed decimal degrees, exposure time, …). It parses the same
//! TIFF-structured payload [`crate::Metadata::exif`] carries, so it
//! also reads the Exif block of any other container.
//!
//! [`GeoTiff`] is the georeferencing of a page: the three raster →
//! model tags and the GeoKeys, each resolved to its SHORT / DOUBLE /
//! ASCII value.

use crate::error::{Result, TiffError as Error};
use crate::ifd::{ByteOrder, Entry, find, parse_header, parse_ifd};
use crate::types::*;

/// Exif field type 129 (Exif 3.0, §4.6.2): a NUL-terminated UTF-8
/// string, "defined independently by this standard, rather than in
/// TIFF6.0".
pub const TYPE_UTF8: u16 = 129;

/// Tag 40965: pointer from the Exif IFD to the Interoperability IFD
/// (§4.6.3.3.1).
pub const TAG_INTEROPERABILITY_IFD: u16 = 40965;

/// Which directory a tag number is looked up in — the same number
/// means different things in each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TagGroup {
    /// The Exif IFD (pointer tag 34665).
    Exif,
    /// The GPS Info IFD (pointer tag 34853).
    Gps,
    /// The Interoperability IFD (pointer tag 40965, inside the Exif
    /// IFD).
    Interoperability,
}

/// The values of one directory field, decoded by field type.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TagValue {
    /// BYTE (1).
    Byte(Vec<u8>),
    /// ASCII (2) or UTF-8 (129), without the terminating NUL.
    Text(String),
    /// SHORT (3).
    Short(Vec<u16>),
    /// LONG (4).
    Long(Vec<u32>),
    /// RATIONAL (5): `(numerator, denominator)` pairs.
    Rational(Vec<(u32, u32)>),
    /// SBYTE (6).
    SByte(Vec<i8>),
    /// UNDEFINED (7): the bytes as stored.
    Undefined(Vec<u8>),
    /// SSHORT (8).
    SShort(Vec<i16>),
    /// SLONG (9).
    SLong(Vec<i32>),
    /// SRATIONAL (10): `(numerator, denominator)` pairs.
    SRational(Vec<(i32, i32)>),
    /// FLOAT (11).
    Float(Vec<f32>),
    /// DOUBLE (12).
    Double(Vec<f64>),
}

impl TagValue {
    /// Decode `count` values of `field_type` from little-endian
    /// `data`. `None` for a type with no rule or a short buffer.
    fn decode(field_type: u16, count: usize, data: &[u8]) -> Option<Self> {
        let unit = match field_type {
            TYPE_UTF8 => 1,
            t => type_size(t) as usize,
        };
        if unit == 0 || data.len() < count.checked_mul(unit)? {
            return None;
        }
        let d = &data[..count * unit];
        let text = |d: &[u8]| {
            let end = d.iter().position(|&b| b == 0).unwrap_or(d.len());
            String::from_utf8_lossy(&d[..end]).into_owned()
        };
        let u32s = |d: &[u8]| -> Vec<u32> {
            d.as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        };
        Some(match field_type {
            TYPE_BYTE => Self::Byte(d.to_vec()),
            TYPE_ASCII | TYPE_UTF8 => Self::Text(text(d)),
            TYPE_SHORT => Self::Short(
                d.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect(),
            ),
            TYPE_LONG => Self::Long(u32s(d)),
            TYPE_RATIONAL => Self::Rational(
                u32s(d)
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| (p[0], p[1]))
                    .collect(),
            ),
            TYPE_SBYTE => Self::SByte(d.iter().map(|&b| b as i8).collect()),
            TYPE_UNDEFINED => Self::Undefined(d.to_vec()),
            TYPE_SSHORT => Self::SShort(
                d.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect(),
            ),
            TYPE_SLONG => Self::SLong(u32s(d).into_iter().map(|v| v as i32).collect()),
            TYPE_SRATIONAL => Self::SRational(
                u32s(d)
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| (p[0] as i32, p[1] as i32))
                    .collect(),
            ),
            TYPE_FLOAT => Self::Float(u32s(d).into_iter().map(f32::from_bits).collect()),
            TYPE_DOUBLE => Self::Double(
                d.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                    .collect(),
            ),
            _ => return None,
        })
    }

    /// The text of an ASCII / UTF-8 field.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Every value as `f64` (a rational with a zero denominator is
    /// `NaN`); `None` for text and UNDEFINED fields.
    pub fn as_f64s(&self) -> Option<Vec<f64>> {
        let ratio = |n: f64, d: f64| if d == 0.0 { f64::NAN } else { n / d };
        Some(match self {
            Self::Byte(v) => v.iter().map(|&x| x as f64).collect(),
            Self::Short(v) => v.iter().map(|&x| x as f64).collect(),
            Self::Long(v) => v.iter().map(|&x| x as f64).collect(),
            Self::SByte(v) => v.iter().map(|&x| x as f64).collect(),
            Self::SShort(v) => v.iter().map(|&x| x as f64).collect(),
            Self::SLong(v) => v.iter().map(|&x| x as f64).collect(),
            Self::Rational(v) => v.iter().map(|&(n, d)| ratio(n as f64, d as f64)).collect(),
            Self::SRational(v) => v.iter().map(|&(n, d)| ratio(n as f64, d as f64)).collect(),
            Self::Float(v) => v.iter().map(|&x| x as f64).collect(),
            Self::Double(v) => v.clone(),
            Self::Text(_) | Self::Undefined(_) => return None,
        })
    }

    /// The first value as `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        self.as_f64s()?.first().copied()
    }

    /// The first value as an unsigned integer (integer types only).
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Self::Byte(v) => v.first().map(|&x| x as u32),
            Self::Short(v) => v.first().map(|&x| x as u32),
            Self::Long(v) => v.first().copied(),
            _ => None,
        }
    }
}

/// One field of an Exif-family directory.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TagEntry {
    /// Tag number.
    pub tag: u16,
    /// Field name from the catalogue of the directory the entry was
    /// read from (`None` for a tag the standard does not list — a
    /// private or newer tag).
    pub name: Option<&'static str>,
    /// Decoded values.
    pub value: TagValue,
}

/// Catalogue name of `tag` in `group` (Exif 3.1 Tables 8, 9, 14 and
/// §4.6.8), or `None` when the standard does not define it.
pub fn tag_name(group: TagGroup, tag: u16) -> Option<&'static str> {
    let table: &[(u16, &str)] = match group {
        TagGroup::Exif => EXIF_TAGS,
        TagGroup::Gps => GPS_TAGS,
        TagGroup::Interoperability => INTEROP_TAGS,
    };
    table
        .binary_search_by_key(&tag, |&(t, _)| t)
        .ok()
        .map(|i| table[i].1)
}

/// Exif IFD fields, Exif 3.1 Table 8 + Table 9 (ascending tag order).
const EXIF_TAGS: &[(u16, &str)] = &[
    (33434, "ExposureTime"),
    (33437, "FNumber"),
    (34850, "ExposureProgram"),
    (34852, "SpectralSensitivity"),
    (34855, "PhotographicSensitivity"),
    (34856, "OECF"),
    (34864, "SensitivityType"),
    (34865, "StandardOutputSensitivity"),
    (34866, "RecommendedExposureIndex"),
    (34867, "ISOSpeed"),
    (34868, "ISOSpeedLatitudeyyy"),
    (34869, "ISOSpeedLatitudezzz"),
    (36864, "ExifVersion"),
    (36867, "DateTimeOriginal"),
    (36868, "DateTimeDigitized"),
    (36880, "OffsetTime"),
    (36881, "OffsetTimeOriginal"),
    (36882, "OffsetTimeDigitized"),
    (37121, "ComponentsConfiguration"),
    (37122, "CompressedBitsPerPixel"),
    (37377, "ShutterSpeedValue"),
    (37378, "ApertureValue"),
    (37379, "BrightnessValue"),
    (37380, "ExposureBiasValue"),
    (37381, "MaxApertureValue"),
    (37382, "SubjectDistance"),
    (37383, "MeteringMode"),
    (37384, "LightSource"),
    (37385, "Flash"),
    (37386, "FocalLength"),
    (37396, "SubjectArea"),
    (37500, "MakerNote"),
    (37510, "UserComment"),
    (37511, "LearningOptOutIn"),
    (37520, "SubSecTime"),
    (37521, "SubSecTimeOriginal"),
    (37522, "SubSecTimeDigitized"),
    (37888, "Temperature"),
    (37889, "Humidity"),
    (37890, "Pressure"),
    (37891, "WaterDepth"),
    (37892, "Acceleration"),
    (37893, "CameraElevationAngle"),
    (40960, "FlashpixVersion"),
    (40961, "ColorSpace"),
    (40962, "PixelXDimension"),
    (40963, "PixelYDimension"),
    (40964, "RelatedSoundFile"),
    (40965, "InteroperabilityIFDPointer"),
    (41483, "FlashEnergy"),
    (41484, "SpatialFrequencyResponse"),
    (41486, "FocalPlaneXResolution"),
    (41487, "FocalPlaneYResolution"),
    (41488, "FocalPlaneResolutionUnit"),
    (41492, "SubjectLocation"),
    (41493, "ExposureIndex"),
    (41495, "SensingMethod"),
    (41728, "FileSource"),
    (41729, "SceneType"),
    (41730, "CFAPattern"),
    (41985, "CustomRendered"),
    (41986, "ExposureMode"),
    (41987, "WhiteBalance"),
    (41988, "DigitalZoomRatio"),
    (41989, "FocalLengthIn35mmFilm"),
    (41990, "SceneCaptureType"),
    (41991, "GainControl"),
    (41992, "Contrast"),
    (41993, "Saturation"),
    (41994, "Sharpness"),
    (41995, "DeviceSettingDescription"),
    (41996, "SubjectDistanceRange"),
    (41997, "DevelopmentType"),
    (41998, "DevelopmentTypeDescription"),
    (41999, "DistortionCorrection"),
    (42000, "ChromaticAberrationCorrection"),
    (42001, "ShadingCorrection"),
    (42002, "NoiseReduction"),
    (42016, "ImageUniqueID"),
    (42032, "CameraOwnerName"),
    (42033, "BodySerialNumber"),
    (42034, "LensSpecification"),
    (42035, "LensMake"),
    (42036, "LensModel"),
    (42037, "LensSerialNumber"),
    (42038, "ImageTitle"),
    (42039, "Photographer"),
    (42040, "ImageEditor"),
    (42041, "CameraFirmware"),
    (42042, "RAWDevelopingSoftware"),
    (42043, "ImageEditingSoftware"),
    (42044, "MetadataEditingSoftware"),
    (42080, "CompositeImage"),
    (42081, "SourceImageNumberOfCompositeImage"),
    (42082, "SourceExposureTimesOfCompositeImage"),
    (42240, "Gamma"),
];

/// GPS Info IFD fields, Exif 3.1 Table 14.
const GPS_TAGS: &[(u16, &str)] = &[
    (0, "GPSVersionID"),
    (1, "GPSLatitudeRef"),
    (2, "GPSLatitude"),
    (3, "GPSLongitudeRef"),
    (4, "GPSLongitude"),
    (5, "GPSAltitudeRef"),
    (6, "GPSAltitude"),
    (7, "GPSTimeStamp"),
    (8, "GPSSatellites"),
    (9, "GPSStatus"),
    (10, "GPSMeasureMode"),
    (11, "GPSDOP"),
    (12, "GPSSpeedRef"),
    (13, "GPSSpeed"),
    (14, "GPSTrackRef"),
    (15, "GPSTrack"),
    (16, "GPSImgDirectionRef"),
    (17, "GPSImgDirection"),
    (18, "GPSMapDatum"),
    (19, "GPSDestLatitudeRef"),
    (20, "GPSDestLatitude"),
    (21, "GPSDestLongitudeRef"),
    (22, "GPSDestLongitude"),
    (23, "GPSDestBearingRef"),
    (24, "GPSDestBearing"),
    (25, "GPSDestDistanceRef"),
    (26, "GPSDestDistance"),
    (27, "GPSProcessingMethod"),
    (28, "GPSAreaInformation"),
    (29, "GPSDateStamp"),
    (30, "GPSDifferential"),
    (31, "GPSHPositioningError"),
];

/// Interoperability IFD fields, Exif 3.1 §4.6.8.
const INTEROP_TAGS: &[(u16, &str)] = &[(1, "InteroperabilityIndex")];

/// A position from the GPS Info IFD.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct GpsPosition {
    /// Latitude in decimal degrees, north positive (`GPSLatitude`
    /// degrees / minutes / seconds, negated for `GPSLatitudeRef = "S"`).
    pub latitude: f64,
    /// Longitude in decimal degrees, east positive (`GPSLongitude`,
    /// negated for `GPSLongitudeRef = "W"`).
    pub longitude: f64,
    /// Altitude in metres relative to the reference `GPSAltitudeRef`
    /// names, negative below it (`GPSAltitudeRef` 1 and 3 mean "below
    /// the reference", §4.6.7.1.6), when `GPSAltitude` is present.
    pub altitude: Option<f64>,
}

/// The Exif-family directories of an image, typed.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ExifData {
    /// Exif IFD fields, ascending tag order.
    pub exif: Vec<TagEntry>,
    /// GPS Info IFD fields, ascending tag order.
    pub gps: Vec<TagEntry>,
    /// Interoperability IFD fields, ascending tag order.
    pub interoperability: Vec<TagEntry>,
}

fn typed(group: TagGroup, entries: &[Entry], bo: ByteOrder) -> Vec<TagEntry> {
    let mut out: Vec<TagEntry> = entries
        .iter()
        .filter_map(|e| {
            let count = usize::try_from(e.count).ok()?;
            let unit = if e.field_type == TYPE_UTF8 {
                1
            } else {
                type_size(e.field_type) as usize
            };
            let len = count.checked_mul(unit)?;
            let raw = e.data.get(..len)?;
            // Entry bytes are in file order; decode from little-endian.
            let le: Vec<u8> = if bo == ByteOrder::Little || unit == 1 {
                raw.to_vec()
            } else {
                let step = match e.field_type {
                    TYPE_RATIONAL | TYPE_SRATIONAL => 4,
                    _ => unit,
                };
                raw.chunks_exact(step)
                    .flat_map(|c| c.iter().rev().copied())
                    .collect()
            };
            Some(TagEntry {
                tag: e.tag,
                name: tag_name(group, e.tag),
                value: TagValue::decode(e.field_type, count, &le)?,
            })
        })
        .collect();
    out.sort_by_key(|e| e.tag);
    out.dedup_by_key(|e| e.tag);
    out
}

impl ExifData {
    /// Parse a TIFF-structured Exif block — the bytes
    /// [`crate::Metadata::exif`] holds: a TIFF header whose first
    /// directory points at the Exif IFD (tag 34665) and / or the GPS
    /// Info IFD (tag 34853). A leading `Exif\0\0` identifier (the APP1
    /// form) is skipped.
    pub fn parse(blob: &[u8]) -> Result<Self> {
        let blob = blob.strip_prefix(b"Exif\0\0").unwrap_or(blob);
        let header = parse_header(blob)
            .map_err(|e| Error::invalid(format!("Exif: not a TIFF-structured block ({e})")))?;
        let (ifd0, _) = parse_ifd(
            blob,
            header.byte_order,
            header.variant,
            header.first_ifd_offset,
        )?;
        Ok(Self::from_directory(blob, &header, &ifd0))
    }

    /// Read the child directories a page's IFD points at.
    pub(crate) fn from_directory(
        input: &[u8],
        header: &crate::ifd::ParsedHeader,
        entries: &[Entry],
    ) -> Self {
        let bo = header.byte_order;
        let child = |from: &[Entry], tag: u16| -> Option<Vec<Entry>> {
            let off = find(from, tag)?.as_u64_vec(bo).ok()?.first().copied()?;
            if off == 0 {
                return None;
            }
            parse_ifd(input, bo, header.variant, off).ok().map(|c| c.0)
        };
        let exif_dir = child(entries, TAG_EXIF_IFD);
        let gps_dir = child(entries, TAG_GPS_IFD);
        let interop_dir = exif_dir
            .as_deref()
            .and_then(|e| child(e, TAG_INTEROPERABILITY_IFD));
        let mut exif = typed(TagGroup::Exif, exif_dir.as_deref().unwrap_or(&[]), bo);
        // The pointer itself is structure, not attribute information.
        exif.retain(|e| e.tag != TAG_INTEROPERABILITY_IFD);
        Self {
            exif,
            gps: typed(TagGroup::Gps, gps_dir.as_deref().unwrap_or(&[]), bo),
            interoperability: typed(
                TagGroup::Interoperability,
                interop_dir.as_deref().unwrap_or(&[]),
                bo,
            ),
        }
    }

    /// `true` when no directory holds a field.
    pub fn is_empty(&self) -> bool {
        self.exif.is_empty() && self.gps.is_empty() && self.interoperability.is_empty()
    }

    /// The field `tag` of `group`.
    pub fn get(&self, group: TagGroup, tag: u16) -> Option<&TagValue> {
        let list = match group {
            TagGroup::Exif => &self.exif,
            TagGroup::Gps => &self.gps,
            TagGroup::Interoperability => &self.interoperability,
        };
        list.binary_search_by_key(&tag, |e| e.tag)
            .ok()
            .map(|i| &list[i].value)
    }

    /// `ExposureTime` (33434) in seconds.
    pub fn exposure_time(&self) -> Option<f64> {
        self.get(TagGroup::Exif, 33434)?.as_f64()
    }

    /// `FNumber` (33437).
    pub fn f_number(&self) -> Option<f64> {
        self.get(TagGroup::Exif, 33437)?.as_f64()
    }

    /// `PhotographicSensitivity` (34855) — the ISO speed setting.
    pub fn photographic_sensitivity(&self) -> Option<u32> {
        self.get(TagGroup::Exif, 34855)?.as_u32()
    }

    /// `FocalLength` (37386) in millimetres.
    pub fn focal_length(&self) -> Option<f64> {
        self.get(TagGroup::Exif, 37386)?.as_f64()
    }

    /// `DateTimeOriginal` (36867), `"YYYY:MM:DD HH:MM:SS"`.
    pub fn date_time_original(&self) -> Option<&str> {
        self.get(TagGroup::Exif, 36867)?.as_text()
    }

    /// `LensModel` (42036).
    pub fn lens_model(&self) -> Option<&str> {
        self.get(TagGroup::Exif, 42036)?.as_text()
    }

    /// `ExifVersion` (36864) as text, e.g. `"0300"` (four ASCII bytes
    /// stored as UNDEFINED, §4.6.6.1.1).
    pub fn exif_version(&self) -> Option<String> {
        match self.get(TagGroup::Exif, 36864)? {
            TagValue::Undefined(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }

    /// `PixelXDimension` × `PixelYDimension` (40962 / 40963).
    pub fn pixel_dimensions(&self) -> Option<(u32, u32)> {
        Some((
            self.get(TagGroup::Exif, 40962)?.as_u32()?,
            self.get(TagGroup::Exif, 40963)?.as_u32()?,
        ))
    }

    /// The GPS position: `GPSLatitude` / `GPSLongitude` are three
    /// RATIONALs "degrees, minutes, and seconds" (§4.6.7.1.3), the
    /// `Ref` fields give the hemisphere.
    pub fn gps_position(&self) -> Option<GpsPosition> {
        let dms = |tag: u16| -> Option<f64> {
            let v = self.get(TagGroup::Gps, tag)?.as_f64s()?;
            let part = |i: usize| v.get(i).copied().filter(|x| x.is_finite()).unwrap_or(0.0);
            (!v.is_empty()).then(|| part(0) + part(1) / 60.0 + part(2) / 3600.0)
        };
        let reference = |tag: u16| self.get(TagGroup::Gps, tag).and_then(TagValue::as_text);
        let mut latitude = dms(2)?;
        let mut longitude = dms(4)?;
        if reference(1).is_some_and(|r| r.starts_with('S')) {
            latitude = -latitude;
        }
        if reference(3).is_some_and(|r| r.starts_with('W')) {
            longitude = -longitude;
        }
        let altitude = self
            .get(TagGroup::Gps, 6)
            .and_then(TagValue::as_f64)
            .map(|a| {
                let below = matches!(
                    self.get(TagGroup::Gps, 5).and_then(TagValue::as_u32),
                    Some(1 | 3)
                );
                if below { -a } else { a }
            });
        Some(GpsPosition {
            latitude,
            longitude,
            altitude,
        })
    }
}

// ---------------------------------------------------------------------
// GeoTIFF
// ---------------------------------------------------------------------

/// Tag 33550 `ModelPixelScaleTag` (GeoTIFF 1.1 §7.1).
pub const TAG_MODEL_PIXEL_SCALE: u16 = 33550;
/// Tag 33922 `ModelTiepointTag`.
pub const TAG_MODEL_TIEPOINT: u16 = 33922;
/// Tag 34264 `ModelTransformationTag`.
pub const TAG_MODEL_TRANSFORMATION: u16 = 34264;
/// Tag 34735 `GeoKeyDirectoryTag`.
pub const TAG_GEO_KEY_DIRECTORY: u16 = 34735;
/// Tag 34736 `GeoDoubleParamsTag`.
pub const TAG_GEO_DOUBLE_PARAMS: u16 = 34736;
/// Tag 34737 `GeoAsciiParamsTag`.
pub const TAG_GEO_ASCII_PARAMS: u16 = 34737;

/// The value of one GeoKey.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GeoKeyValue {
    /// SHORT value(s): one stored in the key entry itself (TIFF tag
    /// location 0), or several read from the directory tag.
    Short(Vec<u16>),
    /// DOUBLE value(s) from `GeoDoubleParamsTag`.
    Double(Vec<f64>),
    /// ASCII value from `GeoAsciiParamsTag`, without the `|`
    /// terminator.
    Ascii(String),
}

/// One GeoKey of the directory.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct GeoKey {
    /// Key identifier.
    pub id: u16,
    /// GeoTIFF 1.1 name of the key (Annex E; `None` for an unlisted
    /// id).
    pub name: Option<&'static str>,
    /// Resolved value.
    pub value: GeoKeyValue,
}

/// GeoTIFF georeferencing of a page (OGC 19-008r4 §7).
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct GeoTiff {
    /// `(KeyDirectoryVersion, KeyRevision, MinorRevision)` — `(1, 1,
    /// 0)` for GeoTIFF 1.0, `(1, 1, 1)` for 1.1.
    pub version: (u16, u16, u16),
    /// The GeoKeys in directory order.
    pub keys: Vec<GeoKey>,
    /// `ModelPixelScaleTag`: `(ScaleX, ScaleY, ScaleZ)`.
    pub pixel_scale: Option<[f64; 3]>,
    /// `ModelTiepointTag`: `(I, J, K, X, Y, Z)` raster ↔ model point
    /// pairs.
    pub tiepoints: Vec<[f64; 6]>,
    /// `ModelTransformationTag`: the 4 × 4 raster → model matrix,
    /// row-major.
    pub transformation: Option<[f64; 16]>,
}

/// GeoTIFF 1.1 key name (Annex E, "This document key name" column;
/// the 1.0 name where the document keeps it).
pub fn geo_key_name(id: u16) -> Option<&'static str> {
    GEO_KEYS
        .binary_search_by_key(&id, |&(k, _)| k)
        .ok()
        .map(|i| GEO_KEYS[i].1)
}

const GEO_KEYS: &[(u16, &str)] = &[
    (1024, "GTModelTypeGeoKey"),
    (1025, "GTRasterTypeGeoKey"),
    (1026, "GTCitationGeoKey"),
    (2048, "GeodeticCRSGeoKey"),
    (2049, "GeodeticCitationGeoKey"),
    (2050, "GeodeticDatumGeoKey"),
    (2051, "PrimeMeridianGeoKey"),
    (2052, "GeogLinearUnitsGeoKey"),
    (2053, "GeogLinearUnitSizeGeoKey"),
    (2054, "GeogAngularUnitsGeoKey"),
    (2055, "GeogAngularUnitSizeGeoKey"),
    (2056, "EllipsoidGeoKey"),
    (2057, "EllipsoidSemiMajorAxisGeoKey"),
    (2058, "EllipsoidSemiMinorAxisGeoKey"),
    (2059, "EllipsoidInvFlatteningGeoKey"),
    (2060, "GeogAzimuthUnitsGeoKey"),
    (2061, "PrimeMeridianLongitudeGeoKey"),
    (3072, "ProjectedCRSGeoKey"),
    (3073, "ProjectedCitationGeoKey"),
    (3074, "ProjectionGeoKey"),
    (3075, "ProjMethodGeoKey"),
    (3076, "ProjLinearUnitsGeoKey"),
    (3077, "ProjLinearUnitSizeGeoKey"),
    (3078, "ProjStdParallel1GeoKey"),
    (3079, "ProjStdParallel2GeoKey"),
    (3080, "ProjNatOriginLongGeoKey"),
    (3081, "ProjNatOriginLatGeoKey"),
    (3082, "ProjFalseEastingGeoKey"),
    (3083, "ProjFalseNorthingGeoKey"),
    (3084, "ProjFalseOriginLongGeoKey"),
    (3085, "ProjFalseOriginLatGeoKey"),
    (3086, "ProjFalseOriginEastingGeoKey"),
    (3087, "ProjFalseOriginNorthingGeoKey"),
    (3088, "ProjCenterLongGeoKey"),
    (3089, "ProjCenterLatGeoKey"),
    (3090, "ProjCenterEastingGeoKey"),
    (3091, "ProjCenterNorthingGeoKey"),
    (3092, "ProjScaleAtNatOriginGeoKey"),
    (3093, "ProjScaleAtCenterGeoKey"),
    (3094, "ProjAzimuthAngleGeoKey"),
    (3095, "ProjStraightVertPoleLongGeoKey"),
    (4096, "VerticalGeoKey"),
    (4097, "VerticalCitationGeoKey"),
    (4098, "VerticalDatumGeoKey"),
    (4099, "VerticalUnitsGeoKey"),
];

impl GeoTiff {
    /// Read the GeoTIFF tags of a directory. `None` when the page has
    /// neither a GeoKey directory nor a raster → model tag; a
    /// directory that breaks §7.1.2 (wrong header, a key pointing past
    /// its value tag) yields the keys that do resolve.
    pub(crate) fn from_directory(entries: &[Entry], bo: ByteOrder) -> Option<Self> {
        let doubles = |tag: u16| -> Option<Vec<f64>> {
            let e = find(entries, tag)?;
            (e.field_type == TYPE_DOUBLE).then(|| e.as_f64_vec(bo).ok())?
        };
        let mut geo = GeoTiff {
            pixel_scale: doubles(TAG_MODEL_PIXEL_SCALE)
                .and_then(|v| <[f64; 3]>::try_from(v.as_slice()).ok()),
            tiepoints: doubles(TAG_MODEL_TIEPOINT)
                .map(|v| {
                    v.as_chunks::<6>()
                        .0
                        .iter()
                        .map(|c| [c[0], c[1], c[2], c[3], c[4], c[5]])
                        .collect()
                })
                .unwrap_or_default(),
            transformation: doubles(TAG_MODEL_TRANSFORMATION)
                .and_then(|v| <[f64; 16]>::try_from(v.as_slice()).ok()),
            ..GeoTiff::default()
        };
        let dir: Option<Vec<u16>> = find(entries, TAG_GEO_KEY_DIRECTORY)
            .filter(|e| e.field_type == TYPE_SHORT)
            .and_then(|e| e.as_u32_vec(bo).ok())
            .map(|v| v.into_iter().map(|x| x as u16).collect());
        if let Some(dir) = dir.as_deref().filter(|d| d.len() >= 4) {
            geo.version = (dir[0], dir[1], dir[2]);
            let double_params = doubles(TAG_GEO_DOUBLE_PARAMS).unwrap_or_default();
            let ascii_params: Vec<u8> = find(entries, TAG_GEO_ASCII_PARAMS)
                .filter(|e| e.field_type == TYPE_ASCII)
                .map(|e| e.data[..(e.count as usize).min(e.data.len())].to_vec())
                .unwrap_or_default();
            // "NumberOfKeys KeyEntry Sets in addition to the header",
            // each "4 unsigned short integer values": KeyID,
            // TIFFTagLocation, Count, ValueOffset.
            for key in dir[4..].as_chunks::<4>().0.iter().take(dir[3] as usize) {
                let (id, location, count, offset) =
                    (key[0], key[1], key[2] as usize, key[3] as usize);
                let value = match location {
                    // "either the key value (if TIFF Tag location = 0)"
                    0 => Some(GeoKeyValue::Short(vec![key[3]])),
                    TAG_GEO_KEY_DIRECTORY => dir
                        .get(offset..offset + count)
                        .map(|v| GeoKeyValue::Short(v.to_vec())),
                    TAG_GEO_DOUBLE_PARAMS => double_params
                        .get(offset..offset + count)
                        .map(|v| GeoKeyValue::Double(v.to_vec())),
                    TAG_GEO_ASCII_PARAMS => ascii_params.get(offset..offset + count).map(|b| {
                        // "The pipe character | … SHALL be used as the
                        // character to terminate a string".
                        let s = String::from_utf8_lossy(b);
                        GeoKeyValue::Ascii(
                            s.trim_end_matches('\0').trim_end_matches('|').to_string(),
                        )
                    }),
                    _ => None,
                };
                if let Some(value) = value {
                    geo.keys.push(GeoKey {
                        id,
                        name: geo_key_name(id),
                        value,
                    });
                }
            }
        }
        let present = dir.is_some()
            || geo.pixel_scale.is_some()
            || !geo.tiepoints.is_empty()
            || geo.transformation.is_some();
        present.then_some(geo)
    }

    /// The value of key `id`.
    pub fn key(&self, id: u16) -> Option<&GeoKeyValue> {
        self.keys.iter().find(|k| k.id == id).map(|k| &k.value)
    }

    fn short_key(&self, id: u16) -> Option<u16> {
        match self.key(id)? {
            GeoKeyValue::Short(v) => v.first().copied(),
            _ => None,
        }
    }

    /// `GTModelTypeGeoKey` (1024): 1 = projected, 2 = geographic,
    /// 3 = geocentric (§7.2).
    pub fn model_type(&self) -> Option<u16> {
        self.short_key(1024)
    }

    /// `GTRasterTypeGeoKey` (1025): 1 = PixelIsArea, 2 = PixelIsPoint.
    pub fn raster_type(&self) -> Option<u16> {
        self.short_key(1025)
    }

    /// `ProjectedCRSGeoKey` (3072): the projected CRS code.
    pub fn projected_crs(&self) -> Option<u16> {
        self.short_key(3072)
    }

    /// `GeodeticCRSGeoKey` (2048): the geographic / geocentric CRS
    /// code.
    pub fn geodetic_crs(&self) -> Option<u16> {
        self.short_key(2048)
    }

    /// Model coordinates of raster position `(i, j)`: the
    /// `ModelTransformationTag` matrix when present (row-major 4 × 4
    /// applied to `(i, j, 0, 1)`), else the first tiepoint plus the
    /// pixel scale — `X = Tx + (i − Ti)·ScaleX`,
    /// `Y = Ty − (j − Tj)·ScaleY` (Requirement 10.4: positive `ScaleX`
    /// means X grows with I, positive `ScaleY` means Y shrinks as J
    /// grows). `None` when the page has neither.
    pub fn raster_to_model(&self, i: f64, j: f64) -> Option<(f64, f64)> {
        if let Some(m) = &self.transformation {
            return Some((m[0] * i + m[1] * j + m[3], m[4] * i + m[5] * j + m[7]));
        }
        let (scale, tie) = (self.pixel_scale?, self.tiepoints.first()?);
        Some((
            tie[3] + (i - tie[0]) * scale[0],
            tie[4] - (j - tie[1]) * scale[1],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogues_are_sorted_for_binary_search() {
        for table in [EXIF_TAGS, GPS_TAGS, INTEROP_TAGS, GEO_KEYS] {
            assert!(table.windows(2).all(|w| w[0].0 < w[1].0));
        }
        assert_eq!(tag_name(TagGroup::Exif, 33434), Some("ExposureTime"));
        assert_eq!(tag_name(TagGroup::Gps, 31), Some("GPSHPositioningError"));
        assert_eq!(tag_name(TagGroup::Gps, 33434), None);
        assert_eq!(geo_key_name(3072), Some("ProjectedCRSGeoKey"));
    }

    #[test]
    fn rational_decoding_and_zero_denominator() {
        let mut d = Vec::new();
        for v in [1u32, 125, 5, 0] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let v = TagValue::decode(TYPE_RATIONAL, 2, &d).unwrap();
        assert_eq!(v, TagValue::Rational(vec![(1, 125), (5, 0)]));
        let f = v.as_f64s().unwrap();
        assert_eq!(f[0], 0.008);
        assert!(f[1].is_nan());
        assert!(TagValue::decode(TYPE_RATIONAL, 3, &d).is_none());
    }
}
