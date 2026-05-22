# oxideav-tiff

Pure-Rust TIFF 6.0 image decoder + encoder + container for the
[`oxideav`](https://github.com/OxideAV/oxideav) framework.

Implements the *Aldus TIFF Revision 6.0 (June 1992)* baseline plus the
universally-deployed Part 2 extensions (LZW + Deflate, tiles, YCbCr /
CMYK photometrics, CCITT Modified Huffman + T.4 1-D), the multi-IFD
chain (multi-page), and the Adobe Pagemaker 6.0 *BigTIFF* design
(8-byte offsets, magic 43). Spec-only clean-room: no external library
source was consulted.

## Decode

| Photometric    | Bit depth      | Compression                        | Output       |
| -------------- | -------------- | ---------------------------------- | ------------ |
| WhiteIsZero    | 1              | None / CCITT-MH / T.4-1D / PackBits / LZW / Deflate    | `Gray8`      |
| WhiteIsZero    | 4 / 8          | None / PackBits / LZW / Deflate    | `Gray8`      |
| WhiteIsZero    | 16             | None / PackBits / LZW / Deflate    | `Gray16Le`   |
| BlackIsZero    | 1              | None / CCITT-MH / T.4-1D / PackBits / LZW / Deflate    | `Gray8`      |
| BlackIsZero    | 4 / 8 / 16     | None / PackBits / LZW / Deflate    | `Gray8` / `Gray16Le` |
| Palette        | 4 / 8          | None / PackBits / LZW / Deflate    | `Rgb24`      |
| RGB (3 chan)   | 8              | None / PackBits / LZW / Deflate    | `Rgb24`      |
| RGB (3 chan)   | 16             | None / PackBits / LZW / Deflate    | `Rgb48Le`    |
| CMYK (4 chan)  | 8              | None / PackBits / LZW / Deflate    | `Rgb24`      |
| YCbCr (3 chan) | 8              | None / PackBits / LZW / Deflate / **JPEG-in-TIFF** (Compression=7) | `Rgb24`      |
| RGB (3 chan)   | 8              | **JPEG-in-TIFF** (Compression=7)   | `Rgb24`      |
| BlackIsZero / WhiteIsZero | 8   | **JPEG-in-TIFF** (Compression=7)   | `Gray8`      |

`Predictor = 1` (no prediction) and `Predictor = 2` (horizontal
differencing, per-component for `SamplesPerPixel > 1`) are both
supported. Strip- and tile-based layouts both decode (any number of
strips or tiles). Multi-page files walk the next-IFD chain via
[`decode_tiff_all`]. Both `II` (little-endian) and `MM` (big-endian)
byte orders are accepted, and both classic 32-bit-offset TIFF and
BigTIFF (8-byte offsets, magic 43) parse.

### JPEG-in-TIFF (Compression = 7)

Per [TIFF Technical Note 2 (DRAFT 17-Mar-95)](docs/image/tiff/technote2-jpeg-in-tiff.html),
`Compression = 7` is "new-style JPEG": each strip or tile is itself a
complete ISO JPEG datastream (SOI..EOI), and the optional `JPEGTables`
field (tag 347) carries a JPEG "abbreviated table specification"
stream whose `DQT` / `DHT` / `DRI` markers apply by reference to
every segment. The decoder merges `JPEGTables` (body between SOI and
EOI) in front of each segment's body and feeds the assembled
freestanding JPEG to [`oxideav-mjpeg`](https://github.com/OxideAV/oxideav-mjpeg)
through its public `Decoder` trait surface.

Supported photometric / sampling combinations:

* `PhotometricInterpretation = 1` (BlackIsZero) or `0` (WhiteIsZero)
  with `SamplesPerPixel = 1`: 1-plane Gray8 JPEG, decoded to `Gray8`.
  `WhiteIsZero` applies a 255-x polarity inversion to match the rest
  of the bilevel/grayscale render paths.
* `PhotometricInterpretation = 2` (RGB) with `SamplesPerPixel = 3`:
  3-plane full-resolution JPEG (no chroma subsampling), decoded to
  `Rgb24`. This is the layout ImageMagick's default
  `convert -compress jpeg` produces from PPM input.
* `PhotometricInterpretation = 6` (YCbCr) with `SamplesPerPixel = 3`:
  3-plane planar YCbCr JPEG with `YCbCrSubSampling` reflected in the
  JPEG sampling factors. 4:4:4 / 4:2:2 / 4:2:0 / 4:1:1 are all
  composited to `Rgb24` via the BT.601 matrix matching TN2's default
  `ReferenceBlackWhite = [0,255,128,255,128,255]`. This is the layout
  `tiffcp -c jpeg` produces.

Out of scope in this round (return precise `Error::Unsupported`):
12-bit (SOF1 with `P = 12`), arithmetic (SOF9 / SOF11), CMYK
(`PhotometricInterpretation = 5`), `PlanarConfiguration = 2`, and the
deprecated TIFF 6.0 §22 "old-style" JPEG (`Compression = 6`).
JPEG-in-TIFF requires the default-on `registry` Cargo feature; with
`default-features = false` the JPEG path returns
`Error::Unsupported`.

`FillOrder = 1` (MSB-first, the baseline default) and `FillOrder = 2`
(LSB-first) are both accepted for the bit orderings the spec admits:
FillOrder=2 is honoured for uncompressed and CCITT-compressed
(`Compression = 1 / 2 / 3`) `BitsPerSample = 1` strips and tiles, per
TIFF 6.0 §FillOrder (page 32). Any other combination of FillOrder=2
with non-bilevel data or with a non-CCITT compressor is rejected with
a precise error.

## Encode

| Photometric    | Bit depth | Compression                                                 | API call                |
| -------------- | --------- | ----------------------------------------------------------- | ----------------------- |
| WhiteIsZero    | 1         | None / CCITT-MH / T.4-1D                                    | `EncodePixelFormat::Bilevel`   |
| BlackIsZero    | 8 / 16    | None / PackBits / LZW / Deflate                             | `EncodePixelFormat::Gray8` / `::Gray16Le` |
| RGB            | 8         | None / PackBits / LZW / Deflate                             | `EncodePixelFormat::Rgb24`     |
| Palette        | 8         | None / PackBits / LZW / Deflate                             | `EncodePixelFormat::Palette8`  |

`TiffCompression::CcittRle` selects Modified Huffman
(`Compression = 2`, TIFF 6.0 §10) and `TiffCompression::CcittT4OneD`
selects T.4 1-D (`Compression = 3`, §11) with an optional
`eol_byte_aligned` flag that writes `T4Options` bit 2. Both
encoders use the same `WHITE` / `BLACK` run-length tables we
transcribed from the TIFF 6.0 PDF for decode. The CCITT writer
rejects non-bilevel inputs with a precise error.

Output is classic II little-endian TIFF, single-IFD via
[`encode_tiff`] or multi-page via [`encode_tiff_multi`]. Files
roundtrip through ImageMagick / `tiffinfo` / `tiffcp`; CCITT
outputs additionally validate by asking `tiffcp -c none` to
transcode our `Compression = 3` stream back to uncompressed and
checking the resulting pixels match the original input.

## Backlog (not yet implemented)

- BigTIFF write, tile write, predictor on encode
- CCITT T.4 2-D coding (`Compression = 3` with `T4Options` bit 0
  set) and T.6 / Group 4 (`Compression = 4`). The 2-D Pass /
  Horizontal / Vertical mode codes are not in the TIFF 6.0 PDF —
  it defers to CCITT Rec. T.4 / T.6 — so implementing these
  requires a clean-room transcription added to
  `docs/image/tiff/` first.
- JPEG-in-TIFF Compression = 6 (old-style, deprecated by TIFF Tech
  Note 2; decoder returns precise `Error::Unsupported`).
  Compression = 7 (new-style) **decodes** as of this round; 12-bit
  precision (SOF1 with `P = 12`), arithmetic coding (SOF9 / SOF11),
  CMYK JPEG, and `PlanarConfiguration = 2` JPEG remain unsupported.
- CIELab / Transparency-mask photometric interpretations
- DNG / GeoTIFF / EXIF blob extraction
- Planar (separate-plane) layout

## Registration

```rust
let mut codecs = oxideav_core::CodecRegistry::new();
let mut containers = oxideav_core::ContainerRegistry::new();
oxideav_tiff::register(&mut codecs, &mut containers);
```
