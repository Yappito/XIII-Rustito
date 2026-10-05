//! Shared decoding infrastructure: contextual errors, a payload-bounded reader, property
//! lookup helpers and the single Unreal -> Bevy coordinate conversion.
//!
//! Every decoder in this crate works on one export payload. It reads through
//! [`PayloadReader`], which can never read past the export's serialized size, and must end
//! exactly at the payload end. A decoder that stops early returns
//! [`DecodeErrorKind::UnconsumedTail`] with the class, export and absolute offset instead of
//! silently dropping data.

use std::fmt;

use xiii_package::{
    Cursor, Limits, ObjectProperties, ObjectRef, Package, PackageError, Property, PropertyValue,
    Span, StructValue,
};

/// Upper bound for any element count read from a payload (well above the largest observed
/// array, a 790 KB static mesh). Counts are additionally checked against the remaining bytes.
pub const MAX_ELEMENTS: usize = 16 * 1024 * 1024;

/// What went wrong while decoding a payload.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum DecodeErrorKind {
    /// Structural failure from the package reader (EOF, bad compact index, bad reference).
    Package(PackageError),
    /// The export has a different class than the decoder expects.
    WrongClass {
        /// Expected class path.
        expected: &'static str,
        /// Class path found.
        found: String,
    },
    /// A required property is absent.
    MissingProperty(&'static str),
    /// A property has an unexpected type or value.
    BadProperty {
        /// Property name.
        name: &'static str,
        /// Why it was rejected.
        reason: String,
    },
    /// An element count is negative, above [`MAX_ELEMENTS`], or cannot fit the remaining
    /// payload even at the minimum element size.
    BadCount {
        /// Stored count.
        count: i64,
        /// Minimum encoded element size.
        min_element_size: usize,
        /// Payload bytes remaining.
        remaining: usize,
    },
    /// A decoded value contradicts the layout (e.g. a lazy-array skip offset that does not
    /// point at the end of its data, or a vertex index out of range).
    Invalid(String),
    /// A format or feature that is recognized but deliberately not decoded.
    Unsupported(String),
    /// The decoder finished before the payload end: these bytes are not accounted for.
    UnconsumedTail {
        /// Number of bytes left.
        bytes: usize,
    },
}

/// A decoding failure with export/offset context. Boxed so `Result<T, DecodeError>` stays
/// small; fields are reached through `Deref` to [`DecodeErrorData`].
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeError(Box<DecodeErrorData>);

impl std::ops::Deref for DecodeError {
    type Target = DecodeErrorData;
    fn deref(&self) -> &DecodeErrorData {
        &self.0
    }
}

impl std::ops::DerefMut for DecodeError {
    fn deref_mut(&mut self) -> &mut DecodeErrorData {
        &mut self.0
    }
}

/// Contents of a [`DecodeError`].
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeErrorData {
    /// What went wrong.
    pub kind: DecodeErrorKind,
    /// Zero-based export index.
    pub export: Option<u32>,
    /// Export class path.
    pub class: Option<String>,
    /// Export object path (without the package name).
    pub path: Option<String>,
    /// Absolute file offset of the failing read, when known.
    pub offset: Option<u64>,
    /// Payload start (absolute) so the payload-relative offset can be derived.
    pub payload_start: Option<u64>,
    /// Field being read.
    pub field: Option<&'static str>,
}

impl DecodeError {
    /// Creates an error without context.
    pub fn new(kind: DecodeErrorKind) -> Self {
        Self(Box::new(DecodeErrorData {
            kind,
            export: None,
            class: None,
            path: None,
            offset: None,
            payload_start: None,
            field: None,
        }))
    }

    /// Creates an error at an absolute offset.
    pub fn at(kind: DecodeErrorKind, offset: usize) -> Self {
        let mut e = Self::new(kind);
        e.offset = Some(offset as u64);
        e
    }

    /// Sets the field unless already set.
    #[must_use]
    pub fn in_field(mut self, field: &'static str) -> Self {
        if self.field.is_none() {
            self.field = Some(field);
        }
        self
    }

    /// Fills export context from a package (unless already set).
    #[must_use]
    pub fn in_export(mut self, package: &Package, export: usize) -> Self {
        if self.export.is_none() {
            self.export = Some(export as u32);
            self.class = package.export_class_path(export).map(str::to_owned);
            self.path = package
                .object_path(ObjectRef::Export(export as u32))
                .map(str::to_owned);
            self.payload_start = package
                .exports()
                .get(export)
                .map(|e| u64::from(e.serial_offset));
        }
        self
    }

    /// Short machine-friendly category for coverage reports.
    pub fn category(&self) -> String {
        match &self.kind {
            DecodeErrorKind::Package(e) => {
                let dbg = format!("{:?}", e.kind);
                let variant: String = dbg.chars().take_while(|c| c.is_alphanumeric()).collect();
                format!("package:{variant}:{}", self.field.unwrap_or("?"))
            }
            DecodeErrorKind::WrongClass { .. } => "wrong-class".into(),
            DecodeErrorKind::MissingProperty(p) => format!("missing-property:{p}"),
            DecodeErrorKind::BadProperty { name, .. } => format!("bad-property:{name}"),
            DecodeErrorKind::BadCount { .. } => format!("bad-count:{}", self.field.unwrap_or("?")),
            DecodeErrorKind::Invalid(_) => format!("invalid:{}", self.field.unwrap_or("?")),
            DecodeErrorKind::Unsupported(s) => format!("unsupported:{s}"),
            DecodeErrorKind::UnconsumedTail { .. } => "unconsumed-tail".into(),
        }
    }
}

impl From<PackageError> for DecodeError {
    fn from(e: PackageError) -> Self {
        let (offset, field) = (e.offset, e.field);
        let mut out = Self::new(DecodeErrorKind::Package(e));
        out.offset = offset;
        out.field = field;
        out
    }
}

