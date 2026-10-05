//! Contextual errors for script payload decoding.
//!
//! Every error carries the absolute file offset of the failing read when known, the memory
//! (code) offset inside a bytecode stream when the failure happened while decoding tokens, the
//! export index and the field being read. Unknown opcodes are reported explicitly; nothing is
//! skipped silently.

use std::fmt;

use xiii_package::PackageError;

/// What went wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptErrorKind {
    /// Low-level read failure from `xiii-package` (end of data, compact-index overflow, bad
    /// tagged-property layout, ...).
    Package(Box<PackageError>),
    /// An opcode outside the accepted UE2 v100 token table.
    UnknownToken {
        /// The opcode byte.
        opcode: u8,
    },
    /// Tokens consumed more memory bytes than the serialized script size.
    ScriptOverrun {
        /// Memory offset after the last token.
        memory_offset: u32,
        /// Declared (in-memory) script size.
        script_size: u32,
    },
    /// The declared script size is negative or above [`crate::ScriptLimits::max_script_size`].
    ScriptSizeInvalid {
        /// Stored value.
        size: i32,
        /// Configured maximum.
        max: u32,
    },
    /// Expression nesting deeper than [`crate::ScriptLimits::max_depth`].
    DepthExceeded {
        /// Configured maximum.
        max: u32,
    },
    /// More tokens than [`crate::ScriptLimits::max_tokens`] in one script.
    TooManyTokens {
        /// Configured maximum.
        max: u32,
    },
    /// A count (array length, label table, string constant) above its configured limit.
    CountTooLarge {
        /// What was being counted.
        what: &'static str,
        /// Stored count.
        count: i64,
        /// Configured maximum.
        max: u64,
    },
    /// Object reference outside the import/export tables.
    BadObjectRef {
        /// Stored value.
        raw: i32,
    },
    /// Name index outside the name table.
    BadNameIndex {
        /// Stored value.
        index: i32,
    },
    /// Decoding ended before the payload end.
    TrailingBytes {
        /// Absolute offset where decoding stopped.
        consumed_to: usize,
        /// Absolute payload end.
        payload_end: usize,
    },
    /// The export's class has no reader here (e.g. a map/fixed-array property).
    UnsupportedClass {
        /// Class path of the export.
        class: String,
    },
    /// A jump/label/state target that does not fit the field width.
    ValueOutOfRange {
        /// Description.
        what: &'static str,
        /// Value.
        value: i64,
    },
}

/// A script decoding failure with context.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    /// The failure.
    pub kind: ScriptErrorKind,
    /// Absolute file offset at which the failing read started, when known.
    pub offset: Option<usize>,
    /// Memory (code) offset inside the bytecode stream, when the failure happened there.
    pub script_offset: Option<u32>,
    /// Zero-based export index, when known.
    pub export: Option<u32>,
    /// Field or token being decoded.
    pub field: Option<&'static str>,
}

/// Result alias.
pub type Result<T> = std::result::Result<T, ScriptError>;

impl ScriptError {
    /// New error at an absolute offset.
    pub fn at(kind: ScriptErrorKind, offset: usize) -> Self {
        Self {
            kind,
            offset: Some(offset),
            script_offset: None,
            export: None,
            field: None,
        }
    }

    /// New error without an offset.
    pub fn new(kind: ScriptErrorKind) -> Self {
        Self {
            kind,
            offset: None,
            script_offset: None,
            export: None,
            field: None,
        }
    }

    /// Wraps a package error, rebasing its slice-relative offset by `base`.
    pub fn from_package(mut e: PackageError, base: usize) -> Self {
        let offset = e.offset.take().map(|o| o as usize + base);
        Self {
            kind: ScriptErrorKind::Package(Box::new(e)),
            offset,
            script_offset: None,
            export: None,
            field: None,
        }
    }

    /// Sets the export index if not already set.
    pub fn in_export(mut self, export: u32) -> Self {
        self.export.get_or_insert(export);
        self
    }

