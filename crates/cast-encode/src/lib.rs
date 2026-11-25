//! Kalango Cast's encoders, behind one trait ([`VideoEncoder`]) and one capability probe ([`probe`]):
//!
//! - **NVENC** (NVIDIA): libnvidia-encode loaded at run time; frames stay on the GPU (a DMA-BUF imported as a GL
//!   texture, scaled/cropped by a GPU blit, read by NVENC through its OpenGL interop). H.264 and HEVC.
//! - **VA-API** (AMD, Intel): probed through libva (loaded at run time); encoding through it comes in phase 2.
//! - **OpenH264** (software, last resort): Cisco's binary, downloaded on demand and checked against a pinned
//!   SHA-256 ([`openh264::download`]); never shipped with Kalango Cast.
//!
//! Audio: [`opus`] (libopus, loaded at run time).
//!
//! Every encoder takes frames on the capture thread (the GL context lives there) and writes Annex B access units.

pub mod convert;
pub mod gl;
pub mod nvenc;
pub mod openh264;
pub mod opus;
pub mod vaapi;

use cast_core::{EncoderKind, Rect, VideoCodec, VideoFrame};
use serde::Serialize;

pub use gl::GlContext;

#[derive(Debug, Clone)]
pub struct EncodeError(pub String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EncodeError {}

impl From<gl::GlError> for EncodeError {
    fn from(e: gl::GlError) -> Self {
        EncodeError(e.0)
    }
}

/// What an encoder session is asked to produce.
#[derive(Clone, Debug)]
pub struct Settings {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Nominal frame rate (screen capture is variable; this sizes the GOP and the rate control).
    pub fps: u32,
    /// Average bitrate; 0 picks one from the size and frame rate.
    pub bitrate_kbps: u32,
    /// Seconds between keyframes (also the MP4 fragment length); the caller forces them by time
    /// ([`VideoEncoder::force_keyframe`]), the encoders' own GOP is open-ended.
    pub keyframe_interval_s: u32,
}

impl Settings {
    /// The bitrate used: the asked one, or ~0.1 bit per pixel per frame (12 Mbit/s at 1080p60), what screen content
    /// needs for sharp text.
    pub fn bitrate_bps(&self) -> u32 {
        if self.bitrate_kbps > 0 {
            return self.bitrate_kbps * 1000;
        }
        let bpp = (self.width as u64 * self.height as u64 * self.fps.max(1) as u64) / 10;
        bpp.clamp(2_000_000, 80_000_000) as u32
    }

    pub fn gop(&self) -> u32 {
        self.fps.max(1) * self.keyframe_interval_s.max(1)
    }
}

/// An encoded access unit (Annex B). `data` keeps its capacity between frames: no allocation per frame.
#[derive(Default, Debug)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct EncoderInfo {
    pub kind: EncoderKind,
    pub codec: VideoCodec,
    /// The device or library (GPU name, library version).
    pub device: String,
    pub hardware: bool,
    /// Frames reach the encoder without passing through the CPU.
    pub zero_copy: bool,
}

pub trait VideoEncoder {
    fn info(&self) -> &EncoderInfo;
    /// The parameter sets (SPS/PPS, and VPS for HEVC) in Annex B, for the MP4 sample description.
    fn parameter_sets(&self) -> &[u8];
    /// Encodes a frame (its `crop`, scaled into the session's size, keeping the aspect ratio).
    fn encode(&mut self, frame: &VideoFrame, crop: Option<Rect>, pts_ns: u64, out: &mut EncodedFrame) -> Result<(), EncodeError>;
    /// The next frame is a keyframe.
    fn force_keyframe(&mut self);
}

/// One encoder family on this machine.
#[derive(Clone, Debug, Serialize)]
pub struct Capability {
    pub kind: EncoderKind,
    pub available: bool,
    pub hardware: bool,
    pub codecs: Vec<VideoCodec>,
    pub device: String,
    /// Why it is not available (for people, translated).
    pub reason: Option<String>,
}

/// What this machine can encode with, best first.
pub fn probe() -> Vec<Capability> {
    vec![nvenc::probe(), vaapi::probe(), openh264::probe()]
}

/// The best available encoder kind for a codec (the asked one if given and available).
pub fn choose(caps: &[Capability], wanted: Option<EncoderKind>, codec: VideoCodec) -> Option<EncoderKind> {
    let usable = |c: &&Capability| c.available && c.codecs.contains(&codec) && c.kind != EncoderKind::Vaapi;
    match wanted {
        Some(k) => caps.iter().filter(usable).find(|c| c.kind == k).map(|c| c.kind),
        None => caps.iter().find(usable).map(|c| c.kind),
    }
}

/// Opens an encoder session. `gl` is the capture thread's GL context (required by NVENC; used by OpenH264 to read
/// DMA-BUF frames back).
pub fn open(kind: EncoderKind, settings: &Settings, gl: Option<GlContext>) -> Result<Box<dyn VideoEncoder>, EncodeError> {
    match kind {
        EncoderKind::Nvenc => {
            let gl = gl.ok_or_else(|| EncodeError(cast_core::tr("NVENC needs the GPU's OpenGL context").into()))?;
            Ok(Box::new(nvenc::NvencEncoder::open(settings, gl)?))
        }
        EncoderKind::Openh264 => Ok(Box::new(openh264::OpenH264Encoder::open(settings, gl)?)),
        EncoderKind::Vaapi => Err(EncodeError(cast_core::tr("VA-API encoding is not available yet").into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bitrate_scales_with_pixels_and_rate() {
        let s = Settings { codec: VideoCodec::H264, width: 1920, height: 1080, fps: 60, bitrate_kbps: 0, keyframe_interval_s: 2 };
        assert_eq!(s.bitrate_bps(), 12_441_600);
        assert_eq!(s.gop(), 120);
        let small = Settings { width: 320, height: 240, fps: 30, ..s.clone() };
        assert_eq!(small.bitrate_bps(), 2_000_000, "floor");
        let asked = Settings { bitrate_kbps: 6000, ..s };
        assert_eq!(asked.bitrate_bps(), 6_000_000);
    }

    #[test]
    fn choose_prefers_hardware_and_honours_the_asked_kind() {
        let cap =
            |kind, available, hw| Capability { kind, available, hardware: hw, codecs: vec![VideoCodec::H264], device: String::new(), reason: None };
        let caps = vec![cap(EncoderKind::Nvenc, true, true), cap(EncoderKind::Vaapi, false, true), cap(EncoderKind::Openh264, true, false)];
        assert_eq!(choose(&caps, None, VideoCodec::H264), Some(EncoderKind::Nvenc));
        assert_eq!(choose(&caps, Some(EncoderKind::Openh264), VideoCodec::H264), Some(EncoderKind::Openh264));
        assert_eq!(choose(&caps, Some(EncoderKind::Vaapi), VideoCodec::H264), None);
        assert_eq!(choose(&caps, None, VideoCodec::Hevc), None, "nobody listed HEVC");
    }
}
