//! Opus audio through libopus (BSD-3-Clause, loaded at run time: every Ubuntu has libopus0).
//!
//! Why Opus and not AAC: Opus is royalty-free under a permissive licence with the reference encoder in every distro,
//! is better than AAC-LC at the same bitrate, and has 20 ms frames with 6.5 ms of lookahead (low latency for the
//! live phases). AAC would need FDK-AAC (a licence outside the accepted list, and patent-encumbered) or ffmpeg's
//! encoder (LGPL, through a library Kalango Cast does not use). Opus-in-MP4 (the ISO BMFF mapping) plays in
//! browsers, ffmpeg, VLC, mpv and DaVinci Resolve; for editors that only take AAC (Premiere, Final Cut), the export
//! phase transcodes.

use std::ffi::{c_int, c_void};

use libloading::Library;

use crate::EncodeError;

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms frames.
pub const FRAME_SAMPLES: usize = 960;
/// The largest packet asked for (RFC 6716 allows up to 1275 bytes per frame).
pub const MAX_PACKET: usize = 1500;

const OPUS_APPLICATION_AUDIO: c_int = 2049;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;

type Create = unsafe extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut c_void;
type Ctl = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type EncodeFloat = unsafe extern "C" fn(*mut c_void, *const f32, c_int, *mut u8, i32) -> i32;
type Destroy = unsafe extern "C" fn(*mut c_void);

pub struct OpusEncoder {
    _lib: Library,
    encode: EncodeFloat,
    destroy: Destroy,
    enc: *mut c_void,
    channels: usize,
    /// Samples the decoder must drop at the start (the encoder's lookahead).
    pub pre_skip: u16,
}

// SAFETY: an OpusEncoder is a plain heap state with no thread affinity; it is used by one thread at a time (&mut).
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(channels: u8, bitrate: i32) -> Result<OpusEncoder, EncodeError> {
        // SAFETY: libopus has no initialisers with side effects.
        let lib = unsafe { Library::new("libopus.so.0") }.map_err(|e| EncodeError(format!("libopus: {e}")))?;
        // SAFETY: the functions of opus.h with their exact signatures (opus_encoder_ctl is variadic).
        let (create, ctl, encode, destroy) = unsafe {
            (
                *lib.get::<Create>(b"opus_encoder_create\0").map_err(|e| EncodeError(e.to_string()))?,
                *lib.get::<Ctl>(b"opus_encoder_ctl\0").map_err(|e| EncodeError(e.to_string()))?,
                *lib.get::<EncodeFloat>(b"opus_encode_float\0").map_err(|e| EncodeError(e.to_string()))?,
                *lib.get::<Destroy>(b"opus_encoder_destroy\0").map_err(|e| EncodeError(e.to_string()))?,
            )
        };
        let mut error = 0;
        // SAFETY: valid rate/channels/application; error receives the status.
        let enc = unsafe { create(SAMPLE_RATE as i32, channels as c_int, OPUS_APPLICATION_AUDIO, &mut error) };
        if enc.is_null() || error != 0 {
            return Err(EncodeError(format!("opus_encoder_create: {error}")));
        }
        let mut lookahead: i32 = 312;
        // SAFETY: the documented ctl requests: SET takes an opus_int32 by value, GET a pointer to one.
        unsafe {
            ctl(enc, OPUS_SET_BITRATE_REQUEST, bitrate);
            ctl(enc, OPUS_GET_LOOKAHEAD_REQUEST, &mut lookahead as *mut i32);
        }
        Ok(OpusEncoder { _lib: lib, encode, destroy, enc, channels: channels as usize, pre_skip: lookahead.clamp(0, u16::MAX as i32) as u16 })
    }

    /// Encodes one 20 ms frame (`FRAME_SAMPLES` × channels interleaved floats) into `out` (cleared first).
    pub fn encode_frame(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<(), EncodeError> {
        if pcm.len() != FRAME_SAMPLES * self.channels {
            return Err(EncodeError("opus: wrong frame size".into()));
        }
        out.resize(MAX_PACKET, 0);
        // SAFETY: pcm holds one frame for the configured channels; out has MAX_PACKET bytes of room.
        let n = unsafe { (self.encode)(self.enc, pcm.as_ptr(), FRAME_SAMPLES as c_int, out.as_mut_ptr(), MAX_PACKET as i32) };
        if n < 0 {
            out.clear();
            return Err(EncodeError(format!("opus_encode_float: {n}")));
        }
        out.truncate(n as usize);
        Ok(())
    }

    pub fn channels(&self) -> usize {
        self.channels
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: the encoder is ours and not used after this.
        unsafe { (self.destroy)(self.enc) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_tone_into_small_packets() {
        let mut enc = OpusEncoder::new(2, 128_000).expect("libopus is installed (libopus0)");
        assert!(enc.pre_skip > 0 && enc.pre_skip < 1000, "lookahead {}", enc.pre_skip);
        let mut out = Vec::new();
        let mut total = 0;
        for f in 0..50 {
            let pcm: Vec<f32> = (0..FRAME_SAMPLES)
                .flat_map(|i| {
                    let t = (f * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                    let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
                    [s, s]
                })
                .collect();
            enc.encode_frame(&pcm, &mut out).unwrap();
            assert!(!out.is_empty() && out.len() <= MAX_PACKET);
            total += out.len();
        }
        // 1 s at 128 kbit/s is ~16 KB
        assert!((8_000..32_000).contains(&total), "{total} bytes");
        assert!(enc.encode_frame(&[0.0; 10], &mut out).is_err());
    }
}
