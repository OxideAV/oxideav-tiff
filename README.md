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
| **Transparency Mask** | 1       | None / CCITT-MH / T.4-1D / PackBits / LZW / Deflate    | `Gray8` (interior = 0xFF, exterior = 0x00) |
| Palette        | 4 / 8          | None / PackBits / LZW / Deflate    | `Rgb24`      |
| RGB (3 chan)   | 8              | None / PackBits / LZW / Deflate    | `Rgb24`      |
| RGB (3 chan)   | 16             | None / PackBits / LZW / Deflate    | `Rgb48Le`    |
| CMYK (4 chan)  | 8              | None / PackBits / LZW / Deflate    | `Rgb24`      |
| YCbCr (3 chan) | 8              | None / PackBits / LZW / Deflate / **JPEG-in-TIFF** (Compression=7) | `Rgb24`      |
| RGB (3 chan)   | 8              | **JPEG-in-TIFF** (Compression=7)   | `Rgb24`      |
| BlackIsZero / WhiteIsZero | 8   | **JPEG-in-TIFF** (Compression=7)   | `Gray8`      |
| CMYK (4 chan)  | 8              | **JPEG-in-TIFF** (Compression=7)   | `Rgb24`      |
| **CIELab (3 chan)** | 8         | None / PackBits / LZW / Deflate    | `Rgb24` (Lab→XYZ@D65→linear NTSC→sRGB) |
| **CIELab (1 chan, L\* only)** | 8 | None / PackBits / LZW / Deflate  | `Gray8`      |

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

### CIELab (PhotometricInterpretation = 8)

Per TIFF 6.0 §23 "CIE L*a*b* Images" (page 110), `PhotometricInterpretation = 8`
identifies a 1976 CIE L\*a\*b\* image whose three (or one) 8-bit
samples per pixel are: L\* unsigned in 0..255 mapping linearly to the
perceptual lightness scale 0..100; a\* and b\* as two's-complement
signed bytes in -128..127 representing the red/green and yellow/blue
chrominance channels. The decoder colorimetrically converts each
pixel to display Rgb24 via Lab → XYZ under the spec's mandated
"perfect reflecting diffuser at D65" reference white, then XYZ →
linear RGB through the analytic inverse of §23's stated NTSC
tristimulus matrix (page 111), and finally linear RGB → 8-bit through
the standard sRGB OETF so the result is ready for a contemporary
display. §23 explicitly leaves the "Converting between RGB and
CIELAB" linear-to-display step open ("some conversion to RGB will be
required"); the sRGB gamma curve is the universally-applicable
choice and matches the render-ready Rgb24 the CMYK / YCbCr
photometrics already produce.

`SamplesPerPixel = 1` is also accepted — §23 "Usage of other Fields"
on page 110: "3 for L\*a\*b\*, 1 implies L\* only, for monochrome data".
The L\*-only path runs the same lightness-curve inverse as the
3-sample render so a chromatically-neutral (a\* = b\* = 0) CIELab pixel
in either layout produces the same gray level.

Compressors accepted: None / PackBits / LZW / Deflate (the
byte-aligned, photometric-agnostic set the rest of the multi-bit
photometric paths use). CCITT (Compression = 2/3/4) is bilevel-only
per spec and rejected here; the JPEG-in-TIFF dispatch (Compression =
7) does not currently recognise CIELab as one of its render targets —
TN2 doesn't list it as a permitted §6.1.4 photometric for new-style
JPEG. Encode-side CIELab is not yet implemented (no Lab variant on
`EncodePixelFormat`).

### Transparency Mask (PhotometricInterpretation = 4)

