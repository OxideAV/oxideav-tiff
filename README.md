# oxideav-tiff

Pure-Rust TIFF 6.0 image decoder + encoder + container for the
[`oxideav`](https://github.com/OxideAV/oxideav) framework.

Implements the *Aldus TIFF Revision 6.0 (June 1992)* baseline plus the
universally-deployed Part 2 extensions (LZW + Deflate, tiles, YCbCr /
CMYK photometrics, CCITT Modified Huffman + T.4 1-D), the multi-IFD
chain (multi-page), the Adobe Pagemaker 6.0 *BigTIFF* design
(8-byte offsets, magic 43), and the de-facto registry extension
`Compression = 50000` (Zstandard). Spec-only clean-room: no external
library source was consulted.

## Decode

| Photometric    | Bit depth      | Compression                        | Output       |
| -------------- | -------------- | ---------------------------------- | ------------ |
| WhiteIsZero    | 1              | None / CCITT-MH / T.4-1D / **T.4-2D** / **T.6 (G4)** / PackBits / LZW / Deflate / **ZSTD** | `Gray8` |
| WhiteIsZero    | 4 / 8          | None / PackBits / LZW / Deflate / **ZSTD** | `Gray8` |
| WhiteIsZero    | 16             | None / PackBits / LZW / Deflate / **ZSTD** | `Gray16Le` |
| WhiteIsZero / BlackIsZero | 8 / 16 | None / PackBits / LZW / Deflate / **ZSTD** + **SampleFormat=2 (signed int)** | `Gray8` / `Gray16Le` (offset-binary display map) |
| WhiteIsZero / BlackIsZero | 16 / 32 / 64 | None / PackBits / LZW / Deflate / **ZSTD** + **SampleFormat=3 (IEEE float)** | `Gray8` (linear extent→display map) |
| RGB (3 chan)   | 16 / 32 / 64   | None / PackBits / LZW / Deflate / **ZSTD** + **SampleFormat=3 (IEEE float)** | `Rgb24` (shared-extent linear→display map) |
| BlackIsZero    | 1              | None / CCITT-MH / T.4-1D / **T.4-2D** / **T.6 (G4)** / PackBits / LZW / Deflate / **ZSTD** | `Gray8` |
| BlackIsZero    | 4 / 8 / 16     | None / PackBits / LZW / Deflate / **ZSTD** | `Gray8` / `Gray16Le` |
| **Transparency Mask** | 1       | None / CCITT-MH / T.4-1D / **T.4-2D** / **T.6 (G4)** / PackBits / LZW / Deflate / **ZSTD** | `Gray8` (interior = 0xFF, exterior = 0x00) |
| Palette        | 4 / 8          | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb24` |
| RGB (3 chan)   | 8              | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb24` |
| RGB (3 chan)   | 16             | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb48Le` |
| CMYK (4 chan)  | 8              | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb24` |
| YCbCr (3 chan) | 8              | None / PackBits / LZW / Deflate / **ZSTD** (incl. **§21 chroma subsampling** `[2,1]`/`[2,2]`/`[4,1]`/`[4,2]`) / **JPEG-in-TIFF** (Compression=7) | `Rgb24` |
| RGB (3 chan)   | 8              | **JPEG-in-TIFF** (Compression=7)   | `Rgb24`      |
| BlackIsZero / WhiteIsZero | 8   | **JPEG-in-TIFF** (Compression=7)   | `Gray8`      |
| CMYK (4 chan)  | 8              | **JPEG-in-TIFF** (Compression=7)   | `Rgb24`      |
| **CIELab (3 chan)** | 8         | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb24` (Lab→XYZ@D65→linear NTSC→sRGB) |
| **CIELab (1 chan, L\* only)** | 8 | None / PackBits / LZW / Deflate / **ZSTD** | `Gray8` |
| BlackIsZero / WhiteIsZero / RGB / YCbCr / CMYK | 8 | **Old-style JPEG** (Compression=6, TIFF 6.0 §22 **interchange-format layout**) | `Gray8` / `Rgb24` |

`Predictor = 1` (no prediction), `Predictor = 2` (horizontal
differencing, per-component for `SamplesPerPixel > 1`) and
`Predictor = 3` (the **IEEE floating-point predictor** — significance-
ordered byte-plane de-interleave followed by an inclusive cumulative byte
sum, per the Adobe TIFF §14 floating-point predictor) are all supported.
Predictor 3 decodes for 16-/32-/64-bit `SampleFormat = 3` grayscale and
RGB across strip and tile layouts (chunky and per-plane), under any
byte-oriented compression (None / PackBits / LZW / Deflate / ZSTD), and
is now **written on encode** for 16-/32-/64-bit float grayscale and RGB
(`EncodePixelFormat::{GrayF16, GrayF32, GrayF64, RgbF16, RgbF32,
RgbF64}` — f16 pages carry raw binary16 bit patterns, with public
`f32_to_f16_bits` / `f16_bits_to_f32` round-to-nearest-even helpers)
across strip / tile / BigTIFF / multi-page — `forward_float_predictor`
being the exact
inverse of the decode-side `undo_float_predictor`. It is
rejected over non-float or non-16/32/64-bit data per the §14 "the reader
must give up" rule. Validated with a binary-independent
encode/decode oracle (including the new float-encode self-roundtrip and
raw-byte predictor-reversal oracles) and against ImageMagick-written
Predictor 1 vs 3 float TIFFs (f32/f64, LZW + Deflate, grayscale + tiled). Strip- and
tile-based layouts both decode (any number of
strips or tiles), including **sub-byte (1-bit and 4-bit) chunky tiles**
(BlackIsZero / WhiteIsZero grayscale and 4-bit palette): per TIFF 6.0
§15, `TileWidth` is a multiple of 16 and each tile row is an
independent byte-padded scanline, so for `SamplesPerPixel = 1` at
`BitsPerSample ∈ {1, 4}` every tile-column boundary lands on a byte
boundary and the tile reassembly copies whole bytes at byte-aligned
offsets (no cross-byte bit shifting). `PlanarConfiguration = 1` (chunky) and
`PlanarConfiguration = 2` (separate component planes) are both
accepted: per TIFF 6.0 §"PlanarConfiguration", the second arrangement
stores `StripOffsets` / `StripByteCounts` (and `TileOffsets` /
`TileByteCounts`) as `SamplesPerPixel × StripsPerImage` entries with
component 0 first, then component 1, etc.; the decoder re-interleaves
the planes into chunky order downstream so every photometric path
(RGB / CMYK / YCbCr) sees the same input shape. JPEG-in-TIFF
(`Compression = 7`) remains chunky-only — TN2's planar-2 rules are
not yet supported. Multi-page files walk the next-IFD chain
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
  `Rgb24`. This is the common full-resolution RGB JPEG layout that a
  PPM-to-JPEG-compressed TIFF transcode produces. `oxideav-mjpeg`
  may hand the decoded full-resolution 3-component frame back either
  as three planar components or as a single packed interleaved
  `R G B` plane (stride = `width × 3`), depending on its build; the
  TIFF compositor accepts both shapes and produces the same `Rgb24`.
* `PhotometricInterpretation = 6` (YCbCr) with `SamplesPerPixel = 3`:
  3-plane planar YCbCr JPEG with `YCbCrSubSampling` reflected in the
  JPEG sampling factors. 4:4:4 / 4:2:2 / 4:2:0 / 4:1:1 are all
  composited to `Rgb24` via the BT.601 matrix matching TN2's default
  `ReferenceBlackWhite = [0,255,128,255,128,255]`. This is the layout a
  YCbCr JPEG-compressed TIFF transcode produces.
* `PhotometricInterpretation = 5` (CMYK) with `SamplesPerPixel = 4`:
  4-component JPEG datastream — `oxideav-mjpeg` reads any optional
  Adobe APP14 marker inside the segment to select the per-sample
  inversion (plain CMYK / Adobe-inverted CMYK / YCCK) and hands back
  packed `C M Y K` bytes (`0 = no ink`). The TIFF compositor then
  converts to `Rgb24` using the same additive-RGB formula the
  uncompressed CMYK path uses (`R = (255 − C) × (255 − K) / 255`,
  etc., TIFF 6.0 §16 `InkSet = 1`). This is the layout a
  CMYK JPEG-compressed TIFF transcode produces.

Both **strip- and tile-organised** JPEG-in-TIFF decode: with
`TileWidth` / `TileLength` present each tile is its own SOI..EOI
datastream (the `JPEGTables` reference applies identically), and the
decoder composites the tile grid into the output plane, clipping
right/bottom edge tiles to the image bounds. RGB, grayscale and YCbCr
tiled JPEG are exercised against ImageMagick-written fixtures (incl. a
partial-edge 48×40 / 16×16 grid).

Not supported (return precise `Error::Unsupported`):
12-bit (SOF1 with `P = 12`), arithmetic (SOF9 / SOF11), and
`PlanarConfiguration = 2` (`Compression = 7` and `= 6` only; the
non-JPEG compressors do accept planar layout — see above).
The deprecated TIFF 6.0 §22 "old-style" JPEG (`Compression = 6`)
decodes in its interchange-format layout — see the next section.
JPEG-in-TIFF requires the default-on `registry` Cargo feature; with
`default-features = false` the JPEG decode paths return
`Error::Unsupported` (the §22 field validation and its precise
rejection errors still run in standalone builds).

### Old-style JPEG (Compression = 6, TIFF 6.0 §22)

TIFF 6.0 §22 predates Tech Note 2 and defines `Compression = 6` with
nine auxiliary fields (tags 512–521). TN2 deprecates the design but
keeps the tag values "reserved indefinitely" so readers can continue
to read existing files; the `jpeg_old` module implements exactly that
reader side.

Two §22 layouts exist:

* **Interchange-format layout — decodes.** §22 "Strips and Tiles":
  "Compressed images conforming to the syntax of the JPEG interchange
  format can be converted to TIFF simply by defining a single strip or
  tile for the entire image and then concatenating the TIFF image
  description fields to the JPEG compressed image data."
  `JPEGInterchangeFormat` (tag 513) "points to the Start of Image
  (SOI) marker code" and `JPEGInterchangeFormatLength` (tag 514)
  gives the bitstream's byte length. The decoder slices that complete
  SOI..EOI bitstream and routes it through the same segment decode +
  composite machinery the `Compression = 7` path uses, as one
  full-image segment. Accepted photometrics are §22's continuous-tone
  set: grayscale (BlackIsZero / WhiteIsZero, with polarity
  inversion), RGB, YCbCr (any sampling the JPEG stream declares), and
  CMYK; both `II` and `MM` byte orders. Real-world §22 laxities are
  tolerated: redundant strip pointers into the interchange area are
  ignored; a missing length field (it is "useful", not mandatory)
  reads to the last EOI before end-of-file; a declared length that
  includes trailing padding is trimmed to the EOI; and a missing
  `JPEGProc` is accepted when the interchange stream is present (the
  SOF marker carries the process — TN2 records writers that omitted
  every auxiliary field). Multi-page §22 chains decode via
  [`decode_tiff_all`]. Byte-exact equivalence with the
  `Compression = 7` wrapping of the identical bitstream is asserted in
  `tests/decode_oldstyle_jpeg.rs`.

* **Tables-form layout — recognised, precisely rejected.** Without an
  interchange stream, §22 stores *raw* table payloads (64-byte
  zigzag-order quantization tables; Huffman tables as "16 BYTES of
  'BITS'" + "VALUES") behind per-component offset arrays
  (`JPEGQTables` / `JPEGDCTables` / `JPEGACTables`), and each strip
  "points directly to the start of the entropy coded data (not to a
  JPEG marker)". Reconstructing a decodable datastream from those
  fields requires synthesizing ISO 10918-1 marker segments
  (DQT / DHT / SOF / SOS), whose byte syntax the TIFF spec does not
  define — TN2 calls this out as the §22 design's core failure ("the
  TIFF control logic must ... synthesize JPEG markers from the TIFF
  fields to feed the codec"). This build reports the layout as
  `Error::Unsupported` with a message naming the missing capability.
  Malformed tables-form IFDs get `Error::InvalidData` first: the §22
  JPEGProc applicability table is enforced (baseline requires
  Q/DC/AC tables; lossless requires JPEGLosslessPredictors +
  JPEGDCTables), `JPEGProc` values other than 1 (baseline) / 14
  (lossless Huffman) are rejected per "will be defined in the
  future", lossless predictor selection-values are range-checked
  (1..=7), and out-of-bounds / non-SOI interchange offsets are typed
  errors.

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

Compressors accepted: None / PackBits / LZW / Deflate / ZSTD (the
byte-aligned, photometric-agnostic set the rest of the multi-bit
photometric paths use). CCITT (Compression = 2/3/4) is bilevel-only
per spec and rejected here; the JPEG-in-TIFF dispatch (Compression =
7) does not currently recognise CIELab as one of its render targets —
TN2 doesn't list it as a permitted §6.1.4 photometric for new-style
JPEG.

Encode-side CIELab is available via
[`EncodePixelFormat::CieLab8`] (3-sample chunky `(L*, a*, b*)`) and
[`EncodePixelFormat::CieLabL8`] (1-sample L\*-only). The encoder
writes the caller-supplied L\*/a\*/b\* bytes through verbatim — the
on-disk bit interpretation is fixed by §23 (L\* unsigned 0..255 →
0..100 perceptual lightness, a\*/b\* two's-complement signed bytes
in -128..127) — and sets `PhotometricInterpretation = 8`,
`SamplesPerPixel = 3` (or `1`), `BitsPerSample = [8,8,8]` (or `[8]`).
The same compressor set the decode path accepts (None / PackBits /
LZW / Deflate / ZSTD) is accepted on the encode side; CCITT (Compression =
2/3/4) is rejected because it is bilevel-only per §10 / §11.
`Predictor = 2` composes with both variants (per-component
differencing with offset = `SamplesPerPixel`, identical to the
Rgb24 / Gray8 paths). `PlanarConfiguration = 2` composes with
`CieLab8` (L\*, a\*, b\* split into three single-component planes,
§"PlanarConfiguration"); it is rejected on `CieLabL8` per
§"PlanarConfiguration" "irrelevant" when `SamplesPerPixel = 1`.
Tiled layout (§15) composes with both variants under chunky and, for
`CieLab8`, under planar (one tile grid per plane, §15
`TileOffsets`). BigTIFF composes unchanged — the 3-entry
`BitsPerSample` SHORT array (6 bytes) stays inline in the widened
8-byte value/offset slot.

### Zstandard (Compression = 50000)

`Compression = 50000` is the de-facto registry extension for
Zstandard (RFC 8478) — there is no Adobe technical note registering
it; the numeric value, the `Predictor` (tag 317) interaction, and the
per-segment frame discipline are transcribed in the OxideAV trace doc
[`docs/image/tiff/tiff-zstd-compression-50000.md`](docs/image/tiff/tiff-zstd-compression-50000.md).
Structurally the scheme is the `Compression = 8` Deflate template
with the codec swapped: each strip or tile is **one self-contained
Zstandard frame** (magic `0x28 0xB5 0x2F 0xFD`) over the
post-predictor sample bytes — no file-global stream, no cross-strip
dictionary — and `StripByteCounts` / `TileByteCounts` give each
frame's compressed length. Because the frame wraps whatever byte
stream the segment would otherwise contain, the scheme is independent
of `PhotometricInterpretation`, `BitsPerSample`,
`PlanarConfiguration`, and the strip-vs-tile organisation; the
decoder reuses the exact predictor-reversal and photometric assembly
the Deflate path runs. `Predictor = 2` (§14 horizontal differencing)
is reversed *after* the frame decode per the trace doc §5 ordering;
`Predictor = 3` (the floating-point predictor) is rejected per the
§14 reader rule ("the reader must give up" on an unrecognised
scheme) rather than mis-decoded. Frame decode is bounded by the
IFD-declared segment size so a malformed frame claiming a multi-GiB
expansion errors instead of OOM-ing.

On the encode side, `TiffCompression::Zstd` composes with every
byte-aligned pixel format plus `Predictor = 2`,
`PlanarConfiguration = 2`, §15 tiling, BigTIFF, and the multi-page
chain — the same axes as Deflate. The compression level is an
out-of-band encoder runtime parameter (range 1–22 in the de-facto
registry; never stored in the file, decoders are level-agnostic),
so the writer simply uses its compression backend's default. Both
directions (and the Deflate path's zlib) go through
[`compcol`](https://crates.io/crates/compcol), the workspace
compression collection. Validation is three-layered
(`tests/zstd_roundtrip.rs`): binary-independent self-roundtrips
(Gray8 / Gray16Le / Rgb24 / Palette8 / Bilevel × predictor / planar /
tiled-with-partial-edges / BigTIFF / multi-page), hand-built classic-II
fixtures whose strips are hand-assembled RFC 8478 `Raw_Block` frames
(wire bytes our encoder never emits), and black-box cross-checks against
an independent reference transcoder (our `Compression = 50000` output
transcoded to uncompressed, and independently-produced Zstandard files
decoded by us — both compared pixel-exact). `Compression = 50001`
(WebP) from the same registry page remains unimplemented.

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
(None / PackBits / LZW / Deflate / ZSTD / CCITT-MH / CCITT-T.4-1D); the spec
recommends PackBits. **Tiled layout (§15) now composes** with both
1-bit variants on encode (1-bit tile-row packing with §15 edge
replication; see the Encode tiling note) and decode; planar and
Predictor=2 layouts remain rejected for 1-bit input on both sides
(§14 component-differencing doesn't apply to packed bits, and §"Planar-
Configuration" is irrelevant at `SamplesPerPixel = 1`).

`FillOrder = 1` (MSB-first, the baseline default) and `FillOrder = 2`
(LSB-first) are both accepted for the bit orderings the spec admits:
FillOrder=2 is honoured for uncompressed and CCITT-compressed
(`Compression = 1 / 2 / 3`) `BitsPerSample = 1` strips and tiles, per
TIFF 6.0 §FillOrder (page 32). Any other combination of FillOrder=2
with non-bilevel data or with a non-CCITT compressor is rejected with
a precise error.

`SampleFormat` (tag 339, TIFF 6.0 §SampleFormat page 80) is inspected
on every IFD. Value `1` (unsigned integer, the spec default) and value
`4` (undefined — the §SampleFormat note recommends "treat … as if the
field were not present, i.e. as unsigned integer data") route through
the unsigned-integer decoder path that the rest of the codec is built
around. Value `2` (two's-complement **signed integer**) now decodes
for the single-channel grayscale photometrics (BlackIsZero /
WhiteIsZero) at the integer widths the codec assembles, 8-bit and
16-bit — the layout scientific and elevation TIFFs use. §SampleFormat
notes the size "is still done by the BitsPerSample field" and the
companion SMinSampleValue (340) / SMaxSampleValue (341) "default … is
the full range of the data type", so the signed samples span the full
signed range and are rendered onto the codec's unsigned display planes
through the order-preserving **offset-binary** map (signed minimum →
display 0, signed maximum → the unsigned ceiling, stored signed 0 →
the display midpoint): a sign-bit flip — `XOR 0x80` at 8-bit, `XOR
0x8000` at 16-bit — applied before the WhiteIsZero polarity inversion.
The mapping is bijective and monotone, so relative brightness is
preserved exactly. SampleFormat = 2 on any other photometric / width
(RGB, palette, CMYK, YCbCr, CIELab, or a sub-byte / non-assembled
width) has no single defensible display mapping in this build and is
surfaced as a precise typed error. Value `3` (IEEE floating-point) now
decodes for the single-channel grayscale photometrics (BlackIsZero /
WhiteIsZero) **and for 3-channel RGB** at the three IEEE 754 widths a
float TIFF stores — 16-bit
(binary16 half), 32-bit (binary32 single), 64-bit (binary64 double),
the layout scientific / elevation / HDR-source TIFFs use. §SampleFormat
fixes the sample size in BitsPerSample (not in this field), so the width
is read there exactly as for the integer paths, and the binary16 half is
widened to single precision losslessly. A float sample carries no
intrinsic display range, so — paralleling the §23 CIELab "some
conversion to RGB will be required" latitude and the signed-integer
offset-binary map — the decoder maps the finite sample extent linearly
onto the 8-bit display plane: a sample at the extent minimum
renders 0, one at the maximum renders 255. The extent is the
SMinSampleValue / SMaxSampleValue pair (tags 340 / 341) when both are
present (§SampleFormat: this "makes it possible for readers to assume
that data samples are bound to the range [SMinSampleValue,
SMaxSampleValue] without scanning the image data"), else the actual
finite min/max scanned from the decoded samples (the spec's stated
fallback when the bound tags are absent). The grayscale path renders a
`Gray8` plane; the 3-channel RGB path renders an `Rgb24` plane using a
**single shared extent across all three colour channels** so the
relative R / G / B magnitudes — the pixel's chromaticity — survive the
display map, where a per-channel extent would re-balance the colour.
Non-finite samples (NaN /
±Inf) are excluded from the extent and render at the display floor, and
a degenerate extent (all samples equal) renders a flat plane. Only
`Predictor = 1` is meaningful — §14 horizontal differencing is defined
over integer samples and the floating-point predictor (`Predictor = 3`)
is rejected at the predictor gate — so a float strip declaring a
predictor is surfaced as a precise typed error. SampleFormat = 3 on any
other photometric (palette, CMYK, YCbCr, CIELab — and RGB at a sample
count other than 3) likewise has no
single defensible display mapping in this build and is rejected per the
§SampleFormat reader rule: "If the SampleFormat field is present and the
value is not 1, a Baseline TIFF reader that cannot handle the
SampleFormat value must terminate the import process gracefully." An
absent field defaults to unsigned (so every existing TIFF fixture
decodes byte-for-byte unchanged); non-uniform per-component values and
out-of-range values (≥ 5) are also rejected.

`Orientation` (tag 274, TIFF 6.0 §Orientation page 36) is inspected on
every IFD. The spec defines eight values mapping the stored 0th row /
0th column onto the displayed image: `1` is canonical (0th row = visual
top, 0th column = visual left-hand side), `2..=8` cover the remaining
horizontal-flip / vertical-flip / 90° / 180° / 270° / transpose /
antitranspose permutations a writer may declare. The decoder now
**re-orients the fully-assembled image into display order** for every
value, so a downstream consumer always sees the visually-correct
geometry regardless of how the writer stored the rows. Values `2..=4`
are in-place mirror / 180° permutations that preserve the stored
`(width, height)` shape; values `5..=8` are transpose-family
permutations that swap width and height (a 90° rotation turns a
landscape strip into a portrait image). The remap operates on
fixed-size pixel cells (`stride / width` bytes), so it is independent of
photometric, bit depth, and compression — every decode path picks it up
uniformly. The geometry of each value is derived directly from the
§Orientation page-36 "0th row represents… / 0th column represents…"
table:

| Value | Display mapping `d[r][c] = s[..]`     | Description     |
| ----- | ------------------------------------- | --------------- |
| 1     | `s[r][c]`                             | identity        |
| 2     | `s[r][w-1-c]`                         | mirror H        |
| 3     | `s[h-1-r][w-1-c]`                     | rotate 180°     |
| 4     | `s[h-1-r][c]`                         | mirror V        |
| 5     | `s[c][r]` (→ h×w)                      | transpose       |
| 6     | `s[h-1-c][r]` (→ h×w)                  | rotate 90° CW   |
| 7     | `s[h-1-c][w-1-r]` (→ h×w)             | antitranspose   |
| 8     | `s[c][w-1-r]` (→ h×w)                  | rotate 90° CCW  |

An absent field defaults to `1` (every existing TIFF fixture decodes
unchanged); value `0` and values `≥ 9` are surfaced as invalid-data
errors because the spec lists `1..=8` only.

`ResolutionUnit` (tag 296, TIFF 6.0 §"Physical Dimensions" page 18) is
inspected on every IFD. The spec defines three values for the unit of
measurement that `XResolution` (tag 282) and `YResolution` (tag 283)
are denominated in: `1` = "No absolute unit of measurement" (used for
images with a non-square aspect ratio but no meaningful absolute
dimensions), `2` = "Inch", `3` = "Centimeter", with "Default = 2
(inch)." The decoder treats this field as metadata only — the on-disk
pixel bytes are independent of the resolution unit, so an absent field
decodes through the same default-inch path and explicit values
`1` / `2` / `3` all route through the unchanged pixel path. Values `0`
and `≥ 4` are surfaced as `Error::InvalidData` because the spec lists
`1..=3` only, rather than silently swallowing the malformed writer's
output.

`ExtraSamples` (tag 338, TIFF 6.0 §ExtraSamples pages 31–32) is
inspected on every IFD. The field declares `m` extra components per
pixel ("When this field is used, the SamplesPerPixel field has a value
greater than the PhotometricInterpretation field suggests"), stored by
convention as the last components of each pixel. The count must leave
a color-component tally the photometric defines (3 for RGB / YCbCr, 4
for CMYK, 1 for the grayscale / palette / mask photometrics, 3-or-1
for CIELab per §23) — any other arithmetic is `Error::InvalidData`.
Per-value policy: all three defined kinds render the leading color
components verbatim and drop the trailing extra(s), so an RGB
`SamplesPerPixel = 4` page renders its `R G B` triple. `0` (unspecified
data) and `2` (unassociated alpha — the soft matte logically
independent of the straight-stored color) carry color stored straight,
so dropping the extra is exact. `1` (associated alpha) **now decodes**:
§18 "Associated Alpha Handling" (page 78) states that "naive
applications that want to display an RGBA image on a display can do so
simply by displaying the RGB component values … because it is
effectively the same as merging the image with a black background"
(`Cr = Cover * Aover`, "which is exactly the pre-multiplied color; i.e.
what is stored in the image"), so the stored pre-multiplied leading
triple **is** the composite-over-black display value a display reader
shows directly — the decoder renders it verbatim and drops the alpha
(§18 page 78: alpha = 0 stores `(0,0,0)`, which renders as black, the
naive-display result). Values `≥ 3` are `Error::InvalidData` because
the spec lists `0..=2` only. An absent field means "no extra samples"
per the spec default, and an RGB page with `SamplesPerPixel ≥ 4` but
no tag 338 still decodes by skipping the undeclared trailing
components.

## Encode

| Photometric    | Bit depth | Compression                                                 | API call                |
| -------------- | --------- | ----------------------------------------------------------- | ----------------------- |
| WhiteIsZero    | 1         | None / CCITT-MH / T.4-1D / **T.4-2D** / **T.6 (G4)** / PackBits / LZW / Deflate / **ZSTD** | `EncodePixelFormat::Bilevel` |
| **Transparency Mask** | 1  | None / CCITT-MH / T.4-1D / PackBits / LZW / Deflate / **ZSTD** | `EncodePixelFormat::TransparencyMask` (sets PhotometricInterpretation = 4 and NewSubfileType bit 2) |
| BlackIsZero    | 8 / 16    | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Gray8` / `::Gray16Le` |
| **BlackIsZero** | 4        | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled** (nibble-granularity edge replication), **`Predictor = 2`** (§14 nibble differencing mod 16), BigTIFF | `EncodePixelFormat::Gray4` (packed high-nibble-first raster, rows byte-padded) |
| **BlackIsZero signed** (SampleFormat = 2) | 8 / 16 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`Predictor = 2`**, BigTIFF, multi-page | `EncodePixelFormat::GrayI8` / `::GrayI16` (two's-complement samples + SampleFormat = 2 tag; decoder renders via the offset-binary map) |
| **BlackIsZero float** (SampleFormat = 3) | **16** / 32 / 64 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, BigTIFF, **`Predictor = 3`** | `EncodePixelFormat::GrayF16` / `::GrayF32` / `::GrayF64` (writes SampleFormat = 3; the §14 floating-point predictor; f16 = raw binary16 bit patterns + `f32_to_f16_bits` helper) |
| RGB            | 8         | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Rgb24`     |
| **RGB + alpha/extra (§ExtraSamples)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`PlanarConfiguration = 2`** / **`Predictor = 2`**, BigTIFF | `EncodePixelFormat::Rgba32` + `ExtraSampleKind` (writes SamplesPerPixel = 4 and ExtraSamples = 0/1/2, tag 338) |
| **RGB**        | 16        | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`PlanarConfiguration = 2`** / **`Predictor = 2`**, BigTIFF | `EncodePixelFormat::Rgb48` (BitsPerSample = [16,16,16] little-endian — encode parity for the `Rgb48Le` decode path) |
| **RGB float** (SampleFormat = 3) | **16** / 32 / 64 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, BigTIFF, **`Predictor = 3`**, **`PlanarConfiguration = 2`** | `EncodePixelFormat::RgbF16` / `::RgbF32` / `::RgbF64` (writes SampleFormat = 3, SamplesPerPixel = 3) |
| Palette        | 8         | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Palette8`  |
| **Palette**    | 4         | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`Predictor = 2`**, BigTIFF | `EncodePixelFormat::Palette4` (48-SHORT §"Palette Color Images" ColorMap) |
| **CIELab (3 chan)** | 8    | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::CieLab8` (writes PhotometricInterpretation = 8, SamplesPerPixel = 3, BitsPerSample = [8,8,8]) |
| **CIELab (1 chan, L\* only)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**             | `EncodePixelFormat::CieLabL8` (writes PhotometricInterpretation = 8, SamplesPerPixel = 1) |
| **CMYK (4 chan)** | 8     | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Cmyk32` (writes PhotometricInterpretation = 5, SamplesPerPixel = 4, BitsPerSample = [8,8,8,8], plus optional `InkSet = 1` / `NumberOfInks = 4`) |
| **YCbCr (3 chan, 4:4:4)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`PlanarConfiguration = 2`** / **`Predictor = 2`** | `EncodePixelFormat::YCbCr24` (writes PhotometricInterpretation = 6, SamplesPerPixel = 3, BitsPerSample = [8,8,8], `YCbCrSubSampling = [1, 1]`, `YCbCrPositioning = 1`, `ReferenceBlackWhite = [0,255,128,255,128,255]` no-headroom full-range) |
| **YCbCr (3 chan, subsampled)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled** | `EncodePixelFormat::YCbCrSubsampled24` (§21 data-unit packing for `[2,1]`/`[2,2]`/`[4,1]`/`[4,2]`; tiles per §21 page 90 multiple-of-subsampling-factor geometry) |

`TiffCompression::CcittRle` selects Modified Huffman
(`Compression = 2`, TIFF 6.0 §10), `TiffCompression::CcittT4OneD`
selects T.4 1-D (`Compression = 3`, §11) with an optional
`eol_byte_aligned` flag that writes `T4Options` bit 2,
`TiffCompression::CcittT4TwoD` selects T.4 2-D / Modified READ
(`Compression = 3` with `T4Options` bit 0 set) and
`TiffCompression::CcittT6` selects T.6 / MMR / Group 4
(`Compression = 4`, `T6Options` written as LONG zero — bit 1
"uncompressed mode allowed" is never set). The 1-D writers all
share the `WHITE` / `BLACK` MH run-length tables transcribed from
the TIFF 6.0 PDF; the 2-D writers additionally emit the Pass /
Horizontal / Vertical mode codes from
[`docs/image/tiff/ccitt-t4-t6-fax-codes.md`](docs/image/tiff/ccitt-t4-t6-fax-codes.md)
§1 (Table 4/T.4 = Table 1/T.6), selecting between modes per
T.4 §4.2.1.3 ("if `b2 < a1` → Pass; else if `|a1 − b1| ≤ 3` →
`V(a1 − b1)`; else Horizontal"). For `T4TwoD` the encoder codes
row 0 as 1-D (tag bit 1) so its decode is independent of the
imaginary-white reference, then every subsequent row as 2-D
(tag bit 0) against the previously coded row — K-parameter
resync is unused. `T6` codes every row 2-D against the previous
row (the first against the imaginary all-white reference per
T.6 §2.2.1) and emits no EOL framing; no EOFB sentinel is
written either, as the decoder stops at `rows` rows regardless.
All CCITT writers reject non-bilevel inputs with a precise
error.

The horizontal-differencing predictor (`Predictor = 2`, TIFF 6.0 §14)
is available on encode via the `EncodePage::predictor` flag. When set,
the encoder writes first differences (per-component, offset
`SamplesPerPixel` for chunky multi-sample data) plus the `Predictor`
tag (317) so the decoder reverses the step — the exact inverse of the
decoder's cumulative add. It applies to the lossless byte-aligned
formats whose decode path already supports it (`Gray8`, `Gray16Le`,
`Rgb24`, `Rgb48` — §14 differencing at the 16-bit sample width —
`Palette8`); combining it with the bilevel CCITT schemes or
with `Bilevel` input is rejected. An independent reference reader
reports horizontal-differencing predictor 2 on the output, and an
independent transcode to uncompressed re-decodes to the original pixels.

`PlanarConfiguration = 2` (separate component planes, TIFF 6.0
§"PlanarConfiguration") is available on encode via the
`EncodePage::planar` flag. When set on an `Rgb24` (or 16-bit `Rgb48`) page the encoder
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
Deflate. An independent transcode of our planar output to an
uncompressed TIFF re-decodes to the original pixels, and an independent
reference reader reads it bit-exactly.

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
`Rgb48` / `Palette8`) under None / PackBits / LZW / Deflate / ZSTD, with or without
`Predictor = 2` (applied per-tile), and for the 1-bit `Bilevel` /
`TransparencyMask` formats under the same byte-aligned compressors
(sub-byte tile-row bit packing with §15 edge replication, no predictor).
Tiling is rejected with the CCITT compressors. An independent transcode of our tiled
output to an uncompressed TIFF re-decodes to the original pixels, and an
independent reference reader reads it bit-exactly.

Tiling composes with `planar = true` on `Rgb24`: the encoder writes one
row-major tile grid per component plane, emitting plane 0's tiles first
then plane 1's, then plane 2's, per §15 `TileOffsets` ("For
PlanarConfiguration = 2, the offsets for the first component plane are
stored first, followed by all the offsets for the second component
plane, and so on"). `TileOffsets` / `TileByteCounts` therefore carry
`SamplesPerPixel × TilesPerImage` entries. Each plane is a
single-component image, so §15 boundary padding and the §14 predictor
run with an offset of one sample (§14: "If PlanarConfiguration is 2 …
Differencing works the same as it does for grayscale data"). An
independent reference transcoder and reader both round-trip the
planar-tiled output back to the original chunky pixels bit-exactly.

On the read side, the decoder walks the same §15 tile fields, decodes
each tile, reverses any per-tile `Predictor = 2` differencing, and
reassembles the grid into the full image — dropping the boundary padding
on right-column / bottom-row tiles. A binary-independent self-roundtrip
suite encodes the identical pixels both tiled and strip-based, decodes
both, and asserts the planes are byte-identical, so tile geometry,
ordering, per-tile predictor reversal, and §15 edge padding are checked
against the strip decode of the same image across Gray8 / Gray16Le /
Rgb24 / Palette8 (None / PackBits / LZW / Deflate / ZSTD, with and without the
predictor) and the full range of edge-tile geometries (exact-fit,
partial edges, non-square tiles, oversized single tile, 1-pixel
overhang).

Sub-byte (1-bit and 4-bit) chunky tiles also **decode** (TIFF 6.0
§15 — see the Decode note above). Because `TileWidth`
is a multiple of 16 and §15 treats each tile row as an independent
byte-padded scanline, the `SamplesPerPixel = 1`, `BitsPerSample ∈
{1, 4}` case keeps every tile-column boundary byte-aligned, so the
existing reassembly path copies whole bytes at byte-aligned offsets.
The new tiled sub-byte decode is validated in
`tests/decode_tiled_subbyte.rs` against the strip decode of the same
hand-built classic-II fixtures (1-bit BlackIsZero / WhiteIsZero
grayscale, 4-bit grayscale, 4-bit palette) across exact-fit,
partial-edge, non-square-tile, odd-width, and oversized-single-tile
geometries — a binary-independent oracle: a decoder that mishandled
tile ordering, tile-row stride, byte-aligned column offsets, or §15
edge padding would diverge from the trusted strip path.

The **encoder** now also **writes 1-bit tiles** for `Bilevel` and
`TransparencyMask` input. `build_tiles_bilevel` slices the MSB-first
packed bilevel raster (`ceil(width / 8)` bytes per image row) into a
row-major tile grid where every tile row is independently packed
MSB-first and padded to a byte boundary (`tile_w / 8` bytes — `tile_w`
is a multiple of 16, so this is exact and every tile-column boundary
lands on a byte boundary at `BitsPerSample = 1`). Boundary tiles are
padded by replicating the last visible column / row (§15 "Padding").
It composes with the byte-aligned compressors the decoder's sub-byte
tile path reads (None / PackBits / LZW / Deflate / ZSTD) and with
BigTIFF; the §14 predictor (undefined for 1-bit) and the strip-oriented
CCITT coders stay rejected. `tests/encode_tiled_bilevel_roundtrip.rs`
encodes the same 1-bit pixels both tiled and strip-based, decodes both,
and asserts the rendered `Gray8` planes are byte-identical across the
full compressor set and the exact-fit / partial-edge / non-square /
odd-width / oversized-single-tile / 1-pixel-overhang geometries — the
strip decode is the independent oracle. The **4-bit sub-byte tile
writer now exists too** (`build_tiles_sub4`): each tile row is an
independent byte-padded `tile_w / 2`-byte scanline (`TileWidth` a
multiple of 16 keeps every tile-column boundary byte-aligned at 4
bits), boundary tiles replicate the last visible column / row at
nibble granularity (§15 "Padding"), and the §14 4-bit predictor
(nibble differencing modulo 16) applies per tile — so the
byte-aligned `Gray8` / `Gray16Le` / `Rgb24` / `Rgb48` / `Palette8`,
the 4-bit `Gray4` / `Palette4`, and the 1-bit `Bilevel` /
`TransparencyMask` formats all write tiles.

Output is classic II little-endian TIFF, single-IFD via
[`encode_tiff`] or multi-page via [`encode_tiff_multi`]. Files
roundtrip through independent reference readers/transcoders; CCITT
outputs additionally validate by transcoding our `Compression = 3`
stream back to uncompressed with an independent tool and checking the
resulting pixels match the original input.

### YCbCr write (PhotometricInterpretation = 6, chunky 4:4:4)

`EncodePixelFormat::YCbCr24` writes per TIFF 6.0 §21 "YCbCr Images"
(page 89): `PhotometricInterpretation = 6`, `SamplesPerPixel = 3`,
`BitsPerSample = [8, 8, 8]`, `PlanarConfiguration = 1` (chunky), and
`YCbCrSubSampling = [1, 1]` (chunky 4:4:4 — one luminance sample per
chroma pair). At the 1:1 subsampling factor the §21 "Ordering of
Component Samples" data-unit (`ChromaSubsampleVert` rows of
`ChromaSubsampleHoriz` Y samples, then one Cb and one Cr) collapses to
a single `(Y, Cb, Cr)` triple per pixel, so the caller-supplied bytes
are the on-disk strip / tile payload exactly as given. The caller owns
the RGB→YCbCr conversion; the encoder transports the bytes verbatim,
matching the decoder's existing §21 chunky walker.

Alongside the §21 / Baseline tags, the encoder emits three §21-required
fields with the §20 / §21 defaults the decoder uses:

* `YCbCrSubSampling = [1, 1]` (tag 530), two inline SHORTs — the
  chunky-444 layout the encoder writes.
* `YCbCrPositioning = 1` (tag 531) — §21 "centered". The positioning
  choice is degenerate at 1:1 subsampling but the §21 default is 1
  so the file is explicit.
* `ReferenceBlackWhite = [0/1, 255/1, 128/1, 255/1, 128/1, 255/1]`
  (tag 532, six RATIONALs out-of-line) — the §20 page 87
  "no headroom/footroom" full-range YCbCr coding value. §21 says
  this field "must be used explicitly" for Class Y images.

The §21 `YCbCrCoefficients` (tag 529) is omitted: its §21 default is
the CCIR Recommendation 601-1 luma weights
`{299/1000, 587/1000, 114/1000}` and the decoder's matrix is the Q16
inverse of those same weights, so writing the tag would just restate
the spec default. Compressors accepted: None / PackBits / LZW /
Deflate (the byte-aligned photometric-agnostic set). CCITT is
bilevel-only per §10 / §11 and rejected with a precise error.
**Tiled layout (§15) now composes** with both `YCbCr24` (4:4:4) and
`YCbCrSubsampled24` (chroma-subsampled): at `YCbCrSubSampling = [1, 1]`
the §21 data unit collapses to a plain chunky `(Y, Cb, Cr)` triple, so
the generic byte-aligned tile packer handles it exactly as it does
`Rgb24`; for the non-1:1 pairs `build_tiles_ycbcr_subsampled` packs each
tile's §21 data units (each `sh*sv` Y samples then the box-averaged Cb /
Cr) from the full-resolution pixels with §15 edge replication, and the
decoder's `decode_tiles_ycbcr_subsampled` reverses it. §21 page 90
requires `TileWidth` / `TileLength` to be integer multiples of the
subsampling factors (enforced on encode). **`PlanarConfiguration = 2`
and `Predictor = 2` now compose with 4:4:4 YCbCr** (`YCbCrSubSampling =
[1, 1]`): at the 1:1 ratio the §21 data unit collapses to a plain chunky
`(Y, Cb, Cr)` triple, so §"PlanarConfiguration" splits the three
full-resolution components into separate Y / Cb / Cr planes exactly as
for `Rgb24`, and §14 differences each component independently
("Differencing works the same as it does for grayscale data" — offset
`SamplesPerPixel` chunky, offset 1 per plane). Both compose with §15
tiling and the byte-aligned compressors.

**The genuinely chroma-subsampled `PlanarConfiguration = 2` strip
layout now encodes and decodes too.** §21 "Ordering of Component
Samples": "For PlanarConfiguration = 2, component samples are stored as
3 separate planes, and the ordering is the same as that used for other
PhotometricInterpretation field values" — plain row-major planes, no
data-unit packing — with the Cb / Cr planes at the reduced §21 "chroma
image" resolution ("ImageWidth of this chroma image is half the
ImageWidth of the associated luma image"): `(width/sh) × (height/sv)`.
The encoder (`build_planes_ycbcr_subsampled`) writes a full-resolution
Y plane plus two box-filter-decimated chroma planes, one strip per
plane; the decoder (`decode_strips_planar_ycbcr_subsampled`) sizes each
plane by its own geometry — honouring multi-strip files under the
§"PlanarConfiguration" `SamplesPerPixel × StripsPerImage` accounting
and the §21 "RowsPerStrip must be an integer multiple of
YCbCrSubsampleVert" constraint (violations rejected precisely) — then
splats each chroma sample back over its `sh × sv` block. `Predictor =
2` composes per-plane (§14: "Differencing works the same as it does
for grayscale data" — each plane differences at its own row width);
the *chunky* subsampled predictor stays rejected (no defined shape
over the packed data-unit stream), as does **tiled** planar subsampled
on both sides (§15 fixes `TileOffsets` at `SamplesPerPixel ×
TilesPerImage` — the same tile count for every plane — which has no
consistent geometry for a reduced-size chroma plane under a shared
`TileWidth × TileLength` grid). Luma round-trips bit-exact; chroma
round-trips exactly when block-constant, matching the chunky
subsampled paths. On the
decode side a planar 4:4:4 YCbCr page re-interleaves the three planes
into chunky order for the §22 matrix. Hand-built classic-II fixtures carrying the same `(Y, Cb, Cr)` bytes
decode to the same `Rgb24` as the encoder's output across the four
accepted compressors (including a hand-built two-strips-per-plane
subsampled planar fixture); the IFD bytes are inspected byte-for-byte
to confirm tags 262 / 277 / 284 / 530 / 531 / 532 carry the documented
values, and `tiffinfo` (black-box) confirms the separate-planes +
subsampling field set.

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

### Multi-strip write (RowsPerStrip)

`PageExtras::rows_per_strip = Some(r)` splits the written image into
`ceil(ImageLength / r)` independently-compressed strips (TIFF 6.0
§"RowsPerStrip" — the spec recommends ~8K-byte strips so readers can
stream), or `ceil / r` strips **per component plane** under
`PlanarConfiguration = 2` with plane 0's strips first
(§"StripOffsets"). The §14 predictors are row-local transforms, so the
per-strip compression restart is the only on-disk difference; the
CCITT coders restart per strip exactly as the per-strip decoder
expects. Composes with every compressor, both predictors, sub-byte
rasters, floats (f16/f32/f64), chunky **and** planar chroma-subsampled
YCbCr (`r` must be an integer multiple of `YCbCrSubsampleVert` per §21
page 90; chroma-plane strips carry `r / sv` reduced rows), BigTIFF and
multi-page. Mutually exclusive with tiled layout (§15: the tile fields
replace RowsPerStrip). `None` keeps the historical single-strip output
byte-for-byte.

### Page extras: PageNumber, NewSubfileType, Exif/GPS child IFDs, SubIFDs tree

`EncodePage::extras` (`PageExtras`, all fields default-off) adds the
page-level metadata writes:

* **PageNumber (tag 297)** — TIFF 6.0 §"PageNumber", `SHORT × 2` as
  `(page, total)` ("If PageNumber[1] is 0, the total number of pages in
  the document is not available").
* **NewSubfileType (254) bits 0–1** — bit 0 "reduced-resolution version
  of another image", bit 1 "single page of a multi-page image" (bit 2,
  transparency mask, stays implied by the TransparencyMask pixel
  format).
* **Exif (34665) / GPS (34853) child IFDs** — `AuxIfdEntry` lists
  transported **verbatim** (tag / §2 field type / count / raw
  little-endian value bytes). The child IFD is plain TIFF 6.0 §2
  structure: ascending-tag entries, values inline when they fit the
  variant's slot (4 B classic / 8 B BigTIFF) or out-of-line on a §2
  word boundary, next-IFD = 0. The writer sorts entries and precisely
  rejects duplicates, unknown type codes, `value.len() != count ×
  type_size`, and BigTIFF-only widths on classic pages. No Exif/GPS
  *semantics* are interpreted on either side — the tag numbers are
  registered-identifier facts; callers own the entry meanings and can
  read them back with `parse_header` / `parse_ifd` at the pointer
  offset (`Entry::data` carries the raw value bytes).
* **SubIFDs (330) child image tree** — each element is a complete
  `EncodePage` (full compressor / predictor / planar / tiling matrix)
  whose IFD hangs off the parent's tag-330 LONG / LONG8 offset array
  instead of the next-IFD chain; children nest (depth-capped at 8) and
  the offsets array spills out-of-line exactly like the strip arrays
  when it outgrows the value slot. `decode_tiff_at(file, offset)`
  decodes a child image from its tag-330 offset; `decode_tiff_all`
  ignores children by construction (they are not chained pages).

Round-trips are byte-inspected with the crate's own public
`parse_ifd` (verbatim aux values, sorted tags, zero child next-IFD,
inline-vs-spilled tag-330 array on classic + BigTIFF), and `tiffinfo`
(black-box) confirms the Page Number / multi-page / SubIFD-pointer
lines.

## Backlog (not yet implemented)

The compression schemes, photometrics, and layout features described
above are all implemented on both decode and encode where stated. The
remaining gaps are:

- **Old-style JPEG `Compression = 6` tables-form layout** — the §22
  interchange-format layout now **decodes** (see the Old-style JPEG
  section above); the tables-form layout (raw table payloads +
  entropy-coded strips) is recognised and precisely rejected because
  rebuilding a datastream from the raw tables needs the ISO 10918-1
  marker byte syntax, which is outside the TIFF spec material. The
  new-style `Compression = 7` path does not support 12-bit precision
  (SOF1 `P = 12`), arithmetic coding (SOF9 / SOF11), or
  `PlanarConfiguration = 2` JPEG segments (the same planar limit
  applies to `Compression = 6`).
- **`Compression = 50001` (WebP)** from the de-facto registry — returns
  the generic unsupported-compression error.
- **CCITT uncompressed-mode *emission*** — the decoder reads the
  optional uncompressed-mode extension, but the encoder never emits it
  (it is a bit-rate control extension, not a compression win for
  facsimile content).
- **DNG / GeoTIFF / Exif tag semantics.** The child-IFD *mechanics* are
  in (write side via `PageExtras::exif_ifd` / `gps_ifd` / `sub_ifds`;
  read side via `parse_ifd` at the pointer offset + `decode_tiff_at`
  for SubIFD images), but the crate interprets no Exif/GPS/DNG/GeoTIFF
  tag meanings.
- **Subsampled-YCbCr residual combinations** — chunky subsampled YCbCr
  encodes and decodes in both strip and tiled layouts (TIFF 6.0 §21),
  4:4:4 composes with `PlanarConfiguration = 2` and `Predictor = 2`,
  and the genuinely chroma-subsampled **separate-planes strip layout**
  now encodes and decodes too (reduced §21 "chroma image" planes,
  per-plane §14 predictor — see the YCbCr write note above). Still
  deferred: §14 differencing over the packed *chunky* data-unit stream
  (no defined shape) and **tiled** planar subsampled (§15's fixed
  `SamplesPerPixel × TilesPerImage` TileOffsets count has no consistent
  geometry for reduced-size chroma planes).
- **Float (`SampleFormat = 3`) grayscale and 3-channel RGB** decode **and
  encode** (`EncodePixelFormat::{GrayF16, GrayF32, GrayF64, RgbF16,
  RgbF32, RgbF64}`, 16-/32-/64-bit), and the **floating-point predictor
  (`Predictor = 3`)** is
  reversed on decode for 16-/32-/64-bit float grayscale and RGB across
  strip / tile and chunky / per-plane layouts, and **written** on encode
  for the same 16-/32-/64-bit widths across strip / tile / BigTIFF /
  multi-page, and float RGB additionally encodes in
  `PlanarConfiguration = 2` (three component planes, §14 float predictor
  per-plane) — see the SampleFormat / Predictor notes above. f16 pages
  carry raw IEEE 754 binary16 bit patterns (Rust has no half type); the
  public `f32_to_f16_bits` (round-to-nearest-even narrowing) and
  `f16_bits_to_f32` (exact widening) helpers convert. The float
  encode + Predictor = 3 writer gives the subsystem a fully
  binary-independent self-roundtrip oracle. Remaining float gaps:
  float palette / CMYK / YCbCr / CIELab (no defensible display
  mapping — precise typed errors on both sides). `Predictor = 3` over
  non-float or non-16/32/64-bit data is rejected per the §14 "the reader
  must give up" rule.

## Registration

```rust
let mut codecs = oxideav_core::CodecRegistry::new();
let mut containers = oxideav_core::ContainerRegistry::new();
oxideav_tiff::register(&mut codecs, &mut containers);
```

## Fuzzing

A `cargo-fuzz` decoder target lives at `fuzz/fuzz_targets/decode.rs`.
It drives arbitrary bytes through `decode_tiff`, `decode_tiff_all`,
`parse_header`, `parse_ifd`, and the four public compression
unpackers (`unpack_packbits` / `unpack_lzw` / `unpack_deflate` /
`unpack_zstd`). The
contract under test is decoder-only panic-freedom — every public
surface must return a `Result` for any input rather than abort,
debug-overflow, OOM, or OOB-index.

Run with nightly + cargo-fuzz:

```sh
cargo +nightly fuzz run decode -- -max_total_time=60
```

The harness has caught and pinned several panic vectors as regression
tests: an LZW first-after-Clear non-leaf code forming a self-referential
prefix chain (`src/compress.rs`), a BigTIFF `first_ifd_offset = u64::MAX`
that overflowed slice math (`src/ifd.rs`), and an attacker-claimed
`ImageWidth × ImageLength` driving a multi-exabyte upfront allocation
(the `src/decoder.rs` `MAX_IMAGE_PIXELS` gate). Deflate output is capped
to bound zip-bomb expansion. Regression tests live in
`tests/decode_fuzz_regressions.rs` plus inline `compress` / `ifd` test
modules so the panic-freedom checks survive in CI even when the fuzzer
isn't being driven.

## Benchmarks

A Criterion bench harness lives at `benches/lzw.rs` covering the
TIFF/LZW encoder hot path (`compress::pack_lzw`). Run with:

```sh
cargo bench -p oxideav-tiff --bench lzw
```

`pack_lzw` uses a flat-array trie (three `[u16; 4096]` / `[u8; 4096]`
arrays — `first_child`, `next_sibling`, `suffix`) for its dictionary
lookup. The bitstream output is identical to the simpler hash-map
formulation (same greedy match, same code-width bump points, same
Clear-on-fill timing), so it is invisible to any decoder, but throughput
on representative image-like strips is far higher: the four bench
scenarios (`lzw_random_64k`, `lzw_repeating_motif_64k`,
`lzw_zeros_256k`, `lzw_natural_image_64k`) range from a modest gain on
random data (no prefix reuse, worst case) up to roughly 20x on a natural
256×256 8-bit greyscale image fixture, where the trie's child lists for
common short prefixes amortise across many matches. Numbers move with
host hardware; the value is the relative cost across scenarios.

## License

MIT. See [LICENSE](LICENSE).
