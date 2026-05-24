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
| CMYK (4 chan)  | 8              | **JPEG-in-TIFF** (Compression=7)   | `Rgb24`      |

`Predictor = 1` (no prediction) and `Predictor = 2` (horizontal
differencing, per-component for `SamplesPerPixel > 1`) are both
supported. Strip- and tile-based layouts both decode (any number of
strips or tiles). `PlanarConfiguration = 1` (chunky) and
`PlanarConfiguration = 2` (separate component planes) are both
accepted: per TIFF 6.0 §"PlanarConfiguration", the second arrangement
stores `StripOffsets` / `StripByteCounts` (and `TileOffsets` /
`TileByteCounts`) as `SamplesPerPixel × StripsPerImage` entries with
component 0 first, then component 1, etc.; the decoder re-interleaves
the planes into chunky order downstream so every photometric path
(RGB / CMYK / YCbCr) sees the same input shape. JPEG-in-TIFF
(`Compression = 7`) remains chunky-only — TN2's planar-2 rules are
out of scope for this round. Multi-page files walk the next-IFD chain
via [`decode_tiff_all`]. Both `II` (little-endian) and `MM`
(big-endian) byte orders are accepted, and both classic
32-bit-offset TIFF and BigTIFF (8-byte offsets, magic 43) parse.

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
* `PhotometricInterpretation = 5` (CMYK) with `SamplesPerPixel = 4`:
  4-component JPEG datastream — `oxideav-mjpeg` reads any optional
  Adobe APP14 marker inside the segment to select the per-sample
  inversion (plain CMYK / Adobe-inverted CMYK / YCCK) and hands back
  packed `C M Y K` bytes (`0 = no ink`). The TIFF compositor then
  converts to `Rgb24` using the same additive-RGB formula the
  uncompressed CMYK path uses (`R = (255 − C) × (255 − K) / 255`,
  etc., TIFF 6.0 §16 `InkSet = 1`). This is the layout
  `convert -colorspace CMYK -compress jpeg` produces.

Out of scope in this round (return precise `Error::Unsupported`):
12-bit (SOF1 with `P = 12`), arithmetic (SOF9 / SOF11),
`PlanarConfiguration = 2` (`Compression = 7` only; the non-JPEG
compressors do accept planar layout — see above), and the
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

The horizontal-differencing predictor (`Predictor = 2`, TIFF 6.0 §14)
is available on encode via the `EncodePage::predictor` flag. When set,
the encoder writes first differences (per-component, offset
`SamplesPerPixel` for chunky multi-sample data) plus the `Predictor`
tag (317) so the decoder reverses the step — the exact inverse of the
decoder's cumulative add. It applies to the lossless byte-aligned
formats whose decode path already supports it (`Gray8`, `Gray16Le`,
`Rgb24`, `Palette8`); combining it with the bilevel CCITT schemes or
with `Bilevel` input is rejected. `tiffinfo` reports
`Predictor: horizontal differencing 2 (0x2)` on the output and
`tiffcp -c none` transcodes our `Predictor = 2` streams back to
uncompressed TIFFs that re-decode to the original pixels.

`PlanarConfiguration = 2` (separate component planes, TIFF 6.0
§"PlanarConfiguration") is available on encode via the
`EncodePage::planar` flag. When set on an `Rgb24` page the encoder
de-interleaves the chunky `RGBRGB…` data into one full-resolution
strip per component plane and writes `StripOffsets` / `StripByteCounts`
as `SamplesPerPixel`-entry arrays in plane order — the spec's
"SamplesPerPixel rows and StripsPerImage columns" layout with
`StripsPerImage = 1`. It composes with `Predictor = 2`: §14 says
differencing on a planar image "works the same as it does for
grayscale data," so each plane is differenced independently with an
offset of one sample. The flag is rejected on the single-sample
formats (`Gray8` / `Gray16Le` / `Palette8` / `Bilevel`), where the
spec says the field is irrelevant. Works under None / PackBits / LZW /
Deflate. `tiffcp -c none` transcodes our planar output back to an
uncompressed TIFF that re-decodes to the original pixels, and
ImageMagick reads it bit-exactly.

