//! Filesystem-free parser for the UE2 `.int` localisation format.
//!
//! Measured over the GOG corpus (216 files), the format is the classic Unreal
//! INI shape with `\r\n` lines:
//!
//! ```text
//! [Section]
//! ; comment
//! Key=Value
//! ```
//!
//! Section and key names match case-insensitively (UE2 stores them as `FName`).
//! Values are taken verbatim to the end of the line, so a trailing space is
//! significant (`(CHAT) `); leading spaces/tabs after `=` are skipped. A value
//! whose first non-space character is `"` has one leading and one trailing
//! quote removed (`"\u{201c} aktiviert\u{201d}"` style quoting); inner quotes
//! are preserved because dialogue values embed them
//! (`Speakers=((PawnName="Pam",...))`). Backslash escapes (only `\n` occurs in
//! the corpus) are **not** expanded, matching a localisation getter that
//! returns the raw text and leaves interpretation to the UI.
//!
//! Duplicate keys and repeated sections occur in the corpus (52 section+key
//! duplicates). Sections merge; for a duplicate key the **last** occurrence
//! wins (Unreal's config cache keeps one value per key). Missing separators,
//! unterminated quotes and keys outside a section are counted warnings so the
//! whole corpus parses with zero hard errors and nothing is silently dropped.

use std::collections::BTreeMap;
use std::fmt;

use crate::encoding::{self, DecodeError, Encoding};

/// A hard parse error for a line the parser cannot represent as an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// `Key=` with an empty key.
    EmptyKey,
    /// A line starting with `[` without a closing `]`.
    UnclosedSection,
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyKey => "empty key before '='",
            Self::UnclosedSection => "section header has no closing ']'",
        })
    }
}

/// One hard parse error, with its 1-based line number and the raw line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub kind: ParseErrorKind,
    pub text: String,
}

/// Non-fatal shape issues that Unreal's config reader tolerates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParseWarningKind {
    /// A non-comment, non-section line without `=`; kept as a key with an
    /// empty value (Unreal config behaviour), never dropped.
    MissingSeparator,
    /// A value starting with `"` but with no closing `"`.
    UnterminatedQuote,
    /// A key line before the first `[Section]`; kept under the empty section.
    KeyOutsideSection,
}

impl fmt::Display for ParseWarningKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingSeparator => "line has no '='; stored as key with empty value",
            Self::UnterminatedQuote => "quoted value has no closing quote",
            Self::KeyOutsideSection => "key before the first section; stored under ''",
        })
    }
}

/// One non-fatal parse issue, with its 1-based line number and the raw line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseWarning {
    pub line: usize,
    pub kind: ParseWarningKind,
    pub text: String,
}

/// One `Key=Value` entry. `key` keeps the file's spelling; lookup is
/// case-insensitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
    /// 1-based line number the winning occurrence was read from.
    pub line: usize,
}

/// One `[Section]`, merging every occurrence of the same name in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// First-seen spelling of the section name.
    pub name: String,
    entries: BTreeMap<String, Entry>,
}

impl Section {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            entries: BTreeMap::new(),
        }
    }

    /// Case-insensitive entry lookup.
    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.get(&key.to_ascii_lowercase())
    }

    /// Number of distinct keys (duplicates collapsed to the last value).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries in case-insensitive key order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }
}

/// A parsed localisation file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalizationFile {
    /// Path relative to the installation root, exact spelling, `/`-separated.
    pub relative: String,
    /// File stem, exact spelling (`Plage01`).
    pub stem: String,
    /// Lowercase language extension (`int`, `frt`, ...).
    pub language: String,
    pub encoding: Encoding,
    /// Set when the bytes could not be decoded; `sections` is then empty.
    pub decode_error: Option<DecodeError>,
    sections: BTreeMap<String, Section>,
    /// Hard errors: `UnclosedSection` / `EmptyKey`, counted with line numbers.
    pub errors: Vec<ParseError>,
    /// Tolerated shape issues, counted with line numbers.
    pub warnings: Vec<ParseWarning>,
    /// Number of entries overwritten by a later duplicate key.
    pub duplicate_keys: usize,
}

