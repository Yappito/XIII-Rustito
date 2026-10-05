//! Bounded parser for Ubisoft HXAudio banks (`.hxc`) and their streamed companion (`.hsc`).
//!
//! The bank is a set of resource records addressed by an `INDX` table. XIII (PC) uses three
//! record classes:
//!
//! * `CPCWaveFileIdObj` — one audio stream: a small RIFF header (codec, channels, sample rate)
//!   plus either inline PCM data or a `datx` chunk that points into a sibling `.hsc` stream file.
//! * `CPCWavResData` — a named sound resource that links to a `CPCWaveFileIdObj` by `cuuid`.
//! * `CProgramResData` — an event/program record that links to other resources.
//!
//! The layout is derived from vgmstream `src/meta/ubi_hx.c`
//! @ `7dc938fa2f210943b37c7b6511852b516ef432ab` (format reference only; no code copied) and
//! confirmed against the GOG corpus (see `local/reports/item6-audio-spike.md`).

use crate::error::{AudioError, AudioErrorKind, Result};

/// A half-open byte range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// First byte of the range.
    pub start: usize,
    /// One past the last byte of the range.
    pub end: usize,
}

impl Span {
    /// Builds a span from a start and a length, checking for overflow.
    fn from_len(start: usize, len: usize) -> Result<Self> {
        let end = start
            .checked_add(len)
            .ok_or_else(|| AudioError::at(AudioErrorKind::BadSize, start, "span overflow"))?;
        Ok(Self { start, end })
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// True when the span is empty.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Parser limits guarding against absurd counts/sizes in a corrupt bank.
#[derive(Debug, Clone, Copy)]
pub struct HxLimits {
    /// Maximum number of index entries.
    pub max_entries: u32,
    /// Maximum byte length of a class/resource/name string.
    pub max_string: u32,
    /// Maximum number of links in one index record.
    pub max_links: u32,
}

impl Default for HxLimits {
    fn default() -> Self {
        Self {
            max_entries: 1 << 20,
            max_string: 0x1_0000,
            max_links: 1 << 20,
        }
    }
}

/// Audio codec identified by the RIFF `wFormatTag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// 16-bit little-endian PCM, interleaved by channel.
    Pcm,
    /// Ubisoft ADPCM (6-bit on the PC corpus).
    UbiAdpcm,
    /// Any other codec id, kept for diagnostics.
    Other(u16),
}

impl Codec {
    /// Short label used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::Pcm => "pcm16",
            Codec::UbiAdpcm => "ubi_adpcm",
            Codec::Other(_) => "other",
        }
    }
}

/// Where the sample bytes live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataLocation {
    /// Data lies in the bank file itself at this range.
    Internal(Span),
    /// Data lies in the named external `.hsc` file at this range.
    External {
        /// Byte offset within the external stream file.
        offset: usize,
        /// Byte length within the external stream file.
        size: usize,
    },
}

/// Parsed audio metadata for one `*WaveFileIdObj` record.
#[derive(Debug, Clone)]
pub struct WaveResource {
    /// Resource class name.
    pub class_name: String,
    /// Raw stream-mode byte (`0`/`2` internal, `1`/`3`/`7`/`0x0a` external).
    pub stream_mode: u32,
    /// External stream file name (as stored, e.g. `.\\Plage00.hsc`).
    pub resource_name: Option<String>,
    /// Codec from the RIFF header.
    pub codec: Codec,
    /// Channel count.
    pub channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Bits per sample declared in the RIFF `fmt` chunk.
    pub bits_per_sample: u16,
    /// Where the sample bytes are.
    pub data: DataLocation,
    /// Sound name resolved from a linking `CPCWavResData` record, if any.
    pub name: Option<String>,
}

