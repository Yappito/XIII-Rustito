//! Sound name resolution: `Sound` object leaf -> HX bank entry -> decoded PCM.
//!
//! This is the thin loader the runtime uses to connect `xiii-script` `PresentationEvent`s to
//! the HX banks parsed by [`crate::hx`]. It is deliberately the same resolution rule the
//! `xiii-tool audio link` spike established (see `local/reports/item6-audio-spike.md`):
//!
//! * a `Sound` object's **leaf name** (after the last `.`) is matched case-insensitively
//!   against a `CPCWavResData` internal name, which already links onwards to a `*WaveFileIdObj`;
//! * localized dialogue resources prefer the English variant (handled inside
//!   [`crate::hx::WavRes::wave_ids`] while parsing);
//! * when several banks contain the same name, the first bank in sorted-by-path order wins
//!   (the item6 trace reports the first alphabetical match). Preferring the map's own bank is
//!   a **hypothesis** that was not established by the spike and is *not* applied here.
//!
//! Unlike the rest of the crate (which is filesystem-free), [`SoundLibrary::scan`] reads the
//! installation read-only to discover `.hxc` banks; the pure [`SoundLibrary::from_banks`]
//! constructor exists so tests never touch the filesystem. All counts of parsed/failed banks
//! and unresolved names are reported (never silent).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::AudioErrorKind;
use crate::hx::{Codec, DataLocation, HxLimits, WaveResource};
use crate::{PcmAudio, decode_entry, hx};

/// One named bank entry: a `WavRes` name resolved to its wave record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankEntryRef {
    /// Bank file path (as scanned).
    pub bank: PathBuf,
    /// Zero-based index of the wave entry in the bank.
    pub entry: usize,
    /// Wave codec.
    pub codec: Codec,
    /// Channel count.
    pub channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// True when the samples live in the sibling `.hsc` stream.
    pub external: bool,
}

/// Why a sound could not be resolved or decoded. Counted (never dropped silently).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveFailure {
    /// The event carried no `Sound` object path (null or unresolved reference).
    NoSoundName,
    /// A `Sound` path was present but no bank `WavRes` name matched its leaf.
    NoNameMatch,
    /// The referenced bank entry exists but decoding failed.
    DecodeFailed,
    /// The bank entry is external and its `.hsc` stream was not found next to the bank.
    StreamMissing,
}

impl ResolveFailure {
    /// Short stable label for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            ResolveFailure::NoSoundName => "no_sound_name",
            ResolveFailure::NoNameMatch => "no_name_match",
            ResolveFailure::DecodeFailed => "decode_failed",
            ResolveFailure::StreamMissing => "stream_missing",
        }
    }
}

/// Statistics for the resolution library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryStats {
    /// `.hxc` banks discovered.
    pub banks_seen: usize,
    /// Banks parsed without error.
    pub banks_parsed: usize,
    /// Banks that failed to parse or read.
    pub banks_failed: usize,
    /// Distinct sound names in the index.
    pub names: usize,
}

/// The name -> bank-entry index plus a decoded-PCM cache.
pub struct SoundLibrary {
    names: BTreeMap<String, Vec<BankEntryRef>>,
    /// Decoded PCM keyed by `(bank path string, entry)`; failures cached too.
    cache: BTreeMap<(String, usize), std::result::Result<Arc<PcmAudio>, ResolveFailure>>,
    stats: LibraryStats,
}

impl SoundLibrary {
    /// Builds an empty library.
    pub fn empty() -> Self {
        Self {
            names: BTreeMap::new(),
            cache: BTreeMap::new(),
            stats: LibraryStats::default(),
        }
    }

    /// Builds a library from in-memory banks (`label`, bytes). Parsed names are indexed; parse
    /// failures are counted. No filesystem access: used by unit tests.
    pub fn from_banks<I>(banks: I) -> Self
    where
        I: IntoIterator<Item = (String, Vec<u8>)>,
    {
        let mut lib = Self::empty();
        for (label, bytes) in banks {
            lib.stats.banks_seen += 1;
            match hx::parse_bank(&bytes, &HxLimits::default()) {
                Ok(bank) => {
                    lib.stats.banks_parsed += 1;
                    let path = PathBuf::from(label);
                    lib.index_bank(&path, &bank);
                }
                Err(_) => lib.stats.banks_failed += 1,
            }
        }
        lib.finish_index()
    }

    /// Scans `<root>` for `.hxc` banks and indexes every named wave. Unreadable/ malformed
    /// banks are counted, not fatal.
    pub fn scan(root: &Path) -> Self {
        let mut banks = Vec::new();
        walk_hxc(root, &mut banks);
        banks.sort();
        let mut lib = Self::empty();
        for path in &banks {
            lib.stats.banks_seen += 1;
            let Ok(bytes) = std::fs::read(path) else {
                lib.stats.banks_failed += 1;
                continue;
            };
            match hx::parse_bank(&bytes, &HxLimits::default()) {
                Ok(bank) => {
                    lib.stats.banks_parsed += 1;
                    lib.index_bank(path, &bank);
                }
                Err(_) => lib.stats.banks_failed += 1,
            }
        }
        lib.finish_index()
    }

