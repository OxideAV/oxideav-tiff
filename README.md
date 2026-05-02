# oxideav-tiff

Pure-Rust TIFF 6.0 image decoder + container for the
[`oxideav`](https://github.com/OxideAV/oxideav) framework.

Implements the *Aldus TIFF Revision 6.0 (June 1992)* baseline plus the
two universally-deployed Part 2 extensions (LZW + Deflate). Spec-only
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

`Predictor = 1` (no prediction) and `Predictor = 2` (horizontal
differencing, per-component for `SamplesPerPixel > 1`) are both
supported. Multi-strip images walk `StripOffsets` / `StripByteCounts`
in order. Both `II` (little-endian) and `MM` (big-endian) byte orders
are accepted.

## Round-2 backlog (not yet implemented)

- BigTIFF (magic 43, 64-bit offsets)
- Tiles (`TileWidth` / `TileLength` / `TileOffsets` / `TileByteCounts`)
- CCITT Group 3 / 4 fax compression (Compression = 2 / 3 / 4)
- JPEG-in-TIFF (Compression = 6 old-style, 7 new-style)
- YCbCr / CIELab / CMYK / Transparency-mask photometric
  interpretations
- Multi-page (chasing the `next-IFD` pointer beyond the first IFD)
- DNG / GeoTIFF / EXIF blob extraction
- Encoder

## Registration

```rust
let mut codecs = oxideav_core::CodecRegistry::new();
let mut containers = oxideav_core::ContainerRegistry::new();
oxideav_tiff::register(&mut codecs, &mut containers);
```
