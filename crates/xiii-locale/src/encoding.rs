//! Byte-encoding detection and decoding for UE2 `.int`-family files.
//!
//! Measured on the GOG corpus (216 files: 48 `.int`, 42 each `.frt`/`.det`/
//! `.est`/`.itt`): every file is a single-byte encoding with `\r\n` line
//! endings; none carries a UTF-16 or UTF-8 BOM. The high bytes present
//! (`0x84 0x85 0x92 0x93 0x94 0x96 0x9c ...`) are Windows-1252 punctuation,
//! not ISO-8859-1: `0x85` is `…`, not the C1 NEL control. UE2 shipped UTF-16 LE
//! with a BOM in other products, so BOM-marked UTF-16 LE/BE and UTF-8 are
//! decoded here; anything else is decoded as Windows-1252.

use std::fmt;

/// The encoding a localisation file was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// UTF-16 little-endian with a `FF FE` BOM.
    Utf16Le,
    /// UTF-16 big-endian with a `FE FF` BOM.
    Utf16Be,
    /// UTF-8 with an `EF BB BF` BOM.
    Utf8Bom,
    /// Valid UTF-8 without a BOM (ASCII and real UTF-8 files).
    Utf8,
    /// Windows-1252 (the fallback for every other byte sequence).
    Windows1252,
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Utf16Le => "utf-16le-bom",
            Self::Utf16Be => "utf-16be-bom",
            Self::Utf8Bom => "utf-8-bom",
            Self::Utf8 => "utf-8",
            Self::Windows1252 => "windows-1252",
        })
    }
}

/// Why a file could not be decoded. These are hard errors (the file's text is
/// unusable), unlike per-line parse issues, which are counted separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// A BOM-marked UTF-16 file has an odd number of bytes.
    OddUtf16Length { bytes: usize },
    /// A UTF-16 surrogate was not followed by its pair.
    UnpairedSurrogate { offset: usize },
    /// A BOM-marked UTF-8 file contains invalid UTF-8.
    InvalidUtf8 { offset: usize },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OddUtf16Length { bytes } => {
                write!(f, "UTF-16 file has an odd byte length ({bytes})")
            }
            Self::UnpairedSurrogate { offset } => {
                write!(f, "UTF-16 surrogate at byte {offset} has no pair")
            }
            Self::InvalidUtf8 { offset } => write!(f, "invalid UTF-8 at byte {offset}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Returns the BOM-stripped bytes and the encoding for `bytes`.
pub fn detect(bytes: &[u8]) -> Encoding {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        Encoding::Utf16Le
    } else if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        Encoding::Utf16Be
    } else if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        Encoding::Utf8Bom
    } else if std::str::from_utf8(bytes).is_ok() {
        Encoding::Utf8
    } else {
        Encoding::Windows1252
    }
}

/// Decodes `bytes`, returning the detected encoding and the text.
pub fn decode(bytes: &[u8]) -> Result<(Encoding, String), DecodeError> {
    let encoding = detect(bytes);
    let text = match encoding {
        Encoding::Utf16Le => decode_utf16(&bytes[2..], true)?,
        Encoding::Utf16Be => decode_utf16(&bytes[2..], false)?,
        Encoding::Utf8Bom => decode_utf8(&bytes[3..])?,
        Encoding::Utf8 => decode_utf8(bytes)?,
        Encoding::Windows1252 => bytes.iter().map(|&b| cp1252_char(b)).collect(),
    };
    Ok((encoding, text))
}

fn decode_utf8(bytes: &[u8]) -> Result<String, DecodeError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|e| DecodeError::InvalidUtf8 {
            offset: e.valid_up_to(),
        })
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String, DecodeError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(DecodeError::OddUtf16Length {
            bytes: bytes.len() + 2,
        });
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            if little_endian {
                u16::from_le_bytes(*c)
            } else {
                u16::from_be_bytes(*c)
            }
        })
        .collect();
    let mut out = String::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let unit = units[i];
        let offset = i * 2 + 2;
        if (0xD800..=0xDBFF).contains(&unit) {
            let Some(&next) = units.get(i + 1) else {
                return Err(DecodeError::UnpairedSurrogate { offset });
            };
            if !(0xDC00..=0xDFFF).contains(&next) {
                return Err(DecodeError::UnpairedSurrogate { offset });
            }
            let scalar = 0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(next) - 0xDC00);
            out.push(char::from_u32(scalar).unwrap_or('\u{FFFD}'));
            i += 2;
        } else if (0xDC00..=0xDFFF).contains(&unit) {
            return Err(DecodeError::UnpairedSurrogate { offset });
        } else {
            out.push(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'));
            i += 1;
        }
    }
    Ok(out)
}

/// Windows-1252 byte to Unicode. Bytes undefined in Windows-1252
/// (`0x81 0x8D 0x8F 0x90 0x9D`) are kept as their C1 code point so no byte is
/// silently lost; none of them occurs in the measured GOG corpus.
fn cp1252_char(b: u8) -> char {
    const TABLE: [char; 32] = [
        '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}',
        '\u{017D}', '\u{008F}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    ];
    match b {
        0x00..=0x7F | 0xA0..=0xFF => char::from(b),
        _ => TABLE[usize::from(b - 0x80)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_decodes_utf16le_bom() {
        let bytes = [0xFF, 0xFE, b'A', 0x00, 0xAC, 0x20];
        assert_eq!(detect(&bytes), Encoding::Utf16Le);
        let (enc, text) = decode(&bytes).unwrap();
        assert_eq!(enc, Encoding::Utf16Le);
        assert_eq!(text, "A\u{20ac}");
    }

    #[test]
    fn detects_and_decodes_utf16be_bom() {
        let bytes = [0xFE, 0xFF, 0x00, b'A'];
        assert_eq!(detect(&bytes), Encoding::Utf16Be);
        assert_eq!(decode(&bytes).unwrap().1, "A");
    }

    #[test]
    fn detects_and_decodes_utf8_bom() {
        let bytes = [0xEF, 0xBB, 0xBF, b'h', 0xC3, 0xA9];
        assert_eq!(detect(&bytes), Encoding::Utf8Bom);
        assert_eq!(decode(&bytes).unwrap().1, "h\u{e9}");
    }

    #[test]
    fn ascii_is_utf8_and_high_bytes_are_cp1252() {
        assert_eq!(detect(b"plain"), Encoding::Utf8);
        // 0x92 is a right single quote in Windows-1252 and invalid UTF-8 alone.
        let bytes = b"Don\x92t";
        assert_eq!(detect(bytes), Encoding::Windows1252);
        assert_eq!(decode(bytes).unwrap().1, "Don\u{2019}t");
    }

    #[test]
    fn cp1252_undefined_bytes_are_preserved() {
        // 0x81, 0x8D, 0x8F, 0x90, 0x9D have no Windows-1252 glyph.
        assert_eq!(cp1252_char(0x81), '\u{81}');
        assert_eq!(cp1252_char(0x8D), '\u{8d}');
        assert_eq!(cp1252_char(0x9D), '\u{9d}');
    }

    #[test]
    fn odd_utf16_is_an_error() {
        let bytes = [0xFF, 0xFE, b'A'];
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::OddUtf16Length { bytes: 3 })
        );
    }

    #[test]
    fn lone_surrogate_is_an_error() {
        // High surrogate 0xD800 with no low surrogate.
        let bytes = [0xFF, 0xFE, 0x00, 0xD8];
        assert!(matches!(
            decode(&bytes),
            Err(DecodeError::UnpairedSurrogate { .. })
        ));
    }
}
