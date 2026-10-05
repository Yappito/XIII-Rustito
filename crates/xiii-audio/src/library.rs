//! Sound name/resource resolution: `Sound` object -> HX bank entry -> decoded PCM.
//!
//! This is the loader the runtime uses to connect `xiii-script` `PresentationEvent`s to the HX
//! banks parsed by [`crate::hx`]. Two rules are applied, in order:
//!
//! 1. **Resource reference (item6c, primary):** an `Engine.Sound` export's 24-byte native tail
//!    stores an HX identity pair (`Cuuid`) as a `XXXXXXXX-XXXX-XXXX` string
//!    ([`crate::hx::SoundRef`]). That pair names either a `CPCWavResData` (a wave resource
//!    directly) or a `CProgramResData`/random/switch event whose index links lead, through the
//!    bank's own class structure, to a `*WaveFileIdObj`. The link was established by the item6c
//!    investigation (measured; see `local/reports/item6c-sound-resolution.md`). When an event
//!    reaches several wave resources, one is chosen deterministically from a seed derived from
//!    the requested path; the candidate count and choice are reported (never silent).
//! 2. **Name fallback (item6, kept):** the object path's **leaf name** (after the last `.`) is
//!    matched case-insensitively against a `CPCWavResData` internal name. When several banks
//!    contain the same name the first in sorted-by-path order wins.
//!
//! Localized dialogue resources prefer the English variant (handled inside
//! [`crate::hx::WavRes::wave_ids`] while parsing).
//!
//! Unlike the rest of the crate (which is filesystem-free), [`SoundLibrary::scan`] reads the
//! installation read-only to discover `.hxc` banks and `.uax` sound packages; the pure
//! [`SoundLibrary::from_banks`] constructor exists so tests never touch the filesystem. All
//! counts of parsed/failed banks and unresolved names are reported (never silent).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::AudioErrorKind;
use crate::hx::{Codec, Cuuid, DataLocation, HxKind, HxLimits, SoundRef, WaveResource};
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

/// Which rule resolved a sound path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionRule {
    /// The `Sound` native tail's HX resource reference (item6c).
    ResourceRef,
    /// The object path's leaf matched a bank `WavRes` name (item6).
    NameMatch,
}

impl ResolutionRule {
    /// Short stable label for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            ResolutionRule::ResourceRef => "resource_ref",
            ResolutionRule::NameMatch => "name_match",
        }
    }
}

/// A resolved sound: the chosen bank entry plus how the choice was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSound {
    /// The wave entry to decode.
    pub entry: BankEntryRef,
    /// Which rule was used.
    pub rule: ResolutionRule,
    /// Number of distinct wave candidates the resource graph produced (1 for the name rule).
    pub candidates: usize,
    /// Zero-based index of the chosen candidate among [`Self::candidates`].
    pub chosen: usize,
    /// Deterministic seed used for a multi-candidate choice.
    pub seed: u64,
}

/// Why a sound could not be resolved or decoded. Counted (never dropped silently).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveFailure {
    /// The event carried no `Sound` object path (null or unresolved reference).
    NoSoundName,
    /// A `Sound` path was present but neither a resource reference nor a name matched.
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

/// Corpus-wide resolution counts over every indexed `Sound` object path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolutionSummary {
    /// `Sound` object paths indexed.
    pub sounds: usize,
    /// Paths resolved by either rule.
    pub resolved: usize,
    /// Resolved by the item6c resource reference.
    pub resource_ref: usize,
    /// Resolved only by the item6 leaf-name fallback.
    pub name_match: usize,
    /// Resolved by the item6 leaf-name rule alone (the before count).
    pub name_rule_only: usize,
    /// Resource pair indexed but no wave reachable through its links.
    pub resource_no_wave: usize,
    /// The `Sound`'s resource pair is not present in any index entry.
    pub resource_missing: usize,
    /// The `Sound` had no native-tail resource reference at all.
    pub no_sound_ref: usize,
    /// Unresolved by both rules.
    pub failed: usize,
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
    /// Distinct sound names in the index (name rule).
    pub names: usize,
    /// `.uax` sound packages discovered.
    pub uax_seen: usize,
    /// `.uax` packages parsed without error.
    pub uax_parsed: usize,
    /// `Engine.Sound` exports indexed.
    pub sound_exports: usize,
    /// `Engine.Sound` exports whose native tail named an HX resource.
    pub sound_refs: usize,
    /// Distinct HX resource pairs indexed from the banks.
    pub resources: usize,
}

/// A location of one resource pair inside a bank: `(bank path, entry index)`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Location {
    bank: PathBuf,
    entry: usize,
}