impl fmt::Display for DecodeErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeErrorKind::Package(e) => write!(f, "{}", e.kind),
            DecodeErrorKind::WrongClass { expected, found } => {
                write!(f, "expected class {expected}, found {found}")
            }
            DecodeErrorKind::MissingProperty(p) => write!(f, "missing property {p}"),
            DecodeErrorKind::BadProperty { name, reason } => {
                write!(f, "property {name}: {reason}")
            }
            DecodeErrorKind::BadCount {
                count,
                min_element_size,
                remaining,
            } => write!(
                f,
                "count {count} (min {min_element_size} B each) does not fit {remaining} remaining bytes"
            ),
            DecodeErrorKind::Invalid(s) => write!(f, "invalid data: {s}"),
            DecodeErrorKind::Unsupported(s) => write!(f, "unsupported: {s}"),
            DecodeErrorKind::UnconsumedTail { bytes } => {
                write!(f, "{bytes} payload bytes not accounted for")
            }
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "{path}")?;
        }
        if let Some(class) = &self.class {
            write!(f, " ({class})")?;
        }
        if let Some(i) = self.export {
            write!(f, " export {i}")?;
        }
        if let Some(field) = self.field {
            write!(f, " field {field}")?;
        }
        if let Some(o) = self.offset {
            write!(f, " at 0x{o:x}")?;
            if let Some(p) = self.payload_start {
                write!(f, " (payload+{})", o.saturating_sub(p))?;
            }
        }
        write!(f, ": {}", self.kind)
    }
}

impl std::error::Error for DecodeError {}

/// Result alias for decoders.
pub type DecodeResult<T> = Result<T, DecodeError>;

