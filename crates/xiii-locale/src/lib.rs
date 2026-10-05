//! UE2 `.int` localisation files for XIII Classic.
//!
//! Two layers, kept separate:
//!
//! * [`parse`] is a dependency-free, filesystem-free parser over `&[u8]`
//!   ([`LocalizationFile::parse`]). It handles the encodings, quoting and
//!   duplicate keys of the shipped corpus and counts every malformed line.
//! * [`localizer::Localizer`] loads every localisation file listed by an
//!   [`xiii_install::Installation`] and answers `Localize(Section, Key,
//!   Package)` queries with fallback to the `int` (English) file.
//!
//! Nothing here writes to an installation.

pub mod encoding;
mod localizer;
mod parse;

pub use encoding::{DecodeError, Encoding, decode, detect};
pub use localizer::{
    FALLBACK_LANGUAGE, IoError, LANGUAGE_CODES, Localizer, detect_language, is_language_code,
};
pub use parse::{
    Entry, LocalizationFile, ParseError, ParseErrorKind, ParseWarning, ParseWarningKind, Section,
};
