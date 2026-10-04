//! Export payloads: bounded slicing, the `UObject` state-frame prefix and UE1/UE2 tagged
//! properties for the measured version-100 dialect.
//!
//! Layout (verified against the GOG corpus, see the crate README):
//!
//! ```text
//! [state frame]        only when the export flags contain RF_HasStack (0x02000000):
//!   compact  node          object reference
//!   compact  state node    object reference
//!   u64      probe mask
//!   u32      latent action
//!   compact  code offset   only when node != 0
//! [tagged properties]  repeated until a tag whose name is "None":
//!   compact  name          name-table index
//!   u8       info          bits 0-3 type, bits 4-6 size code, bit 7 array flag / bool value
//!   compact  struct name   only for StructProperty (type 10)
//!   size     code 0..4 -> 1, 2, 4, 12, 16; code 5 u8, 6 u16, 7 i32
//!   index    only when bit 7 is set and the type is not BoolProperty (1, 2 or 4 bytes)
//!   value    `size` bytes (none for BoolProperty)
//! [native tail]        class-specific data, not decoded here
//! ```
//!
//! Struct values in version 100 are serialized as raw member data (UELib marks tagged struct
//! serialization as starting at version 118), so only struct types whose name *and* size match
//! a known layout are decoded; others keep their bounded byte span. References: UModel
//! `Unreal/UnObject.cpp` (`FPropertyTag`, UE1/UE2 branch) and UELib
//! `src/Core/Classes/UDefaultProperty.cs` / `src/Core/UStateFrame.cs` at the revisions recorded
//! in the README.

use crate::cursor::Cursor;
use crate::error::{ErrorKind, PackageError, Result, Table};
use crate::package::{Limits, NameIndex, ObjectRef, Package, Span};

/// `RF_HasStack`: the payload starts with a script state frame.
pub const RF_HAS_STACK: u32 = 0x0200_0000;

/// Script state frame stored before the properties of objects flagged [`RF_HAS_STACK`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateFrame {
    /// Function or state whose code is executing.
    pub node: ObjectRef,
    /// Current state.
    pub state_node: ObjectRef,
    /// Probe (event) mask.
    pub probe_mask: u64,
    /// Latent action. Observed to contain arbitrary values in saved packages.
    pub latent_action: u32,
    /// Code offset, present only when `node` is not null (`-1` observed for "no code").
    pub offset: Option<i32>,
    /// Absolute byte range of the frame.
    pub span: Span,
}

/// Property type from the low four bits of the tag info byte (UE1/UE2 numbering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PropertyType {
    /// 1: one byte (or enum value).
    Byte,
    /// 2: `i32`.
    Int,
    /// 3: value stored in the info byte's bit 7; no value bytes.
    Bool,
    /// 4: `f32`.
    Float,
    /// 5: compact object reference.
    Object,
    /// 6: compact name index.
    Name,
    /// 7: `StringProperty` in UE1; UELib reads it as a delegate (object + name) from
    /// version 100. Decoded as a delegate only when that exactly fills the value.
    Delegate,
    /// 8: compact object reference to a class.
    Class,
    /// 9: dynamic array: compact count then elements whose layout needs the class schema.
    Array,
    /// 10: struct; the struct name follows the info byte.
    Struct,
    /// 11: vector (12 bytes).
    Vector,
    /// 12: rotator (12 bytes).
    Rotator,
    /// 13: `FString`.
    Str,
    /// 14: map (not decoded).
    Map,
    /// 15: fixed array (not decoded).
    FixedArray,
}

impl PropertyType {
    /// Maps a type nibble (1..=15). Returns `None` for 0 and values above 15.
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Byte,
            2 => Self::Int,
            3 => Self::Bool,
            4 => Self::Float,
            5 => Self::Object,
            6 => Self::Name,
            7 => Self::Delegate,
            8 => Self::Class,
            9 => Self::Array,
            10 => Self::Struct,
            11 => Self::Vector,
            12 => Self::Rotator,
            13 => Self::Str,
            14 => Self::Map,
            15 => Self::FixedArray,
            _ => return None,
        })
    }

    /// Unreal type name, e.g. `IntProperty`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Byte => "ByteProperty",
            Self::Int => "IntProperty",
            Self::Bool => "BoolProperty",
            Self::Float => "FloatProperty",
            Self::Object => "ObjectProperty",
            Self::Name => "NameProperty",
            Self::Delegate => "DelegateProperty",
            Self::Class => "ClassProperty",
            Self::Array => "ArrayProperty",
            Self::Struct => "StructProperty",
            Self::Vector => "VectorProperty",
            Self::Rotator => "RotatorProperty",
            Self::Str => "StrProperty",
            Self::Map => "MapProperty",
            Self::FixedArray => "FixedArrayProperty",
        }
    }
}