Tiled layout (`TileWidth` / `TileLength` / `TileOffsets` /
`TileByteCounts`, TIFF 6.0 §15) is available on encode via the
`EncodePage::tiling` flag (`Some((tile_width, tile_height))`). When set,
the encoder splits the image into a row-major grid of fixed-size tiles,
compresses each independently, and writes the tile fields in place of
the strip fields (§15: the tile fields "replace the StripOffsets,
StripByteCounts, and RowsPerStrip fields"; "Do not use both
strip-oriented and tile-oriented fields in the same TIFF file"). Both
tile dimensions must be a non-zero multiple of 16 per §15's `TileWidth` /
`TileLength` requirement. Boundary tiles are padded out to the tile
geometry by replicating the last visible column / row (§15 "Padding"), so
every tile is the same size before compression; the decoder displays only
the `ImageWidth × ImageLength` region and ignores the padding. Works for
the byte-aligned chunky formats (`Gray8` / `Gray16Le` / `Rgb24` /
`Palette8`) under None / PackBits / LZW / Deflate, with or without
`Predictor = 2` (applied per-tile). Tiling is rejected on `Bilevel`
input and the CCITT compressors. `tiffcp -c
none` transcodes our tiled output back to an uncompressed TIFF that
re-decodes to the original pixels, and ImageMagick reads it bit-exactly.

Tiling composes with `planar = true` on `Rgb24`: the encoder writes one
row-major tile grid per component plane, emitting plane 0's tiles first
then plane 1's, then plane 2's, per §15 `TileOffsets` ("For
PlanarConfiguration = 2, the offsets for the first component plane are
stored first, followed by all the offsets for the second component
plane, and so on"). `TileOffsets` / `TileByteCounts` therefore carry
`SamplesPerPixel × TilesPerImage` entries. Each plane is a
single-component image, so §15 boundary padding and the §14 predictor
run with an offset of one sample (§14: "If PlanarConfiguration is 2 …
Differencing works the same as it does for grayscale data"). `tiffcp -c
none` and ImageMagick both transcode/read the planar-tiled output back
to the original chunky pixels bit-exactly.

On the read side, the decoder walks the same §15 tile fields, decodes
each tile, reverses any per-tile `Predictor = 2` differencing, and
reassembles the grid into the full image — dropping the boundary padding
on right-column / bottom-row tiles. A binary-independent self-roundtrip
suite encodes the identical pixels both tiled and strip-based, decodes
both, and asserts the planes are byte-identical, so tile geometry,
ordering, per-tile predictor reversal, and §15 edge padding are checked
against the strip decode of the same image across Gray8 / Gray16Le /
Rgb24 / Palette8 (None / PackBits / LZW / Deflate, with and without the
predictor) and the full range of edge-tile geometries (exact-fit,
partial edges, non-square tiles, oversized single tile, 1-pixel
overhang). Sub-byte (1-/4-bit) tiled images are not yet supported.

Output is classic II little-endian TIFF, single-IFD via
[`encode_tiff`] or multi-page via [`encode_tiff_multi`]. Files
roundtrip through ImageMagick / `tiffinfo` / `tiffcp`; CCITT
outputs additionally validate by asking `tiffcp -c none` to
transcode our `Compression = 3` stream back to uncompressed and
checking the resulting pixels match the original input.

## Backlog (not yet implemented)

- BigTIFF write
- CCITT T.4 2-D coding (`Compression = 3` with `T4Options` bit 0
  set) and T.6 / Group 4 (`Compression = 4`). The 2-D Pass /
  Horizontal / Vertical mode codes are not in the TIFF 6.0 PDF —
  it defers to CCITT Rec. T.4 / T.6 — so implementing these
  requires a clean-room transcription added to
  `docs/image/tiff/` first.
- JPEG-in-TIFF Compression = 6 (old-style, deprecated by TIFF Tech
  Note 2; decoder returns precise `Error::Unsupported`).
  Compression = 7 (new-style) **decodes** as of this round across
  Gray / RGB / YCbCr / CMYK photometrics; 12-bit precision (SOF1 with
  `P = 12`), arithmetic coding (SOF9 / SOF11), and
  `PlanarConfiguration = 2` JPEG remain unsupported.
- CIELab / Transparency-mask photometric interpretations
- DNG / GeoTIFF / EXIF blob extraction
- Encoder-side planar (`PlanarConfiguration = 2`) writing is now
  supported for `Rgb24` under None / PackBits / LZW / Deflate (with or
  without `Predictor = 2`), both strip-based and tiled (one tile grid
  per component plane, §15); decode covers the same plus CCITT. Tiled
  write (chunky, §15) covers `Gray8` / `Gray16Le` / `Rgb24` /
  `Palette8` under None / PackBits / LZW / Deflate with the optional
  `Predictor = 2`. The remaining gap is planar / tiled write for the
  not-yet-encodable photometrics (CMYK / YCbCr)

## Registration

```rust
let mut codecs = oxideav_core::CodecRegistry::new();
let mut containers = oxideav_core::ContainerRegistry::new();
oxideav_tiff::register(&mut codecs, &mut containers);
```