/// Outgoing links of a resource entry, with the resolution semantics of its class.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OutLinks {
    /// A `CPCWavResData`: the pairs are wave references in preference order (direct links, then
    /// the English localized variant, then the others). The **first** that reaches a wave is the
    /// one the engine would play for the English install, so it is taken without randomizing.
    Wave(Vec<Cuuid>),
    /// A program/random/switch/event: every linked pair is a candidate resource; when more than
    /// one distinct wave results, one is chosen deterministically.
    Program(Vec<Cuuid>),
}

/// The name/resource index plus a decoded-PCM cache.
pub struct SoundLibrary {
    names: BTreeMap<String, Vec<BankEntryRef>>,
    /// Every wave entry by `(bank path string, entry)`.
    entries: BTreeMap<(String, usize), BankEntryRef>,
    /// Resource pair -> the entries carrying it, in sorted-by-path order.
    by_cuuid: BTreeMap<Cuuid, Vec<Location>>,
    /// Outgoing resource pairs of a program/wavres entry, keyed like `entries`.
    links: BTreeMap<(String, usize), OutLinks>,
    /// `Sound` object path (lowercased full path and leaf) -> HX resource pair.
    sounds: BTreeMap<String, Cuuid>,
    /// Distinct full `Sound` object paths (`<package stem>.<relative path>`) -> resource pair.
    sound_full: BTreeMap<String, Cuuid>,
    /// Decoded PCM keyed by `(bank path string, entry)`; failures cached too.
    cache: BTreeMap<(String, usize), std::result::Result<Arc<PcmAudio>, ResolveFailure>>,
    stats: LibraryStats,
}

impl SoundLibrary {
    /// Builds an empty library.
    pub fn empty() -> Self {
        Self {
            names: BTreeMap::new(),
            entries: BTreeMap::new(),
            by_cuuid: BTreeMap::new(),
            links: BTreeMap::new(),
            sounds: BTreeMap::new(),
            sound_full: BTreeMap::new(),
            cache: BTreeMap::new(),
            stats: LibraryStats::default(),
        }
    }

    /// Builds a library from in-memory banks (`label`, bytes). Parsed names and resource links
    /// are indexed; parse failures are counted. No filesystem access: used by unit tests.
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

    /// Scans `<root>` for `.hxc` banks and `.uax` sound packages and indexes every named wave
    /// and `Sound` resource reference. Unreadable/malformed files are counted, not fatal.
    pub fn scan(root: &Path) -> Self {
        let mut banks = Vec::new();
        walk_ext(root, "hxc", &mut banks);
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
        let mut uaxs = Vec::new();
        walk_ext(root, "uax", &mut uaxs);
        uaxs.sort();
        for path in &uaxs {
            lib.stats.uax_seen += 1;
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let Ok(package) =
                xiii_package::Package::parse(&bytes, &xiii_package::Limits::default())
            else {
                continue;
            };
            lib.stats.uax_parsed += 1;
            lib.index_sounds(path, &package, &bytes);
        }
        lib.finish_index()
    }

    /// Indexes every named wave and every outgoing resource link of `bank`.
    fn index_bank(&mut self, path: &Path, bank: &crate::hx::HxBank) {
        for entry in &bank.entries {
            match &entry.kind {
                HxKind::Wave(wave) => self.index_wave(path, entry.index, entry.cuuid.into(), wave),
                HxKind::WavRes(w) => {
                    let ids: Vec<Cuuid> = w.wave_ids().into_iter().map(Cuuid::from).collect();
                    self.links
                        .insert(self.key(path, entry.index), OutLinks::Wave(ids));
                    self.record_cuuid(path, entry.index, Cuuid::from(entry.cuuid));
                }
                HxKind::Program(p) => {
                    let ids: Vec<Cuuid> = p.links.iter().copied().map(Cuuid::from).collect();
                    self.links
                        .insert(self.key(path, entry.index), OutLinks::Program(ids));
                    self.record_cuuid(path, entry.index, Cuuid::from(entry.cuuid));
                }
                HxKind::Other { .. } => {}
            }
        }
    }

    fn index_wave(&mut self, path: &Path, index: usize, cuuid: Cuuid, wave: &WaveResource) {
        let r = BankEntryRef {
            bank: path.to_path_buf(),
            entry: index,
            codec: wave.codec,
            channels: wave.channels,
            sample_rate: wave.sample_rate,
            external: matches!(wave.data, DataLocation::External { .. }),
        };
        self.entries.insert(self.key(path, index), r.clone());
        self.record_cuuid(path, index, cuuid);
        if let Some(name) = &wave.name {
            self.names.entry(name.to_lowercase()).or_default().push(r);
        }
    }

    fn record_cuuid(&mut self, path: &Path, entry: usize, cuuid: Cuuid) {
        self.by_cuuid.entry(cuuid).or_default().push(Location {
            bank: path.to_path_buf(),
            entry,
        });
    }