/// One `CPCWavResData` record: a named resource linking to a wave.
#[derive(Debug, Clone)]
pub struct WavRes {
    /// Resource class name.
    pub class_name: String,
    /// Internal sound name (e.g. `M16Fire1`), if present.
    pub name: Option<String>,
    /// `(id1, id2)` pairs this record points at directly.
    pub links: Vec<(u32, u32)>,
    /// Localized wave references: `(language code bytes, id1, id2)`. Dialogue resources use
    /// this table instead of the direct link list.
    pub localized: Vec<([u8; 4], u32, u32)>,
}

impl WavRes {
    /// Wave identity pairs to try, direct links first, then localized entries with English first.
    pub fn wave_ids(&self) -> Vec<(u32, u32)> {
        let mut ids: Vec<(u32, u32)> = self.links.clone();
        let english = |code: &[u8; 4]| *code == *b"  ne";
        for (code, id1, id2) in &self.localized {
            if english(code) {
                ids.push((*id1, *id2));
            }
        }
        for (code, id1, id2) in &self.localized {
            if !english(code) {
                ids.push((*id1, *id2));
            }
        }
        ids
    }
}

/// One event/program record. Only its links are decoded.
#[derive(Debug, Clone)]
pub struct Program {
    /// Resource class name.
    pub class_name: String,
    /// `(id1, id2)` pairs this record points at.
    pub links: Vec<(u32, u32)>,
}

/// Record payload for an index entry.
#[derive(Debug, Clone)]
pub enum HxKind {
    /// An audio stream.
    Wave(WaveResource),
    /// A named sound resource.
    WavRes(WavRes),
    /// An event/program resource.
    Program(Program),
    /// A known but non-audio resource class.
    Other {
        /// Resource class name.
        class_name: String,
    },
}

/// One parsed index entry.
#[derive(Debug, Clone)]
pub struct HxEntry {
    /// Zero-based position in the `INDX` table.
    pub index: usize,
    /// `(id1, id2)` identity pair.
    pub cuuid: (u32, u32),
    /// Byte range of the record header in the bank.
    pub header_span: Span,
    /// Parsed payload.
    pub kind: HxKind,
}

impl HxEntry {
    /// The record's class name.
    pub fn class_name(&self) -> &str {
        match &self.kind {
            HxKind::Wave(w) => &w.class_name,
            HxKind::WavRes(w) => &w.class_name,
            HxKind::Program(p) => &p.class_name,
            HxKind::Other { class_name } => class_name,
        }
    }

    /// The wave resource, when this entry is a `*WaveFileIdObj`.
    pub fn as_wave(&self) -> Option<&WaveResource> {
        match &self.kind {
            HxKind::Wave(w) => Some(w),
            _ => None,
        }
    }
}

/// A parsed HX bank.
#[derive(Debug, Clone)]
pub struct HxBank {
    /// Index table type (1 or 2).
    pub index_type: u32,
    /// Every index entry, in table order.
    pub entries: Vec<HxEntry>,
    /// Byte range of the index pointer and table.
    pub index_span: Span,
    /// Total file size in bytes.
    pub file_size: usize,
    /// Bytes covered by the index table and record headers.
    pub covered_bytes: usize,
    /// Ranges of the bank not referenced by the index table or any record header.
    pub unparsed: Vec<Span>,
}

fn u8_at(data: &[u8], off: usize) -> Result<u8> {
    data.get(off).copied().ok_or_else(|| {
        AudioError::at(
            AudioErrorKind::Truncated,
            off,
            format!("read u8 at {off:#x} past end {}", data.len()),
        )
    })
}

