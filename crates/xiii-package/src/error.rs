//! Contextual parse errors. Every error carries the absolute byte offset (when known) and
//! the table/entry/field that was being read, so malformed packages can be diagnosed without
//! a debugger.

use std::fmt;

/// Package structure being read when an error occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {
    /// Fixed package summary (magic, version, counts, offsets, GUID).
    Summary,
    /// Generation records following the GUID.
    Generations,
    /// Name table.
    Names,
    /// Import table.
    Imports,
    /// Export table.
    Exports,
    /// An export's serialized payload (state frame, tagged properties); `index` is the export.
    Payload,
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Table::Summary => "summary",
            Table::Generations => "generations",
            Table::Names => "names",
            Table::Imports => "imports",
            Table::Exports => "exports",
            Table::Payload => "payload",
        })
    }
}

/// What went wrong, independent of where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A read needed more bytes than remain in the buffer.
    UnexpectedEof {
        /// Bytes requested.
        needed: u64,
        /// Bytes remaining at the read position.
        available: u64,
    },
    /// A seek target lies beyond the end of the buffer.
    SeekOutOfBounds {
        /// Requested absolute position.
        position: u64,
        /// Buffer length.
        len: u64,
    },
    /// The first four bytes are not the Unreal package tag `0x9E2A83C1`.
    BadMagic {
        /// Value found, read little-endian.
        found: u32,
    },
    /// Only package version 100 is supported (the measured XIII dialect).
    UnsupportedVersion {
        /// File version.
        version: u16,
        /// Licensee version.
        licensee: u16,
    },
    /// The fifth byte of a compact index has bits set above the 32-bit magnitude.
    CompactIndexOverflow,
    /// A compact index magnitude does not fit a signed 32-bit integer.
    CompactIndexOutOfRange {
        /// Decoded magnitude.
        magnitude: u64,
        /// Sign bit of the first byte.
        negative: bool,
    },
    /// A string length (in code units, including the terminator) exceeds the limit.
    StringTooLong {
        /// Signed length prefix as stored.
        length: i32,
        /// Configured maximum code units.
        max: u32,
    },
    /// A string's final code unit is not NUL.
    UnterminatedString,
    /// A UTF-16 string contains an unpaired surrogate.
    InvalidUtf16,
    /// A count is negative or exceeds the configured limit.
    CountOutOfRange {
        /// Which count.
        what: &'static str,
        /// Stored value.
        count: i64,
        /// Configured maximum.
        max: u64,
    },
    /// A table offset is negative or beyond the end of the file.
    OffsetOutOfRange {
        /// Which offset.
        what: &'static str,
        /// Stored value.
        offset: i64,
        /// File length.
        file_len: u64,
    },
    /// A table cannot possibly fit in the remaining bytes, even at minimum entry size.
    /// Checked before allocating storage for the table.
    TableExceedsData {
        /// Entry count from the summary.
        count: u64,
        /// Minimum encoded size of one entry.
        min_entry_size: u64,
        /// Bytes from the table offset to the end of the file.
        available: u64,
    },
    /// A name-table reference is outside the name table.
    NameIndexOutOfRange {
        /// Stored index.
        index: i32,
        /// Name count.
        count: u32,
    },
    /// An object reference is outside `-imports..=exports`.
    ObjectRefOutOfRange {
        /// Stored reference.
        raw: i32,
        /// Import count.
        imports: u32,
        /// Export count.
        exports: u32,
    },
    /// Following `outer` links from `start` revisits an object.
    OuterCycle {
        /// Raw reference the walk started from.
        start: i32,
        /// Raw reference that was seen twice.
        repeated: i32,
    },
    /// Following `outer` links from `start` exceeds the depth limit.
    OuterDepthExceeded {
        /// Raw reference the walk started from.
        start: i32,
        /// Configured maximum depth.
        max: u32,
    },
    /// An export's serial size is negative.
    NegativeSerialSize {
        /// Stored size.
        size: i32,
    },
    /// An export's serial span is negative or ends beyond the file.
    ExportSpanOutOfRange {
        /// Stored serial offset.
        offset: i64,
        /// Stored serial size.
        size: i64,
        /// File length.
        file_len: u64,
    },
    /// A payload accessor was given a buffer whose length differs from the parsed package.
    BufferLengthMismatch {
        /// Length of the buffer the package was parsed from.
        expected: u64,
        /// Length of the buffer supplied.
        found: u64,
    },
    /// An export index is outside the export table.
    ExportIndexOutOfRange {
        /// Requested zero-based index.
        index: u64,
        /// Export count.
        count: u32,
    },
    /// The export has no serialized payload (serial size 0).
    EmptyPayload,
    /// A property tag's type nibble is 0, which is not a property type.
    InvalidPropertyType {
        /// Low four bits of the info byte.
        code: u8,
    },
    /// A property's value size is negative or exceeds the bytes left in the payload.
    PropertySizeOutOfRange {
        /// Declared size.
        size: i64,
        /// Bytes remaining in the payload after the tag.
        available: u64,
    },
    /// The payload ended before a property tag named `None`.
    MissingPropertyTerminator,
    /// A property block holds more tags than the configured limit.
    TooManyProperties {
        /// Configured maximum.
        max: u32,
    },
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::UnexpectedEof { needed, available } => {
                write!(
                    f,
                    "unexpected end of data: needed {needed} bytes, {available} available"
                )
            }
            ErrorKind::SeekOutOfBounds { position, len } => {
                write!(f, "seek to {position} beyond buffer of {len} bytes")
            }
            ErrorKind::BadMagic { found } => {
                write!(f, "expected package tag 0x9e2a83c1, found 0x{found:08x}")
            }
            ErrorKind::UnsupportedVersion { version, licensee } => {
                write!(
                    f,
                    "unsupported package version {version}/{licensee} (expected 100)"
                )
            }
            ErrorKind::CompactIndexOverflow => {
                f.write_str("compact index exceeds 32-bit magnitude")
            }
            ErrorKind::CompactIndexOutOfRange {
                magnitude,
                negative,
            } => write!(
                f,
                "compact index {}{magnitude} outside signed i32",
                if *negative { "-" } else { "" }
            ),
            ErrorKind::StringTooLong { length, max } => {
                write!(
                    f,
                    "string length {length} exceeds limit of {max} code units"
                )
            }
            ErrorKind::UnterminatedString => f.write_str("string is not NUL-terminated"),
            ErrorKind::InvalidUtf16 => f.write_str("string contains invalid UTF-16"),
            ErrorKind::CountOutOfRange { what, count, max } => {
                write!(f, "{what} {count} outside 0..={max}")
            }
            ErrorKind::OffsetOutOfRange {
                what,
                offset,
                file_len,
            } => {
                write!(f, "{what} {offset} outside file of {file_len} bytes")
            }
            ErrorKind::TableExceedsData {
                count,
                min_entry_size,
                available,
            } => write!(
                f,
                "{count} entries of at least {min_entry_size} bytes cannot fit in {available} bytes"
            ),
            ErrorKind::NameIndexOutOfRange { index, count } => {
                write!(f, "name index {index} outside name table of {count}")
            }
            ErrorKind::ObjectRefOutOfRange {
                raw,
                imports,
                exports,
            } => write!(f, "object reference {raw} outside -{imports}..={exports}"),
            ErrorKind::OuterCycle { start, repeated } => {
                write!(f, "outer chain from {start} revisits object {repeated}")
            }
            ErrorKind::OuterDepthExceeded { start, max } => {
                write!(f, "outer chain from {start} exceeds depth limit {max}")
            }
            ErrorKind::NegativeSerialSize { size } => write!(f, "negative serial size {size}"),
            ErrorKind::ExportSpanOutOfRange {
                offset,
                size,
                file_len,
            } => write!(
                f,
                "serial span {offset}+{size} outside file of {file_len} bytes"
            ),
            ErrorKind::BufferLengthMismatch { expected, found } => write!(
                f,
                "buffer of {found} bytes is not the parsed package ({expected} bytes)"
            ),
            ErrorKind::ExportIndexOutOfRange { index, count } => {
                write!(f, "export index {index} outside export table of {count}")
            }
            ErrorKind::EmptyPayload => f.write_str("export has no serialized payload"),
            ErrorKind::InvalidPropertyType { code } => {
                write!(f, "invalid property type {code}")
            }
            ErrorKind::PropertySizeOutOfRange { size, available } => write!(
                f,
                "property size {size} exceeds {available} remaining payload bytes"
            ),
            ErrorKind::MissingPropertyTerminator => {
                f.write_str("payload ended before the 'None' property terminator")
            }
            ErrorKind::TooManyProperties { max } => {
                write!(f, "more than {max} properties in one block")
            }
        }
    }
}

