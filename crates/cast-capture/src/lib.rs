//! Kalango Cast's capture: the screen through the compositor's `org.gnome.Mutter.ScreenCast` D-Bus API (niri
//! implements it; the KalangoOS shell uses it the same way), whose PipeWire stream is consumed with DMA-BUF and
//! explicit modifiers first (frames stay on the GPU; the fds are handed on, never mapped) and shared memory as the
//! fallback; and the sound (desktop and microphone) as PipeWire capture streams.
//!
//! Every timestamp is `CLOCK_MONOTONIC` nanoseconds (see `cast_core::clock`).

pub mod audio;
pub mod guard;
pub mod screencast;
pub mod video;

pub use audio::{AudioCapture, AudioSink, AudioTarget};
pub use screencast::{Monitor, ScreenCast, Sources, Stream, Window};
pub use video::{StreamFormat, VideoCapture, VideoSink};

#[derive(Debug)]
pub struct CaptureError(pub String);

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CaptureError {}

impl From<zbus::Error> for CaptureError {
    fn from(e: zbus::Error) -> Self {
        CaptureError(format!("D-Bus: {e}"))
    }
}

impl From<pipewire::Error> for CaptureError {
    fn from(e: pipewire::Error) -> Self {
        CaptureError(format!("PipeWire: {e}"))
    }
}
