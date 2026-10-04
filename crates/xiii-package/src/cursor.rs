//! Checked little-endian reader over a byte slice.
//!
//! Every failure reports the absolute offset at which the failing read started. Nothing here
//! allocates proportionally to an untrusted length before the corresponding bytes are known to
//! exist in the buffer.

use crate::error::{ErrorKind, PackageError, Result};

/// Checked little-endian cursor. Positions are absolute offsets into the slice.
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// Creates a cursor at offset 0.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Current absolute offset.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Total buffer length.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// True when the underlying buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Bytes remaining after the current position.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Moves to an absolute offset (which may equal the buffer length).
    pub fn seek(&mut self, pos: usize) -> Result<()> {
        if pos > self.data.len() {
            return Err(PackageError::at(
                ErrorKind::SeekOutOfBounds {
                    position: pos as u64,
                    len: self.data.len() as u64,
                },
                self.pos,
            ));
        }
        self.pos = pos;
        Ok(())
    }

    /// Borrows the next `size` bytes and advances.
    pub fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        if size > self.remaining() {
            return Err(PackageError::at(
                ErrorKind::UnexpectedEof {
                    needed: size as u64,
                    available: self.remaining() as u64,
                },
                self.pos,
            ));
        }
        let out = &self.data[self.pos..self.pos + size];
        self.pos += size;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    /// Reads a little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// Reads a little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// Reads a little-endian `i32`.
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    /// Reads 16 raw bytes (a package GUID).
    pub fn guid(&mut self) -> Result<[u8; 16]> {
        self.array()
    }

    /// Reads an old-style Unreal compact index (not LEB128).
    ///
    /// Byte 0: bit 7 sign, bit 6 continuation, bits 0..5 magnitude. Bytes 1..3: bit 7
    /// continuation, bits 0..6 magnitude. Byte 4 (if reached) contributes its bits directly but
    /// must not set bits 5..7, so the magnitude is at most 32 bits. Magnitudes above
    /// `i32::MAX` (positive) or `2^31` (negative) are rejected. A "negative zero" (`0x80`) and
    /// non-minimal encodings decode without error, matching the research probe.
    pub fn compact_index(&mut self) -> Result<i32> {
        let start = self.pos;
        let first = self.u8()?;
        let negative = first & 0x80 != 0;
        let mut magnitude = u64::from(first & 0x3f);
        let mut more = first & 0x40 != 0;
        let mut shift = 6u32;
        for index in 1..5 {
            if !more {
                break;
            }
            let byte = self.u8()?;
            if index == 4 {
                if byte & 0xe0 != 0 {
                    return Err(PackageError::at(ErrorKind::CompactIndexOverflow, start));
                }
                magnitude |= u64::from(byte) << shift;
                more = false;
            } else {
                magnitude |= u64::from(byte & 0x7f) << shift;
                more = byte & 0x80 != 0;
            }
            shift += 7;
        }
        let max = if negative { 0x8000_0000 } else { 0x7fff_ffff };
        if magnitude > max {
            return Err(PackageError::at(
                ErrorKind::CompactIndexOutOfRange {
                    magnitude,
                    negative,
                },
                start,
            ));
        }
        let signed = if negative {
            -(magnitude as i64)
        } else {
            magnitude as i64
        };
        // Range was checked above, so this conversion cannot fail.
        i32::try_from(signed).map_err(|_| {
            PackageError::at(
                ErrorKind::CompactIndexOutOfRange {
                    magnitude,
                    negative,
                },
                start,
            )
        })
    }

    /// Reads an Unreal `FString`: compact-index length in code units including the NUL
    /// terminator; positive = Latin-1 bytes, negative = UTF-16LE units, zero = empty string.
    ///
    /// The length is checked against `max_units` before reading, and the payload must be
    /// present in the buffer before any decoding allocation.
    pub fn fstring(&mut self, max_units: u32) -> Result<String> {
        let start = self.pos;
        let length = self.compact_index()?;
        let units = length.unsigned_abs();
        if units > max_units {
            return Err(PackageError::at(
                ErrorKind::StringTooLong {
                    length,
                    max: max_units,
                },
                start,
            ));
        }
        if length == 0 {
            return Ok(String::new());
        }
        let units = units as usize;
        if length > 0 {
            let raw = self.take(units)?;
            match raw.split_last() {
                Some((0, body)) => Ok(body.iter().map(|&b| char::from(b)).collect()),
                _ => Err(PackageError::at(ErrorKind::UnterminatedString, start)),
            }
        } else {
            let byte_len = units.checked_mul(2).ok_or_else(|| {
                PackageError::at(
                    ErrorKind::StringTooLong {
                        length,
                        max: max_units,
                    },
                    start,
                )
            })?;
            let raw = self.take(byte_len)?;
            if raw[byte_len - 2..] != [0, 0] {
                return Err(PackageError::at(ErrorKind::UnterminatedString, start));
            }
            let body = &raw[..byte_len - 2];
            let decoded: std::result::Result<String, _> = char::decode_utf16(
                body.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair)),
            )
            .collect();
            decoded.map_err(|_| PackageError::at(ErrorKind::InvalidUtf16, start))
        }
    }
}
