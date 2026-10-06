//! Clean-room Bink 1 (revision `i`) reader and video decoder for XIII Classic cutscenes.
//!
//! The crate is dependency-free and never touches the network. It reads the fixed decoder
//! tables from the user's own installation (`system/binkw32.dll`) at runtime, by structural
//! signature, rather than embedding them; see [`tables::BinkTables::from_install`].
//!
//! ```
//! # fn demo(game_dir: &std::path::Path, file: &[u8]) -> Result<(), xiii_video::VideoError> {
//! let tables = xiii_video::BinkTables::from_install(game_dir)?;
//! let bik = xiii_video::container::BikFile::parse(file)?;
//! let decoder = xiii_video::Decoder::new(tables);
//! let packets = bik.frame_packets(file, 0)?;
//! let video = &file[packets.video_offset as usize
//!     ..(packets.video_offset + packets.video_size) as usize];
//! let (frame, stats) = decoder.decode_frame(video, &bik.header, None)?;
//! println!("{}x{} bits {} / {}", frame.width, frame.height, stats.bits_used, stats.total_bits());
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

pub mod bitreader;
pub mod container;
pub mod decoder;
pub mod error;
pub mod huffman;
pub mod tables;

pub use container::{BikFile, FramePackets, Header};
pub use decoder::{Decoder, FrameStats, YuvFrame};
pub use error::{Result, VideoError, VideoErrorKind};
pub use tables::BinkTables;

impl FrameStats {
    /// Total bit capacity of the packet this statistics record belongs to.
    pub fn total_bits(&self) -> usize {
        self.bits_total
    }
}

/// Reads one frame's video payload from a whole-file buffer.
pub fn frame_video<'a>(file: &'a [u8], bik: &BikFile, index: usize) -> Result<&'a [u8]> {
    let packets = bik.frame_packets(file, index)?;
    let start = packets.video_offset as usize;
    let end = start + packets.video_size as usize;
    file.get(start..end).ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::Truncated,
            format!("frame {index} video range {start}..{end} past buffer"),
        )
    })
}