    /// Indexes every `Engine.Sound` export of a parsed `.uax` package.
    fn index_sounds(&mut self, path: &Path, package: &xiii_package::Package, data: &[u8]) {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        for i in 0..package.exports().len() {
            let class = package.export_class_path(i).unwrap_or("");
            if !is_sound_class(class) {
                continue;
            }
            self.stats.sound_exports += 1;
            let Some(rel) = package.object_path(xiii_package::ObjectRef::Export(i as u32)) else {
                continue;
            };
            let leaf = rel.rsplit('.').next().unwrap_or(rel);
            let Ok(payload) = package.export_payload(data, i) else {
                continue;
            };
            let Some(r) = SoundRef::parse(leaf, payload) else {
                continue;
            };
            let Some(cuuid) = r.resource else {
                continue;
            };
            self.stats.sound_refs += 1;
            let full = format!("{stem}.{}", rel.to_lowercase());
            self.sound_full.entry(full.clone()).or_insert(cuuid);
            self.add_sound(&full, cuuid);
            self.add_sound(&leaf.to_lowercase(), cuuid);
        }
    }

    /// Records that a `Sound` object (full path or leaf, case-insensitive) names `resource`.
    pub fn add_sound(&mut self, path_or_leaf: &str, resource: Cuuid) {
        self.sounds
            .entry(path_or_leaf.to_ascii_lowercase())
            .or_insert(resource);
    }

    /// Resolution outcome for one indexed `Sound` object path.
    ///
    /// Returns the rule, or a [`ResolveFailure`] when neither the resource reference (item6c)
    /// nor the leaf-name fallback (item6) reaches a wave entry.
    pub fn classify_path(
        &self,
        sound_path: &str,
    ) -> std::result::Result<ResolutionRule, ResolveFailure> {
        match self.resolve_path(sound_path) {
            Some(r) => Ok(r.rule),
            None => Err(ResolveFailure::NoNameMatch),
        }
    }

    /// Per-`.uax`-package resolution counts (package stem -> summary).
    pub fn resolution_by_package(&self) -> BTreeMap<String, ResolutionSummary> {
        let mut out: BTreeMap<String, ResolutionSummary> = BTreeMap::new();
        for (path, &cuuid) in &self.sound_full {
            let stem = path.split('.').next().unwrap_or(path).to_owned();
            let s = out.entry(stem).or_default();
            s.sounds += 1;
            if self.resolve_cuuid(path, cuuid).is_some() {
                s.resolved += 1;
                s.resource_ref += 1;
            } else if self.resolve_by_name(path).is_some() {
                s.resolved += 1;
                s.name_match += 1;
            } else if self.by_cuuid.contains_key(&cuuid) {
                s.resource_no_wave += 1;
            } else {
                s.resource_missing += 1;
            }
            if self.resolve_by_name(path).is_some() {
                s.name_rule_only += 1;
            }
            s.failed = s.resource_no_wave + s.resource_missing + s.no_sound_ref;
        }
        out
    }

    /// Paths unresolved by both rules, with a short reason label, sorted.
    ///
    /// Reasons: `absent_resource_pair` (the `Sound` tail's pair is in no bank entry),
    /// `no_wave_via_links` (the pair exists but its program/graph does not reach a wave), and
    /// `no_sound_ref` (the `Sound` tail had no resource reference).
    pub fn unresolved_paths(&self) -> Vec<(String, &'static str)> {
        let mut out = Vec::new();
        for (path, &cuuid) in &self.sound_full {
            let resource = self.sounds.get(path).copied().unwrap_or(cuuid);
            if self.resolve_cuuid(path, resource).is_some() || self.resolve_by_name(path).is_some()
            {
                continue;
            }
            let reason = if !self.sounds.contains_key(path) {
                "no_sound_ref"
            } else if !self.by_cuuid.contains_key(&resource) {
                "absent_resource_pair"
            } else {
                "no_wave_via_links"
            };
            out.push((path.clone(), reason));
        }
        out.sort();
        out
    }

    /// Corpus-wide resolution counts for every indexed `Sound` object path, using both rules.
    pub fn resolution_summary(&self) -> ResolutionSummary {
        let mut s = ResolutionSummary::default();
        for (path, &cuuid) in &self.sound_full {
            s.sounds += 1;
            let resource = self
                .sounds
                .get(path)
                .copied()
                .or(Some(cuuid))
                .and_then(|c| self.resolve_cuuid(path, c));
            if resource.is_some() {
                s.resolved += 1;
                s.resource_ref += 1;
            } else if self.resolve_by_name(path).is_some() {
                s.resolved += 1;
                s.name_match += 1;
            } else if let Some(&c) = self.sounds.get(path) {
                if self.by_cuuid.contains_key(&c) {
                    s.resource_no_wave += 1;
                } else {
                    s.resource_missing += 1;
                }
            } else {
                s.no_sound_ref += 1;
            }
        }
        // Name-only count (the item6 rule) for the before/after comparison.
        for path in self.sound_full.keys() {
            if self.resolve_by_name(path).is_some() {
                s.name_rule_only += 1;
            }
        }
        s.failed = s.resource_no_wave + s.resource_missing + s.no_sound_ref;
        s
    }

