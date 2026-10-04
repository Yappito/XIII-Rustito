//! Package summary, name/import/export tables and object-graph resolution for the measured
//! XIII package dialect (file version 100; licensee versions 50/56/57/58 observed).

use std::collections::{BTreeMap, BTreeSet};

use crate::cursor::Cursor;
use crate::error::{ErrorKind, PackageError, Result, Table};

/// Unreal package tag, as read little-endian from the first four bytes.
pub const PACKAGE_TAG: u32 = 0x9e2a_83c1;
/// The only file version this reader accepts.
pub const SUPPORTED_VERSION: u16 = 100;
/// Class path reported for exports whose class reference is null (UClass objects).
pub const NULL_CLASS_PATH: &str = "Core.Class";

/// Minimum encoded sizes, used to reject impossible counts before allocating.
const NAME_MIN_SIZE: u64 = 1 + 4; // empty FString + flags
const IMPORT_MIN_SIZE: u64 = 1 + 1 + 4 + 1; // 3 compact indices + i32 outer
const EXPORT_MIN_SIZE: u64 = 1 + 1 + 4 + 1 + 4 + 1; // class, super, outer, name, flags, size
const GENERATION_SIZE: u64 = 8;

/// Explicit resource limits. All are checked before the corresponding allocation or walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Maximum name-table entries.
    pub max_names: u32,
    /// Maximum import-table entries.
    pub max_imports: u32,
    /// Maximum export-table entries.
    pub max_exports: u32,
    /// Maximum generation records in the summary.
    pub max_generations: u32,
    /// Maximum code units (including the terminator) in one string.
    pub max_string_units: u32,
    /// Maximum objects in one outer chain (an object plus all of its outers).
    pub max_outer_chain: u32,
    /// Maximum tags (including the `None` terminator) in one property block.
    pub max_properties: u32,
}

impl Default for Limits {
    /// Defaults match the research probe (`MAX_ITEMS = 1_000_000`, 257-object outer chains)
    /// with a generation cap well above the measured maximum of 3.
    fn default() -> Self {
        Self {
            max_names: 1_000_000,
            max_imports: 1_000_000,
            max_exports: 1_000_000,
            max_generations: 4096,
            max_string_units: 1_000_000,
            max_outer_chain: 257,
            max_properties: 65_536,
        }
    }
}

/// Half-open absolute byte range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    /// First byte.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
}

impl Span {
    /// Byte length.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// True for an empty range.
    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// Count and absolute offset of one table, as stored in the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableLocation {
    /// Entry count.
    pub count: u32,
    /// Absolute offset of the first entry.
    pub offset: u32,
}

/// One generation record: table sizes at a previous save of the package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation {
    /// Export count at that generation.
    pub export_count: i32,
    /// Name count at that generation.
    pub name_count: i32,
}

/// Package summary (header). Layout for version 100, little-endian:
///
/// ```text
/// 0   u32  tag 0x9E2A83C1        20  i32 export count
/// 4   u16  file version (100)    24  i32 export offset
/// 6   u16  licensee version      28  i32 import count
/// 8   u32  package flags         32  i32 import offset
/// 12  i32  name count            36  [u8;16] GUID
/// 16  i32  name offset           52  i32 generation count, then {i32 exports, i32 names}*
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// File version (always 100 after a successful parse).
    pub version: u16,
    /// Licensee version.
    pub licensee: u16,
    /// Package flags.
    pub package_flags: u32,
    /// Name table location.
    pub names: TableLocation,
    /// Export table location.
    pub exports: TableLocation,
    /// Import table location.
    pub imports: TableLocation,
    /// Raw 16-byte package GUID.
    pub guid: [u8; 16],
    /// Generation records in file order.
    pub generations: Vec<Generation>,
    /// Absolute offset just past the last generation record.
    pub header_end: usize,
}