/// A parse failure with location context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageError {
    /// What went wrong.
    pub kind: ErrorKind,
    /// Absolute byte offset of the failing read, when known.
    pub offset: Option<u64>,
    /// Structure being read.
    pub table: Option<Table>,
    /// Zero-based entry index within `table`.
    pub index: Option<u32>,
    /// Field within the entry, e.g. `"class"` or `"outer"`.
    pub field: Option<&'static str>,
}

impl PackageError {
    /// Creates an error with no location context.
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            offset: None,
            table: None,
            index: None,
            field: None,
        }
    }

    /// Creates an error at an absolute byte offset.
    pub fn at(kind: ErrorKind, offset: usize) -> Self {
        Self {
            offset: Some(offset as u64),
            ..Self::new(kind)
        }
    }

    /// Sets the table/entry context unless already set.
    #[must_use]
    pub fn in_entry(mut self, table: Table, index: Option<u32>) -> Self {
        if self.table.is_none() {
            self.table = Some(table);
            self.index = index;
        }
        self
    }

    /// Sets the field context unless already set.
    #[must_use]
    pub fn in_field(mut self, field: &'static str) -> Self {
        if self.field.is_none() {
            self.field = Some(field);
        }
        self
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut wrote = false;
        if let Some(table) = self.table {
            write!(f, "{table}")?;
            if let Some(index) = self.index {
                write!(f, "[{index}]")?;
            }
            wrote = true;
        }
        if let Some(field) = self.field {
            write!(f, "{}{field}", if wrote { "." } else { "" })?;
            wrote = true;
        }
        if let Some(offset) = self.offset {
            write!(
                f,
                "{}at offset {offset} (0x{offset:x})",
                if wrote { " " } else { "" }
            )?;
            wrote = true;
        }
        if wrote {
            f.write_str(": ")?;
        }
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for PackageError {}

/// Result alias for this crate.
pub type Result<T, E = PackageError> = std::result::Result<T, E>;
