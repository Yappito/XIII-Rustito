//! Bounded payload reader for the skeletal decoders.
//!
//! Wraps [`xiii_package::Cursor`] over one export payload and turns every failure into a
//! [`SkelError`] carrying the class, export index, field and absolute file offset. Array counts
//! are checked against the remaining bytes (count x minimum element size) before anything is
//! allocated. Kept private to this module; candidates for a shared `common` reader later.

use xiii_package::{Cursor, ErrorKind, ObjectRef, Package, PackageError};

use super::error::{SkelError, SkelErrorKind};

/// Hard cap on any decoded array length (independent of the data-size check).
pub(crate) const MAX_ARRAY: usize = 1 << 22;

pub(crate) struct Reader<'a> {
    c: Cursor<'a>,
    base: usize,
    class: &'static str,
    export: Option<u32>,
    names: &'a [xiii_package::NameEntry],
    imports: u32,
    exports: u32,
}

impl<'a> Reader<'a> {
    /// `payload` is the export payload, `base` its absolute file offset and `start` the
    /// payload-relative position to start at (after the property block).
    pub(crate) fn new(
        package: &'a Package,
        payload: &'a [u8],
        base: usize,
        start: usize,
        class: &'static str,
        export: Option<u32>,
    ) -> Result<Self, SkelError> {
        let mut c = Cursor::new(payload);
        let mut r = Self {
            c: Cursor::new(payload),
            base,
            class,
            export,
            names: package.names(),
            imports: package.imports().len() as u32,
            exports: package.exports().len() as u32,
        };
        c.seek(start).map_err(|e| r.pkg("payload.start", e))?;
        r.c = c;
        Ok(r)
    }

    /// Payload-relative position.
    pub(crate) fn pos(&self) -> usize {
        self.c.pos()
    }

    /// Absolute file offset of the current position.
    pub(crate) fn abs(&self) -> u64 {
        (self.base + self.c.pos()) as u64
    }

    pub(crate) fn remaining(&self) -> usize {
        self.c.remaining()
    }

    pub(crate) fn err(&self, field: &'static str, kind: SkelErrorKind) -> SkelError {
        SkelError {
            class: self.class,
            export: self.export,
            offset: Some(self.abs()),
            field,
            kind,
        }
    }

    pub(crate) fn err_at(&self, field: &'static str, pos: usize, kind: SkelErrorKind) -> SkelError {
        SkelError {
            offset: Some((self.base + pos) as u64),
            ..self.err(field, kind)
        }
    }

    fn pkg(&self, field: &'static str, e: PackageError) -> SkelError {
        let offset = e.offset.map(|o| o + self.base as u64).or(Some(self.abs()));
        let kind = match e.kind {
            ErrorKind::UnexpectedEof { needed, available } => {
                SkelErrorKind::UnexpectedEof { needed, available }
            }
            other => SkelErrorKind::Package(other.to_string()),
        };
        SkelError {
            class: self.class,
            export: self.export,
            offset,
            field,
            kind,
        }
    }

    pub(crate) fn u8(&mut self, field: &'static str) -> Result<u8, SkelError> {
        self.c.u8().map_err(|e| self.pkg(field, e))
    }

    pub(crate) fn u16(&mut self, field: &'static str) -> Result<u16, SkelError> {
        self.c.u16().map_err(|e| self.pkg(field, e))
    }

    pub(crate) fn i16(&mut self, field: &'static str) -> Result<i16, SkelError> {
        Ok(self.u16(field)? as i16)
    }

    pub(crate) fn u32(&mut self, field: &'static str) -> Result<u32, SkelError> {
        self.c.u32().map_err(|e| self.pkg(field, e))
    }

    pub(crate) fn i32(&mut self, field: &'static str) -> Result<i32, SkelError> {
        self.c.i32().map_err(|e| self.pkg(field, e))
    }

    pub(crate) fn f32(&mut self, field: &'static str) -> Result<f32, SkelError> {
        Ok(f32::from_bits(self.u32(field)?))
    }

    pub(crate) fn vec3(&mut self, field: &'static str) -> Result<[f32; 3], SkelError> {
        Ok([self.f32(field)?, self.f32(field)?, self.f32(field)?])
    }

    pub(crate) fn quat(&mut self, field: &'static str) -> Result<[f32; 4], SkelError> {
        Ok([
            self.f32(field)?,
            self.f32(field)?,
            self.f32(field)?,
            self.f32(field)?,
        ])
    }

    pub(crate) fn compact(&mut self, field: &'static str) -> Result<i32, SkelError> {
        self.c.compact_index().map_err(|e| self.pkg(field, e))
    }

    /// TArray count (compact index) checked against `min_elem` bytes per element.
    pub(crate) fn count(
        &mut self,
        field: &'static str,
        min_elem: usize,
    ) -> Result<usize, SkelError> {
        let start = self.pos();
        let raw = self.compact(field)?;
        let count = usize::try_from(raw)
            .map_err(|_| self.err_at(field, start, SkelErrorKind::NegativeCount(raw)))?;
        if count > MAX_ARRAY {
            return Err(self.err_at(
                field,
                start,
                SkelErrorKind::CountTooLarge {
                    count: count as u64,
                    max: MAX_ARRAY as u64,
                },
            ));
        }
        let needed = count as u64 * min_elem as u64;
        if needed > self.remaining() as u64 {
            return Err(self.err_at(
                field,
                start,
                SkelErrorKind::ArrayExceedsData {
                    count: count as u64,
                    elem_size: min_elem as u64,
                    available: self.remaining() as u64,
                },
            ));
        }
        Ok(count)
    }

    /// Reads a TArray whose elements are read by `f` (each at least `min_elem` bytes).
    pub(crate) fn array<T>(
        &mut self,
        field: &'static str,
        min_elem: usize,
        mut f: impl FnMut(&mut Self) -> Result<T, SkelError>,
    ) -> Result<Vec<T>, SkelError> {
        let n = self.count(field, min_elem)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(f(self)?);
        }
        Ok(out)
    }

    /// Compact name index resolved to its text.
    pub(crate) fn name(&mut self, field: &'static str) -> Result<String, SkelError> {
        let start = self.pos();
        let raw = self.compact(field)?;
        usize::try_from(raw)
            .ok()
            .and_then(|i| self.names.get(i))
            .map(|n| n.text.clone())
            .ok_or_else(|| self.err_at(field, start, SkelErrorKind::NameOutOfRange(raw)))
    }

    /// Compact object reference checked against the import/export tables.
    pub(crate) fn object(&mut self, field: &'static str) -> Result<ObjectRef, SkelError> {
        let start = self.pos();
        let raw = self.compact(field)?;
        ObjectRef::from_raw(raw, self.imports, self.exports)
            .ok_or_else(|| self.err_at(field, start, SkelErrorKind::ObjectRefOutOfRange(raw)))
    }

    /// Fails with [`SkelErrorKind::TrailingBytes`] unless the payload is fully consumed.
    pub(crate) fn finish(&self, field: &'static str) -> Result<(), SkelError> {
        if self.remaining() != 0 {
            return Err(self.err(
                field,
                SkelErrorKind::TrailingBytes {
                    remaining: self.remaining() as u64,
                },
            ));
        }
        Ok(())
    }
}
