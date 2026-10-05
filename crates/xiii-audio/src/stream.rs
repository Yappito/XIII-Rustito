//! Chunked, bounded decoding of a resolved bank entry, so long streamed music can be decoded
//! frame-by-frame on the calling thread instead of decoding a whole entry into memory at once.
//!
//! [`WaveStream`] wraps either an internal PCM range or a streamed UBI ADPCM range and yields
//! interleaved PCM16 in caller-sized chunks. For UBI ADPCM it delegates to
//! [`crate::adpcm::AdpcmStream`], whose per-frame state blocks make frame-boundary splitting
//! bit-exact with a whole-file [`crate::decode_entry`] (tested in `stream_tests`).

use std::sync::Arc;

use crate::adpcm::AdpcmStream;
use crate::error::{AudioError, AudioErrorKind, Result};
use crate::{Codec, DataLocation, HxBank, PcmAudio, WaveSpec};

/// One decoded chunk of interleaved samples plus its frame count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleChunk {
    /// Interleaved samples (`len == frames * channels`).
    pub samples: Vec<i16>,
    /// Frames (one sample per channel) in this chunk.
    pub frames: usize,
}

/// A chunked decoder over one wave entry's sample bytes.
pub struct WaveStream {
    spec: WaveSpec,
    /// Sample bytes: the whole `.hsc` file (ADPCM) or the bank (PCM), shared.
    bytes: Arc<[u8]>,
    /// Start of the entry's data within `bytes`.
    offset: usize,
    /// Length of the entry's data within `bytes`.
    len: usize,
    /// ADPCM decoder when streamed; `None` for PCM.
    adpcm: Option<AdpcmStream>,
    /// Read cursor within `bytes` for PCM.
    pos: usize,
    /// Total interleaved samples declared.
    total_samples: usize,
    /// Interleaved samples already produced.
    produced: usize,
}

impl WaveStream {
    /// Builds a chunked decoder for a wave entry. `bytes` must be the bank bytes when the entry
    /// is internal, or the `.hsc` file bytes when external; `offset`/`len` select the entry's
    /// sample region within `bytes`.
    pub fn new(spec: WaveSpec, bytes: Arc<[u8]>, offset: usize, len: usize) -> Result<Self> {
        let end = offset.checked_add(len).ok_or_else(|| {
            AudioError::new(AudioErrorKind::BadSize, "stream region offset+len overflow")
        })?;
        if end > bytes.len() {
            return Err(AudioError::at(
                AudioErrorKind::Truncated,
                offset,
                format!("stream region {}..{} past end {}", offset, end, bytes.len()),
            ));
        }
        if spec.channels == 0 || spec.sample_rate == 0 {
            return Err(AudioError::new(
                AudioErrorKind::BadSize,
                format!(
                    "stream channels {} / sample rate {} must be non-zero",
                    spec.channels, spec.sample_rate
                ),
            ));
        }
        let (adpcm, total_samples) = match spec.codec {
            Codec::Pcm => (None, len / 2),
            Codec::UbiAdpcm => {
                let region = &bytes[offset..end];
                let stream = AdpcmStream::new(region, spec.channels, spec.sample_rate)?;
                let total = stream.total_samples();
                (Some(stream), total)
            }
            Codec::Other(id) => {
                return Err(AudioError::at(
                    AudioErrorKind::UnsupportedCodec,
                    offset,
                    format!("stream codec id {id:#x}"),
                ));
            }
        };
        Ok(Self {
            spec,
            bytes,
            offset,
            len,
            adpcm,
            pos: offset,
            total_samples,
            produced: 0,
        })
    }

    /// Channel count.
    pub fn channels(&self) -> u16 {
        self.spec.channels
    }