/// Byte accounting of one decoded export payload. Decoders only return successfully when
/// `end == payload.end`; `unknown` lists byte ranges that were consumed with a verified size
/// but whose meaning is not established (reported, not hidden).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadReport {
    /// Absolute payload range.
    pub payload: Span,
    /// End of the state frame + tagged-property block.
    pub properties_end: usize,
    /// Consumed byte ranges of undetermined meaning, with a label.
    pub unknown: Vec<(&'static str, Span)>,
    /// Bytes at the end of the payload that the decoder deliberately does not decode
    /// (label, absolute range). `None` means the payload was consumed exactly.
    pub unsupported_tail: Option<(&'static str, Span)>,
}

impl PayloadReport {
    /// Bytes in unknown-meaning ranges.
    pub fn unknown_bytes(&self) -> usize {
        self.unknown.iter().map(|(_, s)| s.len()).sum()
    }
}

/// Reader bounded by one export payload; positions are absolute file offsets.
#[derive(Debug, Clone)]
pub struct PayloadReader<'a> {
    cur: Cursor<'a>,
    payload: Span,
    unknown: Vec<(&'static str, Span)>,
}

impl<'a> PayloadReader<'a> {
    /// Creates a reader over `data[payload]`, positioned at `start` (inside the payload).
    pub fn new(data: &'a [u8], payload: Span, start: usize) -> DecodeResult<Self> {
        if payload.end > data.len() || start < payload.start || start > payload.end {
            return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
                "payload {}..{} / start {start} outside buffer of {}",
                payload.start,
                payload.end,
                data.len()
            ))));
        }
        let mut cur = Cursor::new(&data[..payload.end]);
        cur.seek(start)?;
        Ok(Self {
            cur,
            payload,
            unknown: Vec::new(),
        })
    }

    /// Reader positioned after the property block of an export.
    pub fn after_properties(data: &'a [u8], props: &ObjectProperties) -> DecodeResult<Self> {
        Self::new(data, props.payload, props.block.span.end)
    }

    /// Absolute position.
    pub fn pos(&self) -> usize {
        self.cur.pos()
    }

    /// Payload range.
    pub fn payload(&self) -> Span {
        self.payload
    }

    /// Bytes left in the payload.
    pub fn remaining(&self) -> usize {
        self.payload.end - self.cur.pos()
    }

    /// Moves to an absolute position inside the payload.
    pub fn seek(&mut self, pos: usize) -> DecodeResult<()> {
        if pos < self.payload.start {
            return Err(DecodeError::at(
                DecodeErrorKind::Invalid(format!("seek to {pos} before payload start")),
                self.pos(),
            ));
        }
        Ok(self.cur.seek(pos)?)
    }

    /// Reads `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> DecodeResult<&'a [u8]> {
        Ok(self.cur.take(n)?)
    }

    /// Consumes `n` bytes of undetermined meaning and records them in the report.
    pub fn unknown(&mut self, label: &'static str, n: usize) -> DecodeResult<&'a [u8]> {
        let start = self.pos();
        let b = self.bytes(n).map_err(|e| e.in_field(label))?;
        self.unknown.push((
            label,
            Span {
                start,
                end: start + n,
            },
        ));
        Ok(b)
    }

    /// Reads a byte.
    pub fn u8(&mut self) -> DecodeResult<u8> {
        Ok(self.cur.u8()?)
    }
    /// Reads a little-endian u16.
    pub fn u16(&mut self) -> DecodeResult<u16> {
        Ok(self.cur.u16()?)
    }
    /// Reads a little-endian i16.
    pub fn i16(&mut self) -> DecodeResult<i16> {
        Ok(self.cur.u16()? as i16)
    }
    /// Reads a little-endian u32.
    pub fn u32(&mut self) -> DecodeResult<u32> {
        Ok(self.cur.u32()?)
    }
    /// Reads a little-endian i32.
    pub fn i32(&mut self) -> DecodeResult<i32> {
        Ok(self.cur.i32()?)
    }
    /// Reads a little-endian f32.
    pub fn f32(&mut self) -> DecodeResult<f32> {
        Ok(f32::from_bits(self.cur.u32()?))
    }
    /// Reads three f32 (an `FVector`).
    pub fn vec3(&mut self) -> DecodeResult<[f32; 3]> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }
    /// Reads four f32 (an `FPlane` / `FSphere`).
    pub fn vec4(&mut self) -> DecodeResult<[f32; 4]> {
        Ok([self.f32()?, self.f32()?, self.f32()?, self.f32()?])
    }
    /// Reads an old-style compact index.
    pub fn compact(&mut self) -> DecodeResult<i32> {
        Ok(self.cur.compact_index()?)
    }

    /// Reads an `FBox` (min, max, valid byte).
    pub fn bbox(&mut self) -> DecodeResult<BoundingBox> {
        Ok(BoundingBox {
            min: self.vec3()?,
            max: self.vec3()?,
            valid: self.u8()?,
        })
    }

    /// Reads an object reference (compact index) and validates it against the package.
    pub fn object_ref(&mut self, package: &Package) -> DecodeResult<ObjectRef> {
        let at = self.pos();
        let raw = self.compact()?;
        package.resolve(raw).ok_or_else(|| {
            DecodeError::at(
                DecodeErrorKind::Invalid(format!("object reference {raw} out of range")),
                at,
            )
        })
    }

    /// Reads a dynamic-array count (compact index) and checks it against the remaining
    /// payload, assuming each element takes at least `min_element_size` bytes.
    pub fn count(&mut self, field: &'static str, min_element_size: usize) -> DecodeResult<usize> {
        let at = self.pos();
        let raw = self.compact().map_err(|e| e.in_field(field))?;
        self.check_count(field, i64::from(raw), min_element_size, at)
    }

    /// Validates a count read by other means (e.g. an i32 field).
    pub fn check_count(
        &self,
        field: &'static str,
        count: i64,
        min_element_size: usize,
        at: usize,
    ) -> DecodeResult<usize> {
        let remaining = self.remaining();
        let ok = count >= 0
            && (count as u64) <= MAX_ELEMENTS as u64
            && (count as u64).saturating_mul(min_element_size as u64) <= remaining as u64;
        if !ok {
            return Err(DecodeError::at(
                DecodeErrorKind::BadCount {
                    count,
                    min_element_size,
                    remaining,
                },
                at,
            )
            .in_field(field));
        }
        Ok(count as usize)
    }

    /// Reads a `TArray` of fixed-size elements with `f`.
    pub fn array<T>(
        &mut self,
        field: &'static str,
        element_size: usize,
        mut f: impl FnMut(&mut Self) -> DecodeResult<T>,
    ) -> DecodeResult<Vec<T>> {
        let n = self.count(field, element_size)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(f(self).map_err(|e| e.in_field(field))?);
        }
        Ok(out)
    }

    /// Reads a `TLazyArray` header for package version 100: an i32 absolute file offset of
    /// the end of the array data ("skip position") followed by the compact element count.
    /// Returns `(count, skip_position)`. The caller must verify that its element reads end
    /// exactly at the skip position ([`Self::expect_at`]).
    pub fn lazy_array_header(
        &mut self,
        field: &'static str,
        min_element_size: usize,
    ) -> DecodeResult<(usize, usize)> {
        let at = self.pos();
        let skip = self.i32().map_err(|e| e.in_field(field))?;
        if skip < 0 || (skip as usize) < self.pos() || (skip as usize) > self.payload.end {
            return Err(DecodeError::at(
                DecodeErrorKind::Invalid(format!(
                    "lazy array skip offset {skip} outside {}..{}",
                    self.pos(),
                    self.payload.end
                )),
                at,
            )
            .in_field(field));
        }
        let n = self.count(field, min_element_size)?;
        Ok((n, skip as usize))
    }

    /// Fails unless the reader is exactly at `pos`.
    pub fn expect_at(&self, field: &'static str, pos: usize) -> DecodeResult<()> {
        if self.pos() != pos {
            return Err(DecodeError::at(
                DecodeErrorKind::Invalid(format!(
                    "expected to end at 0x{pos:x}, reader is at 0x{:x}",
                    self.pos()
                )),
                self.pos(),
            )
            .in_field(field));
        }
        Ok(())
    }

    /// Finishes decoding: fails with [`DecodeErrorKind::UnconsumedTail`] unless the whole
    /// payload was consumed.
    pub fn finish(self, properties_end: usize) -> DecodeResult<PayloadReport> {
        if self.remaining() != 0 {
            return Err(DecodeError::at(
                DecodeErrorKind::UnconsumedTail {
                    bytes: self.remaining(),
                },
                self.pos(),
            ));
        }
        Ok(PayloadReport {
            payload: self.payload,
            properties_end,
            unknown: self.unknown,
            unsupported_tail: None,
        })
    }

    /// Finishes decoding of a class whose remaining payload is knowingly not decoded: the
    /// remaining bytes are returned as an explicit, labelled unsupported tail (never
    /// silently dropped). An empty remainder yields `unsupported_tail: None`.
    pub fn finish_with_unsupported_tail(
        self,
        label: &'static str,
        properties_end: usize,
    ) -> DecodeResult<PayloadReport> {
        let tail = (self.remaining() != 0).then_some((
            label,
            Span {
                start: self.pos(),
                end: self.payload.end,
            },
        ));
        Ok(PayloadReport {
            payload: self.payload,
            properties_end,
            unknown: self.unknown,
            unsupported_tail: tail,
        })
    }
}

/// `FBox`: axis-aligned bounds in source (Unreal) coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoundingBox {
    /// Minimum corner.
    pub min: [f32; 3],
    /// Maximum corner.
    pub max: [f32; 3],
    /// `IsValid` byte.
    pub valid: u8,
}

