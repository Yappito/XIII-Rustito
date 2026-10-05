//! Bounded reader over one payload range with absolute-offset errors and table-checked
//! object/name references.

use xiii_package::{Cursor, ObjectRef, Package};

use crate::error::{Result, ScriptError, ScriptErrorKind};

/// Table sizes used to validate references without needing a parsed [`Package`]
/// (synthetic tests build these directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tables {
    /// Name-table entries.
    pub names: u32,
    /// Import-table entries.
    pub imports: u32,
    /// Export-table entries.
    pub exports: u32,
}

impl Tables {
    /// Table sizes of a parsed package.
    pub fn of(package: &Package) -> Self {
        Self {
            names: package.names().len() as u32,
            imports: package.imports().len() as u32,
            exports: package.exports().len() as u32,
        }
    }
}

/// Checked little-endian reader over `data[start..end]`; positions and errors are absolute.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    cur: Cursor<'a>,
    base: usize,
    tables: Tables,
}

impl<'a> Reader<'a> {
    /// Reader over `data[start..end]`. Panics never: invalid ranges become an error.
    pub fn new(data: &'a [u8], start: usize, end: usize, tables: Tables) -> Result<Self> {
        if start > end || end > data.len() {
            return Err(ScriptError::new(ScriptErrorKind::ValueOutOfRange {
                what: "payload range",
                value: end as i64,
            }));
        }
        Ok(Self {
            cur: Cursor::new(&data[start..end]),
            base: start,
            tables,
        })
    }

    /// Absolute position.
    pub fn pos(&self) -> usize {
        self.base + self.cur.pos()
    }

    /// Absolute end of the readable range.
    pub fn end(&self) -> usize {
        self.base + self.cur.len()
    }

    /// Bytes left before the end.
    pub fn remaining(&self) -> usize {
        self.cur.remaining()
    }

    /// Table sizes used for reference checks.
    pub fn tables(&self) -> Tables {
        self.tables
    }

    /// Next byte without consuming it.
    pub fn peek_u8(&self) -> Option<u8> {
        let mut c = self.cur.clone();
        c.u8().ok()
    }

    /// The `i32` after the next byte, without consuming anything.
    pub fn peek_i32_after_byte(&self) -> Option<i32> {
        let mut c = self.cur.clone();
        c.u8().ok()?;
        c.i32().ok()
    }

    fn wrap<T>(&self, r: xiii_package::Result<T>) -> Result<T> {
        r.map_err(|e| ScriptError::from_package(e, self.base))
    }

    /// One byte.
    pub fn u8(&mut self) -> Result<u8> {
        let r = self.cur.u8();
        self.wrap(r)
    }

    /// Little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16> {
        let r = self.cur.u16();
        self.wrap(r)
    }

    /// Little-endian `i16`.
    pub fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    /// Little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32> {
        let r = self.cur.u32();
        self.wrap(r)
    }

    /// Little-endian `i32`.
    pub fn i32(&mut self) -> Result<i32> {
        let r = self.cur.i32();
        self.wrap(r)
    }

    /// Little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64> {
        let lo = u64::from(self.u32()?);
        let hi = u64::from(self.u32()?);
        Ok(lo | (hi << 32))
    }

    /// Little-endian IEEE `f32`.
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    /// Little-endian 24-bit unsigned integer (XIII function flags).
    pub fn u24(&mut self) -> Result<u32> {
        let b = self.take(3)?;
        Ok(u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16))
    }

    /// Borrows the next `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let r = self.cur.take(n);
        self.wrap(r)
    }

    /// Old-style compact index.
    pub fn compact(&mut self) -> Result<i32> {
        let r = self.cur.compact_index();
        self.wrap(r)
    }

    /// Compact object reference checked against the tables.
    pub fn object(&mut self) -> Result<ObjectRef> {
        let at = self.pos();
        let raw = self.compact()?;
        ObjectRef::from_raw(raw, self.tables.imports, self.tables.exports)
            .ok_or_else(|| ScriptError::at(ScriptErrorKind::BadObjectRef { raw }, at))
    }

    /// Compact name index checked against the name table.
    pub fn name(&mut self) -> Result<u32> {
        let at = self.pos();
        let index = self.compact()?;
        if index < 0 || index as u32 >= self.tables.names {
            return Err(ScriptError::at(ScriptErrorKind::BadNameIndex { index }, at));
        }
        Ok(index as u32)
    }

    /// Compact count bounded by `max`.
    pub fn count(&mut self, what: &'static str, max: u32) -> Result<u32> {
        let at = self.pos();
        let n = self.compact()?;
        if n < 0 || n as u32 > max {
            return Err(ScriptError::at(
                ScriptErrorKind::CountTooLarge {
                    what,
                    count: i64::from(n),
                    max: u64::from(max),
                },
                at,
            ));
        }
        // Every counted element takes at least one byte, so a count above the remaining
        // byte count cannot be valid; reject it before any allocation.
        if n as usize > self.remaining() {
            return Err(ScriptError::at(
                ScriptErrorKind::CountTooLarge {
                    what,
                    count: i64::from(n),
                    max: self.remaining() as u64,
                },
                at,
            ));
        }
        Ok(n as u32)
    }

    /// Unreal `FString` with at most `max_units` code units.
    pub fn fstring(&mut self, max_units: u32) -> Result<String> {
        let r = self.cur.fstring(max_units);
        self.wrap(r)
    }

    /// Fails with [`ScriptErrorKind::TrailingBytes`] unless the whole range was consumed.
    pub fn expect_end(&self) -> Result<()> {
        if self.remaining() != 0 {
            return Err(ScriptError::at(
                ScriptErrorKind::TrailingBytes {
                    consumed_to: self.pos(),
                    payload_end: self.end(),
                },
                self.pos(),
            ));
        }
        Ok(())
    }
}
