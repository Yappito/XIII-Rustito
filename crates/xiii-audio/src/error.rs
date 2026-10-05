//! Contextual errors for HX bank parsing and audio decoding.
//!
//! Every failure carries the byte offset (when known) and a short context so a corpus run can
//! point at the exact structure that failed. Parsing never panics on malformed input.

use std::fmt;

/// Broad category of an audio error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioErrorKind {
    /// The input ended before a required field or range.
    Truncated,
    /// A magic tag (`INDX`, `RIFF`, ...) did not match.
    BadTag,
    /// The HX index type is neither 1 nor 2.
    BadIndexType,
    /// A declared count or size is larger than the remaining bytes / a configured limit.
    BadSize,
    /// A class name or other string was not valid UTF-8/ASCII.
    BadString,
    /// An index entry uses an unknown resource class.
    UnknownClass,
    /// A wave resource uses an unsupported stream mode.
    BadStreamMode,
    /// The embedded or external RIFF header is malformed.
    BadRiff,
    /// The RIFF codec id is not handled.
    UnsupportedCodec,
    /// A required RIFF chunk (`data`/`datx`) is missing.
    MissingChunk,
    /// The UBI ADPCM stream header is malformed or inconsistent.
    BadAdpcmHeader,
    /// A caller-provided external stream file is missing or too short.
    ExternalData,
    /// The decoded sample count disagrees with the declared sample count.
    SampleCountMismatch,
    /// The requested entry/index does not exist.
    NoSuchEntry,
}

impl AudioErrorKind {
    /// Short stable identifier used in reports and tests.
    pub fn as_str(self) -> &'static str {
        match self {
            AudioErrorKind::Truncated => "truncated",
            AudioErrorKind::BadTag => "bad_tag",
            AudioErrorKind::BadIndexType => "bad_index_type",
            AudioErrorKind::BadSize => "bad_size",
            AudioErrorKind::BadString => "bad_string",
            AudioErrorKind::UnknownClass => "unknown_class",
            AudioErrorKind::BadStreamMode => "bad_stream_mode",
            AudioErrorKind::BadRiff => "bad_riff",
            AudioErrorKind::UnsupportedCodec => "unsupported_codec",
            AudioErrorKind::MissingChunk => "missing_chunk",
            AudioErrorKind::BadAdpcmHeader => "bad_adpcm_header",
            AudioErrorKind::ExternalData => "external_data",
            AudioErrorKind::SampleCountMismatch => "sample_count_mismatch",
            AudioErrorKind::NoSuchEntry => "no_such_entry",
        }
    }
}

/// An error produced while parsing a bank or decoding audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioError {
    kind: AudioErrorKind,
    offset: Option<usize>,
    context: String,
}

impl AudioError {
    /// Builds an error with an explicit byte offset.
    pub fn at(kind: AudioErrorKind, offset: usize, context: impl Into<String>) -> Self {
        Self {
            kind,
            offset: Some(offset),
            context: context.into(),
        }
    }

    /// Builds an error without a meaningful byte offset.
    pub fn new(kind: AudioErrorKind, context: impl Into<String>) -> Self {
        Self {
            kind,
            offset: None,
            context: context.into(),
        }
    }

    /// The broad error category.
    pub fn kind(&self) -> AudioErrorKind {
        self.kind
    }

    /// The byte offset the failure relates to, if known.
    pub fn offset(&self) -> Option<usize> {
        self.offset
    }

    /// A human-readable description of the failing structure.
    pub fn context(&self) -> &str {
        &self.context
    }
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(off) => write!(f, "{} at 0x{off:x}: {}", self.kind.as_str(), self.context),
            None => write!(f, "{}: {}", self.kind.as_str(), self.context),
        }
    }
}

impl std::error::Error for AudioError {}

/// Convenience result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, AudioError>;