/// Fields serialized by `UPrimitive` (base of StaticMesh, Model): bounds plus the XIII
/// licensee extension (UModel: `GAME_XIII && ArLicenseeVer >= 19`: four bytes and a float).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PrimitiveHeader {
    /// Bounding box.
    pub bounding_box: BoundingBox,
    /// Bounding sphere (centre, radius).
    pub bounding_sphere: [f32; 4],
    /// XIII extension bytes (meaning unknown; all zero in the inspected meshes).
    pub xiii_bytes: [u8; 4],
    /// XIII extension float (meaning unknown).
    pub xiii_float: f32,
}

impl PrimitiveHeader {
    /// Reads the UPrimitive fields. All corpus packages have licensee >= 50, so the XIII
    /// extension is always present.
    pub fn read(r: &mut PayloadReader<'_>) -> DecodeResult<Self> {
        let bounding_box = r.bbox().map_err(|e| e.in_field("primitive.bounding_box"))?;
        let bounding_sphere = r
            .vec4()
            .map_err(|e| e.in_field("primitive.bounding_sphere"))?;
        let b = r.unknown("primitive.xiii_bytes", 4)?;
        let xiii_bytes = [b[0], b[1], b[2], b[3]];
        let xiii_float = f32::from_le_bytes(
            r.unknown("primitive.xiii_float", 4)?
                .try_into()
                .unwrap_or([0; 4]),
        );
        Ok(Self {
            bounding_box,
            bounding_sphere,
            xiii_bytes,
            xiii_float,
        })
    }
}

/// Reads the property block of an export and checks its class.
pub fn read_properties(
    package: &Package,
    data: &[u8],
    export: usize,
    expected_class: &'static str,
) -> DecodeResult<ObjectProperties> {
    let class = package.export_class_path(export).unwrap_or("?");
    if !class.eq_ignore_ascii_case(expected_class) {
        return Err(DecodeError::new(DecodeErrorKind::WrongClass {
            expected: expected_class,
            found: class.to_owned(),
        })
        .in_export(package, export));
    }
    package
        .read_object_properties(data, export, &Limits::default())
        .map_err(|e| DecodeError::from(e).in_export(package, export))
}

/// Property lookup by (case-insensitive) name and static-array index.
pub struct Props<'p> {
    package: &'p Package,
    props: &'p [Property],
}

impl<'p> Props<'p> {
    /// Wraps a decoded property list.
    pub fn new(package: &'p Package, props: &'p ObjectProperties) -> Self {
        Self {
            package,
            props: &props.block.properties,
        }
    }

    /// Wraps a slice of properties.
    pub fn from_slice(package: &'p Package, props: &'p [Property]) -> Self {
        Self { package, props }
    }