    /// Sets the field if not already set.
    pub fn in_field(mut self, field: &'static str) -> Self {
        self.field.get_or_insert(field);
        self
    }

    /// Sets the memory offset if not already set.
    pub fn at_script(mut self, offset: u32) -> Self {
        self.script_offset.get_or_insert(offset);
        self
    }

    /// Short stable key for histograms (no offsets or values).
    pub fn kind_key(&self) -> String {
        match &self.kind {
            ScriptErrorKind::Package(p) => format!("Package:{}", package_kind_name(p)),
            ScriptErrorKind::UnknownToken { opcode } => format!("UnknownToken(0x{opcode:02X})"),
            ScriptErrorKind::ScriptOverrun { .. } => "ScriptOverrun".into(),
            ScriptErrorKind::ScriptSizeInvalid { .. } => "ScriptSizeInvalid".into(),
            ScriptErrorKind::DepthExceeded { .. } => "DepthExceeded".into(),
            ScriptErrorKind::TooManyTokens { .. } => "TooManyTokens".into(),
            ScriptErrorKind::CountTooLarge { what, .. } => format!("CountTooLarge({what})"),
            ScriptErrorKind::BadObjectRef { .. } => "BadObjectRef".into(),
            ScriptErrorKind::BadNameIndex { .. } => "BadNameIndex".into(),
            ScriptErrorKind::TrailingBytes { .. } => "TrailingBytes".into(),
            ScriptErrorKind::UnsupportedClass { class } => format!("UnsupportedClass({class})"),
            ScriptErrorKind::ValueOutOfRange { what, .. } => format!("ValueOutOfRange({what})"),
        }
    }
}

fn package_kind_name(p: &PackageError) -> String {
    let s = format!("{:?}", p.kind);
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or("?")
        .to_owned()
}

impl fmt::Display for ScriptErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptErrorKind::Package(p) => write!(f, "{p}"),
            ScriptErrorKind::UnknownToken { opcode } => {
                write!(f, "unknown bytecode token 0x{opcode:02X}")
            }
            ScriptErrorKind::ScriptOverrun {
                memory_offset,
                script_size,
            } => write!(
                f,
                "tokens end at memory offset {memory_offset}, past the script size {script_size}"
            ),
            ScriptErrorKind::ScriptSizeInvalid { size, max } => {
                write!(f, "script size {size} outside 0..={max}")
            }
            ScriptErrorKind::DepthExceeded { max } => {
                write!(f, "expression nesting deeper than {max}")
            }
            ScriptErrorKind::TooManyTokens { max } => write!(f, "more than {max} tokens"),
            ScriptErrorKind::CountTooLarge { what, count, max } => {
                write!(f, "{what} count {count} outside 0..={max}")
            }
            ScriptErrorKind::BadObjectRef { raw } => {
                write!(f, "object reference {raw} outside the import/export tables")
            }
            ScriptErrorKind::BadNameIndex { index } => {
                write!(f, "name index {index} outside the name table")
            }
            ScriptErrorKind::TrailingBytes {
                consumed_to,
                payload_end,
            } => write!(
                f,
                "decoding stopped at {consumed_to}, payload ends at {payload_end} ({} bytes left)",
                *payload_end as i64 - *consumed_to as i64
            ),
            ScriptErrorKind::UnsupportedClass { class } => {
                write!(f, "no script reader for class {class}")
            }
            ScriptErrorKind::ValueOutOfRange { what, value } => {
                write!(f, "{what} value {value} out of range")
            }
        }
    }
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)?;
        if let Some(e) = self.export {
            write!(f, " [export {e}]")?;
        }
        if let Some(field) = self.field {
            write!(f, " [{field}]")?;
        }
        if let Some(o) = self.script_offset {
            write!(f, " [code offset 0x{o:04X}]")?;
        }
        if let Some(o) = self.offset {
            write!(f, " at file offset {o}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ScriptError {}