impl Summary {
    /// Parses the summary from the start of `data`.
    pub fn parse(data: &[u8], limits: &Limits) -> Result<Self> {
        let in_summary = |e: PackageError| e.in_entry(Table::Summary, None);
        let mut c = Cursor::new(data);
        let tag = c.u32().map_err(|e| in_summary(e).in_field("tag"))?;
        if tag != PACKAGE_TAG {
            return Err(in_summary(PackageError::at(
                ErrorKind::BadMagic { found: tag },
                0,
            )));
        }
        let version = c.u16().map_err(|e| in_summary(e).in_field("version"))?;
        let licensee = c.u16().map_err(|e| in_summary(e).in_field("licensee"))?;
        if version != SUPPORTED_VERSION {
            return Err(in_summary(PackageError::at(
                ErrorKind::UnsupportedVersion { version, licensee },
                4,
            )));
        }
        let package_flags = c
            .u32()
            .map_err(|e| in_summary(e).in_field("package_flags"))?;
        let mut raw = [0i32; 6];
        const FIELDS: [&str; 6] = [
            "name_count",
            "name_offset",
            "export_count",
            "export_offset",
            "import_count",
            "import_offset",
        ];
        for (slot, field) in raw.iter_mut().zip(FIELDS) {
            *slot = c.i32().map_err(|e| in_summary(e).in_field(field))?;
        }
        let file_len = data.len();
        let location = |i: usize, max: u32| -> Result<TableLocation> {
            let (count, offset) = (raw[i], raw[i + 1]);
            let count_pos = 12 + 4 * i;
            let count = u32::try_from(count)
                .ok()
                .filter(|&n| n <= max)
                .ok_or_else(|| {
                    in_summary(PackageError::at(
                        ErrorKind::CountOutOfRange {
                            what: "table count",
                            count: i64::from(count),
                            max: u64::from(max),
                        },
                        count_pos,
                    ))
                    .in_field(FIELDS[i])
                })?;
            let offset = u32::try_from(offset)
                .ok()
                .filter(|&o| o as usize <= file_len)
                .ok_or_else(|| {
                    in_summary(PackageError::at(
                        ErrorKind::OffsetOutOfRange {
                            what: "table offset",
                            offset: i64::from(offset),
                            file_len: file_len as u64,
                        },
                        count_pos + 4,
                    ))
                    .in_field(FIELDS[i + 1])
                })?;
            Ok(TableLocation { count, offset })
        };
        let names = location(0, limits.max_names)?;
        let exports = location(2, limits.max_exports)?;
        let imports = location(4, limits.max_imports)?;

        let guid = c.guid().map_err(|e| in_summary(e).in_field("guid"))?;
        let count_pos = c.pos();
        let generation_count = c
            .i32()
            .map_err(|e| in_summary(e).in_field("generation_count"))?;
        let generation_count = u32::try_from(generation_count)
            .ok()
            .filter(|&n| n <= limits.max_generations)
            .ok_or_else(|| {
                in_summary(PackageError::at(
                    ErrorKind::CountOutOfRange {
                        what: "generation count",
                        count: i64::from(generation_count),
                        max: u64::from(limits.max_generations),
                    },
                    count_pos,
                ))
                .in_field("generation_count")
            })?;
        check_fits(&c, generation_count, GENERATION_SIZE)
            .map_err(|e| e.in_entry(Table::Generations, None))?;
        let mut generations = Vec::with_capacity(generation_count as usize);
        for i in 0..generation_count {
            let ctx = |e: PackageError| e.in_entry(Table::Generations, Some(i));
            let export_count = c.i32().map_err(|e| ctx(e).in_field("export_count"))?;
            let name_count = c.i32().map_err(|e| ctx(e).in_field("name_count"))?;
            generations.push(Generation {
                export_count,
                name_count,
            });
        }
        Ok(Self {
            version,
            licensee,
            package_flags,
            names,
            exports,
            imports,
            guid,
            generations,
            header_end: c.pos(),
        })
    }

    /// Lowest offset among the three tables.
    pub fn first_table_offset(&self) -> usize {
        self.names
            .offset
            .min(self.exports.offset)
            .min(self.imports.offset) as usize
    }

    /// Bytes between the end of the summary and the first table; negative if a table offset
    /// points inside the summary. Zero for every measured XIII package.
    pub fn header_gap(&self) -> i64 {
        self.first_table_offset() as i64 - self.header_end as i64
    }

    /// Whether the newest generation records the current export and name counts.
    /// `None` when there are no generations.
    pub fn latest_generation_matches_tables(&self) -> Option<bool> {
        self.generations.last().map(|g| {
            i64::from(g.export_count) == i64::from(self.exports.count)
                && i64::from(g.name_count) == i64::from(self.names.count)
        })
    }

    /// GUID formatted as four little-endian `u32` words in upper-case hex, the way Unreal
    /// prints `FGuid` (`XXXXXXXX-XXXXXXXX-XXXXXXXX-XXXXXXXX`).
    pub fn guid_string(&self) -> String {
        let w = |i: usize| {
            u32::from_le_bytes([
                self.guid[i],
                self.guid[i + 1],
                self.guid[i + 2],
                self.guid[i + 3],
            ])
        };
        format!("{:08X}-{:08X}-{:08X}-{:08X}", w(0), w(4), w(8), w(12))
    }
}

