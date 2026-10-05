//! Bounded, filesystem-free reader/decoder for XIII (Ubisoft HXAudio) audio.
//!
//! The crate parses `.hxc` banks ([`hx`]), decodes the two codecs present on the PC corpus
//! (16-bit PCM in the bank and 6-bit Ubisoft ADPCM in the streamed `.hsc` companion via
//! [`adpcm`]) and writes PCM16 WAV files ([`wav`]). It never touches the filesystem: callers
//! read the bytes and pass them in, mirroring `xiii-package`.
//!
//! ```
//! # fn demo(bank_bytes: &[u8]) -> Result<(), xiii_audio::AudioError> {
//! let bank = xiii_audio::hx::parse_bank(bank_bytes, &xiii_audio::hx::HxLimits::default())?;
//! for entry in &bank.entries {
//!     if let Some(wave) = entry.as_wave() {
//!         println!("{:?} {} {} ch {} Hz", wave.name, wave.codec.as_str(), wave.channels, wave.sample_rate);
//!     }
//! }
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

pub mod adpcm;
pub mod attenuation;
pub mod error;
pub mod hx;
pub mod library;
pub mod stream;
pub mod wav;

pub use attenuation::Attenuation;
pub use error::{AudioError, AudioErrorKind, Result};
pub use hx::{
    Codec, Cuuid, DataLocation, HxBank, HxEntry, HxKind, HxLimits, SoundRef, Span, WaveResource,
};
pub use library::{
    BankEntryRef, LibraryStats, ResolutionRule, ResolutionSummary, ResolveFailure, ResolvedSound,
    SoundLibrary,
};
pub use stream::{SampleChunk, WaveStream, decode_stream, entry_region, stream_entry};
pub use wav::write_wav;

/// Decoded PCM16 audio, interleaved by channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmAudio {
    /// Channel count.
    pub channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Interleaved 16-bit samples (`samples.len()` is `frames * channels`).
    pub samples: Vec<i16>,
}

impl PcmAudio {
    /// Number of frames (one sample per channel).
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    /// Number of samples in one channel.
    pub fn per_channel(&self) -> usize {
        self.frames()
    }

    /// Peak absolute sample value, for clipping checks.
    pub fn peak(&self) -> i32 {
        self.samples
            .iter()
            .map(|s| (*s as i32).abs())
            .max()
            .unwrap_or(0)
    }
}

/// Decodes 16-bit little-endian interleaved PCM.
pub fn decode_pcm16(data: &[u8], channels: u16, sample_rate: u32) -> Result<PcmAudio> {
    if channels == 0 {
        return Err(AudioError::new(
            AudioErrorKind::BadSize,
            "PCM channel count is zero",
        ));
    }
    if !data.len().is_multiple_of(2) {
        return Err(AudioError::at(
            AudioErrorKind::BadSize,
            0,
            format!("PCM byte length {} is odd", data.len()),
        ));
    }
    let samples: Vec<i16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect();
    Ok(PcmAudio {
        channels,
        sample_rate,
        samples,
    })
}

/// Metadata of one wave entry: codec, channels, sample rate and where the samples live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveSpec {
    /// Codec of the sample bytes.
    pub codec: Codec,
    /// Channel count.
    pub channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Where the sample bytes live (in this bank or a sibling `.hsc`).
    pub data: DataLocation,
}

/// Looks up one wave entry of a parsed bank and returns its decoder metadata.
pub fn entry_spec(bank: &HxBank, entry_index: usize) -> Result<WaveSpec> {
    let entry = bank.entries.get(entry_index).ok_or_else(|| {
        AudioError::new(
            AudioErrorKind::NoSuchEntry,
            format!("entry {entry_index} of {}", bank.entries.len()),
        )
    })?;
    let wave = entry.as_wave().ok_or_else(|| {
        AudioError::new(
            AudioErrorKind::NoSuchEntry,
            format!("entry {entry_index} is {} not a wave", entry.class_name()),
        )
    })?;
    Ok(WaveSpec {
        codec: wave.codec,
        channels: wave.channels,
        sample_rate: wave.sample_rate,
        data: wave.data.clone(),
    })
}

/// Decodes one wave entry of a parsed bank.
///
/// `bank_bytes` must be the bytes `bank` was parsed from. `external` must be the `.hsc` stream
/// data when the entry is external; it is ignored otherwise.
pub fn decode_entry(
    bank: &HxBank,
    entry_index: usize,
    bank_bytes: &[u8],
    external: Option<&[u8]>,
) -> Result<PcmAudio> {
    let entry = bank.entries.get(entry_index).ok_or_else(|| {
        AudioError::new(
            AudioErrorKind::NoSuchEntry,
            format!("entry {entry_index} of {}", bank.entries.len()),
        )
    })?;
    let wave = entry.as_wave().ok_or_else(|| {
        AudioError::new(
            AudioErrorKind::NoSuchEntry,
            format!("entry {entry_index} is {} not a wave", entry.class_name()),
        )
    })?;

    match &wave.data {
        DataLocation::Internal(span) => {
            let data = bank_bytes.get(span.start..span.end).ok_or_else(|| {
                AudioError::at(
                    AudioErrorKind::BadSize,
                    span.start,
                    format!("internal data {}..{} past end", span.start, span.end),
                )
            })?;
            match wave.codec {
                Codec::Pcm => decode_pcm16(data, wave.channels, wave.sample_rate),
                other => Err(AudioError::at(
                    AudioErrorKind::UnsupportedCodec,
                    span.start,
                    format!("internal codec {}", other.as_str()),
                )),
            }
        }
        DataLocation::External { offset, size } => {
            let external = external.ok_or_else(|| {
                AudioError::new(
                    AudioErrorKind::ExternalData,
                    format!(
                        "entry {entry_index} is external ({}); stream bytes required",
                        wave.resource_name.as_deref().unwrap_or("?")
                    ),
                )
            })?;
            let end = offset.checked_add(*size).ok_or_else(|| {
                AudioError::at(AudioErrorKind::BadSize, *offset, "external size overflow")
            })?;
            let data = external.get(*offset..end).ok_or_else(|| {
                AudioError::at(
                    AudioErrorKind::ExternalData,
                    *offset,
                    format!(
                        "external range {}..{} past end {}",
                        offset,
                        end,
                        external.len()
                    ),
                )
            })?;
            match wave.codec {
                Codec::UbiAdpcm => adpcm::decode(data, wave.channels, wave.sample_rate),
                other => Err(AudioError::at(
                    AudioErrorKind::UnsupportedCodec,
                    *offset,
                    format!("external codec {}", other.as_str()),
                )),
            }
        }
    }
}
