//! The software fallback: Cisco's OpenH264 (BSD-2-Clause source; the binary is Cisco's, downloaded on demand, see
//! [`download`]), loaded at run time through its C interface (codec_api.h, release 2.6.0). Frames are converted to
//! I420 on the CPU ([`crate::convert`]); DMA-BUF frames are first read back from the GPU.

pub mod download;

use std::ffi::{c_int, c_void};
use std::path::PathBuf;

use cast_core::{tr, EncoderKind, Rect, VideoCodec, VideoFrame};
use libloading::Library;

use crate::convert::{bgrx_to_i420, I420};
use crate::gl::GlContext;
use crate::{Capability, EncodeError, EncodedFrame, EncoderInfo, Settings, VideoEncoder};

// codec_app_def.h / codec_def.h
const SCREEN_CONTENT_REAL_TIME: c_int = 1;
const RC_BITRATE_MODE: c_int = 1;
const VIDEO_FORMAT_I420: c_int = 23;
const ENCODER_OPTION_DATAFORMAT: c_int = 0;
const ENCODER_OPTION_IDR_INTERVAL: c_int = 1;
const VIDEO_FRAME_TYPE_IDR: c_int = 1;
const VIDEO_FRAME_TYPE_SKIP: c_int = 4;
const MAX_LAYER_NUM_OF_FRAME: usize = 128;

#[repr(C)]
struct SEncParamBase {
    usage_type: c_int,
    pic_width: c_int,
    pic_height: c_int,
    target_bitrate: c_int,
    rc_mode: c_int,
    max_frame_rate: f32,
}

#[repr(C)]
struct SSourcePicture {
    color_format: c_int,
    stride: [c_int; 4],
    data: [*mut u8; 4],
    pic_width: c_int,
    pic_height: c_int,
    time_stamp: i64,
    psnr_y: bool,
    psnr_u: bool,
    psnr_v: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SLayerBSInfo {
    temporal_id: u8,
    spatial_id: u8,
    quality_id: u8,
    frame_type: c_int,
    layer_type: u8,
    sub_seq_id: c_int,
    nal_count: c_int,
    nal_length_in_byte: *mut c_int,
    bs_buf: *mut u8,
    psnr: [f32; 3],
}

#[repr(C)]
struct SFrameBSInfo {
    layer_num: c_int,
    layers: [SLayerBSInfo; MAX_LAYER_NUM_OF_FRAME],
    frame_type: c_int,
    frame_size_in_bytes: c_int,
    time_stamp: i64,
}

/// `ISVCEncoder` is a pointer to this table (the C interface of codec_api.h).
#[repr(C)]
struct ISVCEncoderVtbl {
    initialize: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl, *const SEncParamBase) -> c_int,
    initialize_ext: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl, *const c_void) -> c_int,
    get_default_params: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl, *mut c_void) -> c_int,
    uninitialize: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl) -> c_int,
    encode_frame: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl, *const SSourcePicture, *mut SFrameBSInfo) -> c_int,
    encode_parameter_sets: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl, *mut SFrameBSInfo) -> c_int,
    force_intra_frame: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl, bool) -> c_int,
    set_option: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl, c_int, *mut c_void) -> c_int,
    get_option: unsafe extern "C" fn(*mut *const ISVCEncoderVtbl, c_int, *mut c_void) -> c_int,
}

type CreateEncoder = unsafe extern "C" fn(*mut *mut *const ISVCEncoderVtbl) -> c_int;
type DestroyEncoder = unsafe extern "C" fn(*mut *const ISVCEncoderVtbl);

/// Where the library should be: `KALANGO_CAST_OPENH264`, or the cache.
fn candidate_path() -> Option<PathBuf> {
    match std::env::var_os("KALANGO_CAST_OPENH264") {
        Some(p) => Some(PathBuf::from(p)),
        None => Some(download::cache_dir()?.join(download::RELEASE.file_name)),
    }
}

/// The library path, if what is there matches the pinned checksum (for probing; loading uses [`load_library`], which
/// does not trust the path again).
pub fn library_path() -> Option<PathBuf> {
    let p = candidate_path()?;
    download::read_checked(&p, download::RELEASE.sha256_so).map(|_| p)
}

