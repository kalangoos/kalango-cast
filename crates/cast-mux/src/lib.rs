//! Crash-safe fragmented MP4 (ISO BMFF) for Kalango Cast's recordings.
//!
//! The file is `ftyp` + `moov` first (the codec descriptions are known before the first frame), then one `moof` +
//! `mdat` pair per fragment (~2 s, each video fragment starting on a keyframe). Any prefix of the file that ends on a
//! fragment boundary is a complete, playable MP4: a crash, a full disk or a pulled cable loses at most the fragment
//! being written.
//!
//! The design follows isocast-mux (anomalyco/isocast, MIT OR Apache-2.0; see NOTICE): a fragment's index is first
//! written as a `free` box and only becomes a `moof` once its media data is on disk, so a torn fragment is skipped by
//! players instead of pointing at missing bytes. This crate is a new, smaller implementation for H.264/HEVC + Opus
//! with no code copied.
//!
//! [`avc`] turns encoders' Annex B output into MP4 samples and builds `avcC`/`hvcC`; [`inspect`] reads a file
//! back (tests, the CLI's report, recovery checks).

pub mod avc;
mod boxes;
pub mod inspect;
mod writer;

pub use writer::{AudioTrack, Config, Summary, VideoTrack, Writer, AUDIO_TIMESCALE, VIDEO_TIMESCALE};