    fn key(&self, path: &Path, entry: usize) -> (String, usize) {
        (path.to_string_lossy().to_ascii_lowercase(), entry)
    }

    fn finish_index(mut self) -> Self {
        self.stats.names = self.names.len();
        self.stats.resources = self.by_cuuid.len();
        self
    }

    /// Library statistics (bank/name/sound/resource counts).
    pub fn stats(&self) -> LibraryStats {
        self.stats
    }

    /// Every distinct WavRes name in the index, sorted (for diagnostics/tests).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.keys().map(String::as_str)
    }

    /// Every distinct `Sound` path in the index, sorted (for diagnostics/tests).
    pub fn sound_paths(&self) -> impl Iterator<Item = &str> {
        self.sounds.keys().map(String::as_str)
    }

    /// The first bank entry for a `WavRes` name (case-insensitive), or `None`.
    pub fn resolve_name(&self, name: &str) -> Option<&BankEntryRef> {
        self.names.get(&name.to_lowercase()).and_then(|v| v.first())
    }

    /// Resolves a `Sound` object path: first by its HX resource reference (item6c), then by the
    /// leaf-name fallback (item6). `None` when neither rule matches.
    pub fn resolve_path(&self, sound_path: &str) -> Option<ResolvedSound> {
        let lower = sound_path.to_ascii_lowercase();
        if let Some(&cuuid) = self.sounds.get(&lower)
            && let Some(mut r) = self.resolve_cuuid(sound_path, cuuid)
        {
            r.rule = ResolutionRule::ResourceRef;
            return Some(r);
        }
        self.resolve_by_name(sound_path)
    }

    /// Name-only resolution (the item6 rule), exposed for diagnostics and the linkage command.
    pub fn resolve_by_name(&self, sound_path: &str) -> Option<ResolvedSound> {
        let r = leaf(sound_path).and_then(|l| self.resolve_name(l))?;
        Some(ResolvedSound {
            entry: r.clone(),
            rule: ResolutionRule::NameMatch,
            candidates: 1,
            chosen: 0,
            seed: 0,
        })
    }

    /// Follows a resource pair through the bank graph to distinct wave entries. The resource
    /// graph is expanded breadth-first; a `WavRes` (a named sound) follows only its first
    /// wave-reaching reference (direct then English then others), while program/random/switch
    /// entries contribute every linked candidate. The result is then deterministic.
    fn resolve_cuuid(&self, sound_path: &str, root: Cuuid) -> Option<ResolvedSound> {
        let mut visited: BTreeSet<Cuuid> = BTreeSet::new();
        let mut stack = vec![root];
        let mut out: Vec<BankEntryRef> = Vec::new();
        while let Some(c) = stack.pop() {
            if !visited.insert(c) {
                continue;
            }
            let Some(locs) = self.by_cuuid.get(&c) else {
                continue;
            };
            for loc in locs {
                let key = self.key(&loc.bank, loc.entry);
                if let Some(e) = self.entries.get(&key) {
                    if !out.contains(e) {
                        out.push(e.clone());
                    }
                } else if let Some(links) = self.links.get(&key) {
                    match links {
                        OutLinks::Wave(ids) => {
                            // Take the first reference that reaches any wave (English first).
                            if let Some(first) = ids.iter().find(|id| self.reaches_wave(id)) {
                                stack.push(*first);
                            }
                        }
                        OutLinks::Program(ids) => stack.extend(ids.iter().copied()),
                    }
                }
            }
        }
        if out.is_empty() {
            return None;
        }
        out.sort_by(|a, b| {
            a.bank
                .to_string_lossy()
                .to_ascii_lowercase()
                .cmp(&b.bank.to_string_lossy().to_ascii_lowercase())
                .then(a.entry.cmp(&b.entry))
        });
        let seed = fnv1a(sound_path);
        let chosen = (seed % out.len() as u64) as usize;
        Some(ResolvedSound {
            entry: out[chosen].clone(),
            rule: ResolutionRule::ResourceRef,
            candidates: out.len(),
            chosen,
            seed,
        })
    }

    /// True when `cuuid` or its link graph reaches at least one wave entry.
    fn reaches_wave(&self, cuuid: &Cuuid) -> bool {
        let mut visited: BTreeSet<Cuuid> = BTreeSet::new();
        let mut stack = vec![*cuuid];
        while let Some(c) = stack.pop() {
            if !visited.insert(c) {
                continue;
            }
            let Some(locs) = self.by_cuuid.get(&c) else {
                continue;
            };
            for loc in locs {
                let key = self.key(&loc.bank, loc.entry);
                if self.entries.contains_key(&key) {
                    return true;
                }
                if let Some(links) = self.links.get(&key) {
                    match links {
                        OutLinks::Wave(ids) | OutLinks::Program(ids) => {
                            stack.extend(ids.iter().copied());
                        }
                    }
                }
            }
        }
        false
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

/// True when a class path names an Unreal `Sound` object.
fn is_sound_class(class: &str) -> bool {
    class.ends_with(".Sound") || class == "Sound"
}

/// 64-bit FNV-1a hash, so a multi-candidate choice is stable across runs.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl From<(u32, u32)> for Cuuid {
    fn from(p: (u32, u32)) -> Self {
        Cuuid::new(p.0, p.1)
    }
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

fn walk_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_ext(&path, ext, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
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

    /// Builds a 24-byte `Sound` native tail for a resource pair.
    fn sound_tail(c: Cuuid) -> Vec<u8> {
        let mut t = vec![0x01, 0, 0, 0, 0, 18];
        let s = c.guid_string();
        assert_eq!(s.len(), 18);
        t.extend_from_slice(s.as_bytes());
        assert_eq!(t.len(), 24);
        t
    }

    /// A one-wave internal-PCM bank body (without the index) plus its record offsets.
    struct Bank {
        body: Vec<u8>,
        wave_body: usize,
        wavres_body: Option<usize>,
        wave_size: usize,
        wavres_size: usize,
        prog_sizes: Vec<usize>,
    }

    /// Builds a bank with one wave (cuuid `wave`), an optional naming WavRes (`wavres`), and
    /// optional program records linking the given pairs.
    fn synth_bank(
        wave: Cuuid,
        wavres: Option<(&str, Cuuid)>,
        programs: &[(Cuuid, &[Cuuid])],
    ) -> Vec<u8> {
        let base = 4usize;
        let mut b = Bank {
            body: Vec::new(),
            wave_body: 0,
            wavres_body: None,
            wave_size: 0,
            wavres_size: 0,
            prog_sizes: Vec::new(),
        };
        // Wave record: internal PCM16.
        b.wave_body = b.body.len();
        b.body.extend_from_slice(&u32b(16));
        b.body.extend_from_slice(b"CPCWaveFileIdObj");
        b.body.extend_from_slice(&u32b(wave.id1));
        b.body.extend_from_slice(&u32b(wave.id2));
        b.body.extend_from_slice(&u32b(3));
        b.body.extend_from_slice(&u32b(0));
        b.body.push(0);
        b.body.extend_from_slice(b"RIFF");
        b.body.extend_from_slice(&u32b(40));
        b.body.extend_from_slice(b"WAVE");
        b.body.extend_from_slice(b"fmt ");
        b.body.extend_from_slice(&u32b(16));
        b.body.extend_from_slice(&u16b(1));
        b.body.extend_from_slice(&u16b(1));
        b.body.extend_from_slice(&u32b(8000));
        b.body.extend_from_slice(&u32b(16_000));
        b.body.extend_from_slice(&u16b(2));
        b.body.extend_from_slice(&u16b(16));
        b.body.extend_from_slice(b"data");
        b.body.extend_from_slice(&u32b(4));
        b.body.extend_from_slice(&10_000i16.to_le_bytes());
        b.body.extend_from_slice(&(-10_000i16).to_le_bytes());
        b.wave_size = b.body.len() - b.wave_body;

        // Optional naming WavRes linking the wave.
        if let Some((name, c)) = wavres {
            let off = b.body.len();
            b.body.extend_from_slice(&u32b(13));
            b.body.extend_from_slice(b"CPCWavResData");
            b.body.extend_from_slice(&u32b(c.id1));
            b.body.extend_from_slice(&u32b(c.id2));
            b.body.extend_from_slice(&u32b(0));
            b.body.extend_from_slice(&u32b(name.len() as u32));
            b.body.extend_from_slice(name.as_bytes());
            b.wavres_size = b.body.len() - off;
            b.wavres_body = Some(off);
        }

        // Optional program records.
        let mut prog_offs = Vec::new();
        for (_c, _links) in programs {
            let off = b.body.len();
            b.body.extend_from_slice(&u32b(15));
            b.body.extend_from_slice(b"CProgramResData");
            b.body.extend_from_slice(&[0u8; 8]);
            prog_offs.push(off);
            b.prog_sizes.push(b.body.len() - off);
        }

        let wave_off = base + b.wave_body;
        let index_off = base + b.body.len();
        let mut index = Vec::new();
        index.extend_from_slice(b"INDX");
        index.extend_from_slice(&u32b(2));
        index.extend_from_slice(&u32b(1 + wavres.is_some() as u32 + programs.len() as u32));
        // Entry 0: wave.
        index.extend_from_slice(&u32b(16));
        index.extend_from_slice(b"CPCWaveFileIdObj");
        index.extend_from_slice(&u32b(wave.id1));
        index.extend_from_slice(&u32b(wave.id2));
        index.extend_from_slice(&u32b(wave_off as u32));
        index.extend_from_slice(&u32b(b.wave_size as u32));
        index.extend_from_slice(&u32b(0));
        index.extend_from_slice(&u32b(0));
        index.extend_from_slice(&u32b(0));
        // Entry 1: WavRes.
        if let Some((_name, c)) = wavres {
            let off = base + b.wavres_body.unwrap();
            index.extend_from_slice(&u32b(13));
            index.extend_from_slice(b"CPCWavResData");
            index.extend_from_slice(&u32b(c.id1));
            index.extend_from_slice(&u32b(c.id2));
            index.extend_from_slice(&u32b(off as u32));
            index.extend_from_slice(&u32b(b.wavres_size as u32));
            index.extend_from_slice(&u32b(0));
            index.extend_from_slice(&u32b(1));
            index.extend_from_slice(&u32b(wave.id1));
            index.extend_from_slice(&u32b(wave.id2));
            index.extend_from_slice(&u32b(0));
        }
        // Program entries.
        for (k, (c, links)) in programs.iter().enumerate() {
            let off = base + prog_offs[k];
            index.extend_from_slice(&u32b(15));
            index.extend_from_slice(b"CProgramResData");
            index.extend_from_slice(&u32b(c.id1));
            index.extend_from_slice(&u32b(c.id2));
            index.extend_from_slice(&u32b(off as u32));
            index.extend_from_slice(&u32b(b.prog_sizes[k] as u32));
            index.extend_from_slice(&u32b(0));
            index.extend_from_slice(&u32b(links.len() as u32));
            for l in *links {
                index.extend_from_slice(&u32b(l.id1));
                index.extend_from_slice(&u32b(l.id2));
            }
            index.extend_from_slice(&u32b(0));
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32b(index_off as u32));
        bytes.extend_from_slice(&b.body);
        bytes.extend_from_slice(&index);
        bytes
    }

    /// Builds a bank with two internal-PCM waves and a program that links both.
    fn synth_two_wave_bank(w1: Cuuid, w2: Cuuid, program: Cuuid) -> Vec<u8> {
        let base = 4usize;
        let mut body = Vec::new();
        let mut wave_offs = Vec::new();
        let mut wave_sizes = Vec::new();
        for w in [w1, w2] {
            let off = body.len();
            wave_offs.push(off);
            body.extend_from_slice(&u32b(16));
            body.extend_from_slice(b"CPCWaveFileIdObj");
            body.extend_from_slice(&u32b(w.id1));
            body.extend_from_slice(&u32b(w.id2));
            body.extend_from_slice(&u32b(3));
            body.extend_from_slice(&u32b(0));
            body.push(0);
            body.extend_from_slice(b"RIFF");
            body.extend_from_slice(&u32b(40));
            body.extend_from_slice(b"WAVE");
            body.extend_from_slice(b"fmt ");
            body.extend_from_slice(&u32b(16));
            body.extend_from_slice(&u16b(1));
            body.extend_from_slice(&u16b(1));
            body.extend_from_slice(&u32b(8000));
            body.extend_from_slice(&u32b(16_000));
            body.extend_from_slice(&u16b(2));
            body.extend_from_slice(&u16b(16));
            body.extend_from_slice(b"data");
            body.extend_from_slice(&u32b(4));
            body.extend_from_slice(&10_000i16.to_le_bytes());
            body.extend_from_slice(&(-10_000i16).to_le_bytes());
            wave_sizes.push(body.len() - off);
        }
        let prog_off = body.len();
        body.extend_from_slice(&u32b(15));
        body.extend_from_slice(b"CProgramResData");
        body.extend_from_slice(&[0u8; 8]);
        let prog_size = body.len() - prog_off;

        let index_off = base + body.len();
        let mut index = Vec::new();
        index.extend_from_slice(b"INDX");
        index.extend_from_slice(&u32b(2));
        index.extend_from_slice(&u32b(3));
        for (k, w) in [w1, w2].iter().enumerate() {
            index.extend_from_slice(&u32b(16));
            index.extend_from_slice(b"CPCWaveFileIdObj");
            index.extend_from_slice(&u32b(w.id1));
            index.extend_from_slice(&u32b(w.id2));
            index.extend_from_slice(&u32b((base + wave_offs[k]) as u32));
            index.extend_from_slice(&u32b(wave_sizes[k] as u32));
            index.extend_from_slice(&u32b(0));
            index.extend_from_slice(&u32b(0));
            index.extend_from_slice(&u32b(0));
        }
        index.extend_from_slice(&u32b(15));
        index.extend_from_slice(b"CProgramResData");
        index.extend_from_slice(&u32b(program.id1));
        index.extend_from_slice(&u32b(program.id2));
        index.extend_from_slice(&u32b((base + prog_off) as u32));
        index.extend_from_slice(&u32b(prog_size as u32));
        index.extend_from_slice(&u32b(0));
        index.extend_from_slice(&u32b(2));
        for w in [w1, w2] {
            index.extend_from_slice(&u32b(w.id1));
            index.extend_from_slice(&u32b(w.id2));
        }
        index.extend_from_slice(&u32b(0));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u32b(index_off as u32));
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&index);
        bytes
    }

    const WAVE: Cuuid = Cuuid {
        id1: 0x1111_1111,
        id2: 0x2222_2222,
    };
    const WAVRES: Cuuid = Cuuid {
        id1: 0x3333_3333,
        id2: 0x4444_4444,
    };
    const EVENT: Cuuid = Cuuid {
        id1: 0x5555_5555,
        id2: 0x6666_6666,
    };

    #[test]
    fn leaf_takes_last_path_component() {
        assert_eq!(leaf("XIIIsound.Footsteps.FtSkMar1"), Some("FtSkMar1"));
        assert_eq!(leaf("FtSkMar1"), Some("FtSkMar1"));
        assert_eq!(leaf(""), None);
        assert_eq!(leaf("XIIIsound."), None);
    }

    #[test]
    fn sound_tail_parses_guid() {
        let r = SoundRef::parse("X", &sound_tail(EVENT)).expect("parse");
        assert_eq!(r.resource, Some(EVENT));
        assert_eq!(r.leaf, "X");
        // A short payload or wrong length byte yields no resource but still parses the tail.
        assert!(SoundRef::parse("X", &[0u8; 10]).is_none());
        let mut wrong = sound_tail(EVENT);
        wrong[5] = 17;
        let r = SoundRef::parse("X", &wrong).expect("parse");
        assert_eq!(r.resource, None);
    }

    #[test]
    fn indexes_names_and_resolves_by_name() {
        let lib = SoundLibrary::from_banks([(
            "Test.hxc".to_owned(),
            synth_bank(WAVE, Some(("TestSound", WAVRES)), &[]),
        )]);
        let s = lib.stats();
        assert_eq!(s.banks_seen, 1);
        assert_eq!(s.banks_parsed, 1);
        assert_eq!(s.names, 1);

        let r = lib.resolve_name("testsound").expect("case-insensitive");
        assert_eq!(r.entry, 0);
        assert_eq!(r.codec, Codec::Pcm);
        assert_eq!(r.sample_rate, 8000);
        assert!(!r.external);

        let resolved = lib
            .resolve_path("XIIIsound.X.TestSound")
            .expect("name rule");
        assert_eq!(resolved.rule, ResolutionRule::NameMatch);
        assert!(lib.resolve_path("XIIIsound.X.NotHere").is_none());
        assert!(lib.resolve_name("").is_none());
    }

    #[test]
    fn resolves_sound_resource_directly_to_wave() {
        // WavRes named, and a Sound path whose tail names the WAVRES pair.
        let mut lib = SoundLibrary::from_banks([(
            "Bank.hxc".to_owned(),
            synth_bank(WAVE, Some(("Named", WAVRES)), &[]),
        )]);
        lib.add_sound("XIIIsound.Guns.Fire", WAVRES);
        let r = lib
            .resolve_path("XIIIsound.Guns.Fire")
            .expect("resource rule");
        assert_eq!(r.rule, ResolutionRule::ResourceRef);
        assert_eq!(r.entry.bank.to_string_lossy(), "Bank.hxc");
        assert_eq!(r.entry.entry, 0);
        assert_eq!(r.candidates, 1);
    }

    #[test]
    fn follows_program_links_to_wave() {
        // An event pair that links to the wave pair; the Sound tail names the event.
        let mut lib = SoundLibrary::from_banks([(
            "Bank.hxc".to_owned(),
            synth_bank(WAVE, None, &[(EVENT, &[WAVE])]),
        )]);
        lib.add_sound("Pkg.Group.Event__hPlay", EVENT);
        let r = lib
            .resolve_path("Pkg.Group.Event__hPlay")
            .expect("program rule");
        assert_eq!(r.rule, ResolutionRule::ResourceRef);
        assert_eq!(r.entry.entry, 0, "resolved through the program link");
    }

    #[test]
    fn chooses_deterministically_from_several_candidates() {
        let wave2 = Cuuid::new(0xAAAA_AAAA, 0xBBBB_BBBB);
        let bytes = synth_two_wave_bank(WAVE, wave2, EVENT);
        let mut lib = SoundLibrary::from_banks([("B.hxc".to_owned(), bytes)]);
        lib.add_sound("A", EVENT);
        let first = lib.resolve_path("A").expect("resolved");
        let second = lib.resolve_path("A").expect("resolved");
        assert_eq!(first.candidates, 2, "both waves are candidates");
        assert_eq!(first.entry, second.entry, "the choice is deterministic");
        assert_eq!(first.seed, second.seed);
        // Different requested paths can pick different candidates; each is in range.
        lib.add_sound("Z", EVENT);
        let z = lib.resolve_path("Z").expect("resolved");
        assert!(z.chosen < 2);
    }

    #[test]
    fn counts_malformed_banks() {
        let lib = SoundLibrary::from_banks([
            (
                "Good.hxc".to_owned(),
                synth_bank(WAVE, Some(("A", WAVRES)), &[]),
            ),
            ("Bad.hxc".to_owned(), b"not a bank".to_vec()),
        ]);
        let s = lib.stats();
        assert_eq!(s.banks_seen, 2);
        assert_eq!(s.banks_parsed, 1);
        assert_eq!(s.banks_failed, 1);
    }

    #[test]
    fn load_fails_without_a_real_bank_file() {
        let mut lib = SoundLibrary::from_banks([(
            "Absent.hxc".to_owned(),
            synth_bank(WAVE, Some(("Ghost", WAVRES)), &[]),
        )]);
        let r = lib.resolve_name("Ghost").expect("indexed").clone();
        assert!(matches!(lib.load(&r), Err(ResolveFailure::DecodeFailed)));
        assert!(matches!(lib.load(&r), Err(ResolveFailure::DecodeFailed)));
    }

    #[test]
    fn fnv_seed_is_stable() {
        assert_eq!(fnv1a("abc"), fnv1a("abc"));
        assert_ne!(fnv1a("abc"), fnv1a("abd"));
    }

    #[test]
    fn resolution_summary_counts_rules_and_failures() {
        // One bank: a wave with resource WAVE plus a naming WavRes; and a program EVENT -> WAVE.
        let bytes = synth_bank(WAVE, Some(("Named", WAVRES)), &[(EVENT, &[WAVE])]);
        let mut lib = SoundLibrary::from_banks([("XIIIsound.hxc".to_owned(), bytes)]);
        // Two Sound objects resolve by resource reference, one names an absent pair, one has no
        // resource reference at all.
        lib.sound_full.insert("xiiisound.byref".into(), EVENT);
        lib.sound_full.insert("xiiisound.bywave".into(), WAVE);
        lib.sound_full
            .insert("xiiisound.absentpair".into(), Cuuid::new(0, 0));
        lib.sound_full
            .insert("xiiisound.noref".into(), Cuuid::new(0, 0));
        lib.add_sound("xiiisound.byref", EVENT);
        lib.add_sound("xiiisound.bywave", WAVE);
        lib.add_sound("xiiisound.absentpair", Cuuid::new(0, 0));
        let s = lib.resolution_summary();
        assert_eq!(s.sounds, 4);
        assert_eq!(s.resource_ref, 2, "program and direct wave references");
        assert_eq!(s.resolved, 2);
        assert_eq!(s.resource_missing, 1, "absent pair, no name either");
        assert_eq!(s.no_sound_ref, 1, "Sound object without a resource tail");
        assert_eq!(s.failed, 2);
        assert_eq!(s.name_rule_only, 0, "no path leaf matches 'Named'");

        // A WavRes name fallback (leaf 'Named') resolves.
        let mut lib2 = SoundLibrary::from_banks([(
            "B.hxc".to_owned(),
            synth_bank(WAVE, Some(("Named", WAVRES)), &[]),
        )]);
        lib2.sound_full
            .insert("xiiisound.named".into(), Cuuid::new(0, 0));
        let s2 = lib2.resolution_summary();
        assert_eq!(s2.name_match, 1);
        assert_eq!(s2.name_rule_only, 1);

        // Per-package grouping keys on the stem.
        let by_pkg = lib.resolution_by_package();
        assert!(by_pkg.contains_key("xiiisound"));
        assert_eq!(by_pkg["xiiisound"].resolved, 2);
        assert_eq!(by_pkg["xiiisound"].failed, 2);
    }
}
