//! Loader over [`xiii_install::Installation`] and the `Localize` lookup API.
//!
//! The installation's bounded inventory is scanned for files whose extension is
//! a known UE2 language code (`.int`, `.frt`, `.det`, `.est`, `.itt`, ...).
//! Every file is parsed immediately; the active language comes from
//! `[Engine.Engine] Language=` in `Default.ini` (then `XIII.ini`), defaulting to
//! `int`. `get` looks in the active-language file first and falls back to
//! `int`, which is the shipped English/`international` file.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;

use xiii_install::Installation;

use crate::parse::{LocalizationFile, ParseError, ParseWarning};

/// UE2 localisation file extensions (lowercase). `int` is the fallback.
pub const LANGUAGE_CODES: &[&str] = &[
    "int", "frt", "det", "est", "esp", "esn", "itt", "ptg", "ptb", "nld", "swe", "nor", "fin",
    "dan", "pol", "rus", "cze", "hun", "trk", "kor", "jpn", "chs", "cht", "tha",
];

/// The fallback language used when the active file lacks a key.
pub const FALLBACK_LANGUAGE: &str = "int";

/// True for a known UE2 language extension (case-insensitive).
pub fn is_language_code(code: &str) -> bool {
    LANGUAGE_CODES.iter().any(|c| c.eq_ignore_ascii_case(code))
}

/// A file that was listed in the inventory but could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoError {
    pub relative: String,
    pub message: String,
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.relative, self.message)
    }
}

fn file_name(relative: &str) -> &str {
    match relative.rfind('/') {
        Some(i) => &relative[i + 1..],
        None => relative,
    }
}

fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.')
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty())
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// Reads `[Engine.Engine] Language=` from `Default.ini`, then `XIII.ini`,
/// wherever the inventory found them. Returns the lowercased value.
pub fn detect_language(inst: &Installation) -> Option<String> {
    let mut candidates: Vec<(u8, &str)> = Vec::new();
    for f in &inst.inventory().files {
        let name = f.file_name();
        let priority = if name.eq_ignore_ascii_case("default.ini") {
            0
        } else if name.eq_ignore_ascii_case("xiii.ini") {
            1
        } else {
            continue;
        };
        candidates.push((priority, f.relative.as_str()));
    }
    candidates.sort();
    for (_, relative) in candidates {
        let Ok(bytes) = fs::read(inst.root().join(relative)) else {
            continue;
        };
        let ini = LocalizationFile::parse(relative, stem(file_name(relative)), "ini", &bytes);
        if let Some(value) = ini.get("Engine.Engine", "Language") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_ascii_lowercase());
            }
        }
    }
    None
}

/// All localisation files of one installation, indexed by
/// (lowercase stem, lowercase language).
#[derive(Debug, Clone)]
pub struct Localizer {
    language: String,
    files: BTreeMap<(String, String), LocalizationFile>,
    io_errors: Vec<IoError>,
}

impl Localizer {
    /// An empty localizer for `language`; primarily for tests and callers that
    /// assemble files themselves.
    pub fn new(language: impl Into<String>) -> Self {
        Self {
            language: language.into().to_ascii_lowercase(),
            files: BTreeMap::new(),
            io_errors: Vec::new(),
        }
    }

    /// Opens and parses every localisation file of `inst`.
    pub fn from_installation(inst: &Installation) -> Self {
        let language = detect_language(inst).unwrap_or_else(|| FALLBACK_LANGUAGE.to_owned());
        let mut localizer = Self::new(language);
        for f in &inst.inventory().files {
            let name = f.file_name();
            let Some(ext) = extension(name) else {
                continue;
            };
            let ext = ext.to_ascii_lowercase();
            if !is_language_code(&ext) {
                continue;
            }
            match fs::read(inst.root().join(&f.relative)) {
                Ok(bytes) => {
                    localizer.add(stem(name), &ext, &f.relative, &bytes);
                }
                Err(e) => localizer.io_errors.push(IoError {
                    relative: f.relative.clone(),
                    message: e.to_string(),
                }),
            }
        }
        localizer
    }

    /// Parses and stores one file. Later files with the same (stem, language)
    /// replace earlier ones; callers control precedence.
    pub fn add(&mut self, stem: &str, language: &str, relative: &str, bytes: &[u8]) {
        let file = LocalizationFile::parse(relative, stem, language, bytes);
        self.files.insert(
            (stem.to_ascii_lowercase(), language.to_ascii_lowercase()),
            file,
        );
    }

    /// Active language (lowercase), from the install's ini or the default.
    pub fn language(&self) -> &str {
        &self.language
    }