/// Zero-based name-table index, validated against the table at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameIndex(pub(crate) u32);

impl NameIndex {
    /// Zero-based index (equal to the stored compact value).
    pub fn index(self) -> u32 {
        self.0
    }
}

/// Typed object reference. Stored values: `0` null, `> 0` export `n - 1`, `< 0` import `-n - 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectRef {
    /// No object (stored as 0).
    Null,
    /// Zero-based import-table index.
    Import(u32),
    /// Zero-based export-table index.
    Export(u32),
}

impl ObjectRef {
    /// Converts a stored reference, checking it against the table sizes.
    pub fn from_raw(raw: i32, import_count: u32, export_count: u32) -> Option<Self> {
        match raw {
            0 => Some(ObjectRef::Null),
            r if r > 0 => {
                let i = (r - 1) as u32;
                (i < export_count).then_some(ObjectRef::Export(i))
            }
            r => {
                let i = (-(i64::from(r)) - 1) as u32;
                (i < import_count).then_some(ObjectRef::Import(i))
            }
        }
    }

    /// The stored (raw) form of this reference.
    pub fn raw(self) -> i32 {
        match self {
            ObjectRef::Null => 0,
            ObjectRef::Import(i) => (-i64::from(i) - 1) as i32,
            ObjectRef::Export(i) => (i64::from(i) + 1) as i32,
        }
    }

    /// True for [`ObjectRef::Null`].
    pub fn is_null(self) -> bool {
        self == ObjectRef::Null
    }
}

/// One name-table entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameEntry {
    /// Decoded text.
    pub text: String,
    /// Raw name flags.
    pub flags: u32,
    /// Encoded byte range of the entry.
    pub span: Span,
}

/// One import-table entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// Package containing the object's class (e.g. `Core`).
    pub class_package: NameIndex,
    /// Class name (e.g. `Package`, `Class`, `Texture`).
    pub class_name: NameIndex,
    /// Outer object (null for a root package).
    pub outer: ObjectRef,
    /// Object name.
    pub object_name: NameIndex,
    /// Encoded byte range of the entry.
    pub span: Span,
}

/// One export-table entry. Payload bytes are not decoded here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    /// Class (null means the export is itself a class).
    pub class: ObjectRef,
    /// Super struct/class.
    pub super_ref: ObjectRef,
    /// Outer object (null for top-level objects).
    pub outer: ObjectRef,
    /// Object name.
    pub object_name: NameIndex,
    /// Raw object flags.
    pub flags: u32,
    /// Serialized payload size.
    pub serial_size: u32,
    /// Serialized payload offset; 0 and not stored in the file when `serial_size` is 0.
    pub serial_offset: u32,
    /// Encoded byte range of the table entry.
    pub span: Span,
}

impl Export {
    /// Absolute payload range, or `None` for zero-sized exports.
    pub fn serial_span(&self) -> Option<Span> {
        (self.serial_size > 0).then(|| Span {
            start: self.serial_offset as usize,
            end: self.serial_offset as usize + self.serial_size as usize,
        })
    }
}

/// Byte ranges of the three encoded tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSpans {
    /// Name table.
    pub names: Span,
    /// Import table.
    pub imports: Span,
    /// Export table.
    pub exports: Span,
}

/// A parsed and reference-validated package table set.
///
/// A successful parse guarantees: every name index is in range; every object reference is in
/// range; every outer chain terminates within [`Limits::max_outer_chain`] objects; every export
/// payload span lies inside the file. Payload contents are not inspected.
#[derive(Debug, Clone)]
pub struct Package {
    summary: Summary,
    names: Vec<NameEntry>,
    imports: Vec<Import>,
    exports: Vec<Export>,
    import_paths: Vec<String>,
    export_paths: Vec<String>,
    table_spans: TableSpans,
    file_len: usize,
}

fn check_fits(c: &Cursor<'_>, count: u32, min_entry_size: u64) -> Result<()> {
    let available = c.remaining() as u64;
    let needed = u64::from(count) * min_entry_size;
    if needed > available {
        return Err(PackageError::at(
            ErrorKind::TableExceedsData {
                count: u64::from(count),
                min_entry_size,
                available,
            },
            c.pos(),
        ));
    }
    Ok(())
}

