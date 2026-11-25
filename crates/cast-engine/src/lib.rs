//! Kalango Cast's recording engine: one recording at a time, from a screencast source (monitor, window or area)
//! and the sound (desktop, microphone) to a crash-safe fragmented MP4, on one media clock.
//!
//! Threads: the video capture thread (PipeWire `process` → GPU import/blit → hardware encode → MP4 sample), the
//! audio thread (PipeWire → Opus), and the mux thread (writes fragments, syncs them). The hot path does not
//! allocate per frame: encoder output, MP4 samples and packets reuse their buffers (a small pool cycles between the
//! encode and mux threads).
//!
//! The daemon (`kalango-castd`) serves it over JSON-RPC on a Unix socket ([`rpc`]); the CLI (`kalango-cast`) can
//! run it in-process.

pub mod recorder;
pub mod rpc;
pub mod stats;

pub use recorder::{AudioChoice, RecordOptions, Recorder};
pub use stats::Status;