    /// All parsed files, ordered by (stem, language).
    pub fn files(&self) -> impl Iterator<Item = &LocalizationFile> {
        self.files.values()
    }

    /// One file by package stem and language, case-insensitively.
    pub fn file(&self, package: &str, language: &str) -> Option<&LocalizationFile> {
        self.files
            .get(&(package.to_ascii_lowercase(), language.to_ascii_lowercase()))
    }

    /// Files that could not be read.
    pub fn io_errors(&self) -> &[IoError] {
        &self.io_errors
    }

    /// `Localize(Section, Key, Package)` in the active language, with fallback
    /// to `int`.
    pub fn get(&self, package: &str, section: &str, key: &str) -> Option<&str> {
        self.get_for_language(package, section, key, &self.language)
    }

    /// As [`Self::get`] but for an explicit language (falling back to `int`).
    pub fn get_for_language(
        &self,
        package: &str,
        section: &str,
        key: &str,
        language: &str,
    ) -> Option<&str> {
        let package = package.to_ascii_lowercase();
        let language = language.to_ascii_lowercase();
        if let Some(value) = self
            .files
            .get(&(package.clone(), language.clone()))
            .and_then(|f| f.get(section, key))
        {
            return Some(value);
        }
        if language != FALLBACK_LANGUAGE
            && let Some(value) = self
                .files
                .get(&(package, FALLBACK_LANGUAGE.to_owned()))
                .and_then(|f| f.get(section, key))
        {
            return Some(value);
        }
        None
    }

    /// A `localized` class default: section = class name, key = property name,
    /// file = the class's package. `index` selects an array element
    /// (`Property[0]`); pass `None` for a scalar property.
    pub fn class_property(
        &self,
        package: &str,
        class: &str,
        property: &str,
        index: Option<usize>,
    ) -> Option<&str> {
        match index {
            Some(i) => self.get(package, class, &format!("{property}[{i}]")),
            None => self.get(package, class, property),
        }
    }

    /// Every hard parse error in every file.
    pub fn parse_errors(&self) -> impl Iterator<Item = (&LocalizationFile, &ParseError)> {
        self.files
            .values()
            .flat_map(|f| f.errors.iter().map(move |e| (f, e)))
    }

    /// Every tolerated parse warning in every file.
    pub fn parse_warnings(&self) -> impl Iterator<Item = (&LocalizationFile, &ParseWarning)> {
        self.files
            .values()
            .flat_map(|f| f.warnings.iter().map(move |w| (f, w)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN: &[u8] =
        b"[Menu]\r\nTitle=Start\r\nOnlyEnglish=Yes\r\nItems[0]=One\r\nItems[1]=Two\r\n";
    const FR: &[u8] = b"[Menu]\r\nTitle=Commencer\r\n";

    fn sample() -> Localizer {
        let mut l = Localizer::new("frt");
        l.add("Main", "int", "Main.int", EN);
        l.add("Main", "frt", "Main.frt", FR);
        l
    }

    #[test]
    fn active_language_wins_with_int_fallback() {
        let l = sample();
        assert_eq!(l.get("Main", "Menu", "Title"), Some("Commencer"));
        assert_eq!(l.get("Main", "Menu", "OnlyEnglish"), Some("Yes"));
        assert_eq!(l.get("main", "menu", "title"), Some("Commencer"));
    }

    #[test]
    fn explicit_language_and_missing_package() {
        let l = sample();
        assert_eq!(
            l.get_for_language("Main", "Menu", "Title", "int"),
            Some("Start")
        );
        assert_eq!(l.get("Nope", "Menu", "Title"), None);
        assert_eq!(l.get("Main", "Nope", "Title"), None);
    }

    #[test]
    fn class_property_scalar_and_array() {
        let l = sample();
        assert_eq!(
            l.class_property("Main", "Menu", "Title", None),
            Some("Commencer")
        );
        assert_eq!(
            l.class_property("Main", "Menu", "Items", Some(0)),
            Some("One")
        );
        assert_eq!(
            l.class_property("Main", "Menu", "Items", Some(1)),
            Some("Two")
        );
        assert_eq!(l.class_property("Main", "Menu", "Items", Some(2)), None);
    }

    #[test]
    fn extension_helpers() {
        assert_eq!(extension("Plage01.int"), Some("int"));
        assert_eq!(extension("noext"), None);
        assert_eq!(stem("xidmaps.FRT"), "xidmaps");
        assert!(is_language_code("FRT"));
        assert!(!is_language_code("dll"));
    }
}
