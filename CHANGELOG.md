# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Decoder: CMYK JPEG-in-TIFF (`Compression = 7`,
  `PhotometricInterpretation = CMYK (5)`, `SamplesPerPixel = 4`), per
  TIFF Tech Note 2 ("A JPEG-compressed TIFF file will typically have
  PhotometricInterpretation = YCbCr ... unless the source data was
  grayscale or CMYK"). Each strip / tile is a freestanding 4-component
  JPEG datastream (with the optional `JPEGTables` blob applied by
  reference as usual). `oxideav-mjpeg` consumes any Adobe APP14 marker
  inside the segment to pick the correct per-sample inversion (plain
  CMYK / Adobe-inverted CMYK / YCCK) and emits a single packed
  `C M Y K` plane (stride = `width × 4`) where `0 = no ink` — i.e.
  TIFF 6.0 §16's `InkSet = 1` convention. The TIFF compositor then
  converts the packed CMYK buffer to `Rgb24` using the same
  additive-RGB formula the uncompressed CMYK path uses
  (`R = (255 − C) × (255 − K) / 255`, etc.), so the integration
  matches `tiffinfo` / `magick` reference rendering bit-for-bit on the
  composite step. New `JpegPixelFormat::Cmyk8` enum variant +
  `composite_cmyk_to_rgb` public helper in `jpeg.rs`. Validated with
  `convert -colorspace CMYK -compress jpeg`. Strip + tile dispatch
  both reuse the existing per-segment merge / decode path; tiling
  works automatically. `PlanarConfiguration = 2` still returns
  `Error::Unsupported` (TN2 readers are not required to support it),
  and 12-bit / arithmetic-coded JPEG remain rejected at the
  `BitsPerSample / SOFn` gate.

- Encoder: tiled layout (TIFF 6.0 §15) via the new
  `EncodePage::tiling` field (`Some((tile_width, tile_height))`). When
  set, the encoder divides the image into a row-major grid of fixed-size
  tiles, compresses each independently (§15: "Tiles are compressed
  individually, just as strips are compressed"), and writes the
  `TileWidth` / `TileLength` / `TileOffsets` / `TileByteCounts` fields
  (tags 322 / 323 / 324 / 325) in place of the strip fields — §15: "When
  the tiling fields … are used, they replace the StripOffsets,
  StripByteCounts, and RowsPerStrip fields … Do not use both
  strip-oriented and tile-oriented fields in the same TIFF file." Both
  tile dimensions must be a non-zero multiple of 16 (§15's `TileWidth` /
  `TileLength` requirement); other values are rejected with a precise
  error. Boundary tiles are padded out to the tile geometry by
  replicating the last visible column / row (§15 "Padding": "Some
  compression schemes work best if the padding is accomplished by
  replicating the last column and last row instead of padding with
  0's"), so every tile is exactly `tile_w × tile_h` before compression
  and the decoder — which displays only the `ImageWidth × ImageLength`
  region — drops the padding. Tiles are emitted left-to-right then
  top-to-bottom (§15 `TileOffsets`); the `TileOffsets` / `TileByteCounts`
  LONG arrays go inline for a single-tile image and out-of-line when
  `TilesPerImage > 1`, reusing the same array machinery as the strip
  offsets. Supported on the byte-aligned chunky formats (`Gray8` /
  `Gray16Le` / `Rgb24` / `Palette8`) under None / PackBits / LZW /
  Deflate, with or without the §14 predictor (applied per-tile, matching
  the decoder's per-tile cumulative add). Tiling is rejected on
  `Bilevel` input (sub-byte tile slicing is unimplemented on both
  sides) and on the CCITT compressors (bilevel-only); it composes with
  `planar = true` on `Rgb24` (see the tiled-planar entry below).

  Validated by self-roundtrip across Gray8 / Gray16Le / Rgb24 /
  Palette8 under None / PackBits / LZW / Deflate (single-tile,
  exact-fit grids, and partial-edge grids on both axes), with and
  without the predictor, plus a multi-page chain mixing a tiled page
  with a strip page and an IFD-tag inspection (tile tags present with
  `count = TilesPerImage`, strip tags absent, ascending tag order).
  External black-box validation: `tiffcp -c none` transcodes our tiled
  Gray8 LZW and tiled RGB Deflate + Predictor=2 streams back to
  uncompressed TIFFs that re-decode to the original visible pixels
  (proving libtiff walks our tile grid, ordering, boundary padding, and
  per-tile differencing); ImageMagick reads our tiled RGB bit-exactly;
  and `tiffinfo` reports the tile geometry.

- Encoder: tiled `PlanarConfiguration = 2` write (`planar = true` +
  `tiling = Some(..)` on `Rgb24`), per TIFF 6.0 §15 + §"Planar-
  Configuration" (page 38). The encoder de-interleaves the chunky raster
  into one single-component plane per sample and tiles each plane on the
  same `tile_w × tile_h` grid as the chunky path, emitting the compressed
  tiles plane-major: all of plane 0's tiles (row-major, left-to-right
  then top-to-bottom) first, then plane 1's, then plane 2's — exactly the
  order §15 `TileOffsets` prescribes ("For PlanarConfiguration = 2, the
  offsets for the first component plane are stored first, followed by all
  the offsets for the second component plane, and so on"). `TileOffsets`
  / `TileByteCounts` therefore carry `SamplesPerPixel × TilesPerImage`
  entries (the §15 count formula for PlanarConfiguration = 2). Each plane
  is a single-component image, so §15 boundary padding (replicate the
  last visible column / row) and the §14 horizontal-differencing
  predictor both run with an offset of one sample — §14: "If Planar-
  Configuration is 2 … Differencing works the same as it does for
  grayscale data." Matches the decoder's existing `decode_tiles_planar`.
  Validated by self-roundtrip (planar-tiled vs. chunky-strip and vs.
  chunky-tiled decode of the same source, across None / PackBits / LZW /
  Deflate, with and without the predictor, exact-fit / partial-edge /
  non-square / oversized tile geometries), plus external black-box
  validation: `tiffcp -c none` transcodes our planar-tiled RGB LZW and
  RGB Deflate + Predictor=2 streams back to uncompressed TIFFs that
  re-decode to the original pixels; ImageMagick reads the planar-tiled
  RGB bit-exactly; and `tiffinfo` reports both the tile geometry and the
  separate-planes configuration.

- Encoder: `PlanarConfiguration = 2` (separate component planes), per
  TIFF 6.0 §"PlanarConfiguration" (page 38) and the §"StripOffsets"
  count formula (page 67), via the new `EncodePage::planar` flag. When
  set on an `Rgb24` page the encoder de-interleaves the chunky
  `RGBRGB…` data into one full-resolution strip per component plane and
  writes `StripOffsets` / `StripByteCounts` as `SamplesPerPixel`-entry
  LONG arrays in plane order (component 0, then 1, then 2) — the spec's
  "SamplesPerPixel rows and StripsPerImage columns" array with
  `StripsPerImage = 1`. Both arrays are emitted out-of-line as LONG
  blobs when the count exceeds 1; the chunky single-strip path keeps
  them inline, so the default layout is unchanged. `PlanarConfiguration`
  (tag 284) is written as 2. It composes with the §14 predictor: §14
  says "If PlanarConfiguration is 2 … Differencing works the same as it
  does for grayscale data," so each plane is differenced independently
  with an offset of one sample. The flag is rejected on the
  single-sample formats (`Gray8` / `Gray16Le` / `Palette8` /
  `Bilevel`), where §"PlanarConfiguration" says the field "is
  irrelevant."

  Validated by self-roundtrip across None / PackBits / LZW / Deflate
  (with and without `Predictor = 2`) through our own decoder's planar
  walker, an IFD-tag inspection (PlanarConfiguration reads 2 and
  StripOffsets / StripByteCounts carry 3 entries), and a regression
  check that chunky output stays single-strip with
  PlanarConfiguration = 1. External black-box validation: `tiffcp -c
  none` transcodes our planar (and planar + predictor) output back to
  uncompressed TIFFs that re-decode to the original chunky pixels
  (proving libtiff reverses the plane split + per-plane offsets), and
  ImageMagick reads our `PlanarConfiguration = 2` RGB bit-exactly.

- Encoder: horizontal-differencing predictor (`Predictor = 2`, TIFF 6.0
  §14) via the new `EncodePage::predictor` flag. When set, the encoder
  replaces each component with its first difference from the previous
  pixel of the same component before compression — §14's "subtract red
  from red, green from green, and blue from blue" with an offset of
  `SamplesPerPixel` for chunky multi-sample data — and writes the
  `Predictor` tag (317, SHORT). This is the exact inverse of the
  decoder's cumulative left-to-right add: the encoder differences
  right-to-left so each "previous" sample keeps its original magnitude,
  and relies on two's-complement `wrapping_sub` for the §14 "ignore the
  overflow bits" property. Supported on the lossless byte-aligned
  formats whose decode path already reverses it: `Gray8` (8-bit),
  `Gray16Le` (16-bit, II little-endian), `Rgb24` (8-bit × 3), and
  `Palette8` (8-bit indices). §14 ties the predictor to the LZW family,
  so combining `Predictor = 2` with a bilevel CCITT scheme
  (`Compression = 2 / 3`) or with `Bilevel` input is rejected with a
  precise error. The tag is omitted entirely when the flag is off (the
  decoder defaults to `Predictor = 1`).

  Validated by self-roundtrip across Gray8 / Gray16Le / Rgb24 /
  Palette8 under None / PackBits / LZW / Deflate, a unit check that
  forward differencing is the exact inverse of the decoder's add, and
  IFD-tag inspection (tag 317 present iff the flag is set). External
  black-box validation: `tiffinfo` reports
  `Predictor: horizontal differencing 2 (0x2)`; `tiffcp -c none`
  transcodes our `Predictor = 2` LZW / Deflate streams back to
  uncompressed TIFFs that re-decode to the original pixels (proving
  libtiff reverses our first-differences); and ImageMagick's reader
  round-trips Gray8 and RGB `Predictor = 2` output bit-exactly.

- Decoder: `PlanarConfiguration = 2` (separate component planes), per
  TIFF 6.0 §"PlanarConfiguration" (page 38) and the strip / tile
  count formulas in §"StripOffsets" / §"TileOffsets" (pages 67 / 71).
  Strip and tile layouts both support planar: the offset / byte-count
  arrays carry `SamplesPerPixel × StripsPerImage` (or
  `SamplesPerPixel × TilesPerImage`) entries with all of component 0
  first, then all of component 1, then component 2, etc. — exactly
  the row-major (`SamplesPerPixel × StripsPerImage`) ordering the spec
  prescribes. Each plane decodes to a single-component image of the
  full `width × height`, then the planes are re-interleaved into
  chunky pixel order so every downstream photometric path
  (RGB / CMYK / YCbCr / Gray) receives the same input shape it did
  for planar=1. Compression schemes covered: None / PackBits / LZW /
  Deflate / CCITT (any combination that's valid in chunky mode). The
  horizontal predictor (`Predictor = 2`) is driven with `samples = 1`
  per-plane in this path, matching TIFF 6.0 §14: "If
  PlanarConfiguration is 2, there is no problem. Differencing works
  the same as it does for grayscale data."

  JPEG-in-TIFF (`Compression = 7`) stays chunky-only — TN2's
  planar-2 rules require per-segment dimension restatement that we
  don't handle yet; the decoder returns
  `Error::Unsupported("TIFF/JPEG: PlanarConfiguration=2 …")` for
  that combination so the bug surfaces precisely rather than as
  silent corruption.

  Validated against three hand-built planar fixtures (uncompressed
  RGB 32×16, solid-blue 8×8 to catch plane swaps, and a
  distinct-per-plane gradient pattern) plus two
  `tiffcp -p separate` black-box fixtures (uncompressed 64×64 RGB
  and `tiffcp -p separate -c lzw` 64×64 RGB). `SamplesPerPixel = 1`
  files tagged `PlanarConfiguration = 2` are accepted: the spec
  says the field is irrelevant in that case, so we collapse them
  into the chunky walker.

- Decoder: JPEG-in-TIFF (`Compression = 7`, per TIFF Technical
  Note 2 "DRAFT 17-Mar-95"). Each strip or tile is decoded as a
  freestanding ISO JPEG datastream by merging the optional
  `JPEGTables` (tag 347) abbreviated-table-specification body in
  front of the segment body and feeding the result through
  `oxideav-mjpeg`'s public `Decoder` trait surface. Supported
  combinations:
  * `PhotometricInterpretation = 1` (BlackIsZero) or `0`
    (WhiteIsZero) with `SamplesPerPixel = 1` → 1-plane Gray8 JPEG
    composited into `Gray8` output (WhiteIsZero applies the 255-x
    polarity inversion that matches the rest of the bilevel paths).
  * `PhotometricInterpretation = 2` (RGB) with `SamplesPerPixel = 3`
    → 3-plane full-resolution JPEG (no chroma subsampling)
    interleaved into `Rgb24`. Matches what
    `convert -compress jpeg` produces by default from PPM input.
  * `PhotometricInterpretation = 6` (YCbCr) with
    `SamplesPerPixel = 3` → 3-plane planar YCbCr JPEG with chroma
    subsampling reflected in the JPEG sampling factors (4:4:4 /
    4:2:2 / 4:2:0 / 4:1:1 all handled). Composited to `Rgb24`
    using the BT.601 matrix matching TN2's default
    `ReferenceBlackWhite = [0,255,128,255,128,255]`. Matches what
    `tiffcp -c jpeg` produces.

  Both strip and tile layouts are walked; tile-edge clipping is
  handled in the compositor. `Compression = 6` (the deprecated
  old-style JPEG-in-TIFF design described in TIFF 6.0 §22) is
  rejected with a precise `Error::Unsupported`. JPEG-in-TIFF
  requires the default-on `registry` Cargo feature (the JPEG codec
  lives in `oxideav-mjpeg`); with `default-features = false`,
  `Compression = 7` returns `Error::Unsupported`.

  Three new ImageMagick / tiffcp integration tests exercise the
  three supported photometric paths against an MSE-bounded RGB /
  Gray reconstruction tolerance (since JPEG is lossy); seven new
  `merge_jpeg_segment` unit tests cover the SOI/EOI handling
  contract on both the `JPEGTables` blob and per-segment data.

- Encoder: CCITT Modified Huffman (`Compression = 2`) and CCITT
  T.4 1-D (`Compression = 3`, optional `T4Options` bit 2
  byte-aligned EOLs) writers, sharing the same `WHITE` / `BLACK`
  run-length code tables that the decoder uses. A new
  `EncodePixelFormat::Bilevel { pixels }` accepts MSB-first
  byte-packed 1-bit input; the encoder rejects CCITT compression
  with any non-bilevel pixel format. Writes `T4Options` (tag 292)
  when Compression=3 is selected, in the correct ascending-tag-order
  slot of the IFD. Self-roundtrip tests cover all-white / all-black
  /alternating rows, the 64-pixel make-up-code threshold, the
  2624-pixel repeated-2560 path, multi-row byte alignment, and the
  byte-aligned-EOL variant. External validation: `tiffinfo` reports
  the expected metadata on Compression=2 output, and `tiffcp -c
  none` successfully transcodes both flavours of our
  Compression=3 streams (`eol_byte_aligned` true and false) back to
  uncompressed TIFFs that re-decode to the original pixels.
- Decoder: `FillOrder = 2` (LSB-first) for bilevel data. Both
  uncompressed (`Compression = 1`) and CCITT-compressed
  (`Compression = 2 / 3`) bilevel strips and tiles now accept
  FillOrder=2 inputs, per TIFF 6.0 §FillOrder (page 32). The
  decoder normalises every byte to MSB-first canonical layout via a
  bit-reversal helper before the CCITT run-length scanner or the
  bilevel-to-Gray8 expander runs, so downstream paths stay
  FillOrder-agnostic. FillOrder=2 combined with non-bilevel data
  (`BitsPerSample != 1`) or a non-CCITT compressor is explicitly
  rejected, matching the spec's restriction. Validated
  pixel-for-pixel against `tiffcp -f lsb2msb` output for
  Compression=None, Compression=2 (Modified Huffman), and
  Compression=3 (T.4 1-D) fixtures.
- Decoder: CCITT Modified Huffman (`Compression = 2`, TIFF 6.0
  §10) and CCITT T.4 1-D (`Compression = 3` with `T4Options` bit 0
  cleared, §11) decompression. Full white + black terminating and
  make-up code tables (Tables 1/T.4 and 2/T.4 plus the 1792..2560
  additional make-up codes from page 47) transcribed verbatim from
  the TIFF 6.0 PDF. `T4Options` bit 2 (`EOL-byte-aligned`) is
  honoured. `FillOrder` other than 1 (MSB-first) is rejected
  explicitly. CCITT T.4 2-D (`T4Options` bit 0 = 1) and T.6 /
  Group 4 (`Compression = 4`) remain unsupported because the 2-D
  Pass / Horizontal / Vertical mode codes are not part of the TIFF
  6.0 spec — those callers receive a precise error. Validated
  pixel-for-pixel against `tiffcp -c g3:1d` and `tiffcp -c
  g3:1d:fill` output for solid, multi-run, make-up-bumped, and
  diagonal patterns.
- Encoder: classic-II TIFF 6.0 writer covering MinIsBlack 8/16-bit
  greyscale, RGB 8-bit, and 8-bit indexed palette photometrics with
  None / PackBits / LZW / Deflate compression. Single-IFD via
  `encode_tiff`, or chained multi-page via `encode_tiff_multi`.
  Output validates round-trip through ImageMagick `convert`,
  `tiffinfo`, and `tiffcp`.
- Decoder: BigTIFF (8-byte offsets, magic 43) header + IFD parsing
  alongside classic 32-bit TIFF. New `LONG8` / `SLONG8` / `IFD8`
  field types decode through the existing `Entry::as_u64_vec` /
  `as_u32_vec` accessors.
- Decoder: tile layout (`TileWidth` / `TileLength` / `TileOffsets`
  / `TileByteCounts`) for byte-aligned bit depths (8 / 16). Edge
  tiles whose visible region is narrower / shorter than the tile
  geometry are correctly trimmed.
- Decoder: multi-page support via the new `decode_tiff_all` API
  (full next-IFD chain walk with cycle detection).
- Decoder: CMYK photometric (4-sample × 8-bit chunky) → `Rgb24`
  via the standard multiplicative complement-of-coverage transform.
- Decoder: YCbCr photometric (3-sample × 8-bit chunky) →
  `Rgb24` via BT.601 inverse-matrix integer coefficients with
  `YCbCrSubSampling` parsing for the common 1×1 / 2×1 / 2×2 / 1×2 /
  4×1 / 4×2 layouts.
- Container probe now also recognises the BigTIFF II / MM magic
  bytes (`II 2B 00` / `MM 00 2B`).

### Tests

- Read-side tiled-layout decode (TIFF 6.0 §15) is now validated by a
  binary-independent self-roundtrip suite
  (`tests/decode_tiled_roundtrip.rs`). Each case encodes the identical
  source pixels twice through our writer — once tiled, once strip — then
  decodes both with `decode_tiff` and asserts the two pixel planes are
  byte-identical, so the tile path (TileWidth/TileLength +
  TileOffsets/TileByteCounts parse, per-tile decompression, per-tile §14
  predictor reversal, and §15 boundary-padding removal during
  reassembly) is checked against the strip decode of the same image
  with no external reference. Covers Gray8 / Gray16Le / Rgb24 /
  Palette8 under None / PackBits / LZW / Deflate, with and without the
  predictor, across exact-fit grids, partial right-column + bottom-row
  edges, non-square tiles, an oversized tile padded on both axes, a
  whole-image-in-one-tile case, and a 1-pixel overhang on each axis.
  Runs identically with and without the `registry` feature (the tile
  decode is framework-independent).

## [0.0.2](https://github.com/OxideAV/oxideav-tiff/compare/v0.0.1...v0.0.2) - 2026-05-04

### Other

- add default-on `registry` cargo feature for standalone-friendly builds
- Merge pull request #1 from OxideAV/release-plz-2026-05-03T03-53-50Z

## [0.0.1](https://github.com/OxideAV/oxideav-tiff/releases/tag/v0.0.1) - 2026-05-03

### Other

- TIFF 6.0 image decoder (header/IFD/strip parse + None/PackBits/LZW/Deflate)

### Added

- Default-on `registry` Cargo feature gates the `oxideav-core`
  dependency, the `Decoder` trait implementation, the TIFF container
  demuxer + probe, and the `register_codecs` / `register_containers`
  entry points. Image-library consumers can now depend on
  `oxideav-tiff` with `default-features = false` and skip the
  `oxideav-core` dep tree entirely; the standalone path exposes
  `decode_tiff` plus crate-local `TiffImage` / `TiffPixelFormat` /
  `TiffPlane` / `TiffError` types built only on `std`.
- Inline `ci-standalone` CI job verifies `cargo build --lib
  --no-default-features` and `cargo test --no-default-features` stay
  green on every change.
- Initial release: pure-Rust TIFF 6.0 image decoder + container.
- Header parse: `II` (little-endian) and `MM` (big-endian) byte
  orders, magic 42, first-IFD offset.
- IFD parse: 2-byte entry count + N x 12-byte directory entries +
  next-IFD offset. All twelve baseline TIFF 6.0 field types
  (BYTE / ASCII / SHORT / LONG / RATIONAL / SBYTE / UNDEFINED /
  SSHORT / SLONG / SRATIONAL / FLOAT / DOUBLE) decoded inline if
  the value fits in 4 bytes, dereferenced through the offset
  otherwise.
- Photometric interpretations: 0 WhiteIsZero, 1 BlackIsZero,
  2 RGB, 3 Palette.
- Compression: 1 None, 32773 PackBits (Apple RLE), 5 LZW (Welch
  variable-bit, 9..=12 bits, ClearCode 256, EoI 257, TIFF
  high-bit-first packing), 8 Adobe Deflate (zlib via
  `miniz_oxide`).
- Bit depths: 1, 4, 8, 16 (16-bit RGB and grayscale honour the
  file byte order).
- Strip-based decode walks `StripOffsets` / `StripByteCounts` so
  arbitrarily many strips per image work.
- Predictor 1 (none) and 2 (horizontal differencing, per-component
  for `SamplesPerPixel > 1`).
- Output `PixelFormat`s: `Gray8`, `Gray16Le`, `Rgb24`, `Rgb48Le`.
  Palette images resolve the colormap inline and emit `Rgb24`.
  Bilevel images upsample to `Gray8`.
- Container probe + demuxer: detect `II*\0` / `MM\0*` magic, emit
  the whole file as a single keyframe packet, advertise width /
  height / pixel-format in `StreamInfo`.