/// Struct values decoded because both the struct name and the value size match a known
/// raw-member layout.
#[derive(Debug, Clone, PartialEq)]
pub enum StructValue {
    /// `Vector` (12 bytes): X, Y, Z.
    Vector([f32; 3]),
    /// `Rotator` (12 bytes): Pitch, Yaw, Roll in Unreal angle units.
    Rotator([i32; 3]),
    /// `Color` (4 bytes) in stored order. Channel order is not verified for XIII.
    Color([u8; 4]),
    /// `Scale` (17 bytes): scale vector, sheer rate, sheer axis.
    Scale {
        /// Scale vector.
        scale: [f32; 3],
        /// Sheer rate.
        sheer_rate: f32,
        /// Sheer axis enum value.
        sheer_axis: u8,
    },
    /// `Plane` (16 bytes): X, Y, Z, W.
    Plane([f32; 4]),
    /// `Sphere` (16 bytes): centre X, Y, Z and radius W.
    Sphere([f32; 4]),
    /// `Box` (25 bytes): min, max, valid flag.
    Box {
        /// Minimum corner.
        min: [f32; 3],
        /// Maximum corner.
        max: [f32; 3],
        /// `IsValid` byte.
        valid: u8,
    },
    /// `Range` (8 bytes): min, max.
    Range([f32; 2]),
    /// `RangeVector` (24 bytes): X, Y, Z ranges.
    RangeVector([[f32; 2]; 3]),
    /// `Guid` (16 bytes): four `u32`.
    Guid([u32; 4]),
    /// `PointRegion` (variable): zone reference, leaf, zone number.
    PointRegion {
        /// Zone actor.
        zone: ObjectRef,
        /// BSP leaf.
        leaf: i32,
        /// Zone number.
        zone_number: u8,
    },
}

/// Why a value was kept as a raw byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawReason {
    /// Map / fixed-array property: not decoded.
    UnsupportedType,
    /// Struct whose name has no known layout (needs the struct schema).
    UnknownStruct,
    /// A fixed-size type whose declared size differs from the expected size.
    SizeMismatch {
        /// Size the type requires.
        expected: u32,
    },
    /// A variable-length value decoded but did not fill the declared size.
    TrailingBytes {
        /// Bytes the decoded value used.
        consumed: u32,
    },
    /// The value bytes could not be decoded (bad reference, name, string, ...).
    Invalid(ErrorKind),
}

impl RawReason {
    /// True for values that indicate a mismatch with the expected layout (as opposed to a
    /// deliberately unsupported type or an unknown struct).
    pub fn is_anomaly(&self) -> bool {
        !matches!(self, RawReason::UnsupportedType | RawReason::UnknownStruct)
    }
}

/// Decoded property value. The value bytes are always [`Property::value_span`].
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    /// Byte or enum value.
    Byte(u8),
    /// Integer.
    Int(i32),
    /// Boolean (from the info byte).
    Bool(bool),
    /// Float.
    Float(f32),
    /// Object reference.
    Object(ObjectRef),
    /// Class reference.
    Class(ObjectRef),
    /// Name.
    Name(NameIndex),
    /// String.
    Str(String),
    /// Delegate: object and function name.
    Delegate {
        /// Object owning the function (may be null).
        object: ObjectRef,
        /// Function name.
        function: NameIndex,
    },
    /// Known struct (also used for Vector/Rotator property types).
    Struct(StructValue),
    /// Dynamic array: element count and the absolute span of the element bytes.
    Array {
        /// Element count.
        count: u32,
        /// Absolute range of the element bytes (after the count).
        elements: Span,
    },
    /// Not decoded; see [`Property::value_span`].
    Raw(RawReason),
}

/// One tagged property.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    /// Property name.
    pub name: NameIndex,
    /// Property type.
    pub kind: PropertyType,
    /// Raw info byte.
    pub info: u8,
    /// Struct name for [`PropertyType::Struct`].
    pub struct_name: Option<NameIndex>,
    /// Size as declared by the tag (for bools the nominal size of the size code).
    pub size: u32,
    /// Static-array element index (0 when absent).
    pub array_index: u32,
    /// Absolute range of the tag header.
    pub tag_span: Span,
    /// Absolute range of the value bytes (empty for bools).
    pub value_span: Span,
    /// Decoded value.
    pub value: PropertyValue,
}