fn u16_at(data: &[u8], off: usize) -> Result<u16> {
    let bytes = data.get(off..off + 2).ok_or_else(|| {
        AudioError::at(
            AudioErrorKind::Truncated,
            off,
            format!("read u16 at {off:#x} past end {}", data.len()),
        )
    })?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at(data: &[u8], off: usize) -> Result<u32> {
    let bytes = data.get(off..off + 4).ok_or_else(|| {
        AudioError::at(
            AudioErrorKind::Truncated,
            off,
            format!("read u32 at {off:#x} past end {}", data.len()),
        )
    })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn i32_at(data: &[u8], off: usize) -> Result<i32> {
    u32_at(data, off).map(|v| v as i32)
}

fn slice_at(data: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    data.get(off..off + len).ok_or_else(|| {
        AudioError::at(
            AudioErrorKind::Truncated,
            off,
            format!("read {len} bytes at {off:#x} past end {}", data.len()),
        )
    })
}

fn string_at(data: &[u8], off: usize, len: usize, limits: &HxLimits) -> Result<String> {
    if len as u32 > limits.max_string {
        return Err(AudioError::at(
            AudioErrorKind::BadSize,
            off,
            format!("string length {len} exceeds max {}", limits.max_string),
        ));
    }
    let bytes = slice_at(data, off, len)?;
    std::str::from_utf8(bytes)
        .map(|s| s.to_owned())
        .map_err(|e| AudioError::at(AudioErrorKind::BadString, off, format!("{e}")))
}

fn tag_at(data: &[u8], off: usize) -> Result<[u8; 4]> {
    let bytes = slice_at(data, off, 4)?;
    Ok([bytes[0], bytes[1], bytes[2], bytes[3]])
}

const WAVE_CLASSES: &[&str] = &[
    "CPCWaveFileIdObj",
    "CPS2WaveFileIdObj",
    "CGCWaveFileIdObj",
    "CXBoxWaveFileIdObj",
    "CXBoxStaticHWWaveFileIdObj",
    "CXBoxStreamHWWaveFileIdObj",
    "CPS3StaticAC3WaveFileIdObj",
    "CPS3StreamAC3WaveFileIdObj",
];

const WAVRES_CLASSES: &[&str] = &[
    "CPCWavResData",
    "CPS2WavResData",
    "CGCWavResData",
    "CXBoxWavResData",
    "CPS3WavResData",
];

const PROGRAM_CLASSES: &[&str] = &[
    "CEventResData",
    "CProgramResData",
    "CActorResData",
    "CRandomResData",
    "CTreeBank",
    "CTreeRes",
    "CSwitchResData",
];

/// Parses one bank from bytes. The bank is only read; the external `.hsc` stream is not touched.
pub fn parse_bank(data: &[u8], limits: &HxLimits) -> Result<HxBank> {
    if data.len() < 4 {
        return Err(AudioError::at(
            AudioErrorKind::Truncated,
            0,
            "bank smaller than index pointer",
        ));
    }
    let index_offset = u32_at(data, 0)? as usize;
    if tag_at(data, index_offset)? != *b"INDX" {
        return Err(AudioError::at(
            AudioErrorKind::BadTag,
            index_offset,
            "expected INDX tag",
        ));
    }
    let index_type = u32_at(data, index_offset + 4)?;
    if index_type != 1 && index_type != 2 {
        return Err(AudioError::at(
            AudioErrorKind::BadIndexType,
            index_offset + 4,
            format!("index type {index_type}"),
        ));
    }
    let count = i32_at(data, index_offset + 8)?;
    if count < 0 || count as u32 > limits.max_entries {
        return Err(AudioError::at(
            AudioErrorKind::BadSize,
            index_offset + 8,
            format!("entry count {count}"),
        ));
    }

    let mut entries = Vec::with_capacity(count as usize);
    let mut cursor = index_offset + 12;
    for index in 0..count as usize {
        let class_size = u32_at(data, cursor)? as usize;
        let class_name = string_at(data, cursor + 4, class_size, limits)?;
        cursor += 4 + class_size;

        let cuuid = (u32_at(data, cursor)?, u32_at(data, cursor + 4)?);
        let header_offset = u32_at(data, cursor + 8)? as usize;
        let header_size = u32_at(data, cursor + 12)? as usize;
        cursor += 16;

        let unknown_count = i32_at(data, cursor)?;
        cursor += 4;
        if unknown_count != 0 {
            return Err(AudioError::at(
                AudioErrorKind::BadSize,
                cursor - 4,
                format!("unknown_count {unknown_count} != 0"),
            ));
        }

        let mut links = Vec::new();
        let mut localized = Vec::new();
        if index_type == 2 {
            let link_count = i32_at(data, cursor)?;
            cursor += 4;
            if link_count < 0 || link_count as u32 > limits.max_links {
                return Err(AudioError::at(
                    AudioErrorKind::BadSize,
                    cursor - 4,
                    format!("link count {link_count}"),
                ));
            }
            for _ in 0..link_count {
                links.push((u32_at(data, cursor)?, u32_at(data, cursor + 4)?));
                cursor += 8;
            }
            let language_count = i32_at(data, cursor)?;
            cursor += 4;
            if language_count < 0 || language_count as u32 > limits.max_links {
                return Err(AudioError::at(
                    AudioErrorKind::BadSize,
                    cursor - 4,
                    format!("language count {language_count}"),
                ));
            }
            for _ in 0..language_count {
                let code = tag_at(data, cursor)?;
                let id1 = u32_at(data, cursor + 8)?;
                let id2 = u32_at(data, cursor + 12)?;
                localized.push((code, id1, id2));
                cursor += 16;
            }
        }

        let header_span = Span::from_len(header_offset, header_size)?;
        if header_span.end > data.len() {
            return Err(AudioError::at(
                AudioErrorKind::BadSize,
                header_offset,
                format!(
                    "record header {}..{} past end {}",
                    header_span.start,
                    header_span.end,
                    data.len()
                ),
            ));
        }

        let kind = if WAVE_CLASSES.contains(&class_name.as_str()) {
            HxKind::Wave(parse_wave(data, header_offset, &class_name, limits)?)
        } else if WAVRES_CLASSES.contains(&class_name.as_str()) {
            let (name, _) = parse_wavres(data, header_offset, limits)?;
            HxKind::WavRes(WavRes {
                class_name,
                name,
                links,
                localized,
            })
        } else if PROGRAM_CLASSES.contains(&class_name.as_str()) {
            HxKind::Program(Program { class_name, links })
        } else {
            return Err(AudioError::at(
                AudioErrorKind::UnknownClass,
                header_offset,
                format!("unknown resource class {class_name:?}"),
            ));
        };

        entries.push(HxEntry {
            index,
            cuuid,
            header_span,
            kind,
        });
    }

    let index_end = cursor;
    let index_span = Span {
        start: 0,
        end: index_end,
    };

    let mut ranges: Vec<Span> = Vec::with_capacity(entries.len() + 1);
    ranges.push(index_span);
    for e in &entries {
        ranges.push(e.header_span);
    }
    let (covered_bytes, unparsed) = coverage(&ranges, data.len());

    // Resolve names for wave records from the WavRes records that link to them. Each resource
    // names a single wave: its direct link, or the English localized variant when present.
    let mut wave_by_cuuid: Vec<((u32, u32), usize)> = Vec::new();
    for e in &entries {
        if e.as_wave().is_some() {
            wave_by_cuuid.push((e.cuuid, e.index));
        }
    }
    let mut resolved: Vec<(usize, String)> = Vec::new();
    for e in &entries {
        if let HxKind::WavRes(w) = &e.kind
            && let Some(name) = &w.name
        {
            for link in w.wave_ids() {
                if let Some((_, wi)) = wave_by_cuuid.iter().find(|(c, _)| *c == link) {
                    resolved.push((*wi, name.clone()));
                    break;
                }
            }
        }
    }
    for (wi, name) in resolved {
        if let HxKind::Wave(wave) = &mut entries[wi].kind
            && wave.name.is_none()
        {
            wave.name = Some(name);
        }
    }

    Ok(HxBank {
        index_type,
        entries,
        index_span,
        file_size: data.len(),
        covered_bytes,
        unparsed,
    })
}

/// Parses a `*WaveFileIdObj` record header and its RIFF description.
fn parse_wave(
    data: &[u8],
    offset: usize,
    class_name: &str,
    limits: &HxLimits,
) -> Result<WaveResource> {
    let mut o = offset;
    let class_size = u32_at(data, o)? as usize;
    let header_class = string_at(data, o + 4, class_size, limits)?;
    if header_class != class_name {
        return Err(AudioError::at(
            AudioErrorKind::BadString,
            o + 4,
            format!("header class {header_class:?} != index class {class_name:?}"),
        ));
    }
    o += 4 + class_size;
    o += 8; // cuuid1, cuuid2 (repeated from the index)
    let flag_type = u32_at(data, o)?;

    let stream_mode;
    match flag_type {
        1 | 2 => {
            stream_mode = u32_at(data, o + 8)?;
            o += 0x10;
        }
        3 => {
            o += 8; // flag type + parent id
            if class_name == "CGCWaveFileIdObj" {
                if u32_at(data, o)? != u32_at(data, o + 4)? {
                    return Err(AudioError::at(
                        AudioErrorKind::BadStreamMode,
                        o,
                        "CGC wave with mismatched stream-mode pair",
                    ));
                }
                stream_mode = u32_at(data, o + 4)?;
                o += 8;
            } else {
                stream_mode = u8_at(data, o)? as u32;
                o += 1;
            }
        }
        other => {
            return Err(AudioError::at(
                AudioErrorKind::BadStreamMode,
                o,
                format!("unknown wave flag type {other}"),
            ));
        }
    }

    let mut stream_adjust = 0usize;
    if stream_mode == 0x0a {
        stream_adjust = u32_at(data, o)? as usize;
        o += 4;
    }

    let mut resource_name = None;
    let riff_offset;
    match stream_mode {
        0 | 2 => riff_offset = o,
        1 | 3 | 7 | 0x0a => {
            let res_size = u32_at(data, o)? as usize;
            resource_name = Some(string_at(data, o + 4, res_size, limits)?);
            riff_offset = o + 4 + res_size;
        }
        other => {
            return Err(AudioError::at(
                AudioErrorKind::BadStreamMode,
                o,
                format!("unknown stream mode {other:#x}"),
            ));
        }
    }

    if tag_at(data, riff_offset)? != *b"RIFF" {
        return Err(AudioError::at(
            AudioErrorKind::BadRiff,
            riff_offset,
            "expected RIFF tag",
        ));
    }
    let riff_size = u32_at(data, riff_offset + 4)? as usize + 8;
    let riff_end = riff_offset.checked_add(riff_size).ok_or_else(|| {
        AudioError::at(AudioErrorKind::BadSize, riff_offset, "RIFF size overflow")
    })?;
    if riff_offset + 0x24 > data.len() {
        return Err(AudioError::at(
            AudioErrorKind::BadRiff,
            riff_offset,
            "RIFF too short for fmt fields",
        ));
    }

    let codec_id = u16_at(data, riff_offset + 0x14)?;
    let channels = u16_at(data, riff_offset + 0x16)?;
    let sample_rate = u32_at(data, riff_offset + 0x18)?;
    let bits_per_sample = u16_at(data, riff_offset + 0x22)?;
    let codec = match codec_id {
        0x01 => Codec::Pcm,
        0x02 => Codec::UbiAdpcm,
        other => Codec::Other(other),
    };

    let external = matches!(stream_mode, 1 | 3 | 7 | 0x0a);
    let search_end = riff_end.min(data.len());
    let data_location = if external {
        if let Some(c) = find_chunk(data, riff_offset + 12, search_end, b"datx")? {
            let size = u32_at(data, c)? as usize;
            let pos = u32_at(data, c + 4)? as usize + stream_adjust;
            DataLocation::External { offset: pos, size }
        } else if (flag_type == 1 || flag_type == 2)
            && let Some(c) = find_chunk(data, riff_offset + 12, search_end, b"data")?
        {
            let size = u32_at(data, c - 4)? as usize;
            let pos = u32_at(data, c)? as usize + stream_adjust;
            DataLocation::External { offset: pos, size }
        } else {
            return Err(AudioError::at(
                AudioErrorKind::MissingChunk,
                riff_offset,
                "external wave has neither datx nor data chunk",
            ));
        }
    } else {
        match find_chunk(data, riff_offset + 12, search_end, b"data")? {
            Some(c) => {
                let declared = u32_at(data, c - 4)? as usize;
                let available = riff_end.saturating_sub(c).min(data.len().saturating_sub(c));
                let size = if declared == 0 || declared > available {
                    available
                } else {
                    declared
                };
                DataLocation::Internal(Span::from_len(c, size)?)
            }
            None => {
                return Err(AudioError::at(
                    AudioErrorKind::MissingChunk,
                    riff_offset,
                    "internal wave has no data chunk",
                ));
            }
        }
    };

    Ok(WaveResource {
        class_name: class_name.to_owned(),
        stream_mode,
        resource_name,
        codec,
        channels,
        sample_rate,
        bits_per_sample,
        data: data_location,
        name: None,
    })
}

/// Finds a RIFF-style chunk and returns the offset of its payload.
fn find_chunk(data: &[u8], mut pos: usize, end: usize, tag: &[u8; 4]) -> Result<Option<usize>> {
    while pos + 8 <= end {
        let found = tag_at(data, pos)?;
        let size = u32_at(data, pos + 4)? as usize;
        let payload = pos + 8;
        if &found == tag {
            return Ok(Some(payload));
        }
        pos = payload
            .checked_add(size)
            .ok_or_else(|| AudioError::at(AudioErrorKind::BadSize, pos, "chunk size overflow"))?;
    }
    Ok(None)
}

/// Parses a `*WavResData` record and returns its internal name.
fn parse_wavres(data: &[u8], offset: usize, limits: &HxLimits) -> Result<(Option<String>, usize)> {
    let class_size = u32_at(data, offset)? as usize;
    let o = offset + 4 + class_size + 8 + 4;
    let name_size = u32_at(data, o)? as usize;
    let name = if name_size == 0 {
        None
    } else {
        Some(string_at(data, o + 4, name_size, limits)?)
    };
    Ok((name, o))
}

// The PC corpus only uses the classes above.

/// Computes the union coverage of `ranges` and the uncovered ranges within `[0, size)`.
fn coverage(ranges: &[Span], size: usize) -> (usize, Vec<Span>) {
    let mut sorted: Vec<Span> = ranges
        .iter()
        .copied()
        .filter(|s| !s.is_empty() && s.start < size)
        .map(|s| Span {
            start: s.start,
            end: s.end.min(size),
        })
        .collect();
    sorted.sort_by_key(|s| (s.start, s.end));

    let mut covered = 0usize;
    let mut gaps = Vec::new();
    let mut cursor = 0usize;
    for s in sorted {
        if s.start > cursor {
            gaps.push(Span {
                start: cursor,
                end: s.start,
            });
        }
        if s.end > cursor {
            covered += s.end - cursor;
            cursor = s.end;
        }
    }
    if cursor < size {
        gaps.push(Span {
            start: cursor,
            end: size,
        });
    }
    (covered, gaps)
}

impl HxBank {
    /// Resolves an entry selector: a decimal index or a case-insensitive name.
    pub fn find_entry(&self, selector: &str) -> Option<usize> {
        if let Ok(i) = selector.parse::<usize>() {
            return (i < self.entries.len()).then_some(i);
        }
        self.entries.iter().find_map(|e| match &e.kind {
            HxKind::Wave(w)
                if w.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(selector)) =>
            {
                Some(e.index)
            }
            HxKind::WavRes(w)
                if w.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(selector)) =>
            {
                self.resolve_link(e)
            }
            _ => None,
        })
    }

    /// Follows a `WavRes` record's wave references to the wave entry index.
    fn resolve_link(&self, entry: &HxEntry) -> Option<usize> {
        let ids = match &entry.kind {
            HxKind::WavRes(w) => w.wave_ids(),
            HxKind::Program(p) => p.links.clone(),
            _ => return None,
        };
        for link in &ids {
            if let Some(target) = self.entries.iter().find(|e| e.cuuid == *link)
                && target.as_wave().is_some()
            {
                return Some(target.index);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;