impl LocalizationFile {
    /// Parses `bytes`; every problem is recorded on the returned value.
    pub fn parse(relative: &str, stem: &str, language: &str, bytes: &[u8]) -> Self {
        let encoding = encoding::detect(bytes);
        let mut file = Self {
            relative: relative.to_owned(),
            stem: stem.to_owned(),
            language: language.to_ascii_lowercase(),
            encoding,
            decode_error: None,
            sections: BTreeMap::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            duplicate_keys: 0,
        };
        let text = match encoding::decode(bytes) {
            Ok((_, text)) => text,
            Err(e) => {
                file.decode_error = Some(e);
                return file;
            }
        };

        let mut current: Option<String> = None;
        for (idx, raw) in text.split('\n').enumerate() {
            let line_no = idx + 1;
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with(';') {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix('[') {
                match rest.find(']') {
                    Some(end) => {
                        let name = rest[..end].trim();
                        if name.is_empty() {
                            file.errors.push(ParseError {
                                line: line_no,
                                kind: ParseErrorKind::UnclosedSection,
                                text: line.to_owned(),
                            });
                            current = None;
                        } else {
                            let key = name.to_ascii_lowercase();
                            current = Some(key.clone());
                            file.sections
                                .entry(key)
                                .or_insert_with(|| Section::new(name));
                        }
                    }
                    None => {
                        file.errors.push(ParseError {
                            line: line_no,
                            kind: ParseErrorKind::UnclosedSection,
                            text: line.to_owned(),
                        });
                        current = None;
                    }
                }
                continue;
            }

            let (key, value) = match line.split_once('=') {
                Some((k, v)) => (
                    k.trim().to_owned(),
                    parse_value(v, line_no, &mut file.warnings),
                ),
                None => {
                    file.warnings.push(ParseWarning {
                        line: line_no,
                        kind: ParseWarningKind::MissingSeparator,
                        text: line.to_owned(),
                    });
                    (trimmed.to_owned(), String::new())
                }
            };
            if key.is_empty() {
                file.errors.push(ParseError {
                    line: line_no,
                    kind: ParseErrorKind::EmptyKey,
                    text: line.to_owned(),
                });
                continue;
            }
            let section_key = match &current {
                Some(s) => s.clone(),
                None => {
                    file.warnings.push(ParseWarning {
                        line: line_no,
                        kind: ParseWarningKind::KeyOutsideSection,
                        text: line.to_owned(),
                    });
                    String::new()
                }
            };
            file.sections
                .entry(section_key.clone())
                .or_insert_with(|| Section::new(""));
            let entry_key = key.to_ascii_lowercase();
            let duplicate = file
                .sections
                .get(&section_key)
                .is_some_and(|s| s.entries.contains_key(&entry_key));
            if duplicate {
                file.duplicate_keys += 1;
            }
            if let Some(section) = file.sections.get_mut(&section_key) {
                section.entries.insert(
                    entry_key,
                    Entry {
                        key,
                        value,
                        line: line_no,
                    },
                );
            }
        }
        file
    }

    /// Case-insensitive section lookup.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.get(&name.to_ascii_lowercase())
    }

    /// Case-insensitive lookup of `key` inside `section`.
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?.get(key).map(|e| e.value.as_str())
    }

    /// Sections in case-insensitive name order (duplicates merged).
    pub fn sections(&self) -> impl Iterator<Item = &Section> {
        self.sections.values()
    }

    pub fn section_count(&self) -> usize {
        self.sections.len()
    }

    /// Distinct section+key entries in the file.
    pub fn entry_count(&self) -> usize {
        self.sections.values().map(Section::len).sum()
    }

    /// True when the file decoded and has no hard line errors.
    pub fn is_clean(&self) -> bool {
        self.decode_error.is_none() && self.errors.is_empty()
    }
}

