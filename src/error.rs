//! Crate-local error type used by `oxideav-tiff`'s standalone
//! (no `oxideav-core`) public API.
//!
//! Defined as a small std-only enum so the crate can be built with the
//! default `registry` feature off — i.e. without depending on
//! `oxideav-core` at all. When the `registry` feature is on (the
//! default) a `From<TiffError> for oxideav_core::Error` impl is enabled
//! in [`crate::registry`] so the `Decoder` trait surface still
//! interoperates cleanly.
//!
//! The variants mirror the subset of `oxideav_core::Error` the TIFF
//! decoder pipeline actually produces.

use core::fmt;

/// Crate-local error type for the TIFF decoder pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TiffError {
    /// Bitstream / IFD / strip layout was malformed.
    InvalidData(String),
    /// Bitstream was syntactically valid but uses a feature this crate
    /// does not implement yet.
    Unsupported(String),
}

impl TiffError {
    /// Construct a [`TiffError::InvalidData`] from a stringy message.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidData(msg.into())
    }

    /// Construct a [`TiffError::Unsupported`] from a stringy message.
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }
}

impl fmt::Display for TiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidData(s) => write!(f, "invalid data: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

impl std::error::Error for TiffError {}

/// `Result` alias scoped to `oxideav-tiff`. Standalone (no
/// `oxideav-core`) callers see this; framework callers convert via the
/// gated `From<TiffError> for oxideav_core::Error` impl.
pub type Result<T> = core::result::Result<T, TiffError>;