impl Property {
    /// The raw reason when the value indicates a layout anomaly.
    pub fn anomaly(&self) -> Option<&RawReason> {
        match &self.value {
            PropertyValue::Raw(r) if r.is_anomaly() => Some(r),
            _ => None,
        }
    }
}

/// A tagged-property block terminated by `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyBlock {
    /// Properties in file order (without the terminator).
    pub properties: Vec<Property>,
    /// Absolute range from the first tag to just past the `None` terminator.
    pub span: Span,
    /// Name index of the terminator. Packages can hold several identical `None` entries
    /// (e.g. `core.u` names 0, 335 and 336), so the terminator is matched by text.
    pub terminator: NameIndex,
}

/// State frame, properties and tail of one export payload.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectProperties {
    /// Zero-based export index.
    pub export: u32,
    /// Absolute payload range.
    pub payload: Span,
    /// State frame when the export has [`RF_HAS_STACK`].
    pub state_frame: Option<StateFrame>,
    /// Tagged properties.
    pub block: PropertyBlock,
}

impl ObjectProperties {
    /// Payload bytes consumed by the state frame and the property block.
    pub fn consumed(&self) -> usize {
        self.block.span.end - self.payload.start
    }

    /// Absolute range of the bytes after the property block (class-native data).
    pub fn tail(&self) -> Span {
        Span {
            start: self.block.span.end,
            end: self.payload.end,
        }
    }
}

fn rebase(mut e: PackageError, base: usize) -> PackageError {
    if let Some(o) = e.offset.as_mut() {
        *o += base as u64;
    }
    e
}

fn abs(base: usize, start: usize, end: usize) -> Span {
    Span {
        start: base + start,
        end: base + end,
    }
}

impl Package {
    fn check_buffer(&self, data: &[u8]) -> Result<()> {
        if data.len() != self.file_len() {
            return Err(PackageError::new(ErrorKind::BufferLengthMismatch {
                expected: self.file_len() as u64,
                found: data.len() as u64,
            }));
        }
        Ok(())
    }