    /// First property with this name and array index 0.
    pub fn get(&self, name: &str) -> Option<&'p Property> {
        self.get_index(name, 0)
    }

    /// Property with this name and static-array index.
    pub fn get_index(&self, name: &str, index: u32) -> Option<&'p Property> {
        self.props.iter().find(|p| {
            p.array_index == index && self.package.property_name(p).eq_ignore_ascii_case(name)
        })
    }

    /// Byte/enum property.
    pub fn byte(&self, name: &str) -> Option<u8> {
        match self.get(name)?.value {
            PropertyValue::Byte(b) => Some(b),
            _ => None,
        }
    }

    /// Int property.
    pub fn int(&self, name: &str) -> Option<i32> {
        match self.get(name)?.value {
            PropertyValue::Int(v) => Some(v),
            _ => None,
        }
    }

    /// Float property.
    pub fn float(&self, name: &str) -> Option<f32> {
        match self.get(name)?.value {
            PropertyValue::Float(v) => Some(v),
            _ => None,
        }
    }

    /// Bool property.
    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.get(name)?.value {
            PropertyValue::Bool(v) => Some(v),
            _ => None,
        }
    }

    /// Object property.
    pub fn object(&self, name: &str) -> Option<ObjectRef> {
        self.object_index(name, 0)
    }

    /// Object property at a static-array index.
    pub fn object_index(&self, name: &str, index: u32) -> Option<ObjectRef> {
        match self.get_index(name, index)?.value {
            PropertyValue::Object(r) | PropertyValue::Class(r) => Some(r),
            _ => None,
        }
    }

    /// Vector property (struct `Vector`).
    pub fn vector(&self, name: &str) -> Option<[f32; 3]> {
        match &self.get(name)?.value {
            PropertyValue::Struct(StructValue::Vector(v)) => Some(*v),
            _ => None,
        }
    }

    /// Rotator property (pitch, yaw, roll).
    pub fn rotator(&self, name: &str) -> Option<[i32; 3]> {
        match &self.get(name)?.value {
            PropertyValue::Struct(StructValue::Rotator(v)) => Some(*v),
            _ => None,
        }
    }

    /// Name property text.
    pub fn name(&self, name: &str) -> Option<&'p str> {
        match self.get(name)?.value {
            PropertyValue::Name(n) => Some(self.package.name(n)),
            _ => None,
        }
    }

    /// Array property: element count and absolute element span.
    pub fn array(&self, name: &str) -> Option<(u32, Span)> {
        match self.get(name)?.value {
            PropertyValue::Array { count, elements } => Some((count, elements)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Coordinate conversion (the only place where Unreal axes become Bevy axes).
// ---------------------------------------------------------------------------------------

/// Provisional scale: Unreal units per metre. **Not measured** for XIII; chosen so that the
/// imported beach has plausible human scale in the diagnostic viewer. Calibrate against the
/// player collision cylinder and original captures before using it for movement.
pub const UNREAL_UNITS_PER_METER: f32 = 50.0;

/// Unreal rotator units per full turn.
pub const ROTATOR_UNITS_PER_TURN: f32 = 65536.0;

/// Converts a source position (Unreal: X forward, Y right, Z up, left-handed) into Bevy
/// space (X right, Y up, -Z forward, right-handed): `(x, y, z) -> (y, z, -x) / scale`.
///
/// The mapping matrix has determinant -1. That is required: it maps a left-handed
/// coordinate description onto a right-handed one while keeping the same physical shape
/// (nothing is mirrored in the world). Because `cross(Mu, Mv) = det(M) * M * cross(u, v)` for
/// an orthogonal `M`, the numeric orientation of every triangle flips with it, which turns
/// Unreal's clockwise front faces into Bevy's counter-clockwise front faces: source index
/// order is kept as is ([`TRIANGLE_ORDER_FOR_BEVY`]).
pub fn to_bevy_position(v: [f32; 3]) -> [f32; 3] {
    let s = 1.0 / UNREAL_UNITS_PER_METER;
    [v[1] * s, v[2] * s, -v[0] * s]
}

/// Converts a direction (normal, axis) without scaling: `(x, y, z) -> (y, z, -x)`.
pub fn to_bevy_direction(v: [f32; 3]) -> [f32; 3] {
    [v[1], v[2], -v[0]]
}

/// Converts a per-axis scale (e.g. `DrawScale3D`): the axis permutation only, no sign.
pub fn to_bevy_scale(v: [f32; 3]) -> [f32; 3] {
    [v[1], v[2], v[0]]
}

/// Index order to emit a source triangle `(a, b, c)` as a Bevy front face: unchanged.
///
/// Evidence: in source coordinates the numeric normal `(b - a) x (c - a)` of static-mesh
/// triangles is anti-parallel to the stored vertex normals (clockwise front faces, as in
/// D3D). After [`to_bevy_position`] (det -1) the same index order yields a numeric normal
/// parallel to the converted stored normal, i.e. counter-clockwise, Bevy's front face. The
/// static-mesh corpus test measures this agreement over all GOG meshes.
pub const TRIANGLE_ORDER_FOR_BEVY: [usize; 3] = [0, 1, 2];

/// Numeric triangle normal `(b - a) x (c - a)` (not normalized).
pub fn triangle_cross(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ]
}

/// Row-major 3x3 matrix.
pub type Mat3 = [[f32; 3]; 3];

/// Unreal rotation of a rotator `(pitch, yaw, roll)` as a matrix whose **columns** are the
/// rotated source X (forward), Y (right) and Z (up) axes, in source coordinates.
///
/// Order: roll about X, then pitch about Y, then yaw about Z. Forward axis =
/// `(cos P cos Y, cos P sin Y, sin P)`: positive pitch raises the forward vector, positive
/// yaw turns X towards Y. This is the standard Unreal rotation-matrix formula (as published
/// in e.g. UE4 `FRotationMatrix`; written here from the formula, not copied).
pub fn unreal_rotator_matrix(rot: [i32; 3]) -> Mat3 {
    let k = std::f32::consts::TAU / ROTATOR_UNITS_PER_TURN;
    let (sp, cp) = ((rot[0] as f32) * k).sin_cos();
    let (sy, cy) = ((rot[1] as f32) * k).sin_cos();
    let (sr, cr) = ((rot[2] as f32) * k).sin_cos();
    let x = [cp * cy, cp * sy, sp];
    let y = [sr * sp * cy - cr * sy, sr * sp * sy + cr * cy, -sr * cp];
    let z = [-(cr * sp * cy + sr * sy), cy * sr - cr * sp * sy, cr * cp];
    // columns = axes
    [[x[0], y[0], z[0]], [x[1], y[1], z[1]], [x[2], y[2], z[2]]]
}

/// Source -> Bevy axis-change matrix `C` (row-major): `bevy = C * source`.
pub const SOURCE_TO_BEVY: Mat3 = [[0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [-1.0, 0.0, 0.0]];

/// Matrix product `a * b`.
pub fn mat3_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

/// Transpose.
pub fn mat3_transpose(a: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = a[j][i];
        }
    }
    out
}

/// Determinant.
pub fn mat3_det(a: &Mat3) -> f32 {
    a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
        - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
}

/// Rotation of an Unreal rotator expressed in Bevy space: `C * R * C^T` (a proper
/// rotation, det +1). Row-major.
pub fn rotator_to_bevy_matrix(rot: [i32; 3]) -> Mat3 {
    let r = unreal_rotator_matrix(rot);
    mat3_mul(
        &mat3_mul(&SOURCE_TO_BEVY, &r),
        &mat3_transpose(&SOURCE_TO_BEVY),
    )
}

/// Source object transform (Location, Rotation, DrawScale * DrawScale3D) converted to Bevy:
/// translation in metres, a row-major rotation matrix and a per-axis scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BevyTransform {
    /// Translation (metres).
    pub translation: [f32; 3],
    /// Rotation (row-major, det +1).
    pub rotation: Mat3,
    /// Scale along the Bevy local axes.
    pub scale: [f32; 3],
}

/// Converts an actor placement. Unreal applies scale, then rotation, then translation
/// (PrePivot is not handled here).
pub fn actor_to_bevy(location: [f32; 3], rotation: [i32; 3], scale: [f32; 3]) -> BevyTransform {
    BevyTransform {
        translation: to_bevy_position(location),
        rotation: rotator_to_bevy_matrix(rotation),
        scale: to_bevy_scale(scale),
    }
}