    /// Sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.spec.sample_rate
    }

    /// Total frames declared by the entry.
    pub fn total_frames(&self) -> usize {
        let ch = self.spec.channels.max(1) as usize;
        self.total_samples / ch
    }

    /// Frames produced so far.
    pub fn produced_frames(&self) -> usize {
        let ch = self.spec.channels.max(1) as usize;
        self.produced / ch
    }

    /// True when every declared frame has been produced.
    pub fn is_finished(&self) -> bool {
        self.produced >= self.total_samples
    }

    /// The pieces needed to build a fresh decoder over the same entry (shared sample bytes): the
    /// wave spec, the shared bytes and the entry's `(offset, len)`.
    pub fn parts(&self) -> (WaveSpec, Arc<[u8]>, usize, usize) {
        (
            self.spec.clone(),
            Arc::clone(&self.bytes),
            self.offset,
            self.len,
        )
    }

    /// Restarts from the beginning (used for looping playback).
    pub fn rewind(&mut self) {
        self.pos = self.offset;
        self.produced = 0;
        if let Some(s) = self.adpcm.as_mut() {
            s.rewind();
        }
    }

    /// Decodes up to `max_frames` frames (never splits a frame). Returns an empty chunk at the end.
    pub fn next_chunk(&mut self, max_frames: usize) -> Result<SampleChunk> {
        let ch = self.spec.channels.max(1) as usize;
        if self.produced >= self.total_samples {
            return Ok(SampleChunk {
                samples: Vec::new(),
                frames: 0,
            });
        }
        if let Some(adpcm) = self.adpcm.as_mut() {
            let region = &self.bytes[self.offset..self.offset + self.len];
            let want = max_frames * ch;
            let mut samples = adpcm.next_samples(region, want)?;
            // Round to whole frames (next_samples already rounds; guard for the last partial).
            let frames = samples.len() / ch;
            samples.truncate(frames * ch);
            self.produced += samples.len();
            return Ok(SampleChunk { samples, frames });
        }
        // Internal PCM: a plain bounded read.
        let remaining = self.total_samples - self.produced;
        let want = (max_frames * ch).min(remaining);
        let start = self.pos;
        let byte_end = start + want * 2;
        let raw = self.bytes.get(start..byte_end).ok_or_else(|| {
            AudioError::at(
                AudioErrorKind::Truncated,
                start,
                format!("PCM chunk {}..{} past end", start, byte_end),
            )
        })?;
        let samples: Vec<i16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        self.pos = byte_end;
        self.produced += samples.len();
        let frames = samples.len() / ch;
        Ok(SampleChunk { samples, frames })
    }
}

/// Locates the sample region of an entry within the appropriate byte buffer: `(offset, len)`.
/// Internal entries resolve against the bank; external entries against the `.hsc` file.
pub fn entry_region(spec: &WaveSpec) -> Option<(usize, usize)> {
    match &spec.data {
        DataLocation::Internal(span) => Some((span.start, span.end.saturating_sub(span.start))),
        DataLocation::External { offset, size } => Some((*offset, *size)),
    }
}

/// Builds a [`WaveStream`] for a bank entry. `bank_bytes` must be the bytes the bank was parsed
/// from; `external` must be the `.hsc` bytes when the entry is external.
pub fn stream_entry(
    bank: &HxBank,
    entry_index: usize,
    bank_bytes: &[u8],
    external: Option<Arc<[u8]>>,
) -> Result<WaveStream> {
    let spec = crate::entry_spec(bank, entry_index)?;
    let (offset, len) = entry_region(&spec).ok_or_else(|| {
        AudioError::new(AudioErrorKind::NoSuchEntry, "entry has no sample region")
    })?;
    match spec.data {
        DataLocation::Internal(_) => {
            let shared: Arc<[u8]> = Arc::from(bank_bytes.to_vec());
            WaveStream::new(spec, shared, offset, len)
        }
        DataLocation::External { .. } => {
            let external = external.ok_or_else(|| {
                AudioError::new(
                    AudioErrorKind::ExternalData,
                    format!("entry {entry_index} is external; stream bytes required"),
                )
            })?;
            WaveStream::new(spec, external, offset, len)
        }
    }
}

/// Decodes a whole [`WaveStream`] to one [`PcmAudio`] (the chunked path, for tests/validation).
pub fn decode_stream(mut stream: WaveStream, chunk_frames: usize) -> Result<PcmAudio> {
    let channels = stream.channels();
    let sample_rate = stream.sample_rate();
    let mut samples = Vec::with_capacity(stream.total_frames() * channels as usize);
    loop {
        let chunk = stream.next_chunk(chunk_frames)?;
        if chunk.frames == 0 {
            break;
        }
        samples.extend_from_slice(&chunk.samples);
    }
    Ok(PcmAudio {
        channels,
        sample_rate,
        samples,
    })
}

#[cfg(test)]
mod stream_tests;