    /// Payload bytes of an export, bounded by its serial offset and size. `data` must be the
    /// buffer this package was parsed from (its length is checked).
    pub fn export_payload<'a>(&self, data: &'a [u8], export: usize) -> Result<&'a [u8]> {
        self.check_buffer(data)?;
        let e = self.exports().get(export).ok_or_else(|| {
            PackageError::new(ErrorKind::ExportIndexOutOfRange {
                index: export as u64,
                count: self.exports().len() as u32,
            })
        })?;
        let span = e.serial_span().ok_or_else(|| {
            PackageError::new(ErrorKind::EmptyPayload).in_entry(Table::Payload, Some(export as u32))
        })?;
        // Bounds were validated at parse time and the buffer length matches.
        Ok(&data[span.start..span.end])
    }

    /// Reads the state frame (if flagged) and the tagged-property block at the start of an
    /// export payload. Errors carry `Table::Payload`, the export index and absolute offsets.
    pub fn read_object_properties(
        &self,
        data: &[u8],
        export: usize,
        limits: &Limits,
    ) -> Result<ObjectProperties> {
        let bytes = self.export_payload(data, export)?;
        let e = &self.exports()[export];
        let base = e.serial_offset as usize;
        let ctx = |err: PackageError| rebase(err, base).in_entry(Table::Payload, Some(export as u32));
        let mut c = Cursor::new(bytes);
        let state_frame = if e.flags & RF_HAS_STACK != 0 {
            Some(self.read_state_frame(&mut c, base).map_err(ctx)?)
        } else {
            None
        };
        let block = self
            .read_block(bytes, c.pos(), base, limits)
            .map_err(ctx)?;
        Ok(ObjectProperties {
            export: export as u32,
            payload: abs(base, 0, bytes.len()),
            state_frame,
            block,
        })
    }

    /// Reads one tagged-property block starting at absolute offset `start` and bounded by
    /// absolute `end` (e.g. class defaults that follow native class data).
    pub fn read_property_block(
        &self,
        data: &[u8],
        start: usize,
        end: usize,
        limits: &Limits,
    ) -> Result<PropertyBlock> {
        self.check_buffer(data)?;
        if start > end || end > data.len() {
            return Err(PackageError::new(ErrorKind::SeekOutOfBounds {
                position: start.max(end) as u64,
                len: data.len() as u64,
            }));
        }
        self.read_block(&data[start..end], 0, start, limits)
            .map_err(|e| rebase(e, start))
    }

    /// Text of a property's name.
    pub fn property_name(&self, p: &Property) -> &str {
        self.name(p.name)
    }

    fn read_state_frame(&self, c: &mut Cursor<'_>, base: usize) -> Result<StateFrame> {
        let start = c.pos();
        let node = self
            .read_ref(c)
            .map_err(|e| e.in_field("state_frame.node"))?;
        let state_node = self
            .read_ref(c)
            .map_err(|e| e.in_field("state_frame.state_node"))?;
        let probe_mask = {
            let lo = c.u32().map_err(|e| e.in_field("state_frame.probe_mask"))?;
            let hi = c.u32().map_err(|e| e.in_field("state_frame.probe_mask"))?;
            u64::from(lo) | (u64::from(hi) << 32)
        };
        let latent_action = c
            .u32()
            .map_err(|e| e.in_field("state_frame.latent_action"))?;
        let offset = if node.is_null() {
            None
        } else {
            Some(
                c.compact_index()
                    .map_err(|e| e.in_field("state_frame.offset"))?,
            )
        };
        Ok(StateFrame {
            node,
            state_node,
            probe_mask,
            latent_action,
            offset,
            span: abs(base, start, c.pos()),
        })
    }

    fn read_ref(&self, c: &mut Cursor<'_>) -> Result<ObjectRef> {
        let start = c.pos();
        let raw = c.compact_index()?;
        self.resolve(raw).ok_or_else(|| {
            PackageError::at(
                ErrorKind::ObjectRefOutOfRange {
                    raw,
                    imports: self.imports().len() as u32,
                    exports: self.exports().len() as u32,
                },
                start,
            )
        })
    }

    fn read_name(&self, c: &mut Cursor<'_>) -> Result<NameIndex> {
        let start = c.pos();
        let index = c.compact_index()?;
        match u32::try_from(index) {
            Ok(i) if (i as usize) < self.names().len() => Ok(NameIndex(i)),
            _ => Err(PackageError::at(
                ErrorKind::NameIndexOutOfRange {
                    index,
                    count: self.names().len() as u32,
                },
                start,
            )),
        }
    }

    /// Reads tags from `bytes[start..]`; spans are rebased by `base`. Error offsets are left
    /// relative to `bytes` (callers rebase).
    fn read_block(
        &self,
        bytes: &[u8],
        start: usize,
        base: usize,
        limits: &Limits,
    ) -> Result<PropertyBlock> {
        let mut c = Cursor::new(bytes);
        c.seek(start)?;
        let mut properties = Vec::new();
        let mut tags = 0u32;
        loop {
            let tag_start = c.pos();
            if tags >= limits.max_properties {
                return Err(PackageError::at(
                    ErrorKind::TooManyProperties {
                        max: limits.max_properties,
                    },
                    tag_start,
                ));
            }
            tags += 1;
            if c.remaining() == 0 {
                return Err(PackageError::at(
                    ErrorKind::MissingPropertyTerminator,
                    tag_start,
                ));
            }
            let name = self
                .read_name(&mut c)
                .map_err(|e| e.in_field("property.name"))?;
            if self.name(name).eq_ignore_ascii_case("None") {
                return Ok(PropertyBlock {
                    properties,
                    span: abs(base, start, c.pos()),
                    terminator: name,
                });
            }
            let info = c.u8().map_err(|e| e.in_field("property.info"))?;
            let code = info & 0x0f;
            let kind = PropertyType::from_code(code).ok_or_else(|| {
                PackageError::at(ErrorKind::InvalidPropertyType { code }, tag_start)
                    .in_field("property.info")
            })?;
            let struct_name = if kind == PropertyType::Struct {
                Some(
                    self.read_name(&mut c)
                        .map_err(|e| e.in_field("property.struct_name"))?,
                )
            } else {
                None
            };
            let size_pos = c.pos();
            let size: i64 = match (info >> 4) & 7 {
                0 => 1,
                1 => 2,
                2 => 4,
                3 => 12,
                4 => 16,
                5 => i64::from(c.u8().map_err(|e| e.in_field("property.size"))?),
                6 => i64::from(c.u16().map_err(|e| e.in_field("property.size"))?),
                _ => i64::from(c.i32().map_err(|e| e.in_field("property.size"))?),
            };
            let array_flag = info & 0x80 != 0;
            let array_index = if array_flag && kind != PropertyType::Bool {
                read_array_index(&mut c).map_err(|e| e.in_field("property.array_index"))?
            } else {
                0
            };
            let tag_end = c.pos();
            let value_len = if kind == PropertyType::Bool { 0 } else { size };
            if value_len < 0 || value_len as u64 > c.remaining() as u64 {
                return Err(PackageError::at(
                    ErrorKind::PropertySizeOutOfRange {
                        size,
                        available: c.remaining() as u64,
                    },
                    size_pos,
                )
                .in_field("property.size"));
            }
            let value_bytes = c.take(value_len as usize)?;
            let value_start = base + tag_end;
            let value = if kind == PropertyType::Bool {
                PropertyValue::Bool(array_flag)
            } else {
                self.decode_value(kind, struct_name, value_bytes, value_start)
            };
            properties.push(Property {
                name,
                kind,
                info,
                struct_name,
                size: size as u32,
                array_index,
                tag_span: abs(base, tag_start, tag_end),
                value_span: abs(base, tag_end, c.pos()),
                value,
            });
        }
    }

    fn decode_value(
        &self,
        kind: PropertyType,
        struct_name: Option<NameIndex>,
        bytes: &[u8],
        value_start: usize,
    ) -> PropertyValue {
        let fixed = |expected: u32, f: &dyn Fn(&mut Cursor<'_>) -> Result<PropertyValue>| {
            if bytes.len() != expected as usize {
                return PropertyValue::Raw(RawReason::SizeMismatch { expected });
            }
            let mut c = Cursor::new(bytes);
            f(&mut c).unwrap_or_else(|e| PropertyValue::Raw(RawReason::Invalid(e.kind)))
        };
        match kind {
            PropertyType::Byte => fixed(1, &|c| Ok(PropertyValue::Byte(c.u8()?))),
            PropertyType::Int => fixed(4, &|c| Ok(PropertyValue::Int(c.i32()?))),
            PropertyType::Float => fixed(4, &|c| Ok(PropertyValue::Float(f32_le(c)?))),
            PropertyType::Vector => fixed(12, &|c| {
                Ok(PropertyValue::Struct(StructValue::Vector(vec3(c)?)))
            }),
            PropertyType::Rotator => fixed(12, &|c| {
                Ok(PropertyValue::Struct(StructValue::Rotator([
                    c.i32()?,
                    c.i32()?,
                    c.i32()?,
                ])))
            }),
            PropertyType::Object => self.variable(bytes, |c| Ok(PropertyValue::Object(self.read_ref(c)?))),
            PropertyType::Class => self.variable(bytes, |c| Ok(PropertyValue::Class(self.read_ref(c)?))),
            PropertyType::Name => self.variable(bytes, |c| Ok(PropertyValue::Name(self.read_name(c)?))),
            PropertyType::Delegate => self.variable(bytes, |c| {
                Ok(PropertyValue::Delegate {
                    object: self.read_ref(c)?,
                    function: self.read_name(c)?,
                })
            }),
            PropertyType::Str => self.variable(bytes, |c| {
                // A string cannot be longer than its bounded value bytes.
                Ok(PropertyValue::Str(c.fstring(bytes.len() as u32)?))
            }),
            PropertyType::Array => {
                let mut c = Cursor::new(bytes);
                match c.compact_index() {
                    Ok(n) if n >= 0 => PropertyValue::Array {
                        count: n as u32,
                        elements: Span {
                            start: value_start + c.pos(),
                            end: value_start + bytes.len(),
                        },
                    },
                    Ok(n) => PropertyValue::Raw(RawReason::Invalid(ErrorKind::CountOutOfRange {
                        what: "array count",
                        count: i64::from(n),
                        max: i32::MAX as u64,
                    })),
                    Err(e) => PropertyValue::Raw(RawReason::Invalid(e.kind)),
                }
            }
            PropertyType::Struct => {
                let name = struct_name.map_or("", |n| self.name(n));
                self.decode_struct(name, bytes)
            }
            PropertyType::Map | PropertyType::FixedArray => {
                PropertyValue::Raw(RawReason::UnsupportedType)
            }
            // Bools never reach here (handled from the info byte).
            PropertyType::Bool => PropertyValue::Raw(RawReason::UnsupportedType),
        }
    }

    /// Decodes a variable-length value that must exactly fill `bytes`.
    fn variable(
        &self,
        bytes: &[u8],
        f: impl FnOnce(&mut Cursor<'_>) -> Result<PropertyValue>,
    ) -> PropertyValue {
        let mut c = Cursor::new(bytes);
        match f(&mut c) {
            Ok(v) if c.remaining() == 0 => v,
            Ok(_) => PropertyValue::Raw(RawReason::TrailingBytes {
                consumed: c.pos() as u32,
            }),
            Err(e) => PropertyValue::Raw(RawReason::Invalid(e.kind)),
        }
    }

    fn decode_struct(&self, name: &str, bytes: &[u8]) -> PropertyValue {
        let lower = name.to_ascii_lowercase();
        let expected: u32 = match lower.as_str() {
            "vector" | "rotator" => 12,
            "color" => 4,
            "scale" => 17,
            "plane" | "sphere" | "guid" => 16,
            "box" => 25,
            "range" => 8,
            "rangevector" => 24,
            "pointregion" => {
                return self.variable(bytes, |c| {
                    Ok(PropertyValue::Struct(StructValue::PointRegion {
                        zone: self.read_ref(c)?,
                        leaf: c.i32()?,
                        zone_number: c.u8()?,
                    }))
                });
            }
            _ => return PropertyValue::Raw(RawReason::UnknownStruct),
        };
        if bytes.len() != expected as usize {
            return PropertyValue::Raw(RawReason::SizeMismatch { expected });
        }
        let mut c = Cursor::new(bytes);
        let decoded: Result<StructValue> = (|| {
            Ok(match lower.as_str() {
                "vector" => StructValue::Vector(vec3(&mut c)?),
                "rotator" => StructValue::Rotator([c.i32()?, c.i32()?, c.i32()?]),
                "color" => StructValue::Color([c.u8()?, c.u8()?, c.u8()?, c.u8()?]),
                "scale" => StructValue::Scale {
                    scale: vec3(&mut c)?,
                    sheer_rate: f32_le(&mut c)?,
                    sheer_axis: c.u8()?,
                },
                "plane" => StructValue::Plane(vec4(&mut c)?),
                "sphere" => StructValue::Sphere(vec4(&mut c)?),
                "guid" => StructValue::Guid([c.u32()?, c.u32()?, c.u32()?, c.u32()?]),
                "box" => StructValue::Box {
                    min: vec3(&mut c)?,
                    max: vec3(&mut c)?,
                    valid: c.u8()?,
                },
                "range" => StructValue::Range([f32_le(&mut c)?, f32_le(&mut c)?]),
                _ => StructValue::RangeVector([
                    [f32_le(&mut c)?, f32_le(&mut c)?],
                    [f32_le(&mut c)?, f32_le(&mut c)?],
                    [f32_le(&mut c)?, f32_le(&mut c)?],
                ]),
            })
        })();
        match decoded {
            Ok(v) => PropertyValue::Struct(v),
            Err(e) => PropertyValue::Raw(RawReason::Invalid(e.kind)),
        }
    }
}

