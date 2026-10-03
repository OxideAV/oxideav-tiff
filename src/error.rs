//! Crate-local error type used by `oxideav-tiff`'s standalone
//! (no `oxideav-core`) public API.
//!
//! Defined as a small std-only enum so the crate can be built with the
//! default `registry` feature off — i.e. without depending on
//! `oxideav-core` at all. When the `registry` feature is on (the
//! default) a `From<TiffError> for oxideav_core::Error` impl is enabled
//! in [`crate::registry`] so the `Decoder` / `Encoder` trait surface
//! still interoperates cleanly.
//!
//! The variants are the image-crate contract's floor
//! (`IMAGE_CRATE_API`): `InvalidData`, `Unsupported`, `LimitExceeded`,
//! `Io`. The enum carries a `std::io::Error`, so it derives neither
//! `Clone` nor `PartialEq`; tests match on variants or `Display`.

use core::fmt;

/// Crate-local error type for the TIFF decoder / encoder pipeline.
#[derive(Debug)]
#[non_exhaustive]
pub enum TiffError {
    /// Bitstream / IFD / strip layout was malformed.
    InvalidData(String),
    /// Bitstream was syntactically valid but uses a feature this crate
    /// does not implement, or an encode input the format cannot carry.
    Unsupported(String),
    /// A [`crate::DecodeOptions`] limit (dimensions, pixel count,
    /// decoded bytes) would be exceeded; nothing was allocated.
    LimitExceeded(String),
    /// An I/O error from [`crate::decode_from`] / [`crate::encode_to`].
    Io(std::io::Error),
}

/// The contract name for [`TiffError`].
pub type Error = TiffError;

impl TiffError {
    /// Construct a [`TiffError::InvalidData`] from a stringy message.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidData(msg.into())
    }

    /// Construct a [`TiffError::Unsupported`] from a stringy message.
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }

    /// Construct a [`TiffError::LimitExceeded`] from a stringy message.
    pub fn limit(msg: impl Into<String>) -> Self {
        Self::LimitExceeded(msg.into())
    }
}

impl fmt::Display for TiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidData(s) => write!(f, "invalid data: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::LimitExceeded(s) => write!(f, "limit exceeded: {s}"),
            Self::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for TiffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TiffError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// `Result` alias scoped to `oxideav-tiff`. Standalone (no
/// `oxideav-core`) callers see this; framework callers convert via the
/// gated `From<TiffError> for oxideav_core::Error` impl.
pub type Result<T> = core::result::Result<T, TiffError>;
