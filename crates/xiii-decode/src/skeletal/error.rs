//! Error type of the skeletal decoders.

use std::fmt;

/// Why a skeletal-mesh or animation payload could not be decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum SkelErrorKind {
    /// The export is not of the expected class.
    WrongClass {
        /// Class path found in the export table.
        found: String,
    },
    /// Package-level failure (payload slicing, property block).
    Package(String),
    /// A read ran past the end of the payload.
    UnexpectedEof {
        /// Bytes needed.
        needed: u64,
        /// Bytes available.
        available: u64,
    },
    /// A TArray count was negative.
    NegativeCount(i32),
    /// A TArray count exceeded the hard limit.
    CountTooLarge {
        /// Stored count.
        count: u64,
        /// Limit.
        max: u64,
    },
    /// count x minimum element size exceeds the remaining payload.
    ArrayExceedsData {
        /// Stored count.
        count: u64,
        /// Minimum element size in bytes.
        elem_size: u64,
        /// Bytes remaining.
        available: u64,
    },
    /// Name index outside the name table.
    NameOutOfRange(i32),
    /// Object reference outside the import/export tables.
    ObjectRefOutOfRange(i32),
    /// A layout variant that was never observed in the corpus and is not implemented.
    Unsupported(String),
    /// Data decoded but violates a structural invariant (index ranges, counts).
    Invalid(String),
    /// Bytes left after the last known field (unsupported tail).
    TrailingBytes {
        /// Bytes left over.
        remaining: u64,
    },
}

/// A decoding failure with class, export, field and absolute file offset.
#[derive(Debug, Clone, PartialEq)]
pub struct SkelError {
    /// Class being decoded (`SkeletalMesh`, `MeshAnimation`).
    pub class: &'static str,
    /// Zero-based export index.
    pub export: Option<u32>,
    /// Absolute file offset where the failing read started.
    pub offset: Option<u64>,
    /// Field being read.
    pub field: &'static str,
    /// What went wrong.
    pub kind: SkelErrorKind,
}

impl fmt::Display for SkelErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongClass { found } => write!(f, "wrong class {found}"),
            Self::Package(e) => write!(f, "package error: {e}"),
            Self::UnexpectedEof { needed, available } => {
                write!(
                    f,
                    "unexpected end of payload (need {needed}, have {available})"
                )
            }
            Self::NegativeCount(n) => write!(f, "negative array count {n}"),
            Self::CountTooLarge { count, max } => write!(f, "array count {count} above {max}"),
            Self::ArrayExceedsData {
                count,
                elem_size,
                available,
            } => write!(
                f,
                "array of {count} x >= {elem_size} bytes exceeds {available} remaining bytes"
            ),
            Self::NameOutOfRange(i) => write!(f, "name index {i} out of range"),
            Self::ObjectRefOutOfRange(i) => write!(f, "object reference {i} out of range"),
            Self::Unsupported(s) => write!(f, "unsupported: {s}"),
            Self::Invalid(s) => write!(f, "invalid: {s}"),
            Self::TrailingBytes { remaining } => {
                write!(f, "{remaining} unsupported trailing bytes")
            }
        }
    }
}

impl fmt::Display for SkelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.class)?;
        if let Some(e) = self.export {
            write!(f, " export {e}")?;
        }
        write!(f, " field {}", self.field)?;
        if let Some(o) = self.offset {
            write!(f, " at offset {o}")?;
        }
        write!(f, ": {}", self.kind)
    }
}

impl std::error::Error for SkelError {}