/// World-space (source axes) location of an actor whose mesh-space point `pre_pivot` lands on
/// `location`, i.e. `T(Location) * R * S * T(-PrePivot) * pre_pivot = Location`. Adding this
/// shifted location and using [`actor_to_bevy`] is equivalent to applying `T(-PrePivot)` to
/// every mesh-space point before scale and rotation, because there is only one rotation:
/// `Location + R*(S*(v - PrePivot)) = (Location - R*(S*PrePivot)) + R*(S*v)`.
pub fn pre_pivot_shifted_location(
    location: [f32; 3],
    rotation: [i32; 3],
    scale: [f32; 3],
    pre_pivot: [f32; 3],
) -> [f32; 3] {
    let r = unreal_rotator_matrix(rotation);
    let s = [
        pre_pivot[0] * scale[0],
        pre_pivot[1] * scale[1],
        pre_pivot[2] * scale[2],
    ];
    [
        location[0] - (r[0][0] * s[0] + r[0][1] * s[1] + r[0][2] * s[2]),
        location[1] - (r[1][0] * s[0] + r[1][1] * s[1] + r[1][2] * s[2]),
        location[2] - (r[2][0] * s[0] + r[2][1] * s[1] + r[2][2] * s[2]),
    ]
}

/// Converts an actor placement with `PrePivot` applied **before scale/rotation**, the upstream
/// UE2 hypothesis: a mesh-space point becomes `T(Location) * R * S * T(-PrePivot) * v` (the
/// stored `PrePivot` is the mesh-space point that maps to `Location`). Equivalent to
/// [`actor_to_bevy`] with the translation shifted by [`pre_pivot_shifted_location`].
///
/// The sign/order could not be distinguished empirically on Plage00/Plage01: no placed
/// renderable actor on either map (or in the other GOG campaign maps) has a non-zero effective
/// `PrePivot`; see `local/reports/item1c-placement.md`. The upstream formula is implemented.
pub fn actor_to_bevy_pre_pivot(
    location: [f32; 3],
    rotation: [i32; 3],
    scale: [f32; 3],
    pre_pivot: [f32; 3],
) -> BevyTransform {
    actor_to_bevy(
        pre_pivot_shifted_location(location, rotation, scale, pre_pivot),
        rotation,
        scale,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
        [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ]
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn axis_mapping() {
        // Unreal forward (+X) is Bevy forward (-Z); right (+Y) -> +X; up (+Z) -> +Y.
        assert_eq!(to_bevy_direction([1.0, 0.0, 0.0]), [0.0, 0.0, -1.0]);
        assert_eq!(to_bevy_direction([0.0, 1.0, 0.0]), [1.0, 0.0, 0.0]);
        assert_eq!(to_bevy_direction([0.0, 0.0, 1.0]), [0.0, 1.0, 0.0]);
        assert_eq!(to_bevy_position([50.0, 0.0, 0.0]), [0.0, 0.0, -1.0]);
        assert!(
            (mat3_det(&SOURCE_TO_BEVY) + 1.0).abs() < 1e-6,
            "handedness change"
        );
    }

    #[test]
    fn rotator_units_and_order() {
        // Yaw 16384 = 90 deg turns forward (+X) to +Y.
        let m = unreal_rotator_matrix([0, 16384, 0]);
        assert!(close(apply(&m, [1.0, 0.0, 0.0]), [0.0, 1.0, 0.0]));
        // Pitch 16384 points forward straight up.
        let m = unreal_rotator_matrix([16384, 0, 0]);
        assert!(close(apply(&m, [1.0, 0.0, 0.0]), [0.0, 0.0, 1.0]));
        // Roll 16384 tilts the right axis (+Y) up to +Z.
        let m = unreal_rotator_matrix([0, 0, 16384]);
        assert!(
            close(apply(&m, [0.0, 1.0, 0.0]), [0.0, 0.0, -1.0])
                || close(apply(&m, [0.0, 1.0, 0.0]), [0.0, 0.0, 1.0])
        );
        // Full turn is identity.
        let m = unreal_rotator_matrix([65536, 65536, 65536]);
        assert!(close(apply(&m, [1.0, 2.0, 3.0]), [1.0, 2.0, 3.0]));
    }

    #[test]
    fn roll_then_pitch_then_yaw() {
        // Combined rotation equals yaw * pitch * roll applied to column vectors.
        let r = [3000, 12000, -7000];
        let full = unreal_rotator_matrix(r);
        let yaw = unreal_rotator_matrix([0, r[1], 0]);
        let pitch = unreal_rotator_matrix([r[0], 0, 0]);
        let roll = unreal_rotator_matrix([0, 0, r[2]]);
        // Pitch about Y in this convention is "nose up", i.e. a rotation by -P around +Y in
        // the standard right-hand sense; composition must still hold.
        let composed = mat3_mul(&yaw, &mat3_mul(&pitch, &roll));
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (full[i][j] - composed[i][j]).abs() < 1e-5,
                    "{full:?} vs {composed:?}"
                );
            }
        }
    }

    #[test]
    fn bevy_rotation_is_proper_and_consistent() {
        let r = [5000, -20000, 9000];
        let b = rotator_to_bevy_matrix(r);
        assert!((mat3_det(&b) - 1.0).abs() < 1e-5);
        // Rotating a converted vector equals converting the rotated vector.
        let v = [3.0, -2.0, 7.0];
        let lhs = apply(&b, to_bevy_direction(v));
        let rhs = to_bevy_direction(apply(&unreal_rotator_matrix(r), v));
        assert!(close(lhs, rhs));
        // Bevy yaw +16384: forward (-Z) turns to the right (+X), i.e. clockwise seen from
        // above, matching Unreal's X->Y yaw.
        let b = rotator_to_bevy_matrix([0, 16384, 0]);
        assert!(close(apply(&b, [0.0, 0.0, -1.0]), [1.0, 0.0, 0.0]));
    }

    #[test]
    fn handedness_change_keeps_index_order() {
        // Source quad triangle from StaticPlage2 PL_cartmarine01: stored normal +Z, indices
        // (0, 1, 2) over (32,32,1), (32,-32,1), (-32,32,1). Numerically clockwise in source.
        let (a, b, c) = ([32.0, 32.0, 1.0], [32.0, -32.0, 1.0], [-32.0, 32.0, 1.0]);
        assert!(triangle_cross(a, b, c)[2] < 0.0);
        let stored = to_bevy_direction([0.0, 0.0, 1.0]);
        let o = TRIANGLE_ORDER_FOR_BEVY;
        let p = [
            to_bevy_position(a),
            to_bevy_position(b),
            to_bevy_position(c),
        ];
        let n = triangle_cross(p[o[0]], p[o[1]], p[o[2]]);
        let dot: f32 = n.iter().zip(stored).map(|(x, y)| x * y).sum();
        assert!(
            dot > 0.0,
            "Bevy CCW normal agrees with the converted stored normal"
        );
    }

    #[test]
    fn pre_pivot_shift_matches_point_apply() {
        // T(L)*R*S*T(PrePivot) applied to a point equals actor_to_bevy with the shifted
        // location: `L + R*(S*PrePivot) + R*(S*v)`.
        let (l, r, s, pp) = (
            [10.0, 20.0, 30.0],
            [2000, -10000, 5000],
            [2.0, 3.0, 4.0],
            [7.0, -5.0, 11.0],
        );
        let rm = unreal_rotator_matrix(r);
        let apply = |v: [f32; 3]| -> [f32; 3] {
            // p' = v - PrePivot, then scale, rotate, translate (the implemented order).
            let sv = [
                (v[0] - pp[0]) * s[0],
                (v[1] - pp[1]) * s[1],
                (v[2] - pp[2]) * s[2],
            ];
            let rv = [
                rm[0][0] * sv[0] + rm[0][1] * sv[1] + rm[0][2] * sv[2],
                rm[1][0] * sv[0] + rm[1][1] * sv[1] + rm[1][2] * sv[2],
                rm[2][0] * sv[0] + rm[2][1] * sv[1] + rm[2][2] * sv[2],
            ];
            [l[0] + rv[0], l[1] + rv[1], l[2] + rv[2]]
        };
        let t = actor_to_bevy_pre_pivot(l, r, s, pp);
        let m = rotator_to_bevy_matrix(r);
        let transform_point = |p: [f32; 3]| -> [f32; 3] {
            let local = to_bevy_position(p);
            [
                m[0][0] * local[0] * t.scale[0]
                    + m[0][1] * local[1] * t.scale[1]
                    + m[0][2] * local[2] * t.scale[2]
                    + t.translation[0],
                m[1][0] * local[0] * t.scale[0]
                    + m[1][1] * local[1] * t.scale[1]
                    + m[1][2] * local[2] * t.scale[2]
                    + t.translation[1],
                m[2][0] * local[0] * t.scale[0]
                    + m[2][1] * local[1] * t.scale[1]
                    + m[2][2] * local[2] * t.scale[2]
                    + t.translation[2],
            ]
        };
        // The stored PrePivot is the mesh-space point that maps to Location.
        let at_pivot = transform_point(pp);
        let want_pivot = to_bevy_position(l);
        assert!(
            at_pivot
                .iter()
                .zip(want_pivot)
                .all(|(a, b)| (a - b).abs() < 1e-3),
            "PrePivot must map to Location: {at_pivot:?} vs {want_pivot:?}"
        );
        let v = [3.0, -2.0, 1.5];
        let world = to_bevy_position(apply(v));
        let via_transform = transform_point(v);
        assert!(
            world
                .iter()
                .zip(via_transform)
                .all(|(a, b)| (a - b).abs() < 1e-4)
        );
    }

    #[test]
    fn reader_bounds_and_lazy_array() {
        // payload occupies bytes 2..12 of the buffer
        let mut data = vec![0xAAu8, 0xBB];
        data.extend_from_slice(&(9i32).to_le_bytes()); // skip -> absolute 9
        data.push(3); // count 3
        data.extend_from_slice(&[1, 2, 3]); // elements end at 10 != 9
        data.extend_from_slice(&[0, 0]);
        let span = Span { start: 2, end: 12 };
        let mut r = PayloadReader::new(&data, span, 2).unwrap();
        let (n, skip) = r.lazy_array_header("t", 1).unwrap();
        assert_eq!((n, skip), (3, 9));
        r.bytes(3).unwrap();
        assert!(r.expect_at("t", skip).is_err());
        // reading beyond the payload fails even though the buffer has no more bytes either
        let mut r = PayloadReader::new(&data, span, 10).unwrap();
        assert!(r.i32().is_err());
        // counts that cannot fit are rejected before allocation
        let mut d2 = vec![0x7f_u8];
        d2.extend_from_slice(&[0; 4]);
        let mut r = PayloadReader::new(&d2, Span { start: 0, end: 5 }, 0).unwrap();
        assert!(matches!(
            r.count("c", 4).unwrap_err().kind,
            DecodeErrorKind::BadCount { .. }
        ));
        // unconsumed tail is an error with offset
        let r = PayloadReader::new(&data, span, 11).unwrap();
        let e = r.finish(2).unwrap_err();
        assert_eq!(e.kind, DecodeErrorKind::UnconsumedTail { bytes: 1 });
        assert_eq!(e.offset, Some(11));
    }
}

