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

`Predictor = 1` (no prediction) and `Predictor = 2` (horizontal
differencing, per-component for `SamplesPerPixel > 1`) are both
supported. Strip- and tile-based layouts both decode (any number of
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
  `convert -compress jpeg` produces from PPM input. `oxideav-mjpeg`
  may hand the decoded full-resolution 3-component frame back either
  as three planar components or as a single packed interleaved
  `R G B` plane (stride = `width × 3`), depending on its build; the
  TIFF compositor accepts both shapes and produces the same `Rgb24`.
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
directions go through [`compcol`](https://crates.io/crates/compcol)'s
Zstandard codec (this round also migrated the Deflate path to
`compcol`'s zlib, dropping the previous deflate dependency).
Validation is three-layered (`tests/zstd_roundtrip.rs`): 17
binary-independent self-roundtrips (Gray8 / Gray16Le / Rgb24 /
Palette8 / Bilevel × predictor / planar / tiled-with-partial-edges /
BigTIFF / multi-page), hand-built classic-II fixtures whose strips
are hand-assembled RFC 8478 `Raw_Block` frames (wire bytes our
encoder never emits), and black-box cross-checks against `tiffcp`
(our `Compression = 50000` output transcoded to `-c none` by the
independent binary, and independently-produced `-c zstd` /
`-c zstd:2` files decoded by us — both compared pixel-exact).
`Compression = 50001` (WebP) from the same registry page remains
unimplemented.

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
surfaced as a precise typed error. Value `3` (IEEE floating-point) is
likewise rejected — the byte interpretation is a non-integer numeric
type the integer pipeline cannot render — enforcing the §SampleFormat
reader rule: "If the SampleFormat field is present and the value is
not 1, a Baseline TIFF reader that cannot handle the SampleFormat
value must terminate the import process gracefully." An absent field
defaults to unsigned (so every existing TIFF fixture decodes
byte-for-byte unchanged); non-uniform per-component values and
out-of-range values (≥ 5) are also rejected.

`Orientation` (tag 274, TIFF 6.0 §Orientation page 36) is inspected on
every IFD. The spec defines eight values mapping the stored 0th row /
0th column onto the displayed image: `1` is canonical (0th row = visual
top, 0th column = visual left-hand side), `2..=8` cover the remaining
horizontal-flip / vertical-flip / 90° / 180° / 270° / transpose /
antitranspose permutations a writer may declare, and the spec closes
with "Default is 1. Support for orientations other than 1 is not a
Baseline TIFF requirement." The decoder is one such Baseline-only
reader: it surfaces pixels in storage order without rotating or
mirroring them, so the canonical value `1` is the only orientation it
can honour without silently mis-rendering. An absent field defaults to
`1` (every existing TIFF fixture decodes unchanged); an explicit
`Orientation = 1` routes through the same path. Values `2..=8` are
surfaced as precise typed errors rather than silently treated as `1`
(which would yield a correctly-coloured but geometrically wrong
image), and values `0` / `≥ 9` are surfaced as invalid-data errors
because the spec lists `1..=8` only.

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
Per-value policy: `0` (unspecified data) and `2` (unassociated alpha —
the soft matte logically independent of the straight-stored color)
decode with the trailing extras skipped, so an RGB `SamplesPerPixel =
4` page renders its `R G B` triple; `1` (associated alpha) is surfaced
as a precise `Error::Unsupported` because the color components are
pre-multiplied by the alpha being dropped and rendering them verbatim
would be silently wrong; values `≥ 3` are `Error::InvalidData` because
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
| RGB            | 8         | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Rgb24`     |
| Palette        | 8         | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Palette8`  |
| **CIELab (3 chan)** | 8    | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::CieLab8` (writes PhotometricInterpretation = 8, SamplesPerPixel = 3, BitsPerSample = [8,8,8]) |
| **CIELab (1 chan, L\* only)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**             | `EncodePixelFormat::CieLabL8` (writes PhotometricInterpretation = 8, SamplesPerPixel = 1) |
| **CMYK (4 chan)** | 8     | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Cmyk32` (writes PhotometricInterpretation = 5, SamplesPerPixel = 4, BitsPerSample = [8,8,8,8], plus optional `InkSet = 1` / `NumberOfInks = 4`) |
| **YCbCr (3 chan, 4:4:4)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**               | `EncodePixelFormat::YCbCr24` (writes PhotometricInterpretation = 6, SamplesPerPixel = 3, BitsPerSample = [8,8,8], `YCbCrSubSampling = [1, 1]`, `YCbCrPositioning = 1`, `ReferenceBlackWhite = [0,255,128,255,128,255]` no-headroom full-range) |

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
`Palette8`) under None / PackBits / LZW / Deflate / ZSTD, with or without
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
Rgb24 / Palette8 (None / PackBits / LZW / Deflate / ZSTD, with and without the
predictor) and the full range of edge-tile geometries (exact-fit,
partial edges, non-square tiles, oversized single tile, 1-pixel
overhang).

Sub-byte (1-bit and 4-bit) chunky tiles also **decode** as of this
round (TIFF 6.0 §15 — see the Decode note above). Because `TileWidth`
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
edge padding would diverge from the trusted strip path. The
**encoder** still writes only byte-aligned (8-/16-bit) chunky tiles;
sub-byte tile *writing* (which needs sub-byte tile-row bit packing
with §15 edge replication) is a separate increment, so `Bilevel`
input continues to reject `tiling`.

Output is classic II little-endian TIFF, single-IFD via
[`encode_tiff`] or multi-page via [`encode_tiff_multi`]. Files
roundtrip through ImageMagick / `tiffinfo` / `tiffcp`; CCITT
outputs additionally validate by asking `tiffcp -c none` to
transcode our `Compression = 3` stream back to uncompressed and
checking the resulting pixels match the original input.

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
`Predictor = 2`, `PlanarConfiguration = 2`, tiled layout, and the
chroma-subsampled `YCbCrSubSampling` values are deferred to a future
round (the §21 data-unit shape changes shape under non-1:1
subsampling, so the encoder pins those flags off here and rejects the
combinations rather than emit something the decoder might mis-tile).
Hand-built classic-II fixtures carrying the same `(Y, Cb, Cr)` bytes
decode to the same `Rgb24` as the encoder's output across the four
accepted compressors; the IFD bytes are inspected byte-for-byte to
confirm tags 262 / 277 / 284 / 530 / 531 / 532 carry the documented
values.

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
  set) and T.6 / Group 4 (`Compression = 4`) **decode + encode**
  as of this round. The 2-D Pass / Horizontal / Vertical mode
  codes — transcribed clean-room from ITU-T T.4 §4.2 / T.6 into
  `docs/image/tiff/ccitt-t4-t6-fax-codes.md` §1 — drive the same
  READ algorithm both variants share. T.6 adds the
  "imaginary all-white first reference line" + no-EOL framing rule;
  T.4 2-D layers the EOL+tag-bit row separator on top of it. The
  optional uncompressed-mode extension (`0000001111` followed by
  Table 5/T.4 = Table 4/T.6 image-pattern + exit codes) **decodes**
  as of this round: when a 2-D row hits the `0000001xxx` (`xxx = 111`)
  entrance code, the READ loop switches to literal-pixel transmission
  — unary image-pattern codes (`z` whites + 1 black for `z ≤ 4`, the
  `000001` 5-white make-up, and the `0000001T`…`00000000001T` exit
  codes whose tag bit `T` gives the colour of the next coded run) —
  then resumes Pass/Horizontal/Vertical coding from the post-exit
  position. The dispatch no longer rejects `T4Options` bit 1 /
  `T6Options` bit 1 ("uncompressed mode allowed"), since those bits
  are writer-capability hints rather than per-stream requirements and
  the uncompressed segments are self-delimiting; the T.6 dispatch
  additionally rejects any *undefined* `T6Options` bit. Decode is
  validated through 7 end-to-end `run_roundtrip` fixtures (solid
  white, two rectangles, diagonal, wide run) × G4 / T.4-2D against
  `tiffcp -c g4` / `-c g3:2d` oracle outputs, plus in-crate unit
  tests covering the mode-code dictionary entries (V(0), VR/VL(1..3),
  Pass, Horizontal, Uncompressed-extension), the `first_change_after`
  reference-line walker, and the uncompressed-mode segment decoder —
  3 spec-exact hand-built T.6 fixtures (the image-pattern byte, the
  `000001` long-white make-up, and the exit-code `m` trailing-white
  field), 5 encode→decode self-roundtrips exercising every emit
  branch (solid white / black, alternating, all residual-length runs,
  and a segment that resumes 2-D coding after exit). Encode is
  validated through 11 in-crate ccitt-module self-roundtrips (every
  mode-code branch + the byte-aligned EOL pad + the LSB-first fill
  order), 13 binary-independent integration self-roundtrips in
  `tests/encode_ccitt_2d_roundtrip.rs` (solid white / black, two
  rectangles, diagonal, wide pattern × T.4 2-D + T.6, plus T4Options
  bit 2 byte-aligned EOL and Gray8 negative-path rejection), and
  black-box cross-checks against `tiffcp -c none` and `tiffinfo`
  (independent transcode + verbose-report inspection confirms
  `T4Options = 1` for T.4 2-D and `Group 4` for T.6). The production
  READ encoder does not *emit* uncompressed mode (it is a bit-rate
  control extension, never a compression win for facsimile content);
  a test-only `encode_uncompressed_segment` helper drives the
  spec-exact self-roundtrips that prove the decode path.
- JPEG-in-TIFF Compression = 6 (old-style, deprecated by TIFF Tech
  Note 2; decoder returns precise `Error::Unsupported`).
  Compression = 7 (new-style) **decodes** as of this round across
  Gray / RGB / YCbCr / CMYK photometrics; 12-bit precision (SOF1 with
  `P = 12`), arithmetic coding (SOF9 / SOF11), and
  `PlanarConfiguration = 2` JPEG remain unsupported.
- CIELab photometric interpretation (PhotometricInterpretation = 8)
  **decodes** and **encodes** as of this round (3-sample `L*a*b*`
  and 1-sample `L*`-only, 8-bit; both uncompressed and the
  byte-aligned compressors; encode composes with `Predictor = 2`,
  `PlanarConfiguration = 2` on `CieLab8`, tiled layout, and BigTIFF
  per the §23 / §14 / §15 spec rules).
- Zstandard `Compression = 50000` **decodes + encodes** as of this
  round (see the dedicated section above); the sibling de-facto
  registry value `Compression = 50001` (WebP — one VP8/VP8L
  bitstream per strip / tile) remains unimplemented and returns the
  generic unsupported-compression error.
- DNG / GeoTIFF / EXIF blob extraction
- Encoder-side planar (`PlanarConfiguration = 2`) writing is now
  supported for `Rgb24` under None / PackBits / LZW / Deflate / ZSTD (with or
  without `Predictor = 2`), both strip-based and tiled (one tile grid
  per component plane, §15); decode covers the same plus CCITT. Tiled
  write (chunky, §15) covers `Gray8` / `Gray16Le` / `Rgb24` /
  `Palette8` under None / PackBits / LZW / Deflate / ZSTD with the optional
  `Predictor = 2`. Encoder-side YCbCr covers both the chunky
  `YCbCrSubSampling = [1, 1]` 4:4:4 layout
  ([`EncodePixelFormat::YCbCr24`]) **and** the chroma-subsampled
  chunky writes ([`EncodePixelFormat::YCbCrSubsampled24`]) for the
  TIFF 6.0 §21 page 90 legal factor pairs `[2, 1]` / `[2, 2]` /
  `[4, 1]` / `[4, 2]` (the spec requires
  `YCbCrSubsampleVert <= YCbCrSubsampleHoriz`, so `[1, 2]` is rejected)
  as of this round, under None / PackBits / LZW / Deflate / ZSTD. The
  subsampled writer takes a full-resolution interleaved `(Y, Cb, Cr)`
  raster, box-averages the chroma per `sh × sv` block (the symmetric
  even-tap filter §21 page 92 associates with the centered
  `YCbCrPositioning = 1` it declares), and packs the §21 page 93
  "Ordering of Component Samples" data unit (`sv` rows of `sh` Y
  samples, then one Cb, then one Cr — the §21 page 94 `[4, 2]` worked
  example `Y00 Y01 Y02 Y03 Y10 Y11 Y12 Y13 Cb00 Cr00 …`). The
  matching **decode** side was completed this round too: the strip
  reader now sizes subsampled-YCbCr strips by the data-unit geometry
  (it previously assumed full-resolution rows and rejected the smaller
  strip as truncated), so uncompressed / byte-aligned subsampled YCbCr
  now decodes end-to-end, not only inside a JPEG-in-TIFF segment.
  ImageWidth / ImageLength must be integer multiples of the factors
  (§21 page 90); YCbCr planar / tiled / predictor combinations remain
  deferred (the data-unit packing is single-strip chunky here).

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
