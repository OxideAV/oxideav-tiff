# oxideav-tiff

Pure-Rust TIFF 6.0 image decoder + encoder + container for the
[`oxideav`](https://github.com/OxideAV/oxideav) framework.

Implements the *Aldus TIFF Revision 6.0 (June 1992)* baseline plus the
universally-deployed Part 2 extensions (LZW + Deflate, tiles, YCbCr /
CMYK photometrics), the multi-IFD chain (multi-page), and the Adobe
Pagemaker 6.0 *BigTIFF* design (8-byte offsets, magic 43). Spec-only
clean-room: no external library source was consulted.

## Decode

| Photometric    | Bit depth      | Compression                        | Output       |
| -------------- | -------------- | ---------------------------------- | ------------ |
| WhiteIsZero    | 1              | None / PackBits / LZW / Deflate    | `Gray8`      |
| WhiteIsZero    | 4 / 8          | None / PackBits / LZW / Deflate    | `Gray8`      |
| WhiteIsZero    | 16             | None / PackBits / LZW / Deflate    | `Gray16Le`   |
| BlackIsZero    | 1 / 4 / 8 / 16 | None / PackBits / LZW / Deflate    | `Gray8` / `Gray16Le` |
| Palette        | 4 / 8          | None / PackBits / LZW / Deflate    | `Rgb24`      |
| RGB (3 chan)   | 8              | None / PackBits / LZW / Deflate    | `Rgb24`      |
| RGB (3 chan)   | 16             | None / PackBits / LZW / Deflate    | `Rgb48Le`    |
| CMYK (4 chan)  | 8              | None / PackBits / LZW / Deflate    | `Rgb24`      |
| YCbCr (3 chan) | 8              | None / PackBits / LZW / Deflate    | `Rgb24`      |

`Predictor = 1` (no prediction) and `Predictor = 2` (horizontal
differencing, per-component for `SamplesPerPixel > 1`) are both
supported. Strip- and tile-based layouts both decode (any number of
strips or tiles). Multi-page files walk the next-IFD chain via
[`decode_tiff_all`]. Both `II` (little-endian) and `MM` (big-endian)
byte orders are accepted, and both classic 32-bit-offset TIFF and
BigTIFF (8-byte offsets, magic 43) parse.

## Encode

| Photometric    | Bit depth | Compression                        | API call                |
| -------------- | --------- | ---------------------------------- | ----------------------- |
| BlackIsZero    | 8 / 16    | None / PackBits / LZW / Deflate    | `EncodePixelFormat::Gray8` / `::Gray16Le` |
| RGB            | 8         | None / PackBits / LZW / Deflate    | `EncodePixelFormat::Rgb24`     |
| Palette        | 8         | None / PackBits / LZW / Deflate    | `EncodePixelFormat::Palette8`  |

Output is classic II little-endian TIFF, single-IFD via
[`encode_tiff`] or multi-page via [`encode_tiff_multi`]. Files
roundtrip through ImageMagick / `tiffinfo` / `tiffcp`.

## Round-3 backlog (not yet implemented)

- BigTIFF write, tile write, predictor on encode
- CCITT Group 3 / 4 fax compression (Compression = 2 / 3 / 4)
- JPEG-in-TIFF (Compression = 6 old-style, 7 new-style)
- CIELab / Transparency-mask photometric interpretations
- DNG / GeoTIFF / EXIF blob extraction
- Planar (separate-plane) layout

## Registration

```rust
let mut codecs = oxideav_core::CodecRegistry::new();
let mut containers = oxideav_core::ContainerRegistry::new();
oxideav_tiff::register(&mut codecs, &mut containers);
```