fn read_name_ref(c: &mut Cursor<'_>, name_count: u32) -> Result<NameIndex> {
    let start = c.pos();
    let index = c.compact_index()?;
    match u32::try_from(index) {
        Ok(i) if i < name_count => Ok(NameIndex(i)),
        _ => Err(PackageError::at(
            ErrorKind::NameIndexOutOfRange {
                index,
                count: name_count,
            },
            start,
        )),
    }
}

fn check_object_ref(raw: i32, start: usize, summary: &Summary) -> Result<ObjectRef> {
    let (imports, exports) = (summary.imports.count, summary.exports.count);
    ObjectRef::from_raw(raw, imports, exports).ok_or_else(|| {
        PackageError::at(
            ErrorKind::ObjectRefOutOfRange {
                raw,
                imports,
                exports,
            },
            start,
        )
    })
}

fn read_compact_ref(c: &mut Cursor<'_>, summary: &Summary) -> Result<ObjectRef> {
    let start = c.pos();
    let raw = c.compact_index()?;
    check_object_ref(raw, start, summary)
}

fn read_i32_ref(c: &mut Cursor<'_>, summary: &Summary) -> Result<ObjectRef> {
    let start = c.pos();
    let raw = c.i32()?;
    check_object_ref(raw, start, summary)
}

impl Package {
    /// Parses and validates the summary and all tables of a version-100 package.
    pub fn parse(data: &[u8], limits: &Limits) -> Result<Self> {
        let summary = Summary::parse(data, limits)?;
        let mut c = Cursor::new(data);

        // Names.
        let name_count = summary.names.count;
        c.seek(summary.names.offset as usize)?;
        check_fits(&c, name_count, NAME_MIN_SIZE).map_err(|e| e.in_entry(Table::Names, None))?;
        let mut names = Vec::with_capacity(name_count as usize);
        for i in 0..name_count {
            let ctx = |e: PackageError| e.in_entry(Table::Names, Some(i));
            let start = c.pos();
            let text = c
                .fstring(limits.max_string_units)
                .map_err(|e| ctx(e).in_field("name"))?;
            let flags = c.u32().map_err(|e| ctx(e).in_field("flags"))?;
            names.push(NameEntry {
                text,
                flags,
                span: Span {
                    start,
                    end: c.pos(),
                },
            });
        }
        let names_span = Span {
            start: summary.names.offset as usize,
            end: c.pos(),
        };

        // Imports.
        c.seek(summary.imports.offset as usize)?;
        check_fits(&c, summary.imports.count, IMPORT_MIN_SIZE)
            .map_err(|e| e.in_entry(Table::Imports, None))?;
        let mut imports = Vec::with_capacity(summary.imports.count as usize);
        for i in 0..summary.imports.count {
            let ctx = |e: PackageError| e.in_entry(Table::Imports, Some(i));
            let start = c.pos();
            let class_package =
                read_name_ref(&mut c, name_count).map_err(|e| ctx(e).in_field("class_package"))?;
            let class_name =
                read_name_ref(&mut c, name_count).map_err(|e| ctx(e).in_field("class_name"))?;
            let outer = read_i32_ref(&mut c, &summary).map_err(|e| ctx(e).in_field("outer"))?;
            let object_name =
                read_name_ref(&mut c, name_count).map_err(|e| ctx(e).in_field("object_name"))?;
            imports.push(Import {
                class_package,
                class_name,
                outer,
                object_name,
                span: Span {
                    start,
                    end: c.pos(),
                },
            });
        }
        let imports_span = Span {
            start: summary.imports.offset as usize,
            end: c.pos(),
        };

        // Exports.
        c.seek(summary.exports.offset as usize)?;
        check_fits(&c, summary.exports.count, EXPORT_MIN_SIZE)
            .map_err(|e| e.in_entry(Table::Exports, None))?;
        let mut exports = Vec::with_capacity(summary.exports.count as usize);
        for i in 0..summary.exports.count {
            let ctx = |e: PackageError| e.in_entry(Table::Exports, Some(i));
            let start = c.pos();
            let class = read_compact_ref(&mut c, &summary).map_err(|e| ctx(e).in_field("class"))?;
            let super_ref =
                read_compact_ref(&mut c, &summary).map_err(|e| ctx(e).in_field("super"))?;
            let outer = read_i32_ref(&mut c, &summary).map_err(|e| ctx(e).in_field("outer"))?;
            let object_name =
                read_name_ref(&mut c, name_count).map_err(|e| ctx(e).in_field("object_name"))?;
            let flags = c.u32().map_err(|e| ctx(e).in_field("flags"))?;
            let size_pos = c.pos();
            let size = c
                .compact_index()
                .map_err(|e| ctx(e).in_field("serial_size"))?;
            // As in the probe, the offset is present whenever the size is non-zero.
            let offset = if size != 0 {
                c.compact_index()
                    .map_err(|e| ctx(e).in_field("serial_offset"))?
            } else {
                0
            };
            if size < 0 {
                return Err(ctx(PackageError::at(
                    ErrorKind::NegativeSerialSize { size },
                    size_pos,
                ))
                .in_field("serial_size"));
            }
            if offset < 0 || i64::from(offset) + i64::from(size) > data.len() as i64 {
                return Err(ctx(PackageError::at(
                    ErrorKind::ExportSpanOutOfRange {
                        offset: i64::from(offset),
                        size: i64::from(size),
                        file_len: data.len() as u64,
                    },
                    size_pos,
                ))
                .in_field("serial_offset"));
            }
            exports.push(Export {
                class,
                super_ref,
                outer,
                object_name,
                flags,
                serial_size: size as u32,
                serial_offset: offset as u32,
                span: Span {
                    start,
                    end: c.pos(),
                },
            });
        }
        let exports_span = Span {
            start: summary.exports.offset as usize,
            end: c.pos(),
        };

        let mut package = Package {
            summary,
            names,
            imports,
            exports,
            import_paths: Vec::new(),
            export_paths: Vec::new(),
            table_spans: TableSpans {
                names: names_span,
                imports: imports_span,
                exports: exports_span,
            },
            file_len: data.len(),
        };
        // Second pass: every outer chain must terminate (cycle/depth checked).
        let mut import_paths = Vec::with_capacity(package.imports.len());
        for i in 0..package.imports.len() as u32 {
            import_paths.push(
                package
                    .walk_path(ObjectRef::Import(i), limits.max_outer_chain)
                    .map_err(|e| e.in_entry(Table::Imports, Some(i)).in_field("outer"))?,
            );
        }
        let mut export_paths = Vec::with_capacity(package.exports.len());
        for i in 0..package.exports.len() as u32 {
            export_paths.push(
                package
                    .walk_path(ObjectRef::Export(i), limits.max_outer_chain)
                    .map_err(|e| e.in_entry(Table::Exports, Some(i)).in_field("outer"))?,
            );
        }
        package.import_paths = import_paths;
        package.export_paths = export_paths;
        Ok(package)
    }

