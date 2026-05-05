# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