    /// Indexes every named wave of `bank` under its lowercased `WavRes` name.
    fn index_bank(&mut self, path: &Path, bank: &crate::hx::HxBank) {
        for entry in &bank.entries {
            let Some(wave) = entry.as_wave() else {
                continue;
            };
            let Some(name) = &wave.name else {
                continue;
            };
            self.names
                .entry(name.to_lowercase())
                .or_default()
                .push(BankEntryRef {
                    bank: path.to_path_buf(),
                    entry: entry.index,
                    codec: wave.codec,
                    channels: wave.channels,
                    sample_rate: wave.sample_rate,
                    external: matches!(wave.data, DataLocation::External { .. }),
                });
        }
    }

    fn finish_index(mut self) -> Self {
        self.stats.names = self.names.len();
        self
    }

    /// Library statistics (bank/name counts).
    pub fn stats(&self) -> LibraryStats {
        self.stats
    }

    /// Every distinct name in the index, sorted (for diagnostics/tests).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.keys().map(String::as_str)
    }

    /// The first bank entry for a name (case-insensitive), or `None`.
    pub fn resolve_name(&self, name: &str) -> Option<&BankEntryRef> {
        self.names.get(&name.to_lowercase()).and_then(|v| v.first())
    }

    /// Resolves a `Sound` object path's leaf to its bank entry. `None` when the path has no
    /// leaf or no bank name matches.
    pub fn resolve_path(&self, sound_path: &str) -> Option<&BankEntryRef> {
        leaf(sound_path).and_then(|leaf| self.resolve_name(leaf))
    }

    /// Loads (and caches) the decoded PCM of a resolved bank entry. External `.hsc` streams are
    /// read from the bank's directory; a missing stream is [`ResolveFailure::StreamMissing`].
    pub fn load(&mut self, r: &BankEntryRef) -> std::result::Result<Arc<PcmAudio>, ResolveFailure> {
        let key = (r.bank.to_string_lossy().to_ascii_lowercase(), r.entry);
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        let result = self.load_uncached(r);
        self.cache.insert(key, result.clone());
        result
    }

    fn load_uncached(
        &self,
        r: &BankEntryRef,
    ) -> std::result::Result<Arc<PcmAudio>, ResolveFailure> {
        let bytes = std::fs::read(&r.bank).map_err(|_| ResolveFailure::DecodeFailed)?;
        let bank = hx::parse_bank(&bytes, &HxLimits::default())
            .map_err(|_| ResolveFailure::DecodeFailed)?;
        let wave = bank
            .entries
            .get(r.entry)
            .and_then(|e| e.as_wave())
            .ok_or(ResolveFailure::DecodeFailed)?;
        let stream = self.read_stream(&r.bank, wave);
        if matches!(wave.data, DataLocation::External { .. }) && stream.is_none() {
            return Err(ResolveFailure::StreamMissing);
        }
        decode_entry(&bank, r.entry, &bytes, stream.as_deref())
            .map(Arc::new)
            .map_err(|e| match e.kind() {
                AudioErrorKind::ExternalData => ResolveFailure::StreamMissing,
                _ => ResolveFailure::DecodeFailed,
            })
    }

    /// Reads the sibling stream file named by `wave.resource_name`, if any.
    fn read_stream(&self, bank_path: &Path, wave: &WaveResource) -> Option<Vec<u8>> {
        let resource = wave.resource_name.as_ref()?;
        let name = resource_file(resource);
        let dir = bank_path.parent().unwrap_or(Path::new("."));
        let path = find_case_insensitive(dir, &name)?;
        std::fs::read(path).ok()
    }
}

impl Default for SoundLibrary {
    fn default() -> Self {
        Self::empty()
    }
}

/// Leaf name of an object path (after the last `.`).
pub fn leaf(path: &str) -> Option<&str> {
    path.rsplit('.').next().filter(|s| !s.is_empty())
}

/// Extracts the file name from a stored resource path such as `.\\Plage00.hsc`.
fn resource_file(resource: &str) -> String {
    resource
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(resource)
        .to_owned()
}

fn find_case_insensitive(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
        {
            return Some(path);
        }
    }
    None
}

