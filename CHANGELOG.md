# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Decoder: `SampleFormat` tag (339) inspection, per TIFF 6.0
  §SampleFormat (page 80). The decoder now reads the field when
  present and routes through the unsigned-integer path for values
  `1` (unsigned, the spec default) and `4` (undefined — folded back
  to unsigned per the §SampleFormat note "A reader would typically
  treat an image with 'undefined' data as if the field were not
  present"). Values `2` (two's-complement signed integer) and `3`
  (IEEE floating-point) are rejected with a precise typed error
  rather than silently re-interpreted as unsigned bytes — this
  enforces the §SampleFormat reader rule: "If the SampleFormat
  field is present and the value is not 1, a Baseline TIFF reader
  that cannot handle the SampleFormat value must terminate the
  import process gracefully." Per-component non-uniform values and
  out-of-range values (≥ 5) are also rejected. An absent field
  continues to default to unsigned, so every existing fixture
  decodes byte-for-byte unchanged. New integration test file
  `tests/decode_sample_format.rs` covers all six branches (absent,
  1, 2, 3, 4, 99) against a hand-rolled 1×1 Gray8 classic-TIFF
  byte string.

- Encoder: `PhotometricInterpretation = 8` (1976 CIE L*a*b*), per
  TIFF 6.0 §23 "CIE L*a*b* Images" (page 110). Two new
  `EncodePixelFormat` variants:

  * `CieLab8 { pixels }` — 3-sample chunky `(L*, a*, b*)` at 8 bits
    per sample. Writes `PhotometricInterpretation = 8`,
    `SamplesPerPixel = 3`, `BitsPerSample = [8, 8, 8]`. The bit
    interpretation is fixed by §23 — L* unsigned 0..255 mapping
    linearly to the 0..100 perceptual lightness scale, a*/b*
    two's-complement signed bytes in -128..127 — so the encoder
    takes the caller-supplied bytes through verbatim, exactly the
    inverse of the decoder's verbatim strip read.

  * `CieLabL8 { pixels }` — 1-sample L*-only at 8 bits per sample
    (§23 page 110 "Usage of other Fields": "3 for L*a*b*, 1 implies
    L* only, for monochrome data"). Writes
    `PhotometricInterpretation = 8`, `SamplesPerPixel = 1`,
    `BitsPerSample = [8]`.

  Compressors accepted on both variants: None / PackBits / LZW /
  Deflate (the byte-aligned set the rest of the multi-bit
  photometric paths use); CCITT (Compression = 2 / 3) is bilevel-only
  per §10 / §11 and rejected via the existing CCITT-input gate.
  `Predictor = 2` (TIFF 6.0 §14 horizontal differencing) composes
  with both — per-component differencing with offset =
  `SamplesPerPixel`, identical to the Rgb24 / Gray8 paths.
  `PlanarConfiguration = 2` composes with `CieLab8` (three
  single-component planes via §"PlanarConfiguration"); it is
  rejected on `CieLabL8` per §"PlanarConfiguration" "irrelevant"
  when `SamplesPerPixel = 1`. Tiled layout (§15) composes with both
  variants under chunky, and for `CieLab8` under planar (one tile
  grid per L*/a*/b* plane, §15 `TileOffsets`). BigTIFF composes
  unchanged — the 3-entry `BitsPerSample` SHORT array (6 bytes)
  stays inline in the widened 8-byte value/offset slot.

  Validated by 11 in-crate unit tests covering uncompressed
  roundtrip, all four compressors, the Predictor=2 composition, the
  PlanarConfiguration=2 composition, the tiled-§15 composition, the
  BigTIFF composition, the CCITT rejection, the wrong-buffer-size
  rejection, the 1-sample roundtrip, the planar-rejection on 1-sample
  L*-only input, and an IFD-level photometric byte check; plus 9
  integration tests in `tests/encode_cielab_roundtrip.rs` mirroring
  `decode_cielab.rs`'s fixture shape — the neutral L* gradient, each
  of the four chromatic primary directions, the lossless-compressor
  parity check, an exhaustive Predictor / Planar / Tiled / combined
  composition check, 1-sample L*-only ramp + tiled composition +
  predictor / compressor matrix, an IFD-level byte walker that asserts
  `tag 262 = 8` and `tag 277 = 3 (or 1)`, and a multi-page chain that
  mixes `CieLab8`, `Gray8`, and `CieLabL8` pages.

## [0.0.3](https://github.com/OxideAV/oxideav-tiff/compare/v0.0.2...v0.0.3) - 2026-05-29

### Other

- CCITT T.4 2-D + T.6 (Group 4) decode via Table 4/T.4
- PhotometricInterpretation = 8 (CIE L*a*b*) per TIFF 6.0 §23
- BigTIFF write (Adobe Pagemaker 6.0 BigTIFF design)
- decoder + encoder: PhotometricInterpretation = 4 (Transparency Mask) per TIFF 6.0 page 37
- pack_lzw flat-array trie + criterion bench (round 129)
- add cargo-fuzz decoder target + fix 3 panic vectors it caught
- CMYK JPEG-in-TIFF (Compression=7, Photometric=5, SamplesPerPixel=4)
- tiled PlanarConfiguration=2 write (one tile grid per plane, §15)
- validate tiled-layout read path via binary-independent self-roundtrip (TIFF 6.0 §15)
- tiled layout (TIFF 6.0 §15) via EncodePage::tiling
- PlanarConfiguration = 2 (separate component planes)
- horizontal-differencing predictor (Predictor=2, TIFF 6.0 §14)
- PlanarConfiguration = 2 (separate component planes)
- JPEG-in-TIFF (Compression=7, TIFF Tech Note 2)
- CCITT Modified Huffman (Compression=2) + T.4 1-D (Compression=3)
- rewrite 4 libtiff / libjpeg cross-reference comments
- FillOrder = 2 (LSB-first) for bilevel strips and tiles
- CCITT Modified Huffman (Compression=2) + T.4 1-D (Compression=3)
- update description to reflect round-2 encoder + BigTIFF/tiles/CMYK/YCbCr/multi-page
- encoder + BigTIFF/tiles/CMYK/YCbCr/multi-page decode (round 2)

### Added

- Decoder: CCITT T.4 2-D (Modified READ / MR — `Compression = 3`
  with `T4Options` bit 0 set) and T.6 / Group 4 (MMR —
  `Compression = 4`). The TIFF 6.0 PDF defers the 2-D Pass /
  Horizontal / Vertical mode codes to ITU-T Recommendations T.4 §4.2
  and T.6; those recommendations are now staged in
  `docs/image/tiff/T-REC-T.4.pdf` / `T-REC-T.6.pdf` and the normative
  Table 4/T.4 = Table 1/T.6 mode-code dictionary is transcribed
  clean-room into `docs/image/tiff/ccitt-t4-t6-fax-codes.md` §1.

  The decoder implements the READ algorithm (T.4 §4.2.1 / T.6
  §2.2.1) directly:

  * `CcittVariant::T6` — every row is 2-D coded against the previous
    decoded row; the first reference line is an imaginary all-white
    line; no EOL framing between rows.
  * `CcittVariant::T4TwoD { eol_byte_aligned }` — each row is
    preceded by the 12-bit EOL code (optionally byte-aligned per
    `T4Options` bit 2) followed by a one-bit tag selecting 1-D
    (tag = 1) vs 2-D (tag = 0) coding for the next row; 2-D rows
    use the same mode-code dictionary as T.6.

  Mode codes implemented: `Vertical(0)` (= `1`),
  `Vertical(±1)` (= `011` / `010`), `Vertical(±2)` (= `000011` /
  `000010`), `Vertical(±3)` (= `0000011` / `0000010`), `Horizontal`
  (= `001` + two MH runs, colour-toggling), `Pass` (= `0001`,
  encoder coalesces a0..b2 of a0's colour). The optional
  `0000001111` uncompressed-mode extension (Table 5/T.4) is rejected
  explicitly — `tiffcp` does not emit it for normal facsimile
  content and the docs flag it as out of scope. `T6Options` (tag
  293) is now read; bit 1 (uncompressed mode allowed) is similarly
  rejected.

  Validated by 7 new `tiffcp` round-trip fixtures (`tests/
  decode_ccitt_fixtures.rs`): solid-white 32×4, two-rectangle 64×8,
  diagonal 32×32, wide-run 128×4 — each under `-c g4` (Compression
  = 4) and `-c g3:2d` (Compression = 3 with T4Options = 1). Pixels
  must match the uncompressed reference byte-for-byte. The two
  preexisting `ccitt_g4_is_unsupported` / `ccitt_t4_2d_is_unsupported`
  tests are removed (they encoded a docs-gap that has been
  resolved). 13 in-crate unit tests cover the mode-code dictionary
  entries and the `first_change_after` reference-line walker (T.4
  §4.2.1.2).

  Encoder side is unchanged — `encode_ccitt` now explicitly returns
  `InvalidData` for `CcittVariant::T4TwoD` / `T6` rather than
  silently emitting a 1-D stream, so callers get a clean
  diagnostic. The MH + T.4 1-D encode paths are untouched and
  remain bit-exact.

- Decoder: `PhotometricInterpretation = 8` (1976 CIE L*a*b*), per
  TIFF 6.0 §23 "CIE L*a*b* Images" (page 110). Both the spec-defined
  layouts decode:

  * `SamplesPerPixel = 3, BitsPerSample = 8` — chunky `(L*, a*, b*)`
    triples where L* is unsigned 0..255 mapping linearly to the
    perceptual 0..100 lightness scale, and a*, b* are
    two's-complement signed 8-bit values in -128..127 representing
    the red/green and yellow/blue chrominance channels (§23: "L*
    range is from 0 ... to 100 ... The a* and b* ranges will be
    represented as signed 8 bit values having the range -127 to
    +127"). Decoded to display `Rgb24` via the §23 conversion
    pipeline: Lab → XYZ (D65 reference white per §23 page 111
    "Generally, D65 illumination is used and a perfect reflecting
    diffuser is used for the reference white"), XYZ → linear RGB
    through the analytic inverse of §23's stated NTSC tristimulus
    matrix `[0.6070, 0.1740, 0.2000; 0.2990, 0.5870, 0.1140;
    0.0000, 0.0660, 1.1110]` (det ≈ 0.337438), and linear RGB →
    8-bit through the standard sRGB OETF so the result is ready
    for a contemporary display. §23's "Converting between RGB and
    CIELAB, a Caveat" leaves the linear-to-display step open
    ("some conversion to RGB will be required"); the sRGB gamma
    curve is the universally-applicable choice and matches the
    other photometric paths' display-ready output.

  * `SamplesPerPixel = 1, BitsPerSample = 8` — §23 page 110 "Usage
    of other Fields": "3 for L*a*b*, 1 implies L* only, for
    monochrome data". The single byte is treated as L* and run
    through the same lightness-curve inverse as the 3-sample path
    (so a chromatically-neutral a* = b* = 0 CIELab pixel in either
    layout produces the same gray level), then sRGB-encoded to
    `Gray8`.

  Compressors accepted by both layouts: None / PackBits / LZW /
  Deflate (the byte-aligned, photometric-agnostic set the other
  multi-bit photometric paths use). CCITT (Compression = 2/3/4) is
  bilevel-only per spec and rejected here; the JPEG-in-TIFF
  dispatch (Compression = 7) does not list CIELab as a permitted
  TN2 photometric and is unchanged.

  Validated by 7 hand-built classic-TIFF integration tests
  (`tests/decode_cielab.rs`, no external library / binary
  dependency) covering: a 4-pixel chromatically-neutral L* gradient
  (0/33/66/100) asserting monotonic luminance, near-neutral RGB
  channels, and saturation at the L* = 100 endpoint; the four
  primary chromatic directions (+a* → red-dominant, -a* →
  green-dominant, +b* → blue-suppressed yellow, -b* →
  blue-dominant); a `SamplesPerPixel = 1` L*-only gradient
  asserting the same monotonicity and endpoints in the Gray8 path;
  and a cross-check that a 1×1 L* = 49.8 mono fixture decodes
  within 2 levels of the green channel of the matching 3-sample
  `L*=49.8/a*=0/b*=0` fixture. No external image library, decoder,
  or trace was consulted — the conversion math is a direct
  transcription of §23's formulas plus the analytic inverse of
  §23's stated matrix.

- Encoder: BigTIFF write (Adobe Pagemaker 6.0 BigTIFF design) via the
  new `EncodePage::bigtiff` flag. When `true`, the encoder produces a
  16-byte header (II/MM + magic 43 + offset-bytesize 8 + reserved 0 +
  8-byte first-IFD offset), 20-byte IFD entries (`tag:u16 + type:u16 +
  count:u64 + value-or-offset:u64`), an 8-byte next-IFD pointer, and
  widens the inline-value threshold from 4 to 8 bytes. `StripOffsets`,
  `StripByteCounts`, `TileOffsets`, and `TileByteCounts` switch from
  LONG (type 4) to LONG8 (type 16) so multi-GiB offsets fit; the
  `BitsPerSample[3]` SHORT array for `Rgb24` (6 bytes) now stays inline
  rather than spilling out-of-line. The classic-TIFF 32-bit-offset
  overflow check is lifted in BigTIFF mode (the on-disk ceiling becomes
  the full u64 file-offset range). Pixel formats, compressors,
  `predictor` / `planar` / `tiling` flags all compose unchanged.
  `encode_tiff_multi` requires every page to agree on the variant —
  mixing classic and BigTIFF IFDs in one chain is rejected with a
  precise error since the on-disk layouts are wire-incompatible. The
  decoder already accepts both variants via `parse_header` /
  `parse_ifd` (round-136 BigTIFF read), so every supported pixel
  format / compression combination now round-trips on both sides.

  Validated by 14 new self-roundtrip + on-disk-layout tests
  (`tests/encode_bigtiff.rs`): header byte assertions (II + magic 43 +
  off-size 8 + reserved 0 + 8-byte first-IFD offset, plus a regression
  case proving `bigtiff = false` keeps the classic 8-byte header with
  magic 42); an IFD-walking helper independent of our decoder that
  pulls `(tag, field_type, count, value_or_offset)` straight from
  bytes to assert `StripOffsets` / `StripByteCounts` carry
  `field_type = 16` (LONG8); a `BitsPerSample` inlining check for
  `Rgb24` that asserts the three SHORT values pack into the 8-byte
  value slot with no external blob; pixel-roundtrips for Gray8
  (uncompressed and LZW + Predictor=2), Gray16Le (Deflate), Rgb24
  (PackBits, planar+LZW, tiled), Palette8, and bilevel CCITT-MH; a
  two-page BigTIFF chain that mixes LZW Gray8 with Deflate Rgb24; and
  a negative test pinning the mixed-variant rejection. The planar
  case exercises the out-of-line LONG8 array path (three plane
  offsets in `StripOffsets`); the tiled case asserts the
  `TileOffsets` `field_type = 16` and the 4-tile count for a
  32x32 image over 16x16 tiles.

- Decoder + Encoder: `PhotometricInterpretation = 4` (Transparency
  Mask), per TIFF 6.0 page 37 + the `NewSubfileType` bit-2 companion
  flag (page 36, "1 if the image defines a transparency mask for
  another image in this TIFF file. The PhotometricInterpretation
  value must be 4"). A mask is a 1-bit-per-pixel image with
  `SamplesPerPixel = 1` whose 1-bits define the interior of the
  region and whose 0-bits define the exterior. The decoder routes
  mask pages through the existing bilevel expander with the bit
  polarity pinned to spec (1-bit -> 0xFF, 0-bit -> 0x00, no
  `WhiteIsZero` inversion) so a downstream compositor can multiply
  the resulting `Gray8` plane against the main image directly. The
  encoder exposes a new `EncodePixelFormat::TransparencyMask`
  variant that writes `PhotometricInterpretation = 4` and sets
  bit 2 of `NewSubfileType` (tag 254) — the companion field the
  spec uses to let multi-page readers spot a mask IFD without
  consulting the photometric tag. Other `NewSubfileType` bits stay
  clear (the encoder still emits bit 2 only for mask pages and
  zero for every other photometric, including Gray8 / Rgb24 /
  Palette8 / Bilevel / Gray16Le).

  Compressors accepted on both sides: None / PackBits / LZW /
  Deflate / CCITT-MH (Compression = 2) / CCITT-T.4-1D
  (Compression = 3, with or without `T4Options` bit 2 byte-aligned
  EOLs). The spec recommends PackBits ("PackBits compression is
  recommended", page 37) but does not forbid the others, and our
  per-strip CCITT writer already accepts arbitrary 1-bit input. The
  same per-strip `FillOrder` (tag 266) normalisation the
  `BlackIsZero` / `WhiteIsZero` bilevel paths use applies, so
  `FillOrder = 2` masks decode correctly when paired with
  Compression = None / 2 / 3.

  Layouts rejected on encode for 1-bit input (both `Bilevel` and
  `TransparencyMask`) with a precise error: tiled
  (sub-byte tile-row slicing isn't implemented on either side),
  `PlanarConfiguration = 2` (the spec calls the field "irrelevant"
  for `SamplesPerPixel = 1`), and `Predictor = 2` (TIFF 6.0 §14
  differences whole sample components, undefined for bit-packed
  data). Pixel-buffer size validation matches `Bilevel` (`pixels.len()
  == ceil(width / 8) * height`, otherwise a precise error).

  Validated by a binary-independent self-roundtrip suite (15 tests,
  `tests/transparency_mask_roundtrip.rs`): all six accepted
  compressors round-trip a 16x8 top-exterior/bottom-interior mask;
  a 24x4 diagonal stripe stresses non-byte-aligned widths and the
  CCITT scanner; a multi-page TIFF chain pairs a Gray8 main image
  with a PackBits-compressed mask page and the decoder walks both
  IFDs via `decode_tiff_all`. Two IFD-level inspection tests confirm
  the encoder writes `PhotometricInterpretation = 4` and sets
  `NewSubfileType` bit 2 only on `TransparencyMask` pages
  (`gray8_page_does_not_set_mask_bit` catches accidental
  fallthrough). Four negative tests pin the rejected combinations
  (tiled / planar / predictor / wrong buffer size) so future
  refactors keep the same shape of error message. The suite runs
  identically with and without the `registry` Cargo feature — the
  bilevel mask path is framework-independent.

### Performance

- Encoder: `compress::pack_lzw` (TIFF 6.0 §13 LZW) now backs the
  dictionary with a flat-array trie (`first_child` / `next_sibling`
  / `suffix`, all sized at `LZW_MAX_CODE + 1 = 4096`, total ~20 KiB
  per encoder invocation behind a `Box`) instead of the previous
  `HashMap<(u16, u8), u16>`. The bitstream output is byte-identical
  to the pre-r129 encoder (same greedy match, same code-width bump
  points, same Clear-on-fill timing), so the change is invisible to
  any decoder, but lookup is now a short chain walk over up-to-12-bit
  codes rather than a `(u16, u8)` hash + bucket scan. New
  `benches/lzw.rs` Criterion harness (run with `cargo bench -p
  oxideav-tiff --bench lzw`) reports the following on Apple Silicon
  `release` builds:
  - `lzw_random_64k` (xorshift random bytes, worst case): ~53 MiB/s
    → ~65 MiB/s (~1.2x).
  - `lzw_repeating_motif_64k` (16-byte motif × 4096): ~44 MiB/s →
    ~640 MiB/s (~14.5x).
  - `lzw_zeros_256k` (all-zero, exercises the Clear-on-fill path):
    ~46 MiB/s → ~640 MiB/s (~14.1x).
  - `lzw_natural_image_64k` (256×256 8-bit greyscale gradient,
    representative image strip): ~39 MiB/s → ~809 MiB/s (~20.5x).
  Three new inline roundtrip tests (`lzw_roundtrip_table_fill_clear`,
  `lzw_roundtrip_byte_pattern_repeated`, `lzw_trie_lookup_and_insert`)
  exercise the trie directly and the table-fill / Clear-reset path
  end-to-end.

### Added

- `cargo-fuzz` decoder target at `fuzz/fuzz_targets/decode.rs`
  driving arbitrary attacker bytes through `decode_tiff`,
  `decode_tiff_all`, `parse_header`, `parse_ifd`, `unpack_packbits`,
  `unpack_lzw`, and `unpack_deflate`. Contract: decoder-only
  panic-freedom (no abort, debug-overflow, OOM, or OOB-index for any
  input). Mirrors the `mkv` / `flac` / `id3` `decode` /`parse` fuzz
  shape. Round 126 wall-respected: 7.7 M iterations green after the
  fix commit; the three crashes the fuzzer caught in its first 5
  minutes are now regression-tested in
  `tests/decode_fuzz_regressions.rs` plus inline `compress` and
  `ifd` test modules.

### Fixed

- `unpack_lzw` first-after-Clear panic: a code `>= 256` emitted
  before any dictionary entries were added would index into an
  uninitialised table slot; the next iteration's `KwKwK` /
  `code == next_code` writer would then set
  `prefix[next_code] = prev`, forming a self-referential prefix
  chain that spun the `emit` walk forever (~2 GiB scratch growth
  before OOM). The decoder now rejects first-after-Clear codes
  outside the `0..=255` leaf range, and `emit` is hardened with a
  `LZW_MAX_CODE + 1`-hop chain-length cap as defence-in-depth.
- `unpack_lzw` / `unpack_packbits` initial-reserve OOM:
  `Vec::with_capacity(expected_len)` accepted the attacker-claimed
  per-strip output length directly. Capped at
  `MAX_INITIAL_RESERVE = 64 KiB` (with per-input upper-bound for
  `unpack_packbits` based on the file size and PackBits's 128x
  worst-case expansion ratio). The vector still grows naturally
  past the cap as decompression progresses.
- `unpack_deflate` zip-bomb OOM: switched the underlying
  `miniz_oxide` call from `decompress_to_vec_zlib` (unbounded
  output) to `decompress_to_vec_zlib_with_limit` capped at
  `MAX_DEFLATE_OUTPUT = 64 MiB` (clamped further by the IFD's
  `expected_len`). A malformed stream that claimed to expand a
  100-byte payload into gigabytes now surfaces as a regular
  `TiffError` rather than aborting the process.
- BigTIFF `parse_ifd_big` `off + 8` debug-overflow panic: a
  malformed header whose `first_ifd_offset = u64::MAX` cast to
  `usize::MAX` on 64-bit hosts, then `off + 8` debug-panicked with
  "attempt to add with overflow". All `off + N` arithmetic now goes
  through `checked_add`. Same fix applied to `parse_ifd_classic`
  defensively (a hand-crafted `decode_tiff` caller could pass
  `u64::MAX` even on the classic path).
- BigTIFF entry `type_size × count` silent u64 overflow:
  `ts as u64 * cnt` for a malformed `count = u64::MAX` wrapped to a
  small value that bypassed the `total <= 8` inline-vs-offset
  check, then read the wrong number of bytes from the value-or-
  offset slot. Now uses `checked_mul` and `usize::try_from`. The
  BigTIFF entry-list `Vec::with_capacity(count_us)` is also capped
  by `input.len() / 20 + 1` so a 16-byte BigTIFF header can't force
  a multi-GiB upfront reservation.
- Decoder up-front dimension sanity gate: any IFD claiming
  `ImageWidth * ImageLength > MAX_IMAGE_PIXELS` (256 megapixels —
  covers every legitimate single-image TIFF in the wild) is now
  rejected before any strip / tile / plane buffer is allocated.
  Prevents an attacker-supplied 16-byte IFD claiming
  `u32::MAX × u32::MAX` pixels from driving a multi-exabyte
  `Vec::with_capacity` downstream.

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