fn f32_le(c: &mut Cursor<'_>) -> Result<f32> {
    Ok(f32::from_bits(c.u32()?))
}

fn vec3(c: &mut Cursor<'_>) -> Result<[f32; 3]> {
    Ok([f32_le(c)?, f32_le(c)?, f32_le(c)?])
}

fn vec4(c: &mut Cursor<'_>) -> Result<[f32; 4]> {
    Ok([f32_le(c)?, f32_le(c)?, f32_le(c)?, f32_le(c)?])
}

/// UE1/UE2 static-array index: `0xxxxxxx` (7 bits), `10xxxxxx b` (14 bits), or
/// `11xxxxxx b b b` (30 bits), most significant byte first. (UModel masks the four-byte form
/// to 22 bits; UELib keeps 30 bits. The four-byte form was not observed in the corpus.)
fn read_array_index(c: &mut Cursor<'_>) -> Result<u32> {
    let b0 = u32::from(c.u8()?);
    if b0 & 0x80 == 0 {
        return Ok(b0);
    }
    let b1 = u32::from(c.u8()?);
    if b0 & 0xc0 == 0x80 {
        return Ok(((b0 & 0x7f) << 8) | b1);
    }
    let b2 = u32::from(c.u8()?);
    let b3 = u32::from(c.u8()?);
    Ok(((b0 & 0x3f) << 24) | (b1 << 16) | (b2 << 8) | b3)
}