fn walk_hxc(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_hxc(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("hxc"))
        {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod library_tests {
    use super::*;

    fn u32b(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    fn u16b(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }

    /// A one-wave internal-PCM bank with a `WavRes` name (`sound_name`).
    fn synth_bank(sound_name: &str) -> Vec<u8> {
        let base = 4usize;
        let mut body = Vec::new();

        // Wave record: internal PCM16.
        let wave_body = body.len();
        body.extend_from_slice(&u32b(16));
        body.extend_from_slice(b"CPCWaveFileIdObj");
        body.extend_from_slice(&u32b(0x1111_1111));
        body.extend_from_slice(&u32b(0x2222_2222));
        body.extend_from_slice(&u32b(3)); // flag type
        body.extend_from_slice(&u32b(0)); // parent id
        body.push(0); // stream mode internal
        body.extend_from_slice(b"RIFF");
        body.extend_from_slice(&u32b(40));
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&u32b(16));
        body.extend_from_slice(&u16b(1)); // PCM
        body.extend_from_slice(&u16b(1)); // mono
        body.extend_from_slice(&u32b(8000));
        body.extend_from_slice(&u32b(16_000));
        body.extend_from_slice(&u16b(2));
        body.extend_from_slice(&u16b(16));
        body.extend_from_slice(b"data");
        body.extend_from_slice(&u32b(4));
        body.extend_from_slice(&10_000i16.to_le_bytes());
        body.extend_from_slice(&(-10_000i16).to_le_bytes());
        let wave_size = body.len() - wave_body;

        // WavRes record naming the wave.
        let wavres_body = body.len();
        body.extend_from_slice(&u32b(13));
        body.extend_from_slice(b"CPCWavResData");
        body.extend_from_slice(&u32b(0x3333_3333));
        body.extend_from_slice(&u32b(0x4444_4444));
        body.extend_from_slice(&u32b(0));
        body.extend_from_slice(&u32b(sound_name.len() as u32));
        body.extend_from_slice(sound_name.as_bytes());
        let wavres_size = body.len() - wavres_body;

        let wave_off = base + wave_body;
        let wavres_off = base + wavres_body;
        let index_off = base + body.len();
        let mut index = Vec::new();
        index.extend_from_slice(b"INDX");
        index.extend_from_slice(&u32b(2));
        index.extend_from_slice(&u32b(2));
        // Entry 0: wave.
        index.extend_from_slice(&u32b(16));
        index.extend_from_slice(b"CPCWaveFileIdObj");
        index.extend_from_slice(&u32b(0x1111_1111));
        index.extend_from_slice(&u32b(0x2222_2222));
        index.extend_from_slice(&u32b(wave_off as u32));
        index.extend_from_slice(&u32b(wave_size as u32));
        index.extend_from_slice(&u32b(0));
        index.extend_from_slice(&u32b(0));
        index.extend_from_slice(&u32b(0));
        // Entry 1: WavRes linking to the wave.
        index.extend_from_slice(&u32b(13));
        index.extend_from_slice(b"CPCWavResData");
        index.extend_from_slice(&u32b(0x3333_3333));
        index.extend_from_slice(&u32b(0x4444_4444));
        index.extend_from_slice(&u32b(wavres_off as u32));
        index.extend_from_slice(&u32b(wavres_size as u32));
        index.extend_from_slice(&u32b(0));
        index.extend_from_slice(&u32b(1)); // one link
        index.extend_from_slice(&u32b(0x1111_1111));
        index.extend_from_slice(&u32b(0x2222_2222));
        index.extend_from_slice(&u32b(0));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32b(index_off as u32));
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&index);
        bytes
    }

    #[test]
    fn leaf_takes_last_path_component() {
        assert_eq!(leaf("XIIIsound.Footsteps.FtSkMar1"), Some("FtSkMar1"));
        assert_eq!(leaf("FtSkMar1"), Some("FtSkMar1"));
        assert_eq!(leaf(""), None);
        assert_eq!(leaf("XIIIsound."), None);
    }

    #[test]
    fn indexes_names_case_insensitively() {
        let lib = SoundLibrary::from_banks([("Test.hxc".to_owned(), synth_bank("TestSound"))]);
        let s = lib.stats();
        assert_eq!(s.banks_seen, 1);
        assert_eq!(s.banks_parsed, 1);
        assert_eq!(s.banks_failed, 0);
        assert_eq!(s.names, 1);

        let r = lib
            .resolve_name("testsound")
            .expect("case-insensitive match");
        assert_eq!(r.entry, 0, "wave entry index");
        assert_eq!(r.codec, Codec::Pcm);
        assert_eq!(r.sample_rate, 8000);
        assert!(!r.external);
        // Path resolution strips the object path; a null/garbage path does not.
        assert!(lib.resolve_path("XIIIsound.X.TestSound").is_some());
        assert!(lib.resolve_path("XIIIsound.X.NotHere").is_none());
        assert!(lib.resolve_name("").is_none());
    }

    #[test]
    fn counts_malformed_banks() {
        let lib = SoundLibrary::from_banks([
            ("Good.hxc".to_owned(), synth_bank("A")),
            ("Bad.hxc".to_owned(), b"not a bank".to_vec()),
        ]);
        let s = lib.stats();
        assert_eq!(s.banks_seen, 2);
        assert_eq!(s.banks_parsed, 1);
        assert_eq!(s.banks_failed, 1);
    }

    #[test]
    fn load_fails_without_a_real_bank_file() {
        // Resolution is filesystem-free from memory, but decoding needs the bank bytes; a
        // missing file is a counted DecodeFailed, never a panic.
        let mut lib = SoundLibrary::from_banks([("Absent.hxc".to_owned(), synth_bank("Ghost"))]);
        let r = lib.resolve_name("Ghost").expect("indexed").clone();
        assert!(matches!(lib.load(&r), Err(ResolveFailure::DecodeFailed)));
        // The failure is cached (second call returns the same without touching disk again).
        assert!(matches!(lib.load(&r), Err(ResolveFailure::DecodeFailed)));
    }
}