/// Applies the value rules: skip leading spaces/tabs, unquote one outer pair,
/// keep the rest verbatim (including trailing spaces and embedded quotes).
fn parse_value(raw: &str, line_no: usize, warnings: &mut Vec<ParseWarning>) -> String {
    let value = raw.trim_start_matches([' ', '\t']);
    if let Some(rest) = value.strip_prefix('"') {
        if let Some(inner) = rest.strip_suffix('"') {
            inner.to_owned()
        } else {
            warnings.push(ParseWarning {
                line: line_no,
                kind: ParseWarningKind::UnterminatedQuote,
                text: raw.to_owned(),
            });
            rest.to_owned()
        }
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> LocalizationFile {
        LocalizationFile::parse("Test.int", "Test", "int", text.as_bytes())
    }

    #[test]
    fn parses_sections_and_keys_case_insensitively() {
        let f = parse("[General]\r\nTitle=Hello\r\n[Other]\r\nTitle=World\r\n");
        assert_eq!(f.section_count(), 2);
        assert_eq!(f.get("general", "title"), Some("Hello"));
        assert_eq!(f.get("GENERAL", "TITLE"), Some("Hello"));
        assert_eq!(f.get("other", "title"), Some("World"));
        assert!(f.is_clean());
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let f = parse("; a comment\r\n\r\n[Sec]\r\n; another\r\nKey=Value\r\n");
        assert_eq!(f.get("Sec", "Key"), Some("Value"));
        assert_eq!(f.entry_count(), 1);
        assert!(f.warnings.is_empty());
    }

    #[test]
    fn unquotes_outer_pair_and_preserves_inner_quotes() {
        let f = parse("[S]\r\nA=\"quoted value\"\r\nB=(caption=\"x\",y=1)\r\nC=\"a\"b\"\r\n");
        assert_eq!(f.get("S", "A"), Some("quoted value"));
        assert_eq!(f.get("S", "B"), Some("(caption=\"x\",y=1)"));
        assert_eq!(f.get("S", "C"), Some("a\"b"));
    }

    #[test]
    fn preserves_trailing_spaces_and_trims_leading() {
        let f = parse("[S]\r\nA=trailing \r\nB= leading\r\n");
        assert_eq!(f.get("S", "A"), Some("trailing "));
        assert_eq!(f.get("S", "B"), Some("leading"));
    }

    #[test]
    fn keeps_backslash_escapes_verbatim() {
        let f = parse("[S]\r\nA=line1\\nline2\r\n");
        assert_eq!(f.get("S", "A"), Some("line1\\nline2"));
    }

    #[test]
    fn repeated_sections_merge_and_last_duplicate_wins() {
        let f = parse("[S]\r\nK=first\r\n[T]\r\nX=1\r\n[S]\r\nK=second\r\nY=2\r\n");
        assert_eq!(f.section_count(), 2);
        assert_eq!(f.get("S", "K"), Some("second"));
        assert_eq!(f.get("S", "Y"), Some("2"));
        assert_eq!(f.get("S", "X"), None);
        assert_eq!(f.duplicate_keys, 1);
    }

    #[test]
    fn missing_separator_is_a_warning_kept_as_empty_entry() {
        let f = parse("[S]\r\nKey1=Value\r\nBogus line\r\nKey2=Ok\r\n");
        assert!(f.errors.is_empty());
        assert_eq!(f.warnings.len(), 1);
        assert_eq!(f.warnings[0].kind, ParseWarningKind::MissingSeparator);
        assert_eq!(f.warnings[0].line, 3);
        assert_eq!(f.get("S", "Bogus line"), Some(""));
        assert_eq!(f.get("S", "Key2"), Some("Ok"));
    }

    #[test]
    fn unterminated_quote_is_a_warning_and_keeps_value() {
        let f = parse("[S]\r\nA=\"oops\r\n");
        assert!(f.errors.is_empty());
        assert_eq!(f.warnings.len(), 1);
        assert_eq!(f.warnings[0].kind, ParseWarningKind::UnterminatedQuote);
        assert_eq!(f.get("S", "A"), Some("oops"));
    }

    #[test]
    fn unclosed_section_and_empty_key_are_errors_with_line_numbers() {
        let f = parse("[S]\r\n=value\r\nK=V\r\n[Unclosed\r\n");
        let kinds: Vec<_> = f.errors.iter().map(|e| (e.line, e.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                (2, ParseErrorKind::EmptyKey),
                (4, ParseErrorKind::UnclosedSection)
            ]
        );
        assert!(!f.is_clean());
        assert_eq!(f.get("S", "K"), Some("V"));
    }

    #[test]
    fn keys_before_a_section_are_warned_not_dropped() {
        let f = parse("Loose=1\r\n[S]\r\nK=V\r\n");
        assert_eq!(f.warnings.len(), 1);
        assert_eq!(f.warnings[0].kind, ParseWarningKind::KeyOutsideSection);
        assert_eq!(f.get("", "Loose"), Some("1"));
    }

    #[test]
    fn lone_cr_inside_a_value_is_preserved() {
        let f = parse("[S]\r\nA=before\rafter\r\n");
        assert_eq!(f.get("S", "A"), Some("before\rafter"));
    }

    #[test]
    fn empty_input_is_a_clean_empty_file() {
        let f = parse("");
        assert!(f.is_clean());
        assert_eq!(f.section_count(), 0);
        assert_eq!(f.entry_count(), 0);
        assert_eq!(f.get("Any", "Key"), None);
    }

    #[test]
    fn section_header_ignores_trailing_text() {
        let f = parse("[Sec] trailing\r\nK=V\r\n");
        assert_eq!(f.get("Sec", "K"), Some("V"));
        assert!(f.is_clean());
    }

    #[test]
    fn utf16_bom_is_decoded() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "[S]\r\nK=V\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let f = LocalizationFile::parse("X.int", "X", "int", &bytes);
        assert_eq!(f.encoding, Encoding::Utf16Le);
        assert_eq!(f.get("S", "K"), Some("V"));
        assert!(f.is_clean());
    }

    #[test]
    fn undecodable_file_reports_a_decode_error() {
        let f = LocalizationFile::parse("X.int", "X", "int", &[0xFF, 0xFE, 0x00]);
        assert!(f.decode_error.is_some());
        assert_eq!(f.entry_count(), 0);
        assert!(!f.is_clean());
    }
}
