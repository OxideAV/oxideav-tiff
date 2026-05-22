# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