/// Loads the library from the very bytes that were checked: the file is opened and hashed once, the bytes go to an
/// in-memory file, and that is what `dlopen` gets (no window between verifying and loading).
fn load_library() -> Result<Library, EncodeError> {
    let path = candidate_path().ok_or_else(|| EncodeError(tr("OpenH264 is not downloaded").into()))?;
    let bytes = download::read_checked(&path, download::RELEASE.sha256_so).ok_or_else(|| EncodeError(tr("OpenH264 is not downloaded").into()))?;
    let mem = download::memfd_copy(&bytes).map_err(|e| EncodeError(e.to_string()))?;
    let via = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&mem));
    // SAFETY: the bytes match the pinned SHA-256 of Cisco's release (read_checked), and they are the bytes loaded.
    unsafe { Library::new(&via) }.map_err(|e| EncodeError(e.to_string()))
}

pub fn probe() -> Capability {
    let (available, reason) = match library_path() {
        Some(_) => (true, None),
        None => (false, Some(tr("Not downloaded yet: Cisco's binary is fetched on demand").to_string())),
    };
    Capability {
        kind: EncoderKind::Openh264,
        available,
        hardware: false,
        codecs: vec![VideoCodec::H264],
        device: format!("OpenH264 {}", download::RELEASE.version),
        reason,
    }
}

pub struct OpenH264Encoder {
    info: EncoderInfo,
    _lib: Library,
    destroy: DestroyEncoder,
    enc: *mut *const ISVCEncoderVtbl,
    gl: Option<GlContext>,
    target: Option<u32>,
    width: u32,
    height: u32,
    readback: Vec<u8>,
    i420: I420,
    bs: Box<SFrameBSInfo>,
    param_sets: Vec<u8>,
}

impl OpenH264Encoder {
    pub fn open(s: &Settings, gl: Option<GlContext>) -> Result<OpenH264Encoder, EncodeError> {
        if s.codec != VideoCodec::H264 {
            return Err(EncodeError(tr("OpenH264 encodes H.264 only").into()));
        }
        let lib = load_library()?;
        // SAFETY: the C entry points of codec_api.h, with their exact signatures.
        let (create, destroy): (CreateEncoder, DestroyEncoder) = unsafe {
            (
                *lib.get::<CreateEncoder>(b"WelsCreateSVCEncoder\0").map_err(|e| EncodeError(e.to_string()))?,
                *lib.get::<DestroyEncoder>(b"WelsDestroySVCEncoder\0").map_err(|e| EncodeError(e.to_string()))?,
            )
        };
        let mut enc = std::ptr::null_mut();
        // SAFETY: writes the new encoder's handle.
        if unsafe { create(&mut enc) } != 0 || enc.is_null() {
            return Err(EncodeError("WelsCreateSVCEncoder".into()));
        }
        let (w, h) = (s.width & !1, s.height & !1);
        let base = SEncParamBase {
            usage_type: SCREEN_CONTENT_REAL_TIME,
            pic_width: w as c_int,
            pic_height: h as c_int,
            target_bitrate: s.bitrate_bps() as c_int,
            rc_mode: RC_BITRATE_MODE,
            max_frame_rate: s.fps.max(1) as f32,
        };
        // SAFETY: enc is a valid encoder; its table's functions take it as first argument.
        let vt = unsafe { &**enc };
        let mut fmt = VIDEO_FORMAT_I420;
        // 0: only the first frame is an IDR; the engine forces the others (every 2 s of media time)
        let mut idr: c_int = 0;
        // SAFETY: as above; options take pointers to ints that live during the call.
        let ok = unsafe {
            (vt.initialize)(enc, &base) == 0
                && (vt.set_option)(enc, ENCODER_OPTION_DATAFORMAT, (&mut fmt as *mut c_int).cast()) == 0
                && (vt.set_option)(enc, ENCODER_OPTION_IDR_INTERVAL, (&mut idr as *mut c_int).cast()) == 0
        };
        if !ok {
            // SAFETY: the encoder was created above and is not used after this.
            unsafe { destroy(enc) };
            return Err(EncodeError("OpenH264 Initialize".into()));
        }
        let mut i420 = I420::default();
        i420.resize(w, h);
        // SAFETY: SFrameBSInfo is plain data (ints and pointers): all zeroes is its documented empty state.
        let bs: Box<SFrameBSInfo> = Box::new(unsafe { std::mem::zeroed() });
        let target = gl.as_ref().map(|g| g.create_texture(w, h));
        let mut e = OpenH264Encoder {
            info: EncoderInfo {
                kind: EncoderKind::Openh264,
                codec: VideoCodec::H264,
                device: format!("OpenH264 {}", download::RELEASE.version),
                hardware: false,
                zero_copy: false,
            },
            _lib: lib,
            destroy,
            enc,
            gl,
            target,
            width: w,
            height: h,
            readback: Vec::new(),
            i420,
            bs,
            param_sets: Vec::new(),
        };
        // SAFETY: the encoder is initialised; bs is a valid output struct.
        if unsafe { (vt.encode_parameter_sets)(e.enc, &mut *e.bs) } == 0 {
            let mut out = Vec::new();
            collect(&e.bs, &mut out);
            e.param_sets = out;
        }
        Ok(e)
    }
}