Per TIFF 6.0 §"PhotometricInterpretation" value 4 (page 37) and
§"NewSubfileType" bit 2 (page 36), a mask page is a 1-bit-per-pixel
image with `SamplesPerPixel = 1` whose 1-bits define the interior
("visible region") of another image in the same TIFF file and whose
0-bits define the exterior. The decoder routes mask pages through the
same bilevel expander as `BlackIsZero` 1-bit pages but pins the bit
polarity to spec — 1-bit = `0xFF`, 0-bit = `0x00` — irrespective of
`FillOrder` (the FillOrder normalisation runs in the strip walker, as
for every bilevel path), so a downstream compositor can multiply the
result with the main image directly. On the encode side
[`EncodePixelFormat::TransparencyMask`] writes
`PhotometricInterpretation = 4` and sets bit 2 of `NewSubfileType`
(tag 254) — the companion field the spec uses to let multi-page
readers spot a mask IFD without consulting the photometric tag. Mask
pages accept the same compressors a `Bilevel` page does
(None / PackBits / LZW / Deflate / CCITT-MH / CCITT-T.4-1D); the spec
recommends PackBits. Tiled, planar, and Predictor=2 layouts are
rejected for 1-bit input on both encode and decode (sub-byte tile
slicing and §14 component-differencing don't apply to packed bits).

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
| **Transparency Mask** | 1  | None / CCITT-MH / T.4-1D / PackBits / LZW / Deflate         | `EncodePixelFormat::TransparencyMask` (sets PhotometricInterpretation = 4 and NewSubfileType bit 2) |
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

### BigTIFF write

`EncodePage::bigtiff = true` switches the writer from classic TIFF
(8-byte header, magic 42, 32-bit offsets, 12-byte IFD entries) to
BigTIFF (16-byte header, magic 43, offset-bytesize 8 + reserved 0 + 8-byte
first-IFD offset, 20-byte IFD entries with u64 count and u64
value-or-offset, 8-byte next-IFD pointer) per the Adobe Pagemaker 6.0
BigTIFF design that the decoder's `parse_header` / `parse_ifd` already
read. `StripOffsets`, `StripByteCounts`, `TileOffsets`, and
`TileByteCounts` are written as `LONG8` (field type 16, 8 bytes per
value), and the wider 8-byte value/offset slot lets a 3-component
`BitsPerSample` array stay inline that classic TIFF would have spilled
out-of-line. The classic 32-bit-offset overflow check the encoder
performs against `u32::MAX` is lifted in BigTIFF mode (the on-disk
ceiling is the full u64 file-offset range). Pixel formats, compressors,
predictor / planar / tiling flags compose with `bigtiff = true`
unchanged, and the multi-IFD chain ([`encode_tiff_multi`]) is supported
as long as every page agrees on the variant (a mixed-variant chain
errors out — classic and BigTIFF IFD layouts are wire-incompatible).

## Backlog (not yet implemented)

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
- CIELab photometric interpretation (PhotometricInterpretation = 8)
  **decodes** as of this round (3-sample `L*a*b*` and 1-sample
  `L*`-only, 8-bit; both uncompressed and the byte-aligned
  compressors). Encode-side CIELab remains a backlog item — the
  `EncodePixelFormat` enum does not yet expose a Lab variant.
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

## Fuzzing

A `cargo-fuzz` decoder target lives at `fuzz/fuzz_targets/decode.rs`.
It drives arbitrary bytes through `decode_tiff`, `decode_tiff_all`,
`parse_header`, `parse_ifd`, and the three public compression
unpackers (`unpack_packbits` / `unpack_lzw` / `unpack_deflate`). The
contract under test is decoder-only panic-freedom — every public
surface must return a `Result` for any input rather than abort,
debug-overflow, OOM, or OOB-index.

Run with nightly + cargo-fuzz:

```sh
cargo +nightly fuzz run decode -- -max_total_time=60
```

Round 126 added the target and fixed three panic vectors it caught
within the first ~10 M iterations: an LZW first-after-Clear
non-leaf code that formed a self-referential prefix chain
(`src/compress.rs`), a BigTIFF `first_ifd_offset = u64::MAX` that
debug-panicked the `off + 8` slice math (`src/ifd.rs`), and an
attacker-claimed `ImageWidth * ImageLength` that drove a
multi-exabyte upfront `Vec::with_capacity` (`src/decoder.rs`
`MAX_IMAGE_PIXELS` gate). Deflate output is now capped via
`miniz_oxide`'s `decompress_to_vec_zlib_with_limit` to bound
zip-bomb expansion. Regression tests live in
`tests/decode_fuzz_regressions.rs` plus inline `compress` /
`ifd` test modules so the panic-freedom checks survive in CI even
when the fuzzer isn't being driven.

## Benchmarks

A Criterion bench harness lives at `benches/lzw.rs` covering the
TIFF/LZW encoder hot path (`compress::pack_lzw`). Run with:

```sh
cargo bench -p oxideav-tiff --bench lzw
```

Round 129 replaced the per-byte `HashMap<(u16, u8), u16>` dictionary
lookup in `pack_lzw` with a flat-array trie (three `[u16; 4096]` /
`[u8; 4096]` arrays — `first_child`, `next_sibling`, `suffix`). The
bitstream output is byte-identical to the pre-r129 encoder (same
greedy match, same code-width bump points, same Clear-on-fill
timing), so the change is invisible to any decoder, but throughput
on representative image-like strips climbed substantially. On Apple
Silicon `release` builds the four bench scenarios moved from:

| Scenario                  | r128 HashMap | r129 trie   | Speedup |
| ------------------------- | -----------: | ----------: | ------: |
| `lzw_random_64k`          | ~53 MiB/s    | ~65 MiB/s   |   ~1.2x |
| `lzw_repeating_motif_64k` | ~44 MiB/s    | ~640 MiB/s  |  ~14.5x |
| `lzw_zeros_256k`          | ~46 MiB/s    | ~640 MiB/s  |  ~14.1x |
| `lzw_natural_image_64k`   | ~39 MiB/s    | ~809 MiB/s  |  ~20.5x |

The "natural image" 256×256 8-bit greyscale fixture (vertical ramp +
horizontal sinusoid) is the most representative of a real TIFF strip
and gives the largest speedup because the trie's child lists for the
common short prefixes get amortised across many matches. The random
fixture is the worst case (no prefix reuse) and still moves up
modestly because the trie eliminates the per-byte hash + bucket
probe.