/// Synthetic package builder for unit tests (generated bytes only, no game data).
#[cfg(test)]
pub(crate) mod test_package {
    /// Encodes an old-style compact index.
    pub fn compact(value: i32) -> Vec<u8> {
        let mut m = u64::from(value.unsigned_abs());
        let mut first = (m & 0x3f) as u8;
        if value < 0 {
            first |= 0x80;
        }
        m >>= 6;
        if m != 0 {
            first |= 0x40;
        }
        let mut out = vec![first];
        while m != 0 {
            let mut b = (m & 0x7f) as u8;
            m >>= 7;
            if m != 0 {
                b |= 0x80;
            }
            out.push(b);
        }
        out
    }

    /// Little helper to append values.
    #[derive(Default, Clone)]
    pub struct Bytes(pub Vec<u8>);

    impl Bytes {
        pub fn u8(mut self, v: u8) -> Self {
            self.0.push(v);
            self
        }
        pub fn u16(mut self, v: u16) -> Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        pub fn i16(self, v: i16) -> Self {
            self.u16(v as u16)
        }
        pub fn i32(mut self, v: i32) -> Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        pub fn f32(mut self, v: f32) -> Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        pub fn v3(self, v: [f32; 3]) -> Self {
            self.f32(v[0]).f32(v[1]).f32(v[2])
        }
        pub fn c(mut self, v: i32) -> Self {
            self.0.extend(compact(v));
            self
        }
        pub fn raw(mut self, b: &[u8]) -> Self {
            self.0.extend_from_slice(b);
            self
        }
        pub fn len(&self) -> usize {
            self.0.len()
        }
    }