/// Copies every layer's NAL units (Annex B, with their start codes) into `out`.
fn collect(bs: &SFrameBSInfo, out: &mut Vec<u8>) {
    out.clear();
    for layer in &bs.layers[..bs.layer_num.clamp(0, MAX_LAYER_NUM_OF_FRAME as c_int) as usize] {
        let mut size = 0usize;
        for n in 0..layer.nal_count.max(0) as usize {
            // SAFETY: the encoder's arrays hold nal_count lengths and the bytes they add up to.
            size += unsafe { *layer.nal_length_in_byte.add(n) }.max(0) as usize;
        }
        if size > 0 && !layer.bs_buf.is_null() {
            // SAFETY: bs_buf points at `size` bytes owned by the encoder until the next call.
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(layer.bs_buf, size) });
        }
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn info(&self) -> &EncoderInfo {
        &self.info
    }

    fn parameter_sets(&self) -> &[u8] {
        &self.param_sets
    }

    fn force_keyframe(&mut self) {
        // SAFETY: a valid encoder.
        unsafe { ((**self.enc).force_intra_frame)(self.enc, true) };
    }

    fn encode(&mut self, frame: &VideoFrame, crop: Option<Rect>, pts_ns: u64, out: &mut EncodedFrame) -> Result<(), EncodeError> {
        match (frame, &mut self.gl, self.target) {
            // shared memory of the session's size, uncropped: convert straight from it
            (VideoFrame::Shm(f), _, _) if crop.is_none() && f.width & !1 == self.width && f.height & !1 == self.height => {
                bgrx_to_i420(f.data, f.stride as usize, &mut self.i420);
            }
            (_, Some(gl), Some(target)) => {
                let (src, w, h) = gl.frame_texture(frame)?;
                gl.blit(src, w, h, crop, target, self.width, self.height)?;
                gl.read_bgra(target, self.width, self.height, &mut self.readback);
                bgrx_to_i420(&self.readback, self.width as usize * 4, &mut self.i420);
            }
            _ => return Err(EncodeError(tr("This frame needs the GPU to be read").into())),
        }
        let pic = SSourcePicture {
            color_format: VIDEO_FORMAT_I420,
            stride: [self.width as c_int, (self.width / 2) as c_int, (self.width / 2) as c_int, 0],
            data: [self.i420.y.as_mut_ptr(), self.i420.u.as_mut_ptr(), self.i420.v.as_mut_ptr(), std::ptr::null_mut()],
            pic_width: self.width as c_int,
            pic_height: self.height as c_int,
            time_stamp: (pts_ns / 1_000_000) as i64,
            psnr_y: false,
            psnr_u: false,
            psnr_v: false,
        };
        // SAFETY: the picture's planes hold width×height (and quarter) bytes; bs is a valid output struct.
        let r = unsafe { ((**self.enc).encode_frame)(self.enc, &pic, &mut *self.bs) };
        if r != 0 {
            return Err(EncodeError(format!("OpenH264 EncodeFrame: {r}")));
        }
        if self.bs.frame_type == VIDEO_FRAME_TYPE_SKIP {
            out.data.clear();
            out.keyframe = false;
            return Ok(());
        }
        collect(&self.bs, &mut out.data);
        out.keyframe = self.bs.frame_type == VIDEO_FRAME_TYPE_IDR;
        Ok(())
    }
}

impl Drop for OpenH264Encoder {
    fn drop(&mut self) {
        // SAFETY: the encoder is ours and not used after this.
        unsafe {
            ((**self.enc).uninitialize)(self.enc);
            (self.destroy)(self.enc);
        }
        if let (Some(gl), Some(t)) = (&self.gl, self.target) {
            gl.delete_texture(t);
        }
    }
}