    fn walk_path(&self, start: ObjectRef, max_chain: u32) -> Result<String> {
        let mut seen: Vec<ObjectRef> = Vec::new();
        let mut parts: Vec<&str> = Vec::new();
        let mut current = start;
        while !current.is_null() {
            if seen.contains(&current) {
                return Err(PackageError::new(ErrorKind::OuterCycle {
                    start: start.raw(),
                    repeated: current.raw(),
                }));
            }
            if seen.len() >= max_chain as usize {
                return Err(PackageError::new(ErrorKind::OuterDepthExceeded {
                    start: start.raw(),
                    max: max_chain,
                }));
            }
            seen.push(current);
            // References were range-checked while reading the tables.
            parts.push(self.object_name(current).unwrap_or_default());
            current = self.object_outer(current).unwrap_or(ObjectRef::Null);
        }
        parts.reverse();
        Ok(parts.join("."))
    }

    /// Package summary.
    pub fn summary(&self) -> &Summary {
        &self.summary
    }

    /// Name table.
    pub fn names(&self) -> &[NameEntry] {
        &self.names
    }

    /// Import table.
    pub fn imports(&self) -> &[Import] {
        &self.imports
    }

    /// Export table.
    pub fn exports(&self) -> &[Export] {
        &self.exports
    }

    /// Encoded table byte ranges.
    pub fn table_spans(&self) -> TableSpans {
        self.table_spans
    }

    /// Length of the parsed buffer.
    pub fn file_len(&self) -> usize {
        self.file_len
    }

    /// Text of a name-table entry.
    pub fn name(&self, index: NameIndex) -> &str {
        self.names
            .get(index.0 as usize)
            .map_or("", |n| n.text.as_str())
    }

    /// Converts a stored reference into a typed one, checking range.
    pub fn resolve(&self, raw: i32) -> Option<ObjectRef> {
        ObjectRef::from_raw(raw, self.imports.len() as u32, self.exports.len() as u32)
    }

