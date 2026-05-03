# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.1](https://github.com/OxideAV/oxideav-tiff/releases/tag/v0.0.1) - 2026-05-03

### Other

- TIFF 6.0 image decoder (header/IFD/strip parse + None/PackBits/LZW/Deflate)

### Added

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
