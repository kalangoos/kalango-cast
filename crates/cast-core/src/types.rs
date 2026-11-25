//! Types shared by the capture, encode, mux and engine crates.

use std::os::fd::RawFd;

use serde::{Deserialize, Serialize};

/// What to record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A whole monitor, by connector name (`DP-1`, `winit`).
    Monitor { connector: String },
    /// One window, by the compositor's window id.
    Window { id: u64 },
    /// A rectangle of a monitor (in the monitor's physical pixels); recorded as the monitor's stream, cropped on the
    /// GPU before encoding.
    Area { connector: String, x: u32, y: u32, width: u32, height: u32 },
}

impl Source {
    pub fn crop(&self) -> Option<Rect> {
        match self {
            Source::Area { x, y, width, height, .. } => Some(Rect { x: *x, y: *y, width: *width, height: *height }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// The rectangle clipped to a `width` × `height` frame, with even sizes (4:2:0 video needs them); None if
    /// nothing is left.
    pub fn clipped(&self, width: u32, height: u32) -> Option<Rect> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let w = self.width.min(width - x) & !1;
        let h = self.height.min(height - y) & !1;
        (w > 0 && h > 0).then_some(Rect { x, y, width: w, height: h })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
}

/// The encoder families, in the order they are preferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncoderKind {
    /// NVIDIA's hardware encoder (libnvidia-encode, loaded at run time).
    Nvenc,
    /// VA-API hardware encoders (AMD, Intel; libva, loaded at run time).
    Vaapi,
    /// Cisco's OpenH264 binary, downloaded on demand: software, the last resort.
    Openh264,
}

impl EncoderKind {
    pub fn name(&self) -> &'static str {
        match self {
            EncoderKind::Nvenc => "nvenc",
            EncoderKind::Vaapi => "vaapi",
            EncoderKind::Openh264 => "openh264",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nvenc" => Some(EncoderKind::Nvenc),
            "vaapi" => Some(EncoderKind::Vaapi),
            "openh264" => Some(EncoderKind::Openh264),
            _ => None,
        }
    }
}

/// DRM fourcc codes (linux/drm_fourcc.h) of the formats screencasts come in.
pub mod fourcc {
    const fn code(a: u8, b: u8, c: u8, d: u8) -> u32 {
        a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
    }
    /// `[31:0] x:R:G:B 8:8:8:8` little endian: bytes B, G, R, x in memory (PipeWire's BGRx).
    pub const XRGB8888: u32 = code(b'X', b'R', b'2', b'4');
    /// Like XRGB8888 with alpha (PipeWire's BGRA).
    pub const ARGB8888: u32 = code(b'A', b'R', b'2', b'4');
    /// Bytes R, G, B, x (PipeWire's RGBx).
    pub const XBGR8888: u32 = code(b'X', b'B', b'2', b'4');
    pub const ABGR8888: u32 = code(b'A', b'B', b'2', b'4');
}

/// DRM format modifiers (linux/drm_fourcc.h).
pub mod modifier {
    pub const LINEAR: u64 = 0;
    /// "The producer picks" (implicit modifier).
    pub const INVALID: u64 = 0x00ff_ffff_ffff_ffff;
}

/// One plane of a DMA-BUF frame; the fd belongs to the capture (PipeWire's buffer) and is valid during the call.
#[derive(Clone, Copy, Debug)]
pub struct DmaBufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
}

/// A frame that stays in GPU memory: the encoder imports it, no CPU copy.
#[derive(Clone, Copy, Debug)]
pub struct DmaBufFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: &'a [DmaBufPlane],
}

/// A frame in shared memory (the fallback when the compositor cannot give DMA-BUFs).
#[derive(Clone, Copy, Debug)]
pub struct ShmFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub enum VideoFrame<'a> {
    DmaBuf(DmaBufFrame<'a>),
    Shm(ShmFrame<'a>),
}

impl VideoFrame<'_> {
    pub fn size(&self) -> (u32, u32) {
        match self {
            VideoFrame::DmaBuf(f) => (f.width, f.height),
            VideoFrame::Shm(f) => (f.width, f.height),
        }
    }

    pub fn fourcc(&self) -> u32 {
        match self {
            VideoFrame::DmaBuf(f) => f.fourcc,
            VideoFrame::Shm(f) => f.fourcc,
        }
    }

    pub fn is_dmabuf(&self) -> bool {
        matches!(self, VideoFrame::DmaBuf(_))
    }
}

/// The two kinds of sound a recording takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioSource {
    /// What the computer plays (the default output's monitor).
    Desktop,
    /// The microphone (the default input).
    Mic,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_are_clipped_to_even_sizes_inside_the_frame() {
        let r = Rect { x: 100, y: 50, width: 801, height: 601 };
        assert_eq!(r.clipped(1920, 1080), Some(Rect { x: 100, y: 50, width: 800, height: 600 }));
        let out = Rect { x: 1900, y: 1000, width: 400, height: 400 };
        assert_eq!(out.clipped(1920, 1080), Some(Rect { x: 1900, y: 1000, width: 20, height: 80 }));
        assert_eq!(Rect { x: 1920, y: 0, width: 10, height: 10 }.clipped(1920, 1080), None);
    }

    #[test]
    fn sources_round_trip_as_json() {
        let s = Source::Area { connector: "DP-1".into(), x: 1, y: 2, width: 3, height: 4 };
        let j = serde_json::to_string(&s).unwrap();
        assert_eq!(j, r#"{"kind":"area","connector":"DP-1","x":1,"y":2,"width":3,"height":4}"#);
        assert_eq!(serde_json::from_str::<Source>(&j).unwrap(), s);
        assert_eq!(serde_json::from_str::<Source>(r#"{"kind":"window","id":7}"#).unwrap(), Source::Window { id: 7 });
    }

    #[test]
    fn fourcc_codes_match_drm() {
        assert_eq!(fourcc::XRGB8888, 0x3432_5258);
        assert_eq!(fourcc::ARGB8888, 0x3432_5241);
    }
}
