//! Compression = 50001 — WebP-in-TIFF codec-in-container carriage.
//!
//! The value is the de-facto registry extension transcribed in the
//! OxideAV trace doc `docs/image/tiff/tiff-zstd-compression-50000.md`
//! (§1 registers both 50000 Zstandard and 50001 WebP; §3 fixes the
//! shared per-strip / per-tile discipline: "WebP (50001) follows the
//! identical per-strip / per-tile container discipline: each
//! strip/tile becomes one WebP (VP8 / VP8L) bitstream"). The exact
//! segment framing — each payload is a complete WebP *file* (RIFF
//! `WEBP` container), the final strip of a multi-strip page carries
//! only the remaining rows, edge tiles stay padded to the full tile
//! geometry — was pinned empirically against independently produced
//! black-box reference samples (third-party TIFF tooling driven as
//! opaque binaries; the samples are committed under
//! `tests/data/webp50001/` with their generation recipe).
//!
//! All WebP bitstream logic lives in the `oxideav-webp` sibling crate;
//! this module only hands the strip / tile payload bytes across that
//! crate's public still-image API and converts between the TIFF chunky
//! sample layout (8-bit RGB / RGBA) and WebP's canonical RGBA surface:
//!
//! * decode: [`oxideav_webp::decode_webp_image`] — handles both the
//!   `VP8L` (lossless) and `VP8 ` (lossy) payload flavours, returning
//!   `width * height * 4` RGBA bytes we re-pack to the strip's
//!   `SamplesPerPixel`.
//! * encode: [`oxideav_webp::encode_webp_lossless`] — one `VP8L`
//!   lossless file per strip / tile, so every Compression=50001 write
//!   round-trips pixel-exact like the rest of the encoder's schemes.

use crate::error::{Result, TiffError as Error};

/// Decode one Compression=50001 strip / tile payload (`raw`, a
/// complete WebP file) and re-pack it to the TIFF chunky sample bytes
/// the downstream assembly expects: `width * rows * samples_per_pixel`
/// bytes, `samples_per_pixel` ∈ {3 (RGB), 4 (RGBA)}.
///
/// The WebP frame dimensions are validated against the strip / tile
/// geometry from the IFD — a payload whose frame is not exactly
/// `width` × `rows` is rejected rather than resampled or cropped.
pub(crate) fn unpack_webp(
    raw: &[u8],
    width: u32,
    rows: u32,
    samples_per_pixel: u16,
) -> Result<Vec<u8>> {
    let decoded = oxideav_webp::decode_webp_image(raw).map_err(|e| {
        Error::invalid(format!(
            "TIFF/WebP: Compression=50001 strip/tile payload is not a decodable WebP \
             still image: {e}"
        ))
    })?;
    if decoded.width != width || decoded.height != rows {
        return Err(Error::invalid(format!(
            "TIFF/WebP: strip/tile WebP frame is {}x{}, IFD geometry says {width}x{rows}",
            decoded.width, decoded.height
        )));
    }
    let pixels = (width as usize) * (rows as usize);
    if decoded.rgba.len() != pixels * 4 {
        return Err(Error::invalid(format!(
            "TIFF/WebP: decoded RGBA is {} bytes, expected {}",
            decoded.rgba.len(),
            pixels * 4
        )));
    }
    match samples_per_pixel {
        // RGBA page: WebP's RGBA surface is byte-for-byte the TIFF
        // chunky (R, G, B, extra) layout already.
        4 => Ok(decoded.rgba),
        // RGB page: drop the alpha byte of each pixel (an RGB strip's
        // WebP payload is opaque; a stray alpha channel has no TIFF
        // sample to land in at SamplesPerPixel = 3).
        3 => {
            let mut out = Vec::with_capacity(pixels * 3);
            for px in decoded.rgba.chunks_exact(4) {
                out.extend_from_slice(&px[..3]);
            }
            Ok(out)
        }
        other => Err(Error::invalid(format!(
            "TIFF/WebP: SamplesPerPixel={other} cannot carry a WebP payload (3 or 4 only)"
        ))),
    }
}

/// Encode one strip / tile of chunky 8-bit samples (`raw`, exactly
/// `width * rows * spp` bytes with `spp` ∈ {3, 4}) as one complete
/// lossless (`VP8L`) WebP file — the Compression=50001 segment
/// payload. `spp` is recovered from the byte count; the encoder-side
/// validation in [`crate::encoder`] restricts Compression=50001 to
/// the `Rgb24` / `Rgba32` inputs, so the division is always exact.
///
/// Lossless VP8L is the natural fit for a TIFF writer: every
/// Compression=50001 page round-trips pixel-exact, matching the
/// guarantee of all the other compression schemes this encoder emits.
pub(crate) fn pack_webp(raw: &[u8], width: u32, rows: u32) -> Result<Vec<u8>> {
    let pixels = (width as usize) * (rows as usize);
    if pixels == 0 {
        return Err(Error::invalid("TIFF/WebP: zero-pixel strip/tile"));
    }
    let rgba: std::borrow::Cow<'_, [u8]> = if raw.len() == pixels * 4 {
        std::borrow::Cow::Borrowed(raw)
    } else if raw.len() == pixels * 3 {
        // RGB in: interleave an opaque alpha byte per pixel for WebP's
        // RGBA surface. VP8L stores the constant channel in a handful
        // of bytes, so the padding is free on the wire.
        let mut v = Vec::with_capacity(pixels * 4);
        for px in raw.chunks_exact(3) {
            v.extend_from_slice(px);
            v.push(0xFF);
        }
        std::borrow::Cow::Owned(v)
    } else {
        return Err(Error::invalid(format!(
            "TIFF/WebP: strip/tile has {} bytes for {width}x{rows}; Compression=50001 \
             carries 8-bit chunky RGB or RGBA only",
            raw.len()
        )));
    };
    oxideav_webp::encode_webp_lossless(&rgba, width, rows).map_err(|e| {
        Error::invalid(format!(
            "TIFF/WebP: lossless WebP encode of a {width}x{rows} strip/tile failed: {e}"
        ))
    })
}