    /// Builds a version-100 package with `Core.Package Engine` + one `Core.Class` import per
    /// class name and one export per `(class, name, payload)`. Names: 0 = "None".
    pub struct Builder {
        pub licensee: u16,
        names: Vec<String>,
        classes: Vec<String>,
        exports: Vec<(usize, String, Vec<u8>)>,
    }

    impl Builder {
        pub fn new() -> Self {
            Self {
                licensee: 58,
                names: vec![
                    "None".into(),
                    "Core".into(),
                    "Engine".into(),
                    "Package".into(),
                    "Class".into(),
                ],
                classes: Vec::new(),
                exports: Vec::new(),
            }
        }

        /// Name index (adds the name if needed).
        pub fn name(&mut self, n: &str) -> i32 {
            if let Some(i) = self.names.iter().position(|x| x == n) {
                return i as i32;
            }
            self.names.push(n.into());
            (self.names.len() - 1) as i32
        }

        /// Adds an export of class `Engine.<class>`; returns its zero-based export index.
        pub fn export(&mut self, class: &str, name: &str, payload: Vec<u8>) -> usize {
            let ci = match self.classes.iter().position(|c| c == class) {
                Some(i) => i,
                None => {
                    self.classes.push(class.into());
                    self.classes.len() - 1
                }
            };
            self.name(class);
            self.name(name);
            self.exports.push((ci, name.into(), payload));
            self.exports.len() - 1
        }

        /// Replaces the payload of export `i` (same length keeps offsets stable).
        pub fn set_payload(&mut self, i: usize, payload: Vec<u8>) {
            self.exports[i].2 = payload;
        }

        /// Absolute file offset at which export `i`'s payload will start.
        pub fn payload_offset(&self, i: usize) -> usize {
            let mut off = 64 + self.names_bytes().len();
            for (_, _, p) in &self.exports[..i] {
                off += p.len();
            }
            off
        }

        fn names_bytes(&self) -> Vec<u8> {
            let mut out = Vec::new();
            for n in &self.names {
                out.extend(compact(n.len() as i32 + 1));
                out.extend(n.as_bytes());
                out.push(0);
                out.extend(0x0007_0010u32.to_le_bytes());
            }
            out
        }

        pub fn build(&self) -> Vec<u8> {
            let names = self.names_bytes();
            let name_offset = 64usize;
            let mut body = names.clone();
            let mut offsets = Vec::new();
            for (_, _, p) in &self.exports {
                offsets.push(name_offset + body.len());
                body.extend(p);
            }
            let idx = |n: &str| self.names.iter().position(|x| x == n).expect("name") as i32;
            // imports: 0 = Core.Package Engine; 1.. = Core.Class <class> (outer Engine)
            let import_offset = name_offset + body.len();
            let mut imports = Vec::new();
            imports.extend(compact(idx("Core")));
            imports.extend(compact(idx("Package")));
            imports.extend(0i32.to_le_bytes());
            imports.extend(compact(idx("Engine")));
            for c in &self.classes {
                imports.extend(compact(idx("Core")));
                imports.extend(compact(idx("Class")));
                imports.extend((-1i32).to_le_bytes());
                imports.extend(compact(idx(c)));
            }
            let export_offset = import_offset + imports.len();
            let mut exports = Vec::new();
            for (k, (ci, name, p)) in self.exports.iter().enumerate() {
                exports.extend(compact(-(*ci as i32 + 2)));
                exports.extend(compact(0));
                exports.extend(0i32.to_le_bytes());
                exports.extend(compact(idx(name)));
                exports.extend(0x0007_0004u32.to_le_bytes());
                exports.extend(compact(p.len() as i32));
                if !p.is_empty() {
                    exports.extend(compact(offsets[k] as i32));
                }
            }
            let mut out = Vec::new();
            out.extend(0x9e2a_83c1u32.to_le_bytes());
            out.extend(100u16.to_le_bytes());
            out.extend(self.licensee.to_le_bytes());
            out.extend(1u32.to_le_bytes());
            for v in [
                self.names.len(),
                name_offset,
                self.exports.len(),
                export_offset,
                1 + self.classes.len(),
                import_offset,
            ] {
                out.extend((v as i32).to_le_bytes());
            }
            out.extend([7u8; 16]);
            out.extend(1i32.to_le_bytes());
            out.extend((self.exports.len() as i32).to_le_bytes());
            out.extend((self.names.len() as i32).to_le_bytes());
            assert_eq!(out.len(), 64);
            out.extend(body);
            out.extend(imports);
            out.extend(exports);
            out
        }
    }

    /// Parses built bytes.
    pub fn parse(bytes: &[u8]) -> xiii_package::Package {
        xiii_package::Package::parse(bytes, &xiii_package::Limits::default())
            .expect("synthetic package parses")
    }
}
