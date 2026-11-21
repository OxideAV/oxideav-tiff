# oxideav-tiff

[![CI](https://github.com/OxideAV/oxideav-tiff/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-tiff/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-tiff.svg)](https://crates.io/crates/oxideav-tiff) [![docs.rs](https://docs.rs/oxideav-tiff/badge.svg)](https://docs.rs/oxideav-tiff) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Pure-Rust TIFF 6.0 image decoder + encoder + container for the
[`oxideav`](https://github.com/OxideAV/oxideav) framework.

Implements the *Aldus TIFF Revision 6.0 (June 1992)* baseline plus the
universally-deployed Part 2 extensions (LZW + Deflate, tiles, YCbCr /
CMYK photometrics, CCITT Modified Huffman + T.4 1-D), the multi-IFD
chain (multi-page), the Adobe Pagemaker 6.0 *BigTIFF* design
(8-byte offsets, magic 43), and the de-facto registry extensions
`Compression = 50000` (Zstandard) and `Compression = 50001`
(WebP-in-TIFF, via the `oxideav-webp` sibling crate's public API).
Spec-only clean-room: no external library source was consulted.

## Standalone use

The crate follows the OxideAV image-crate contract (`IMAGE_CRATE_API`):
the same small root vocabulary every `oxideav-<format>` crate exposes,
usable with `default-features = false` and no `oxideav-core`.

```rust
// Cargo.toml: oxideav-tiff = { version = "…", default-features = false }

let bytes = std::fs::read("in.tif")?;
if oxideav_tiff::probe(&bytes) {
    let info = oxideav_tiff::info(&bytes)?;          // header + IFD chain only
    println!("{}x{} {:?}, {} page(s)", info.width, info.height, info.format, info.frames);

    let img = oxideav_tiff::decode(&bytes)?;         // TiffImage, native layout
    let rgba: Vec<u8> = img.to_rgba8();              // tightly packed RGBA, 4 × width per row
    let (w, h) = (img.width(), img.height());

    let opts = oxideav_tiff::EncodeOptions::default()
        .with_compression(oxideav_tiff::TiffCompression::Lzw)
        .with_predictor(true);
    let out = oxideav_tiff::encode_rgba8(w, h, &rgba, &opts)?;
    std::fs::write("out.tif", out)?;
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

| Item | Notes |
|---|---|
| `probe(&[u8]) -> bool` | `II*\0` / `MM\0*` (classic) or `II+\0` / `MM\0+` (BigTIFF); no allocation. |
| `info(&[u8]) -> ImageInfo` | Width / height (after `Orientation`), the layout `decode` returns, `frames` (IFDs on the chain), `has_alpha`, `color`, `has_icc` / `has_exif` / `has_xmp`, plus TIFF extras: `bits_per_sample`, `samples_per_pixel`, `photometric`, `compression`, `planar`, `tiled`, `big_tiff`, `orientation`. Never reads a strip or tile. |
| `decode` / `decode_with(&DecodeOptions)` | First IFD as a `TiffImage` (native layout, `color` + `metadata` filled). |
| `decode_rgb8` / `decode_rgba8` | `RgbImage` / `RgbaImage { width, height, data }`, tightly packed. |
| `decode_all` / `decode_all_with` | Every IFD on the next-IFD chain as `Frame { image, delay: None, index, page_number, new_subfile_type }`. |
| `encode_all(&[Frame], &EncodeOptions)` | Multi-page file, one IFD per frame in order, each written as `encode` would (`PageNumber = (i, n)` + `NewSubfileType` bit 1 unless the frame sets its own); `decode_all(encode_all(frames)) == frames` is pinned. `encode_pages` / `EncodePage` is the depth form. |
| `decode_from<R: Read>` | Reads to end, then `decode`. |
| `encode(&TiffImage, &EncodeOptions)` | Single-page file; every native layout is written as itself (no silent conversion). |
| `encode_rgb8` / `encode_rgba8` | `Rgb24` page / RGB + one unassociated-alpha `ExtraSamples` (TIFF carries alpha; nothing is dropped). |
| `encode_to<W: Write>` | Streaming variant of `encode`. |
| `TiffImage { width, height, format, planes, color, metadata, palette }` | `PixelFormat` (= `TiffPixelFormat`), one packed `Plane { stride, data }`, `ColorInfo`, `Metadata { icc, exif, xmp, gamma }`, `Palette { entries: Vec<[u8; 4]> }` for `Pal8`. `new(..) -> Result`, `from_rgb8(..) -> Result` / `from_rgba8(..) -> Result` validate geometry (`InvalidData` on a short buffer); `to_rgb8()` / `to_rgba8()` are exact and infallible; `as_bytes()` / `into_raw()`. |
| `TiffError` (= `Error`) | `InvalidData`, `Unsupported`, `LimitExceeded`, `Io(std::io::Error)`. |

TIFF-specific depth keeps its own names: `decode_page` /
`decode_page_at` / `decode_pages` return a `Page { image, metadata:
TiffMetadata, layout: TiffFormatInfo }` (the TIFF 6.0 §8 descriptive
fields, resolution, orientation, page tags, raw structural tags), and
`EncodePage` / `encode_page` / `encode_pages` expose every pixel kind,
sub-IFD and page option the writer has (see *Format specifics*).

The pre-contract names (`decode_tiff`, `decode_tiff_at`,
`decode_tiff_all`, `decode_tiff_all_pages`, `encode_tiff`,
`encode_tiff_multi`, `DecodedTiff`, `TiffPlane`) remain for one release
as `#[deprecated]` wrappers; the decode wrappers flatten `Pal8` / `Rgba`
/ `Cmyk` to the `Rgb24` bytes the historical decoder emitted.

## Framework use

With the default `registry` feature the crate plugs into `oxideav-core`:

```rust
let mut ctx = oxideav_core::RuntimeContext::new();
oxideav_tiff::register(&mut ctx);   // codec (decoder + encoder) + container (demuxer, muxer, probe)
```

`register_codecs(&mut CodecRegistry)` / `register_containers(&mut
ContainerRegistry)` install the two halves separately; `make_decoder` /
`make_encoder` are the codec factories (the framework `Decoder` /
`Encoder` call the standalone functions above — one implementation).
`From<TiffImage> for VideoFrame`, `TiffImage::from_video_frame(&VideoFrame,
&CodecParameters)` and `TryFrom<(&VideoFrame, &CodecParameters)>` bridge
the two layers; `Pal8` travels with the frame's palette side-channel
(plus the palette-alpha record when an entry is not opaque), a
specified `ColorInfo` as its colour-signal side-channel, and the
image's metadata as the frame's blobs record. The
encoder's options schema (`compression`, `quality`, `predictor`,
`planar`, `tile`, `rows_per_strip`, `bigtiff`) maps onto
`EncodeOptions`; `quality` (0..=100) is the framework's advisory knob
and governs JPEG-in-TIFF only — `1..=99` the DCT quantiser scale,
`100` the lossless `SOF3` process, `0` the lowest DCT quality — in any
option order; every other scheme is lossless and ignores it (it never
switches the scheme).

**Declared layouts:** the registry encoder declares exactly the
thirteen layouts it codes on `CodecCapabilities::accepted_pixel_formats`
(`ACCEPTED_PIXEL_FORMATS`, ladder order: `Rgb24`, `Rgba`, `Rgb48Le`,
`Rgba64Le`, `RgbF32Le`, `RgbaF32Le`, `Cmyk`, `Pal8`, `Gray8`, `Ya8`,
`Gray16Le`, `Ya16Le`, `GrayF32Le`), writes each as itself with the
output label equal to the input, and refuses anything else — and every
layout × scheme pair the writer cannot code (WebP with anything but
`Rgb24` / `Rgba`; JPEG with `Pal8`, an alpha layout or float, or with
`Gray16Le` / `Rgb48Le` under the DCT process) — with a typed
`Unsupported` from `make_encoder`, never a silent conversion. A `Pal8`
frame whose palette-alpha record has a transparent entry is refused the
same way (a TIFF `ColorMap` holds no alpha).

| Declared input | Written as | Schemes |
|---|---|---|
| `Rgb24` | RGB, 8-bit | all; WebP; JPEG (DCT / lossless) |
| `Rgba` | RGB + `ExtraSamples = [2]` | all; WebP |
| `Rgb48Le` | RGB, 16-bit | all; JPEG lossless (`quality = 100`) |
| `Rgba64Le` | RGB + `ExtraSamples = [2]`, 16-bit | all |
| `RgbF32Le` / `RgbaF32Le` | RGB (+ `ExtraSamples = [2]`), `SampleFormat = 3`, 32-bit | all |
| `Cmyk` | `PhotometricInterpretation = 5` | all; JPEG |
| `Pal8` | `PhotometricInterpretation = 3` + `ColorMap` | all |
| `Gray8` | `PhotometricInterpretation = 1`, 8-bit | all; JPEG |
| `Ya8` / `Ya16Le` | `PhotometricInterpretation = 1` + `ExtraSamples = [2]`, 8 / 16-bit | all |
| `Gray16Le` | `PhotometricInterpretation = 1`, 16-bit | all; JPEG lossless |
| `GrayF32Le` | `PhotometricInterpretation = 1`, `SampleFormat = 3` | all |

("all" = `none` / `packbits` / `lzw` / `deflate` / `zstd`.)

**Metadata blobs** (core 0.1.40 `oxideav_core::blob`): the four
payloads TIFF carries are framework `MetadataBlob`s —
`BlobKind::ICC` = tag 34675 (a complete profile), `BlobKind::EXIF` =
the Exif + GPS IFDs as a TIFF-structured Exif block starting at the
byte-order mark (`Metadata::exif`), `BlobKind::XMP` = tag 700,
`BlobKind::IPTC` = tag 33723 — converted by `metadata_to_blobs` /
`metadata_from_blobs`. The demuxer puts the first page of each stream's
blobs on `CodecParameters::blobs`; the decoder attaches every page's
own blobs to its frame record (pages differ); the encoder writes the
union, a frame blob overriding a stream blob of the same kind
(`merge_blobs`). Other kinds are ignored (TIFF has no tag for them);
`SaveOptions`-level replacement is the gateway's job.

**Multi-page files:** the demuxer declares one video stream per
distinct page geometry (`width × height × layout` after `Orientation`,
first-appearance order — a uniform multi-page file is one stream), each
carrying the whole file as its single packet; the decoder of a stream
returns the pages whose native layout equals the stream's declared one,
one frame per page (`pts` = the page's position on the IFD chain), so
every frame's label is truthful (a decoder built without a geometry
returns every page). The first page must be decodable; a later page
outside the decode table is left out of every stream.

**Muxer:** one or more `tiff` streams (the gateway's one per picture
geometry). A single packet is written verbatim; several packets become
the pages of one multi-page file in write order — pages may differ in
geometry — each page re-encoded under its stream's options
(compression / predictor / planar / tile / bigtiff, the same option bag
the encoder parsed) with its own metadata blobs and TIFF 6.0 §8 strings
/ resolution carried along and `PageNumber = (i, n)` written. Streams
must agree on the BigTIFF variant. A lossy JPEG stream is quantised a
second time through the same tables (use `encode_all` / `encode_pages`
for a single-pass multi-page JPEG file); every other scheme reproduces
the pixels exactly. Identical packet payloads — a multi-stream file
remuxed straight from the demuxer — are written once, so a remux is
byte-identical. JPEG-in-TIFF *decode* (`Compression = 7` / `6`) routes
through `oxideav-mjpeg` and therefore needs `registry`; everything else
— including WebP-in-TIFF (`Compression = 50001`, via `oxideav-webp`'s
framework-free surface) and the in-crate JPEG-in-TIFF *encoder* — works
standalone.

## Supported layouts

Native layouts (`PixelFormat`): `Gray8`, `Gray16Le`, `Ya8`, `Ya16Le`,
`Rgb24`, `Rgb48Le`, `Rgba`, `Rgba64Le` (straight alpha), `Pal8` +
palette, `Cmyk`, `GrayF32Le`, `RgbF32Le`, `RgbaF32Le` (little-endian
`f32` samples). All are packed (one plane, `stride = width × bytes per
pixel`).

**Decode** — what each on-disk shape becomes (the full matrix is in
*Format specifics → Decode*):

| On disk | `format` |
|---|---|
| Bilevel / 2..8-bit gray (`WhiteIsZero` inverted, `TransparencyMask` 0xFF = interior), 8-bit CIELab / ICCLab L\* | `Gray8` |
| 9..64-bit gray (unsigned; signed via offset-binary map), 16-bit L\* | `Gray16Le` |
| Gray + alpha extra sample, ≤ 8 / > 8 bits | `Ya8` / `Ya16Le` |
| 1..8-bit RGB; YCbCr (any §21 subsampling, chunky / planar, JPEG); 8-bit CIELab / ICCLab; RGB with unspecified extra samples (dropped) | `Rgb24` |
| 9..64-bit RGB; 9..16-bit JPEG RGB / YCbCr; 16-bit CIELab / ICCLab | `Rgb48Le` |
| RGB + alpha extra sample (`ExtraSamples` 1 or 2), ≤ 8 / > 8 bits | `Rgba` / `Rgba64Le` |
| 1- / 2- / 4- / 8-bit palette (`ColorMap`) | `Pal8` (indices verbatim, unpacked) + `palette` |
| CMYK (`InkSet = 1`) at any width, with or without extra samples, uncompressed / byte-stream / 8-bit JPEG | `Cmyk` |
| IEEE float gray / RGB / RGB + alpha (`SampleFormat = 3`, 16 / 32 / 64-bit) | `GrayF32Le` / `RgbF32Le` / `RgbaF32Le` — samples as stored (half widened, double narrowed to `f32`), `WhiteIsZero` re-expressed as `1 − x`; no display scaling. `SMinSampleValue` / `SMaxSampleValue` are reported on `TiffFormatInfo` (`Page.layout`) |

Rules that apply across the table:

- **Width.** A sample narrower than its layout is widened by bit
  replication (so 0 and the maximum map to 0 and the maximum); a wider
  one keeps its top bits (a 32-bit sample keeps 16, a 16-bit CMYK ink
  keeps 8). TIFF 6.0 fixes no rule; a reader that rounds instead
  differs by at most one step.
- **Alpha.** The first `ExtraSamples` entry of kind 1 or 2 is the
  alpha. Associated alpha (kind 1, TIFF 6.0 §18: the colour is stored
  multiplied by it) is divided back out — `c × max / α`, rounded,
  clamped; `α = 0` gives colour 0 — so every alpha layout is straight.
  Unspecified extras (kind 0, or no `ExtraSamples` field) and extras
  past the alpha are dropped. CMYK, palette and float-gray pages hand
  back no alpha.
- **Signed integers** (`SampleFormat = 2`) flip the sign bit
  (offset-binary: the signed minimum is black) and then follow the
  unsigned rules, on any colour model.

`to_rgb8` / `to_rgba8` kernels: gray replicated; 16-bit samples keep
the high byte (the same reduction the `ColorMap` words get); the alpha
layouts drop / keep their straight alpha (high byte at 16 bits,
tone-scaled for `RgbaF32Le`); `Pal8` looks up the palette (an index
past it is black); `Cmyk` applies the TIFF 6.0 §16 inversion
`R = (255−C)(255−K)/255` (and likewise G / B, integer division);
`GrayF32Le` / `RgbF32Le` are tone-scaled by clamping each sample to
`[0, 1]` and scaling to 255 (round half-up; NaN / ±Inf render 0) — a
caller wanting the file-declared `SMin` / `SMax` mapping applies it to
the native plane. Alpha is `255` for every other layout.

**Encode** — `encode` writes every native layout as itself:

| `format` | On disk |
|---|---|
| `Gray8` / `Gray16Le` | `PhotometricInterpretation = 1`, 8 / 16 bits |
| `Rgb24` / `Rgb48Le` | `PhotometricInterpretation = 2`, 8 / 16 bits |
| `Rgba` / `Rgba64Le` | RGB + `ExtraSamples = [2]`, 8 / 16 bits |
| `Ya8` / `Ya16Le` | BlackIsZero + `ExtraSamples = [2]`, 8 / 16 bits |
| `Pal8` | `PhotometricInterpretation = 3`, 8-bit indices, `ColorMap` from the palette (`Error::Unsupported` if an entry is not opaque — TIFF palettes carry no alpha) |
| `Cmyk` | `PhotometricInterpretation = 5`, `InkSet = 1` |
| `GrayF32Le` / `RgbF32Le` / `RgbaF32Le` | `PhotometricInterpretation = 1` / `2` / `2` + `ExtraSamples = [2]`, 32 bits, `SampleFormat = 3` (`Predictor` = the floating-point predictor when enabled) |

Compression: `None` (default), `PackBits`, `Lzw`, `Deflate`, `Zstd`
(50000) for every layout; `Webp` (50001) for `Rgb24` / `Rgba`; `Jpeg`
(7, in-crate T.81 writer) for `Gray8` / `Rgb24` / `Cmyk` (and
`Gray16Le` / `Rgb48Le` lossless). Bilevel, 4-bit, signed, float, YCbCr
and CIELab pages are written through `EncodePage` / `encode_page`.
`decode(encode(img)) == img` (planes, palette, colour and metadata) is
pinned for every native layout × compression in
`tests/image_crate_api.rs`.

## Options

`DecodeOptions` (`Default` + `with_*`): `max_width`, `max_height`,
`max_pixels` (default 256 Mi), `max_bytes` (assembled sample bytes,
default 1 GiB) — `None` lifts a limit, `unlimited()` lifts all; checked
against the IFD's claims before any allocation, failing with
`Error::LimitExceeded`. `threads` (default 1 = serial) grants a budget
for strip / tile decompression. `strict` (default `false`) rejects the
`SampleFormat` single-entry shorthand on multi-sample images; every
other malformed layout tag is already an error in both modes.

`EncodeOptions` (`Default` + `with_*`): `compression`
(`TiffCompression`, default `None`; JPEG quality / process live in
`TiffCompression::Jpeg(JpegOptions)`), `predictor` (§14 horizontal
differencing; takes effect with `Lzw` / `Deflate` / `Zstd` — §14 defines
it for the LZW family, and a page written with `None` / `PackBits`
carries none), `planar` (`PlanarConfiguration = 2`), `tiling`
(`(TileWidth, TileLength)`, multiples of 16), `rows_per_strip`,
`bigtiff`, `embed_icc` / `embed_xmp` / `embed_exif` / `embed_iptc`
(default `true`),
`software` (tag 305), `resolution` (`PageResolution`), `threads`
(default 1 = serial) and `level` (Deflate / Zstandard effort, default
the coder's).

## Metadata and colour

`TiffImage::metadata` carries the ICC profile (tag 34675, verbatim),
the XMP packet (tag 700, verbatim), the IPTC-IIM record stream (tag
33723, verbatim) and `exif`: the page's Exif IFD
(34665) and GPS IFD (34853) re-serialised as one little-endian Exif
TIFF payload (IFD0 holding the two pointer tags, child entries copied
with their values re-laid out; IFD-typed pointers such as
Interoperability are dropped). `encode` parses such a payload — either
byte order — back into the child IFDs. `gamma` is always `None` (TIFF
has no gamma tag; its §20 `TransferFunction` is a table). The TIFF 6.0
§8 descriptive strings, resolution, orientation and page tags are on
`Page::metadata` (`TiffMetadata`).

`TiffImage::color` is TIFF 6.0 §20 colorimetry as H.273 code points:
`ColorInfo { range: Full, primaries, transfer: 2, matrix: 0 }`. An image
"has a colorimetric interpretation if and only if both the WhitePoint
and PrimaryChromaticities fields are present", so `primaries` is set
only when both are present and match an H.273 Table 2 row (BT.709 → 1,
BT.470 M → 4, BT.470 B/G → 5, BT.601 525 → 6, BT.2020 → 9, P3 DCI → 11,
P3 D65 → 12, ±0.0015); `transfer` stays unspecified (no code point
exists for a `TransferFunction` table), `matrix` is identity because
every layout the decoder hands back is RGB / gray / palette / CMYK, and
integer samples are full range. Absent colorimetry is
`ColorInfo::tiff_default()` (Full / 2 / 2 / 0). The encoder does not yet
write `WhitePoint` / `PrimaryChromaticities`, so `color` is not part of
the lossless round-trip pin.

## Limits

- 256 megapixels and 1 GiB of assembled samples per IFD by default
  (`DecodeOptions`), checked from the IFD tags before allocation; with
  `registry` the framework's `DecoderLimits` can only tighten them.
- Layouts outside the decode table are `Error::Unsupported` /
  `InvalidData`: non-uniform `BitsPerSample`, palettes wider than 8
  bits, YCbCr at other than 8 bits (outside JPEG), float CMYK / palette
  / CIELab, `Compression = 34925` (LZMA2) and the other unlisted
  schemes, deep CMYK JPEG; see *Backlog*.
- One decompressed tile is held to the same `max_bytes` limit as the
  whole image; a strip or tile may be as large as that allows (a page
  stored as a single strip decodes whatever its size).
- A sample wider than the layout that carries it is narrowed: 17..64-bit
  integers keep their top 16 bits, CMYK inks wider than 8 bits their
  top 8.
- Float samples decode to `f32` (`GrayF32Le` / `RgbF32Le`); a 64-bit
  double is narrowed. The 16- and 64-bit float encode kinds are on
  `EncodePage`.
- `PlanarConfiguration = 2` pages are re-interleaved into the packed
  layout on decode (`planes` always has one entry).

## Format specifics

### Decode

| Photometric    | Bit depth      | Compression                        | Output       |
| -------------- | -------------- | ---------------------------------- | ------------ |
| WhiteIsZero    | 1              | None / CCITT-MH / T.4-1D / **T.4-2D** / **T.6 (G4)** / PackBits / LZW / Deflate / **ZSTD** | `Gray8` |
| WhiteIsZero    | 4 / 8          | None / PackBits / LZW / Deflate / **ZSTD** | `Gray8` |
| WhiteIsZero    | 16             | None / PackBits / LZW / Deflate / **ZSTD** | `Gray16Le` |
| WhiteIsZero / BlackIsZero | 8 / 16 | None / PackBits / LZW / Deflate / **ZSTD** + **SampleFormat=2 (signed int)** | `Gray8` / `Gray16Le` (offset-binary display map) |
| WhiteIsZero / BlackIsZero | 16 / 32 / 64 | None / PackBits / LZW / Deflate / **ZSTD** + **SampleFormat=3 (IEEE float)** | `GrayF32Le` (native `f32` samples) |
| RGB (3 chan)   | 16 / 32 / 64   | None / PackBits / LZW / Deflate / **ZSTD** + **SampleFormat=3 (IEEE float)** | `RgbF32Le` (native `f32` samples) |
| BlackIsZero    | 1              | None / CCITT-MH / T.4-1D / **T.4-2D** / **T.6 (G4)** / PackBits / LZW / Deflate / **ZSTD** | `Gray8` |
| BlackIsZero    | 4 / 8 / 16     | None / PackBits / LZW / Deflate / **ZSTD** | `Gray8` / `Gray16Le` |
| **Transparency Mask** | 1       | None / CCITT-MH / T.4-1D / **T.4-2D** / **T.6 (G4)** / PackBits / LZW / Deflate / **ZSTD** | `Gray8` (interior = 0xFF, exterior = 0x00) |
| Palette        | 1 / 2 / 4 / 8  | None / PackBits / LZW / Deflate / **ZSTD** | `Pal8` + `palette` (ColorMap, high byte of each 16-bit word) |
| RGB (3 chan)   | 8              | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb24` |
| RGB (3 chan)   | 16             | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb48Le` |
| RGB (3 chan / 4 chan + §ExtraSamples) | 8 | **WebP-in-TIFF** (Compression=50001; VP8L lossless + VP8 lossy segments) | `Rgb24` / `Rgba` (ExtraSamples = [2]) |
| RGB (4 chan, §ExtraSamples = [2] unassociated alpha) | 8 | None / PackBits / LZW / Deflate / **ZSTD** | `Rgba` (straight alpha, verbatim) |
| RGB + extra samples, any other arrangement | 1..64 | None / PackBits / LZW / Deflate / **ZSTD** (8-bit also WebP) | `Rgba` / `Rgba64Le` when an extra is alpha (associated alpha divided out), else `Rgb24` / `Rgb48Le` |
| Gray + extra samples | 1..64 | None / PackBits / LZW / Deflate / **ZSTD** | `Ya8` / `Ya16Le` when an extra is alpha, else `Gray8` / `Gray16Le` |
| Gray / RGB at any other width (2, 3, 5..7, 9..15, 17..64 bits; signed or unsigned) | — | None / PackBits / LZW / Deflate / **ZSTD** | 8-bit layout up to 8 bits, 16-bit layout above (see *Supported layouts* for the widening rule) |
| RGB / CMYK, signed (`SampleFormat = 2`) | 8 / 16 | None / PackBits / LZW / Deflate / **ZSTD** | as unsigned after the sign-bit flip |
| RGB + alpha, IEEE float | 16 / 32 / 64 | None / PackBits / LZW / Deflate / **ZSTD** | `RgbaF32Le` |
| CMYK (4 chan)  | 8              | None / PackBits / LZW / Deflate / **ZSTD** | `Cmyk` (ink coverages verbatim; `to_rgb8` = §16 inversion) |
| CMYK, other widths or with extra samples | 1..64 | None / PackBits / LZW / Deflate / **ZSTD** | `Cmyk` (top 8 bits of each ink; extras dropped) |
| YCbCr (3 chan) | 8              | None / PackBits / LZW / Deflate / **ZSTD** (incl. **§21 chroma subsampling** `[2,1]`/`[2,2]`/`[4,1]`/`[4,2]`, chunky + planar, strip + tiled) / **JPEG-in-TIFF** (Compression=7, chunky + **TN2 planar**) | `Rgb24` |
| RGB (3 chan)   | 8              | **JPEG-in-TIFF** (Compression=7, chunky + planar) | `Rgb24`      |
| BlackIsZero / WhiteIsZero | 8   | **JPEG-in-TIFF** (Compression=7)   | `Gray8`      |
| CMYK (4 chan)  | 8              | **JPEG-in-TIFF** (Compression=7, chunky + planar) | `Cmyk`       |
| BlackIsZero / WhiteIsZero | **9..=16 (12-bit SOF1, 9..16-bit SOF3)** | **JPEG-in-TIFF** (Compression=7) + **old-style Compression=6 interchange** | `Gray16Le` (bit-replication widening) |
| YCbCr / RGB (3 chan) | **9..=16** | **JPEG-in-TIFF** (Compression=7, chunky + planar) | `Rgb48Le` |
| **CIELab / ICCLab (3 chan)** | 8 / 16 | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb24` / `Rgb48Le` (Lab → XYZ under the page's white → D65 → BT.709 → sRGB curve) |
| **CIELab / ICCLab (1 chan, L\* only)** | 8 / 16 | None / PackBits / LZW / Deflate / **ZSTD** | `Gray8` / `Gray16Le` |
| YCbCr (3 chan) with `ReferenceBlackWhite` / `YCbCrCoefficients` | 8 | None / PackBits / LZW / Deflate / **ZSTD** | `Rgb24` through the §20 / §21 transform the two fields define (JPEG pages use the default transform) |
| BlackIsZero / WhiteIsZero / RGB / YCbCr / CMYK | 8 | **Old-style JPEG** (Compression=6, TIFF 6.0 §22 — **both layouts**: interchange-format, and **tables-form** via T.81 Annex B marker synthesis; chunky + planar) | `Gray8` / `Rgb24` / `Cmyk` |

`Predictor = 1` (no prediction), `Predictor = 2` (horizontal
differencing, with an offset of `SamplesPerPixel` on chunky data) and
`Predictor = 3` (the **IEEE floating-point predictor** — significance-
ordered byte-plane de-interleave followed by an inclusive cumulative
byte sum, with the same `SamplesPerPixel` offset on chunky data) are
all supported. `Predictor = 2` is reversed at every sample width:
modular 8 / 16 / 32 / 64-bit words in file byte order (also over float
words, which is what other writers store when asked for a horizontal
predictor on float data), and — §14: "expand each component … perform
the horizontal differencing … repack" — modulo 2ⁿ on bit-packed
samples of any other width.
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
Predictor 1 vs 3 float TIFFs (f32/f64, LZW + Deflate, grayscale + tiled);
the chunky-RGB offset was wrong (1 instead of `SamplesPerPixel`) until
the black-box matrix caught it — see *Black-box matrix*. Strip- and
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
(RGB / CMYK / YCbCr) sees the same input shape — including
JPEG-in-TIFF (`Compression = 7`), whose planar form follows TN2's
"Special considerations for PlanarConfiguration 2" (single-channel
segments, subsampled chroma segments at scaled-down SOF dimensions).
Multi-page files walk the next-IFD chain
via [`decode_all`] / [`decode_pages`]. Both `II` (little-endian) and `MM`
(big-endian) byte orders are accepted, and both classic
32-bit-offset TIFF and BigTIFF (8-byte offsets, magic 43) parse.

#### JPEG-in-TIFF (Compression = 7)

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

**Deep precisions** decode per TN2's rule that the SOFn `P` field
"shall agree with the TIFF BitsPerSample field" (SOF1 permits 8 or
12; SOF3 lossless permits 2..=16): the 9..=16-bit range decodes for
grayscale (→ `Gray16Le`) and YCbCr / RGB (→ `Rgb48Le`), each raw code
value widened onto the 16-bit display extent by bit replication (the
4-bit grayscale display map generalised), the BT.601 matrix evaluated
at the stream's own precision. **`PlanarConfiguration = 2`** decodes
per TN2's "Special considerations for PlanarConfiguration 2": each
segment is a single-channel JPEG datastream, segments run plane-major
(`SamplesPerPixel × StripsPerImage` / `× TilesPerImage`), and a
subsampled chroma segment's SOF dimensions are scaled down by the
sampling factors (last strip rounded up; tile dimensions must divide).
Still precise `Error::Unsupported`: deep (>8-bit) CMYK and sub-8-bit
lossless precisions.
The deprecated TIFF 6.0 §22 "old-style" JPEG (`Compression = 6`)
decodes in both §22 layouts — see the next section.
JPEG-in-TIFF requires the default-on `registry` Cargo feature; with
`default-features = false` the JPEG decode paths return
`Error::Unsupported` (the §22 field validation and its precise
rejection errors still run in standalone builds).

#### Old-style JPEG (Compression = 6, TIFF 6.0 §22)

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
  [`decode_all`] / [`decode_pages`]. Byte-exact equivalence with the
  `Compression = 7` wrapping of the identical bitstream is asserted in
  `tests/decode_oldstyle_jpeg.rs`.

* **Tables-form layout — decodes** (via ISO 10918-1 marker
  synthesis). Without an interchange stream, §22 stores *raw* table
  payloads (64-byte zigzag-order quantization tables; Huffman tables
  as "16 BYTES of 'BITS'" + "VALUES") behind per-component offset
  arrays (`JPEGQTables` / `JPEGDCTables` / `JPEGACTables`), and each
  strip "points directly to the start of the entropy coded data (not
  to a JPEG marker)". The decoder rebuilds one complete datastream
  per strip using the T.81 / ISO 10918-1 Annex B marker syntax staged
  in `docs/image/jpeg/` — DQT / DHT / DRI / SOFn / SOS around the raw
  payloads and the strip's entropy data (SOF0 for `JPEGProc = 1`
  baseline; SOF3 for `JPEGProc = 14` lossless with the
  `JPEGLosslessPredictors` selection-value as `Ss` and
  `JPEGPointTransforms` as `Al`), deduping per-component table
  offsets onto the four T.81 destinations and synthesizing DRI from
  `JPEGRestartInterval`. Chunky (interleaved, YCbCr sampling factors
  from `YCbCrSubSampling`) and planar ("one JPEG scan per component",
  reduced chroma geometry) both decode, single- and multi-strip and
  **tiled** (§22 "Strips and Tiles": each tile "points directly to the
  start of the entropy coded data"; every tile is rebuilt at
  `TileWidth × TileLength` with the §15 edge padding clipped on
  composite — chunky, planar, and planar subsampled-chroma tiles at
  their reduced size); every synthesized stream then routes through
  the same segment decode + composite machinery as `Compression = 7`.
  Tables-form round trips are pinned by decomposing black-box `cjpeg`
  bitstreams (strips) and the crate's own tiled `Compression = 7`
  output (tiles) into §22 raw payloads and requiring byte-identical
  output against the `Compression = 7` wrap of the same bitstream.
  Still precise errors: per-component `Ss` / `Al` divergence within
  one interleaved scan, and the malformed-field set — the §22
  JPEGProc applicability table is enforced (baseline requires
  Q/DC/AC tables; lossless requires JPEGLosslessPredictors +
  JPEGDCTables), `JPEGProc` values other than 1 / 14 are rejected per
  "will be defined in the future", lossless predictor
  selection-values are range-checked (1..=7), and out-of-bounds /
  non-SOI interchange offsets are typed errors.

#### CIELab (PhotometricInterpretation = 8)

`PhotometricInterpretation = 8` (TIFF 6.0 §23 "CIE L\*a\*b\* Images")
and `= 9` (ICCLab) are decoded at 8 and 16 bits per the Lab update of
the *Adobe Photoshop TIFF Technical Notes*:

| | L\* | a\* / b\* |
|---|---|---|
| CIELab, 8-bit | unsigned, 0..255 ↦ 0..100 | signed byte = the value |
| CIELab, 16-bit | unsigned, 0..65535 ↦ 0..100 | signed word = 256 × the value |
| ICCLab, 8-bit | unsigned, 0..255 ↦ 0..100 | unsigned, value + 128 |
| ICCLab, 16-bit | unsigned, 0..65280 ↦ 0..100 | unsigned, 256 × value + 32768 |

The page is rendered to display RGB of the same depth (`Rgb24` /
`Rgb48Le`; `Gray8` / `Gray16Le` for an L\*-only page): Lab → XYZ by the
inverse of the §23 formulas under the page's reference white — the
`WhitePoint` field when present, else D50 ("WhitePoint: default is CIE
D50") — then the linear Bradford adaptation to D65 (ICC.1:2010 Annex
E.3), the XYZ → RGB matrix of the BT.709 primaries, and the sRGB
transfer curve. The page's `color` reports that result (H.273 primaries
1, transfer 13). A neutral Lab sample (a\* = b\* = 0) is neutral RGB
under any white. §23 leaves this step to the reader ("Since CIELAB is
not a directly displayable format, some conversion to RGB will be
required"), and readers do differ: two black-box readers are up to 42
levels apart on saturated colours of the matrix raster, and they do not
agree on the white of an untagged page either. (Until this round the
conversion used the §23 example matrix for NTSC primaries against a D65
white, which tinted neutrals.)

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

#### Zstandard (Compression = 50000)

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
(WebP) from the same registry page is implemented too — see the next
section.

#### WebP (Compression = 50001)

`Compression = 50001` is the pixel-codec sibling of the 50000
Zstandard registry extension, from the same de-facto registry page
transcribed in
[`docs/image/tiff/tiff-zstd-compression-50000.md`](docs/image/tiff/tiff-zstd-compression-50000.md)
(§1 registers both values; §3 fixes the shared per-strip / per-tile
discipline). Unlike the byte-stream schemes, WebP is a **pixel**
codec, so the carriage is codec-in-container: every strip or tile
payload is **one complete WebP still-image file** (RIFF `WEBP`
container holding a `VP8L` lossless or `VP8 ` lossy bitstream) whose
frame dimensions equal the segment geometry — the final strip of a
multi-strip page carries only the remaining rows, while §15 edge
tiles stay padded to the full `TileWidth` × `TileLength`. That exact
framing was pinned against independently produced black-box reference
samples committed under `tests/data/webp50001/` (generation formulas
reproduced in `tests/webp50001.rs`).

All WebP bitstream logic lives in the
[`oxideav-webp`](https://crates.io/crates/oxideav-webp) sibling
crate, consumed strictly through its public framework-free API
(`decode_webp_image` / `encode_webp_lossless`) with
`default-features = false` — so `Compression = 50001` works in this
crate's standalone (`default-features = false`) build too, and no
WebP parsing exists in this crate beyond handing the payload bytes
across and validating the returned frame geometry against the IFD.

Because the codec is pixel-aware, only the pixel shapes WebP can
represent compose: `PhotometricInterpretation = 2`,
`BitsPerSample = 8`, `SamplesPerPixel = 3` (RGB) or `4` (RGBA + one
§ExtraSamples entry), chunky. `PlanarConfiguration = 2` is undefined
for it, and a `Compression = 50001` IFD declaring any `Predictor` is
rejected per the §14 reader rule — the §14 transforms are byte-stream
constructs that never compose with a pixel codec. Decode returns
`Rgb24` (the crate's §ExtraSamples policy renders the leading RGB
triple of an RGBA page); RGBA payload alpha is preserved on the wire
and reachable through the raw strip payload.

On the encode side `TiffCompression::Webp` writes one **lossless**
`VP8L` file per strip / tile for `EncodePixelFormat::Rgb24` /
`::Rgba32` input, so a `Compression = 50001` page round-trips
pixel-exact like every other scheme this encoder emits. Multi-strip
(`PageExtras::rows_per_strip`), §15 tiling (edge tiles padded), BigTIFF
and the multi-page chain all compose; `predictor` / `planar` /
non-RGB(A) inputs are rejected at plan time with precise errors.
WebP's own still-image ceiling (16384 per side) bounds a single
segment; larger pages should be split into strips or tiles within
that bound. Validation is three-layered (`tests/webp50001.rs`):
independent-fixture cross-reads (single-strip, multi-strip with a
short final strip, RGBA, partial-edge tiles, and a `VP8 ` lossy file
asserted byte-exact against a direct `oxideav-webp` decode of the
extracted payload — the YUV→RGB display conversion of a lossy frame
is reader-defined, so cross-reader pixel values legitimately differ),
webp-vs-none self-roundtrips across every layout axis (including
alpha carriage verified by re-decoding our emitted RIFF payload), and
hostile-IFD rejections (declared Predictor, frame/IFD geometry
mismatch).

#### Transparency Mask (PhotometricInterpretation = 4)

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
(LSB-first) are both accepted. `FillOrder` is "the logical order of
bits within a byte": for the two cases TIFF 6.0 page 32 recommends —
`BitsPerSample = 1`, uncompressed or CCITT-compressed — the bit order
is honoured where the bits are consumed. Page 32 warns that anything
else is "potentially ambiguous"; other writers do produce such pages
(one black-box tool tags every file it writes `FillOrder = 2`), and
what they store is every byte of the strip / tile data bit-reversed,
compressed or not, at any sample width, WebP segments included. That
is how they are read here. A JPEG segment that starts with its SOI
marker as stored is not reversed (that tool stores JPEG data plainly
under the same tag); old-style JPEG under `FillOrder = 2` is refused.

`SampleFormat` (tag 339, TIFF 6.0 §SampleFormat page 80) is inspected
on every IFD. Value `1` (unsigned integer, the spec default) and value
`4` (undefined — "treat … as if the field were not present, i.e. as
unsigned integer data") are unsigned integers.

Value `2` (two's-complement **signed integer**) decodes at every
integer width and on grayscale, RGB and CMYK pages. §SampleFormat notes
the size "is still done by the BitsPerSample field" and that
SMinSampleValue / SMaxSampleValue default to "the full range of the
data type", so the samples are rendered through the order-preserving
**offset-binary** map: the sign bit is flipped (signed minimum → 0,
stored 0 → the midpoint, signed maximum → the unsigned ceiling), after
which the unsigned rules apply (widening, `WhiteIsZero` inversion). A
signed palette index is refused.

Value `3` (IEEE floating-point) decodes for grayscale and RGB at the
three IEEE 754 widths — 16-bit half, 32-bit single, 64-bit double — to
the native `GrayF32Le` / `RgbF32Le` plane, or `RgbaF32Le` when an extra
sample is alpha: the samples as stored (a half widened losslessly, a
double narrowed), `WhiteIsZero` re-expressed as `1 − x`, no display
scaling. The SMinSampleValue / SMaxSampleValue pair (tags 340 / 341) is
reported on `TiffFormatInfo` (`Page.layout`) for callers who want that
mapping; `to_rgb8` / `to_rgba8` clamp each sample to `[0, 1]` and scale
to 255 (NaN / ±Inf render 0). Float pages take `Predictor = 3` or
`Predictor = 2` (word differencing). Float palette / CMYK / YCbCr /
CIELab pages are refused per the §SampleFormat reader rule ("must
terminate the import process gracefully"), as are non-uniform
per-component values and values ≥ 5. An absent field is unsigned.

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
pixel, stored by convention as the last components of each pixel. The
count must leave a colour-component tally the photometric defines (3
for RGB / YCbCr, 4 for CMYK, 1 for the grayscale / palette / mask
photometrics, 3-or-1 for CIELab per §23) — any other arithmetic is
`Error::InvalidData`; values `≥ 3` are `Error::InvalidData` too (the
spec lists `0..=2`). Per kind:

- `2` (unassociated alpha, "a soft matte"): the colour is stored
  straight; the page decodes to the alpha layout verbatim.
- `1` (associated alpha): §18 stores the colour multiplied by the alpha
  ("Cr = Cover * Aover … which is exactly the pre-multiplied color;
  i.e. what is stored in the image"). The alpha is kept and divided
  back out — `c × max / α`, rounded and clamped, and `α = 0` gives
  colour 0 ("If A is zero, then the color components should be
  interpreted as zero") — so the result is the same straight-alpha
  layout. (Earlier releases dropped the alpha and returned the
  pre-multiplied colour as opaque RGB.)
- `0` (unspecified), extras past the first alpha, and trailing samples
  of a page with no tag 338 at all: dropped.

Gray + alpha is `Ya8` / `Ya16Le`, RGB + alpha `Rgba` / `Rgba64Le` /
`RgbaF32Le`; CMYK, palette and float-gray pages hand back no alpha.

### Metadata & format introspection

Every decode result carries the descriptive and structural tags that sit
alongside the pixels, so a caller can read back exactly what a writer
stored without re-walking the IFD. `decode_page` / `decode_page_at`
return a `Page` whose `metadata: TiffMetadata` and
`format: TiffFormatInfo` fields expose:

| Field group | Tags | Reported as |
| ----------- | ---- | ----------- |
| §8 ASCII descriptive | DocumentName (269), ImageDescription (270), Make (271), Model (272), PageName (285), Software (305), DateTime (306), Artist (315), HostComputer (316), Copyright (33432) | `Option<String>` each (lossy, NUL-trimmed) |
| Resolution | XResolution (282), YResolution (283) as raw `(num, den)` RATIONALs; ResolutionUnit (296) | `Option<(u32, u32)>` / `Option<ResolutionUnit>` |
| Page structure | Orientation (274), PageNumber (297), NewSubfileType (254), SubfileType (255) | `Option<u16>` / `Option<(u16, u16)>` / `Option<u32>` |
| Format (`TiffFormatInfo`) | PhotometricInterpretation (262), Compression (259), BitsPerSample (258), SamplesPerPixel (277), PlanarConfiguration (284), Predictor (317), FillOrder (266), SampleFormat (339), tiled-vs-stripped layout (TileWidth/Length 322/323 or RowsPerStrip 278) | raw tag codes / `Vec<u16>` / `bool` / `Option<(u32, u32)>` |
| Registered opaque payloads | XMP packet (700), embedded ICC profile / InterColorProfile (34675), IPTC-IIM record stream (33723) | `Option<Vec<u8>>` each, byte-for-byte |

Extraction is **total**: a malformed informational entry (wrong field
type, truncated RATIONAL, out-of-range enum, over-long / unterminated /
non-UTF-8 ASCII) leaves that one field empty and never gates the pixel
decode or panics. `decode_pages` returns one `Page` per
IFD so the metadata travels with each page of a multi-page file.

On the write side the encoder's `PageExtras` emits every §8 ASCII field,
the resolution triple, Orientation, PageNumber and the NewSubfileType
bits, interleaved into the IFD in the §2-required ascending tag order —
so the whole §8 metadata surface round-trips end to end.

#### ICC profile (34675) & XMP packet (700)

Neither tag is defined by TIFF 6.0 itself — tag 34675 (InterColorProfile)
comes from the TIFF/EP (ISO 12234-2) assignment and tag 700 (XMP) from
the Adobe XMP Specification Part 3; both are implemented per the trace
doc [`docs/image/tiff/tiff-icc-xmp-tags.md`](docs/image/tiff/tiff-icc-xmp-tags.md).
Decode accepts either registered field type (BYTE 1 / UNDEFINED 7 —
1-byte opaque elements both ways, `Count` = exact payload length,
inline value-slot payloads included) and surfaces the bytes **verbatim**
in `TiffMetadata::icc_profile` / `TiffMetadata::xmp`: the file's
`II`/`MM` byte order applies to the IFD entry's integer fields only,
never to the payloads (an ICC profile is internally big-endian,
always). The ICC profile's own big-endian size field at offset +0 is
the authoritative length and must equal the IFD `Count`; a mismatch is
malformed and drops the field without gating the pixel decode. Encode
writes both via `PageExtras::icc_profile` (UNDEFINED) / `PageExtras::xmp`
(BYTE) in the §2 ascending tag positions (700 before Copyright; 34675
between the Exif and GPS pointers), validating the ICC header/size
precisely before writing. Round-trips byte-exact across classic +
BigTIFF, strip/tile/planar layouts and per page of a multi-page chain;
black-box interop (`tests/icc_xmp_blackbox.rs`) proves both directions
against `magick`, `tiffcp` and `tiffdump` as opaque validator
processes.

#### IPTC record stream (33723)

Tag 33723 carries the IPTC-IIM dataset stream (`1C <record> <dataset>
<len> …`) as an opaque byte string — the same carriage as the XMP
packet: decode accepts BYTE or UNDEFINED, `Count` is the exact payload
length, the bytes are surfaced verbatim in `TiffMetadata::iptc` /
`Metadata::iptc` and never byte-swapped; encode writes
`PageExtras::iptc` / `Metadata::iptc` as UNDEFINED between Copyright
(33432) and the Exif pointer (34665), refusing an empty payload. The
datasets are not interpreted. `tests/iptc_roundtrip.rs` pins the
round-trip, both field types, the ordering and a `tiffdump` listing.

### Tag catalogues: Exif, GPS, GeoTIFF

`Page::exif` is the typed view of the directories a page points at
(`tags::ExifData`): the Exif IFD (pointer tag 34665), the GPS Info IFD
(34853) and the Interoperability IFD (40965, inside the Exif IFD), each
as a list of `TagEntry { tag, name, value }` in ascending tag order.
Names come from the Exif 3.1 catalogues (CIPA DC-008 Tables 8, 9 and
14, §4.6.8); a tag the standard does not list keeps `name = None` and
its decoded value. `TagValue` covers every field type the directories
use, including the Exif 3.0 UTF-8 type 129. Derived accessors:
`exposure_time`, `f_number`, `photographic_sensitivity`,
`focal_length`, `date_time_original`, `lens_model`, `exif_version`,
`pixel_dimensions`, and `gps_position` — latitude / longitude in signed
decimal degrees from the degrees / minutes / seconds RATIONALs and
their `Ref` fields, altitude negated for `GPSAltitudeRef` 1 and 3
("below" the ellipsoid / sea level). `ExifData::parse` reads the same
TIFF-structured block `Metadata::exif` carries (with or without the
`Exif\0\0` identifier), so it also serves the Exif block of any other
container.

`Page::geotiff` (`tags::GeoTiff`, OGC 19-008r4 §7) is the
georeferencing of a page: the GeoKey directory of tag 34735 resolved
key by key — a SHORT stored in the entry itself, SHORTs read back from
the directory, DOUBLEs from tag 34736, ASCII from tag 34737 (the `|`
terminator stripped) — with the 1.1 key names of Annex E, plus
`ModelPixelScaleTag`, `ModelTiepointTag` and `ModelTransformationTag`.
`model_type`, `raster_type`, `projected_crs`, `geodetic_crs` and
`raster_to_model(i, j)` (matrix, or tiepoint + scale with the
Requirement 10.4 sign convention) are the shortcuts. CRS codes are
reported, not looked up.

Verification: Exif / GPS directories laid out by another tool (`sips`,
from a JPEG carrying the block) read back field for field in
`tests/exif_geotiff_tags.rs`; the GeoTIFF reader has only met
hand-built directories (no tool on hand writes the tags).

### Encode

| Photometric    | Bit depth | Compression                                                 | API call                |
| -------------- | --------- | ----------------------------------------------------------- | ----------------------- |
| WhiteIsZero    | 1         | None / CCITT-MH / T.4-1D / **T.4-2D** / **T.6 (G4)** (both with **opt-in uncompressed-mode emission**, §11 options bit 1) / PackBits / LZW / Deflate / **ZSTD** | `EncodePixelFormat::Bilevel` |
| **Transparency Mask** | 1  | None / CCITT-MH / T.4-1D / PackBits / LZW / Deflate / **ZSTD** | `EncodePixelFormat::TransparencyMask` (sets PhotometricInterpretation = 4 and NewSubfileType bit 2) |
| BlackIsZero    | 8 / 16    | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Gray8` / `::Gray16Le` |
| **BlackIsZero** | 4        | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled** (nibble-granularity edge replication), **`Predictor = 2`** (§14 nibble differencing mod 16), BigTIFF | `EncodePixelFormat::Gray4` (packed high-nibble-first raster, rows byte-padded) |
| **BlackIsZero signed** (SampleFormat = 2) | 8 / 16 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`Predictor = 2`**, BigTIFF, multi-page | `EncodePixelFormat::GrayI8` / `::GrayI16` (two's-complement samples + SampleFormat = 2 tag; decoder renders via the offset-binary map) |
| **BlackIsZero float** (SampleFormat = 3) | **16** / 32 / 64 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, BigTIFF, **`Predictor = 3`** | `EncodePixelFormat::GrayF16` / `::GrayF32` / `::GrayF64` (writes SampleFormat = 3; the §14 floating-point predictor; f16 = raw binary16 bit patterns + `f32_to_f16_bits` helper) |
| RGB            | 8         | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Rgb24`     |
| **RGB / RGBA** | 8         | **WebP-in-TIFF** (Compression=50001, lossless VP8L per segment), strip / multi-strip / **§15 tiled**, BigTIFF, multi-page | `EncodePixelFormat::Rgb24` / `::Rgba32` + `TiffCompression::Webp` (predictor / planar rejected — pixel codec) |
| **RGB + alpha/extra (§ExtraSamples)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`PlanarConfiguration = 2`** / **`Predictor = 2`**, BigTIFF | `EncodePixelFormat::Rgba32` + `ExtraSampleKind` (writes SamplesPerPixel = 4 and ExtraSamples = 0/1/2, tag 338) |
| **RGB**        | 16        | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`PlanarConfiguration = 2`** / **`Predictor = 2`**, BigTIFF | `EncodePixelFormat::Rgb48` (BitsPerSample = [16,16,16] little-endian — encode parity for the `Rgb48Le` decode path) |
| **RGB float** (SampleFormat = 3) | **16** / 32 / 64 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, BigTIFF, **`Predictor = 3`**, **`PlanarConfiguration = 2`** | `EncodePixelFormat::RgbF16` / `::RgbF32` / `::RgbF64` (writes SampleFormat = 3, SamplesPerPixel = 3) |
| Palette        | 8         | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Palette8`  |
| **Palette**    | 4         | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`Predictor = 2`**, BigTIFF | `EncodePixelFormat::Palette4` (48-SHORT §"Palette Color Images" ColorMap) |
| **CIELab (3 chan)** | 8    | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::CieLab8` (writes PhotometricInterpretation = 8, SamplesPerPixel = 3, BitsPerSample = [8,8,8]) |
| **CIELab (1 chan, L\* only)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**             | `EncodePixelFormat::CieLabL8` (writes PhotometricInterpretation = 8, SamplesPerPixel = 1) |
| **CMYK (4 chan)** | 8     | None / PackBits / LZW / Deflate / **ZSTD**                  | `EncodePixelFormat::Cmyk32` (writes PhotometricInterpretation = 5, SamplesPerPixel = 4, BitsPerSample = [8,8,8,8], plus optional `InkSet = 1` / `NumberOfInks = 4`) |
| **YCbCr (3 chan, 4:4:4)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, **`PlanarConfiguration = 2`** / **`Predictor = 2`** | `EncodePixelFormat::YCbCr24` (writes PhotometricInterpretation = 6, SamplesPerPixel = 3, BitsPerSample = [8,8,8], `YCbCrSubSampling = [1, 1]`, `YCbCrPositioning = 1`, `ReferenceBlackWhite = [0,255,128,255,128,255]` no-headroom full-range) |
| **YCbCr (3 chan, subsampled)** | 8 | None / PackBits / LZW / Deflate / **ZSTD**, strip or **§15 tiled**, chunky or **`PlanarConfiguration = 2`** (strip + **tiled**, per-plane `Predictor = 2`) | `EncodePixelFormat::YCbCrSubsampled24` (§21 data-unit packing for `[2,1]`/`[2,2]`/`[4,1]`/`[4,2]`; planar per the TN2-amended §21 reduced chroma-plane geometry) |
| **JPEG-in-TIFF** (Compression = 7, TN2) — BlackIsZero / RGB / YCbCr (4:4:4 + every §21 subsampling) / CMYK | 8 | **baseline `SOF0`** (Annex K.3 typical or K.2 optimal Huffman tables, K.1/K.2 quantisation × quality knob), strip / multi-strip / **§15 tiled** / **`PlanarConfiguration = 2`**, BigTIFF, multi-page; tables shared through **`JPEGTables`** (tag 347) or per segment | `Gray8` / `Rgb24` / `YCbCr24` / `YCbCrSubsampled24` / `Cmyk32` + `TiffCompression::Jpeg(JpegOptions)` (predictor rejected — TN2 defines none) |
| **JPEG-in-TIFF 12-bit** | 12 | **extended sequential `SOF1`** (K.2 optimal tables, `Pq = 1` quantisers) or **lossless `SOF3`** | **`EncodePixelFormat::Gray12` / `::Rgb36`** (JPEG-only inputs; decode renders `Gray16Le` / `Rgb48Le`) |
| **JPEG-in-TIFF lossless** | 8 / 12 / 16 | **`SOF3`**, any Table H.1 predictor 1..=7, sample-exact; strips of any height, tiles, planar | `Gray8` / `Rgb24` / `YCbCr24` / `Cmyk32` / `Gray12` / `Rgb36` / `Gray16Le` / `Rgb48` + `JpegOptions { process: JpegProcess::Lossless { predictor }, .. }` |

`TiffCompression::CcittRle` selects Modified Huffman
(`Compression = 2`, TIFF 6.0 §10), `TiffCompression::CcittT4OneD`
selects T.4 1-D (`Compression = 3`, §11) with an optional
`eol_byte_aligned` flag that writes `T4Options` bit 2,
`TiffCompression::CcittT4TwoD` selects T.4 2-D / Modified READ
(`Compression = 3` with `T4Options` bit 0 set) and
`TiffCompression::CcittT6` selects T.6 / MMR / Group 4
(`Compression = 4`). Both 2-D variants carry an opt-in
`uncompressed` flag: when set, every 2-D coded row is encoded both
as an ordinary READ row and as a Table 5/T.4 (= Table 4/T.6)
*uncompressed-mode* segment (entrance code + literal image patterns
+ exit code) and the cheaper form is emitted — the bit-rate-control
role the optional extension was defined for, so finely dithered rows
fall back to literal pixel transmission — and the file advertises the
extension via `T4Options` bit 1 / `T6Options` bit 1 per §11 (the
options tags are all-zero-bits otherwise). The 1-D writers all
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
The CCITT schemes compose with tiles as well (§15: "Tiles are compressed
individually, just as strips are compressed" — each tile is coded as a
page `TileWidth` wide and `TileLength` long). An independent transcode of our tiled
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
It composes with None / PackBits / LZW / Deflate / ZSTD, with the
CCITT schemes (one facsimile page per tile) and with BigTIFF; the §14
predictor (undefined for 1-bit) stays rejected. `tests/encode_tiled_bilevel_roundtrip.rs`
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
[`encode_page`] (or the contract's [`encode`]) or multi-page via [`encode_pages`]. Files
roundtrip through independent reference readers/transcoders; CCITT
outputs additionally validate by transcoding our `Compression = 3`
stream back to uncompressed with an independent tool and checking the
resulting pixels match the original input.

#### YCbCr write (PhotometricInterpretation = 6, chunky 4:4:4)

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
over the packed data-unit stream). **Tiled planar subsampled encodes
and decodes too**, on the geometry the TN2 "Changes to Section 21"
pin down: "each strip or tile covers the same image area despite
subsampling ... the total number of strips or tiles in the image is
the same for each component. Therefore strips or tiles of the
subsampled components contain fewer samples" — `TileOffsets` keeps
its `SamplesPerPixel × TilesPerImage` plane-major count, the tile
grid comes from the full-resolution dimensions, and a chroma tile
holds `TileWidth/sh × TileLength/sv` samples of the reduced chroma
plane (tile dimensions must be integer multiples of the factors; §14
predictor per-plane per-tile). Luma round-trips bit-exact; chroma
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

#### JPEG-in-TIFF write (Compression = 7)

`TiffCompression::Jpeg(JpegOptions)` writes TIFF Technical Note 2
"new-style" JPEG: every strip / tile is one complete ISO/IEC 10918-1
datastream produced by the crate's **own T.81 encoder**
(`src/jpeg_enc.rs`, transcribed from the staged
`docs/image/jpeg/T-REC-T.81-199209-I.pdf` — no dependency on
`oxideav-mjpeg` for writing), wrapped by the TN2 segment planner
(`src/jpeg_wrap.rs`).

```rust
use oxideav_tiff::{
    encode_page, rgb24_to_ycbcr24, EncodePage, EncodePixelFormat, JpegOptions,
    JpegProcess, JpegTablesLayout, PageExtras, TiffCompression,
};
# let (width, height) = (64u32, 64u32);
# let rgb = vec![0u8; 64 * 64 * 3];

let ycc = rgb24_to_ycbcr24(&rgb);          // §21 default coefficients
let mut page = EncodePage {
    width, height,
    kind: EncodePixelFormat::YCbCrSubsampled24 { pixels: &ycc, subsampling: (2, 2) },
    compression: TiffCompression::Jpeg(JpegOptions { quality: 90, ..JpegOptions::default() }),
    predictor: false, planar: false, tiling: None, bigtiff: false,
    extras: PageExtras::default(),
};
page.extras.rows_per_strip = Some(16);     // a multiple of the 4:2:0 MCU height
let tiff = encode_page(&page)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

* **Processes** (`JpegOptions::process`): `JpegProcess::Dct` writes
  baseline `SOF0` for 8-bit inputs and extended-sequential `SOF1`
  for the 12-bit `Gray12` / `Rgb36` inputs (T.81 F.1.5 categories,
  16-bit `Pq = 1` quantisers, K.2 optimal Huffman tables since the
  Annex K.3 typical tables cannot code the extended alphabet);
  `JpegProcess::Lossless { predictor }` writes `SOF3` with one of
  the seven Table H.1 predictors and is **sample-exact** at 8, 12
  and 16 bits (`Gray16Le` / `Rgb48` are lossless-only per TN2's
  "for SOF1, precision 8 or 12 is permitted").
* **Tables** (`JpegOptions::tables`): `JpegTablesLayout::Shared`
  (default) writes one `JPEGTables` field (tag 347, type UNDEFINED)
  holding the abbreviated table-specification stream and abbreviated
  image segments that reference it; `PerSegment` makes every strip /
  tile a self-contained interchange stream. `quality` (1..=100)
  scales the Annex K.1 / K.2 tables (50 = as printed, 75 = halved per
  the K.1 note, 100 = all ones); `optimize_huffman` derives per-image
  tables via the K.2 procedure (always on for 12-bit DCT and
  lossless); `restart_interval` (MCUs) writes a `DRI` segment into
  every strip / tile and `RSTm` markers between intervals (T.81
  E.1.3 / E.1.4 — DC predictions reset, 1-bit padding before the
  marker). Restarts are DCT-only at the TIFF level: the engine also
  writes H.1.1 row-aligned lossless restarts (exact under `djpeg`,
  libtiff and ImageMagick), but the crate's own `Compression = 7`
  reader does not restore the H.1.2.1 start-of-interval prediction
  rule yet, so lossless + restart is a precise `Unsupported`.
* **Colour** follows TN2's "color blind" rule: the JPEG components
  are whatever the TIFF fields say — `Rgb24` is stored as three
  untransformed R/G/B components under `PhotometricInterpretation =
  2`, `YCbCr24` / `YCbCrSubsampled24` under `= 6` with the SOF
  sampling factors equal to `YCbCrSubSampling` (luma `h×v`, chroma
  `1×1`; chroma box-decimated exactly like the uncompressed §21
  writer), `Cmyk32` as four components under `= 5`. Writers are
  expected to convert RGB to YCbCr themselves — `rgb24_to_ycbcr24`
  applies the §21 transformation with the default CCIR 601-1
  coefficients and the no-headroom `ReferenceBlackWhite` the page
  carries.
* **Layouts**: strips (`RowsPerStrip` must be a multiple of the MCU
  height `8 × YCbCrSubsampleVert` for the DCT processes unless the
  page is single-strip; the last strip's SOF height is the remaining
  rows), §15 tiles (`TileWidth` / `TileLength` multiples of the MCU
  size — 32 for `[4,x]` subsampling), `PlanarConfiguration = 2`
  (one single-component stream per plane; subsampled chroma planes
  at their scaled-down size, strips rounded up per TN2), BigTIFF and
  multi-page all compose. `Predictor` never composes (TN2 defines no
  differencing over JPEG); bilevel / palette / transparency-mask /
  CIELab / float inputs are precise rejections.
* **Validation**: every emitted layout is decoded by our reader and
  black-box by ImageMagick, `tiffcp -c none` (libtiff's JPEG decode,
  re-read by us), `tiffinfo` and `djpeg` on the `JPEGTables`-merged
  segments (the only route for 12-/16-bit and lossless). Own-decoder
  PSNR against the source is ≈ 50 dB at quality 90 on smooth
  content, external decoders agree with ours to ≈ 64–74 dB on
  non-subsampled pages; chroma-subsampled pages agree to ≈ 35 dB
  only because the two readers upsample chroma differently
  (nearest-neighbour splat vs. interpolation). The chroma-subsampled
  `PlanarConfiguration = 2` layout is the one TN2 says readers "are
  not required to support": libtiff mis-reads it, so it is validated
  plane segment by plane segment with `djpeg`. `[4,2]` subsampling
  writes and decodes externally but our own reader (via
  `oxideav-mjpeg`) does not accept 4×2 luma sampling yet.

#### BigTIFF write

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
unchanged, and the multi-IFD chain ([`encode_pages`]) is supported
as long as every page agrees on the variant (a mixed-variant chain
errors out — classic and BigTIFF IFD layouts are wire-incompatible).

#### Multi-strip write (RowsPerStrip)

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

#### Page extras: PageNumber, NewSubfileType, Exif/GPS child IFDs, SubIFDs tree

`EncodePage::extras` (`PageExtras`, all fields default-off) adds the
page-level metadata writes:

* **Orientation (274)** — §Orientation values 1..=8; the raster stays
  in stored layout, the tag drives reader-side re-orientation (this
  crate's decoder applies it on decode).
* **Resolution (282 / 283 / 296)** — `PageResolution` writes the
  §"Physical Dimensions" XResolution / YResolution RATIONALs plus
  ResolutionUnit (1 none / 2 inch / 3 centimeter, validated).
* **§8 ASCII fields** — ImageDescription (270), Software (305),
  DateTime (306, the spec's exact `"YYYY:MM:DD HH:MM:SS"` shape
  enforced), Artist (315), Copyright (33432). Written per §2 ASCII
  rules: 7-bit, no embedded NUL, writer-appended terminating NUL,
  `count = len + 1`.

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
  when it outgrows the value slot. `decode_page_at(file, offset)`
  decodes a child image from its tag-330 offset; `decode_all`
  ignores children by construction (they are not chained pages).

Round-trips are byte-inspected with the crate's own public
`parse_ifd` (verbatim aux values, sorted tags, zero child next-IFD,
inline-vs-spilled tag-330 array on classic + BigTIFF), and `tiffinfo`
(black-box) confirms the Page Number / multi-page / SubIFD-pointer
lines.

### Backlog (not yet implemented)

The compression schemes, photometrics, and layout features described
above are all implemented on both decode and encode where stated. The
remaining gaps are:

- **JPEG-in-TIFF encode gaps** — the deprecated `Compression = 6`
  (§22) layouts are decode-only (TN2 discourages writing them);
  `[4,2]` chroma subsampling writes but the crate's own reader
  (`oxideav-mjpeg`) rejects 4×2 luma sampling and does not apply the
  T.81 H.1.2.1 restart rule to 2-D lossless predictors (so lossless +
  `restart_interval` stays a precise `Unsupported` while DCT restarts
  write and read back); arithmetic coding is not written (TN2
  discourages it).
- **Deep (>8-bit) CMYK JPEG-in-TIFF and sub-8-bit SOF3 precisions** —
  precise `Error::Unsupported` (no deployed layout to validate
  against); likewise per-component `Ss` / `Al` divergence within one
  interleaved §22 tables-form scan (would need one synthesized scan
  per component).
- **Unstaged compression schemes** — `Compression = 34925` (LZMA2),
  LERC, JBIG, SGILog / LogLuv and the other codes with no description
  under the project's staged documents are refused with a typed error.
- **`ReferenceBlackWhite` / `YCbCrCoefficients` on JPEG pages** — the
  byte-stream YCbCr paths honour both fields; JPEG-in-TIFF pages are
  still converted with the default transform (CCIR 601-1, no headroom).
- **Tag catalogues: write side and DNG.** Exif / GPS / Interoperability
  directories and the GeoTIFF GeoKey directory are *read* typed (see
  *Tag catalogues*); writing goes through raw `AuxIfdEntry` lists
  (Exif / GPS) and there is no GeoTIFF writer. DNG raw pages
  (`PhotometricInterpretation` 32803 / 34892, the DNG predictors, the
  colour pipeline of the DNG chapters) are not decoded, and the CFA
  pattern tags are defined by a document (TIFF/EP) that is not staged.
- **Chunky-subsampled YCbCr × `Predictor = 2`** — §14 differencing has
  no defined shape over the packed §21 data-unit stream (a 2-D luma
  block then single Cb / Cr samples per unit), so the combination
  stays a precise rejection on both sides; every other subsampled
  layout combination (chunky/planar × strip/tiled, per-plane
  predictor) is implemented.
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

## Black-box matrix

`tests/blackbox_matrix.rs` generates its files at run time (nothing is
committed) and skips when the tools are absent. Counts below are from
one run with `magick` 7.1.2, the `tiffcp` / `tiffinfo` / `tiff2rgba` /
`raw2tiff` / `tiffset` / `tiffcrop` 4.7.2 tools, `ffmpeg` and `sips`.

**Foreign → ours — 5522 files, 5156 decoded and confirmed, 362 refused
as a known gap, 4 with no reader to compare against, 0 wrong.** Files
written by other software, decoded here and
by a black-box reader, compared sample-exact wherever the layout is
lossless (floats bit-exact; JPEG / chroma-subsampled / CIELab pages
within the bound stated next to each generator, because TIFF 6.0 §21 /
§23 and T.81 leave that reconstruction to the reader):

| Writer | Files | Pass | Known gap | Sweep |
|---|---:|---:|---:|---|
| `magick` | 2381 | 2219 | 162 | 59 sample kinds (unsigned at 1..16, 31, 32 and 64 bits, signed, half / single / double float, bilevel both polarities, palette 1 / 2 / 4 / 8-bit, gray + alpha, RGB(A) with associated / unassociated / unspecified extra, CMYK(A), YCbCr, CIELab 8 / 16) × None / PackBits / LZW / Deflate / Zstd / LZMA / WebP / JPEG / G3 / G4 × predictor 1 / 2 / 3 × strips (1, 7, all rows) / tiles (16², 32 × 48) × chunky / separate × both byte orders × classic / BigTIFF × `FillOrder`; JPEG sampling 1×1 … 4×2 at three qualities; sizes 1 × 1 … 640 × 400 |
| `tiffcp` | 2074 | 1870 | 200 | re-writes of 16 base pages: every `-c` scheme incl. G3 1-D / 2-D / fill, G4, JPEG (YCbCr 2 × 2 and RGB), `-r` 1 / 5 / 16, tiles 16² / 32 × 16 / 128², `-p separate`, `-B` / `-L`, `-8`, `-f lsb2msb` (every scheme of every page) |
| `raw2tiff` | 874 | 874 | 0 | byte / short / long / sbyte / sshort / slong / float / double × gray (both polarities) / gray + extra / RGB / RGB + extra / CMYK / CIELab × pixel / band order × None / PackBits / LZW / Deflate (± predictor) / JPEG; the tool tags every file `FillOrder = 2` |
| `ffmpeg` | 116 | 116 | 0 | 17 pixel formats (gray, gray + alpha, RGB(A) 8 / 16, palette, bilevel both polarities, YCbCr 4:4:4 / 4:2:2 / 4:4:0 / 4:2:0 / 4:1:1 / 4:1:0 with video-range `ReferenceBlackWhite`) × raw / PackBits / LZW / Deflate (32946) |
| `sips` | 21 | 21 | 0 | 8 / 16-bit gray, gray + alpha, RGB, RGBA × default / LZW / PackBits |
| `tiffset`, multi-page | 56 | 56 | 0 | all eight `Orientation` values on six layouts; five-page mixed chains (classic / BigTIFF, both byte orders, tiled); reduced-resolution pyramids |

The known gap is `Compression = 34925` (LZMA2) alone: no description
of the code or its stream framing is staged, so those files are
refused with a typed error. Four files (separate-plane YCbCr
JPEG) decode here but are refused by both black-box readers, so there
is nothing to compare them with.

**Ours → foreign — 14 110 files, 0 refused or mis-read.** Every layout
the writer emits (43 input kinds ×
24 scheme settings (None, PackBits, LZW, Deflate, Zstd, WebP, CCITT
RLE / G3 1-D / G3 2-D / G4 with and without fill, five JPEG DCT option
sets, the seven lossless-JPEG predictors) × predictor × chunky / separate × strips / `RowsPerStrip`
1 / 7 / 16 / tiles 16² / 32 × 48 × classic / BigTIFF, at six image
sizes from 1 × 1 to 300 × 200): 14 110 files, all accepted by
`tiffinfo -D` without a warning; 5 868 confirmed sample-exact by two
black-box readers, 6 522 by one, and 1 720 in layouts none of the readers
on hand reads at all:

- JPEG at 12 / 16-bit precision (both readers are 8-bit JPEG builds);
- `Predictor = 2` on 4-bit samples (TIFF 6.0 §14 describes it; the
  reader reports it unsupported);
- chroma-subsampled YCbCr in separate planes (§21; the black-box tools
  neither write nor read the reduced chroma planes);
- lossless-JPEG YCbCr and a few degenerate 1 × 1 JPEG pages the
  readers refuse.

Those are checked against this crate's own reader only.

What the matrix found and this round fixed: the floating-point
predictor on chunky RGB (wrong differencing offset, both directions),
the PackBits writer crossing row boundaries, the palette lost under
`Orientation`, CIELab rendered through the wrong primaries,
`ReferenceBlackWhite` ignored, odd-width subsampled JPEG refused,
Zstandard frames refused, a predictor written next to uncompressed
data — and every layout outside 1 / 4 / 8 / 16-bit unsigned that used
to be refused (see *Supported layouts*).

Never verified against an outside stream: GeoTIFF tags (no tool on
hand writes them; hand-built directories only), `SubIFDs` image trees,
old-style JPEG (`Compression = 6`, no current writer), CCITT
uncompressed-mode segments, `Compression = 2` (CCITT RLE) from another
writer, ICCLab pages, DNG raw pages, and whole-byte samples that are
not 8 / 16 / 32 / 64 bits wide (24, 40, 48, 56): TIFF 6.0 states the
byte order for 16- and 32-bit integers only; this crate reads such
samples in the file's byte order, and the one black-box writer that
produces 24-bit samples stores them in the opposite order, so there is
no outside reference for them.

## Fuzzing

Three `cargo-fuzz` targets live under `fuzz/fuzz_targets/`:
`decode.rs` (arbitrary bytes through every public decoder surface, the
demuxer's geometry-grouped streams and the registry decoder's one
frame per page), `registry_encode.rs` (the registry seam: the input
steers the framework encoder's layout / scheme / quality option bag and
supplies hostile metadata blobs on the stream and the frame record —
an Exif block the encoder parses into child IFDs, an ICC profile whose
size field it checks, XMP / IPTC it copies, a palette-alpha record —
and whatever it writes must decode through the registry decoder, the
demuxer and the muxer's multi-packet merge, which is also fed the raw
input as a hostile packet) and
`jpeg_roundtrip.rs` (the JPEG-in-TIFF writer → reader pair: the input
steers `TiffCompression::Jpeg`'s configuration and raster; every file
the writer produces must decode — lossless sample-exact — a one-byte
mutation of it must not panic the reader, and the bare T.81 engine
must reject arbitrary component geometry with typed errors). Its first
bounded run found that the codec returns a three-component lossless
frame as one packed `Y Cb Cr` plane, which the segment classifier only
accepted under `PhotometricInterpretation = 2`; packed YCbCr now
composites through the BT.601 matrix at 8 and deep precisions.

The decoder target `fuzz/fuzz_targets/decode.rs` drives arbitrary
bytes through the contract surface (`probe` / `info` / `decode` /
`decode_rgba8` / `decode_with` under tight limits / `decode_all`), the
`Page` depth API (`decode_page` / `decode_pages` / `decode_page_at`),
`parse_header`, `parse_ifd`, and the four public compression
unpackers (`unpack_packbits` / `unpack_lzw` / `unpack_deflate` /
`unpack_zstd`). The
contract under test is decoder-only panic-freedom — every public
surface must return a `Result` for any input rather than abort,
debug-overflow, OOM, or OOB-index.

`.github/workflows/fuzz.yml` runs the three targets daily for a bounded
budget. Run locally with nightly + cargo-fuzz:

```sh
cargo +nightly fuzz run decode -- -max_total_time=60
cargo +nightly fuzz run registry_encode -- -max_total_time=60
```

Round 477 ran `decode` (541 k runs) and `registry_encode` (162 k runs
over two sessions, one at full input length) for ten minutes each
after the blob / multi-page / muxer work landed; neither found a
crash.

After the round that added the general sample path, the tag readers,
the segment threads and the LZW rewrite, bounded foreground runs on the
final tree — `decode` 452 k executions over three sessions (the target
now also drives `ExifData::parse` and a three-thread decode),
`jpeg_roundtrip` 132 k, `registry_encode` 5.7 k — found no crash,
timeout or out-of-memory.

The harness has caught and pinned several panic vectors as regression
tests: an LZW first-after-Clear non-leaf code forming a self-referential
prefix chain (`src/compress.rs`), a BigTIFF `first_ifd_offset = u64::MAX`
that overflowed slice math (`src/ifd.rs`), and an attacker-claimed
`ImageWidth × ImageLength` driving a multi-exabyte upfront allocation
(the `DecodeOptions::max_pixels` gate), and an unknown-typed
colorimetry entry whose `count` sized a 3.5 GB float vector
(`Entry::as_f64_vec`, r466). Deflate and Zstandard output is capped
at the segment size the directory declares (itself bounded by
`DecodeOptions::max_bytes`). Regression tests live in
`tests/decode_fuzz_regressions.rs` plus inline `compress` / `ifd` test
modules so the panic-freedom checks survive in CI even when the fuzzer
isn't being driven.

## Benchmarks

### Throughput

`benches/throughput.rs` (`cargo bench --bench throughput`) times whole
pages — 2048 × 1536 (3.15 MP), a synthetic photo-like raster for the
continuous-tone rows and a text-like page for the bilevel ones — and
prints megapixels per second. One core unless a thread budget is named.

Before → after this round's performance work, serial, and the same
after-build with a budget of four threads. Each figure is the median of
three runs of the bench (itself a median of seven), old and new
binaries alternating, on an Apple-silicon machine shared with other
builds (load average 10 to 40 during the run):

| Layout | encode MP/s | encode, 4 threads | decode MP/s | decode, 4 threads |
|---|---:|---:|---:|---:|
| RGB8 uncompressed, 16-row strips | 6149 → 5836 | 5985 | 4477 → 5728 | 6304 |
| RGB8 PackBits, strips | 404 → 396 | 1081 | 2638 → 5028 | 6104 |
| RGB8 LZW, strips | 25 → 49 | 180 | 59 → 108 | 384 |
| RGB8 LZW + predictor, strips | 24 → 64 | 195 | 64 → 83 | 320 |
| RGB8 LZW + predictor, 256² tiles | 22 → 61 | 228 | 57 → 85 | 309 |
| RGB8 Deflate + predictor, strips | 12 → 12 | 38 | 122 → 123 | 446 |
| RGB8 Deflate level 1 + predictor, strips | — → 56 | 164 | — → 127 | 468 |
| RGB8 Deflate + predictor, 256² tiles | 10 → 11 | 35 | 125 → 120 | 434 |
| RGB8 Zstd + predictor, strips | 20 → 21 | 72 | 58 → 60 | 226 |
| Gray16 LZW + predictor, strips | 44 → 102 | 304 | 91 → 149 | 491 |
| Gray16 Deflate + predictor, strips | 11 → 13 | 42 | 197 → 210 | 631 |
| Bilevel CCITT G4, 64-row strips | 66 → 75 | 232 | 309 → 314 | 732 |
| Bilevel CCITT G3 1-D, strips | 787 → 846 | 1997 | 106 → 100 | 298 |

Serial: LZW encode 2.0× to 2.8×, LZW decode 1.3× to 1.8×, PackBits
decode 1.9×, uncompressed decode 1.3×; the rest is within run-to-run
noise. Four threads: 3.2× to 3.8× the serial figure on the LZW /
Deflate / Zstandard rows, in both directions. What changed:

- **LZW decode** keeps its string table as spans of the output: a table
  string is the previous string plus one byte, and it sits in the
  output already, so emitting a code is one block copy instead of a
  walk along a prefix chain, a reversal and a byte-by-byte push; codes
  are read through a 64-bit window. Clears no longer wipe anything.
- **LZW encode** looks `(prefix, byte)` up in a generation-stamped
  hashed table (one probe per input byte; a Clear is a counter bump)
  instead of walking a child list. Same code stream as before, bit for
  bit.
- **Strips and tiles** are no longer copied once more after
  decompression, and a tile is assembled row-span by row-span.
- **Opt-in threads.** `DecodeOptions::with_threads(n)` /
  `EncodeOptions::with_threads(n)` / `PageExtras::with_threads(n)` (and
  the framework's `ExecutionContext` through the registry codec) let up
  to `n` threads decompress / compress the strips or tiles of a chunky
  page. The default is serial; the result — decoded page or written
  file — is identical at every budget (pinned in
  `tests/threads_and_level.rs` and on every file of the black-box
  matrix). A threaded decode holds all decompressed segments of a page
  at once. Separate-plane pages, the general sample path and
  JPEG-in-TIFF stay serial.
- **Coder level.** `EncodeOptions::with_level` / `PageExtras::with_level`
  set the Deflate (0..=9, default 6) or Zstandard (1..=22, default 3)
  effort. Deflate encode is the slowest row by far and its time is the
  coder's: level 1 is 4.7× faster for a 4 % larger file on the bench
  raster (5 277 530 against 5 076 610 bytes). The default is unchanged.

Not improved: Deflate / Zstandard encode at their default levels (time
spent in the `compcol` coders), CCITT G3 decode, uncompressed encode.

### LZW encoder micro-bench

`benches/lzw.rs` (Criterion, `cargo bench --bench lzw`) isolates
`compress::pack_lzw` on four payloads (`lzw_random_64k`,
`lzw_repeating_motif_64k`, `lzw_zeros_256k`, `lzw_natural_image_64k`)
for A/B work on the dictionary.

## License

MIT. See [LICENSE](LICENSE).