    /// Object name of a reference; `None` for null or out-of-range references.
    pub fn object_name(&self, r: ObjectRef) -> Option<&str> {
        match r {
            ObjectRef::Null => None,
            ObjectRef::Import(i) => self
                .imports
                .get(i as usize)
                .map(|o| self.name(o.object_name)),
            ObjectRef::Export(i) => self
                .exports
                .get(i as usize)
                .map(|o| self.name(o.object_name)),
        }
    }

    /// Outer of a reference; `None` for null or out-of-range references.
    pub fn object_outer(&self, r: ObjectRef) -> Option<ObjectRef> {
        match r {
            ObjectRef::Null => None,
            ObjectRef::Import(i) => self.imports.get(i as usize).map(|o| o.outer),
            ObjectRef::Export(i) => self.exports.get(i as usize).map(|o| o.outer),
        }
    }

    /// Dotted outer path (outermost first), e.g. `Engine.Texture` for an imported class.
    /// Export paths do not include the containing package's own name.
    pub fn object_path(&self, r: ObjectRef) -> Option<&str> {
        match r {
            ObjectRef::Null => None,
            ObjectRef::Import(i) => self.import_paths.get(i as usize).map(String::as_str),
            ObjectRef::Export(i) => self.export_paths.get(i as usize).map(String::as_str),
        }
    }

    /// Class path of an export: the class object's path, or [`NULL_CLASS_PATH`] when the
    /// class reference is null. `None` only for an out-of-range export index.
    pub fn export_class_path(&self, export: usize) -> Option<&str> {
        let e = self.exports.get(export)?;
        Some(self.object_path(e.class).unwrap_or(NULL_CLASS_PATH))
    }

    /// Sorted, de-duplicated names of imported root packages (imports with a null outer and
    /// class name `Package`).
    pub fn imported_packages(&self) -> Vec<&str> {
        let set: BTreeSet<&str> = self
            .imports
            .iter()
            .filter(|i| i.outer.is_null() && self.name(i.class_name) == "Package")
            .map(|i| self.name(i.object_name))
            .collect();
        set.into_iter().collect()
    }

    /// Number of exports per class path.
    pub fn export_class_counts(&self) -> BTreeMap<String, u32> {
        self.class_counts(|_| true)
    }

    /// Number of zero-sized exports per class path.
    pub fn zero_size_export_counts(&self) -> BTreeMap<String, u32> {
        self.class_counts(|e| e.serial_size == 0)
    }

    fn class_counts(&self, filter: impl Fn(&Export) -> bool) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        for (i, e) in self.exports.iter().enumerate() {
            if filter(e) {
                let class = self.export_class_path(i).unwrap_or(NULL_CLASS_PATH);
                *out.entry(class.to_owned()).or_insert(0) += 1;
            }
        }
        out
    }

    /// File ranges not covered by the summary, the three tables or any export payload.
    /// Overlaps are merged. Useful to find unaccounted bytes.
    pub fn unaccounted_ranges(&self) -> Vec<Span> {
        let mut covered = vec![
            Span {
                start: 0,
                end: self.summary.header_end,
            },
            self.table_spans.names,
            self.table_spans.imports,
            self.table_spans.exports,
        ];
        covered.extend(self.exports.iter().filter_map(Export::serial_span));
        covered.retain(|s| !s.is_empty());
        covered.sort();
        let mut gaps = Vec::new();
        let mut cursor = 0usize;
        for s in covered {
            if s.start > cursor {
                gaps.push(Span {
                    start: cursor,
                    end: s.start,
                });
            }
            cursor = cursor.max(s.end);
        }
        if cursor < self.file_len {
            gaps.push(Span {
                start: cursor,
                end: self.file_len,
            });
        }
        gaps
    }

    /// Pairs of covered ranges (summary, tables, export payloads) that overlap. Each entry is
    /// the overlapping byte range. Expected to be empty for well-formed packages.
    pub fn overlapping_ranges(&self) -> Vec<Span> {
        let mut covered = vec![
            Span {
                start: 0,
                end: self.summary.header_end,
            },
            self.table_spans.names,
            self.table_spans.imports,
            self.table_spans.exports,
        ];
        covered.extend(self.exports.iter().filter_map(Export::serial_span));
        covered.retain(|s| !s.is_empty());
        covered.sort();
        let mut overlaps = Vec::new();
        let mut reach = 0usize;
        for s in covered {
            if s.start < reach {
                overlaps.push(Span {
                    start: s.start,
                    end: reach.min(s.end),
                });
            }
            reach = reach.max(s.end);
        }
        overlaps
    }
}
