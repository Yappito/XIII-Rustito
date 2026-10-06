//! Error types for the Bink container reader, table locator and decoder.

use std::fmt;

/// Categories of [`VideoError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoErrorKind {
    /// The bytes are too short for the structure being read.
    Truncated,
    /// A signature or version field is wrong or unsupported.
    BadSignature,
    /// A numeric field or offset is inconsistent or out of range.
    BadValue,
    /// A table could not be located in the installed `binkw32.dll`.
    TableNotFound,
    /// A located table failed a structural invariant.
    BadTable,
    /// A file-system read failed.
    Io,
    /// The bitstream ended before the expected data was read.
    OutOfData,
    /// A block/plane used a feature this decoder does not implement.
    Unsupported,
}

impl VideoErrorKind {
    /// Stable machine-readable tag.
    pub fn as_str(self) -> &'static str {
        match self {
            VideoErrorKind::Truncated => "truncated",
            VideoErrorKind::BadSignature => "bad-signature",
            VideoErrorKind::BadValue => "bad-value",
            VideoErrorKind::TableNotFound => "table-not-found",
            VideoErrorKind::BadTable => "bad-table",
            VideoErrorKind::Io => "io",
            VideoErrorKind::OutOfData => "out-of-data",
            VideoErrorKind::Unsupported => "unsupported",
        }
    }
}

/// A bounded error with the byte/file offset where it was detected.
#[derive(Debug, Clone)]
pub struct VideoError {
    kind: VideoErrorKind,
    offset: Option<u64>,
    message: String,
}

impl VideoError {
    /// Creates an error with no offset.
    pub fn new(kind: VideoErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            offset: None,
            message: message.into(),
        }
    }

    /// Creates an error with a byte offset.
    pub fn at(kind: VideoErrorKind, offset: u64, message: impl Into<String>) -> Self {
        Self {
            kind,
            offset: Some(offset),
            message: message.into(),
        }
    }

    /// Error category.
    pub fn kind(&self) -> VideoErrorKind {
        self.kind
    }

    /// Byte offset associated with the error, when known.
    pub fn offset(&self) -> Option<u64> {
        self.offset
    }

    /// Human-readable message (without the offset prefix).
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(o) => write!(f, "{} at byte {}: {}", self.kind.as_str(), o, self.message),
            None => write!(f, "{}: {}", self.kind.as_str(), self.message),
        }
    }
}

impl std::error::Error for VideoError {}

impl From<std::io::Error> for VideoError {
    fn from(e: std::io::Error) -> Self {
        VideoError::new(VideoErrorKind::Io, e.to_string())
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, VideoError>;
