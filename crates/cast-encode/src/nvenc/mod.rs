//! NVIDIA NVENC through libnvidia-encode (loaded at run time, API 13.0) with its OpenGL interface: the frame is a
//! GL texture (a DMA-BUF imported in place, then a GPU blit into the registered input texture), so pixels never pass
//! through the CPU. Encoding is synchronous (Linux has no async mode): submit, then lock the bitstream, which waits
//! for the GPU (~1–3 ms at 1080p).

#[allow(clippy::all)]
#[allow(
    unnecessary_transmutes,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    unused_imports,
    clippy::all,
    clippy::pedantic,
    clippy::undocumented_unsafe_blocks
)]
mod sys;

use std::ffi::{c_void, CStr};
use std::ptr;

use cast_core::{tr, EncoderKind, Rect, VideoCodec, VideoFrame};
use libloading::Library;
use sys::*;

use crate::gl::{GlContext, GL_TEXTURE_2D};
use crate::{Capability, EncodeError, EncodedFrame, EncoderInfo, Settings, VideoEncoder};

const API_MAJOR: u32 = 13;
const API_MINOR: u32 = 0;
const API_VERSION: u32 = API_MAJOR | (API_MINOR << 24);

const fn struct_version(v: u32) -> u32 {
    API_VERSION | (v << 16) | (0x7 << 28)
}

const FUNCTION_LIST_VER: u32 = struct_version(2);
const OPEN_SESSION_VER: u32 = struct_version(1);
const INITIALIZE_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
const CONFIG_VER: u32 = struct_version(9) | (1 << 31);
const PRESET_CONFIG_VER: u32 = struct_version(5) | (1 << 31);
const PIC_PARAMS_VER: u32 = struct_version(7) | (1 << 31);
const LOCK_BITSTREAM_VER: u32 = struct_version(2) | (1 << 31);
const MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
const REGISTER_RESOURCE_VER: u32 = struct_version(5);
const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
const SEQUENCE_PARAM_PAYLOAD_VER: u32 = struct_version(1);

/// NVENC_INFINITE_GOPLENGTH
const INFINITE_GOP: u32 = 0xffff_ffff;

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> GUID {
    GUID { Data1: d1, Data2: d2, Data3: d3, Data4: d4 }
}

const CODEC_H264: GUID = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
const CODEC_HEVC: GUID = guid(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
/// P4: the middle of NVENC's speed/quality scale (P1 fastest, P7 best).
const PRESET_P4: GUID = guid(0x90a7b826, 0xdf06, 0x4862, [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]);

fn same_guid(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

fn codec_guid(c: VideoCodec) -> GUID {
    match c {
        VideoCodec::H264 => CODEC_H264,
        VideoCodec::Hevc => CODEC_HEVC,
    }
}

/// libnvidia-encode and its function table.
struct Api {
    _lib: Library,
    f: NV_ENCODE_API_FUNCTION_LIST,
}

impl Api {
    fn load() -> Result<Api, EncodeError> {
        // SAFETY: loading the driver's encode library runs its initialisers, which support dlopen (ffmpeg, OBS do it).
        let lib = unsafe { Library::new("libnvidia-encode.so.1") }.map_err(|e| EncodeError(format!("libnvidia-encode: {e}")))?;
        type MaxVersion = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;
        type CreateInstance = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
        // SAFETY: the two entry points of nvEncodeAPI.h, looked up with their exact signatures.
        let (max_version, create): (libloading::Symbol<MaxVersion>, libloading::Symbol<CreateInstance>) = unsafe {
            (
                lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0").map_err(|e| EncodeError(e.to_string()))?,
                lib.get(b"NvEncodeAPICreateInstance\0").map_err(|e| EncodeError(e.to_string()))?,
            )
        };
        let mut max = 0;
        // SAFETY: writes one u32.
        unsafe { max_version(&mut max) };
        let (dmajor, dminor) = (max >> 4, max & 0xf);
        if (dmajor, dminor) < (API_MAJOR, API_MINOR) {
            return Err(EncodeError(format!("{} {dmajor}.{dminor} < {API_MAJOR}.{API_MINOR}", tr("NVIDIA driver too old for NVENC API"))));
        }
        let mut f = NV_ENCODE_API_FUNCTION_LIST { version: FUNCTION_LIST_VER, ..Default::default() };
        // SAFETY: fills the function table we pass (its version set as the API requires).
        let st = unsafe { create(&mut f) };
        check(st, "NvEncodeAPICreateInstance")?;
        let _ = (max_version, create);
        Ok(Api { _lib: lib, f })
    }
}

fn check(status: NVENCSTATUS, what: &str) -> Result<(), EncodeError> {
    if status == _NVENCSTATUS_NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(EncodeError(format!("{what}: NVENCSTATUS {status}")))
    }
}

macro_rules! call {
    ($api:expr, $f:ident ( $($arg:expr),* )) => {{
        let f = $api.f.$f.ok_or_else(|| EncodeError(concat!(stringify!($f), " missing").into()))?;
        // SAFETY: an NVENC entry point from the driver's function table, called with the arguments its header
        // declares; the session handle and buffers passed are ours and alive.
        let st = unsafe { f($($arg),*) };
        check(st, stringify!($f))
    }};
}

/// An open encode session on the current GL context.
struct Session {
    api: Api,
    enc: *mut c_void,
}

impl Session {
    fn open() -> Result<Session, EncodeError> {
        let api = Api::load()?;
        let mut params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: OPEN_SESSION_VER,
            deviceType: _NV_ENC_DEVICE_TYPE_NV_ENC_DEVICE_TYPE_OPENGL,
            device: ptr::null_mut(),
            apiVersion: API_VERSION,
            ..Default::default()
        };
        let mut enc = ptr::null_mut();
        call!(api, nvEncOpenEncodeSessionEx(&mut params, &mut enc))?;
        Ok(Session { api, enc })
    }

    fn codecs(&self) -> Result<Vec<VideoCodec>, EncodeError> {
        let mut n = 0;
        call!(self.api, nvEncGetEncodeGUIDCount(self.enc, &mut n))?;
        let mut guids = vec![GUID::default(); n as usize];
        let mut got = 0;
        call!(self.api, nvEncGetEncodeGUIDs(self.enc, guids.as_mut_ptr(), n, &mut got))?;
        guids.truncate(got as usize);
        Ok([VideoCodec::H264, VideoCodec::Hevc].into_iter().filter(|c| guids.iter().any(|g| same_guid(g, &codec_guid(*c)))).collect())
    }

    fn last_error(&self) -> String {
        let Some(f) = self.api.f.nvEncGetLastErrorString else { return String::new() };
        // SAFETY: returns a NUL-terminated string owned by the session (or null).
        let p = unsafe { f(self.enc) };
        if p.is_null() {
            return String::new();
        }
        // SAFETY: non-null, NUL-terminated.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(f) = self.api.f.nvEncDestroyEncoder {
            // SAFETY: the session is ours; nothing uses it after this.
            unsafe { f(self.enc) };
        }
    }
}

/// Is NVENC usable here, and with which codecs (opens a GL context and a session on a short-lived thread).
pub fn probe() -> Capability {
    let result = std::thread::spawn(|| -> Result<(Vec<VideoCodec>, String), EncodeError> {
        let gl = GlContext::new(None)?;
        let s = Session::open()?;
        let codecs = s.codecs()?;
        drop(s);
        Ok((codecs, gl.renderer.clone()))
    })
    .join()
    .unwrap_or_else(|_| Err(EncodeError("probe panicked".into())));
    match result {
        Ok((codecs, device)) => Capability { kind: EncoderKind::Nvenc, available: !codecs.is_empty(), hardware: true, codecs, device, reason: None },
        Err(e) => Capability { kind: EncoderKind::Nvenc, available: false, hardware: true, codecs: vec![], device: String::new(), reason: Some(e.0) },
    }
}

pub struct NvencEncoder {
    info: EncoderInfo,
    // drop order: session resources first (Drop below), then the session, then GL
    session: Session,
    gl: GlContext,
    texture: u32,
    registered: *mut c_void,
    bitstream: *mut c_void,
    width: u32,
    height: u32,
    param_sets: Vec<u8>,
    force_idr: bool,
    frame: u64,
}

impl NvencEncoder {
    pub fn open(s: &Settings, gl: GlContext) -> Result<NvencEncoder, EncodeError> {
        let session = Session::open()?;
        let codec = codec_guid(s.codec);
        if !session.codecs()?.contains(&s.codec) {
            return Err(EncodeError(format!("NVENC: {:?} {}", s.codec, tr("is not supported by this GPU"))));
        }
        let tuning = NV_ENC_TUNING_INFO_NV_ENC_TUNING_INFO_LOW_LATENCY;
        let mut preset = NV_ENC_PRESET_CONFIG { version: PRESET_CONFIG_VER, ..Default::default() };
        preset.presetCfg.version = CONFIG_VER;
        call!(session.api, nvEncGetEncodePresetConfigEx(session.enc, codec, PRESET_P4, tuning, &mut preset))?;
        let mut cfg = preset.presetCfg;
        cfg.version = CONFIG_VER;
        // keyframes come when the engine asks (every 2 s of media time: screen capture has a variable frame rate)
        cfg.gopLength = INFINITE_GOP;
        // IPPP: no B-frames, so frames come out in order with no reordering delay
        cfg.frameIntervalP = 1;
        let bitrate = s.bitrate_bps();
        cfg.rcParams.rateControlMode = _NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_VBR;
        cfg.rcParams.averageBitRate = bitrate;
        cfg.rcParams.maxBitRate = bitrate.saturating_mul(2);
        cfg.rcParams.vbvBufferSize = bitrate;
        cfg.rcParams.vbvInitialDelay = bitrate;
        // SAFETY: the union member written is the one of the codec being configured.
        unsafe {
            match s.codec {
                VideoCodec::H264 => {
                    let h = &mut cfg.encodeCodecConfig.h264Config;
                    h.idrPeriod = INFINITE_GOP;
                    h.set_repeatSPSPPS(0);
                    h.set_outputAUD(0);
                    let v = &mut h.h264VUIParameters;
                    v.videoSignalTypePresentFlag = 1;
                    v.videoFormat = _NV_ENC_VUI_VIDEO_FORMAT_NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                    v.videoFullRangeFlag = 0;
                    v.colourDescriptionPresentFlag = 1;
                    v.colourPrimaries = _NV_ENC_VUI_COLOR_PRIMARIES_NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                    v.transferCharacteristics = _NV_ENC_VUI_TRANSFER_CHARACTERISTIC_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                    v.colourMatrix = _NV_ENC_VUI_MATRIX_COEFFS_NV_ENC_VUI_MATRIX_COEFFS_BT709;
                }
                VideoCodec::Hevc => {
                    let h = &mut cfg.encodeCodecConfig.hevcConfig;
                    h.idrPeriod = INFINITE_GOP;
                    h.set_repeatSPSPPS(0);
                    h.set_outputAUD(0);
                    let v = &mut h.hevcVUIParameters;
                    v.videoSignalTypePresentFlag = 1;
                    v.videoFormat = _NV_ENC_VUI_VIDEO_FORMAT_NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                    v.videoFullRangeFlag = 0;
                    v.colourDescriptionPresentFlag = 1;
                    v.colourPrimaries = _NV_ENC_VUI_COLOR_PRIMARIES_NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                    v.transferCharacteristics = _NV_ENC_VUI_TRANSFER_CHARACTERISTIC_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                    v.colourMatrix = _NV_ENC_VUI_MATRIX_COEFFS_NV_ENC_VUI_MATRIX_COEFFS_BT709;
                }
            }
        }
        let mut init = NV_ENC_INITIALIZE_PARAMS {
            version: INITIALIZE_PARAMS_VER,
            encodeGUID: codec,
            presetGUID: PRESET_P4,
            encodeWidth: s.width,
            encodeHeight: s.height,
            darWidth: s.width,
            darHeight: s.height,
            frameRateNum: s.fps.max(1),
            frameRateDen: 1,
            enablePTD: 1,
            encodeConfig: &mut cfg,
            maxEncodeWidth: s.width,
            maxEncodeHeight: s.height,
            tuningInfo: tuning,
            ..Default::default()
        };
        if let Err(e) = call!(session.api, nvEncInitializeEncoder(session.enc, &mut init)) {
            return Err(EncodeError(format!("{e} ({})", session.last_error())));
        }
        let texture = gl.create_texture(s.width, s.height);
        let mut tex = NV_ENC_INPUT_RESOURCE_OPENGL_TEX { texture, target: GL_TEXTURE_2D };
        let mut reg = NV_ENC_REGISTER_RESOURCE {
            version: REGISTER_RESOURCE_VER,
            resourceType: _NV_ENC_INPUT_RESOURCE_TYPE_NV_ENC_INPUT_RESOURCE_TYPE_OPENGL_TEX,
            width: s.width,
            height: s.height,
            pitch: 0,
            resourceToRegister: (&mut tex as *mut NV_ENC_INPUT_RESOURCE_OPENGL_TEX).cast(),
            // GL's RGBA8 is bytes R, G, B, A: NVENC calls that ABGR (a little-endian word)
            bufferFormat: _NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_ABGR,
            bufferUsage: _NV_ENC_BUFFER_USAGE_NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        if let Err(e) = call!(session.api, nvEncRegisterResource(session.enc, &mut reg)) {
            gl.delete_texture(texture);
            return Err(EncodeError(format!("{e} ({})", session.last_error())));
        }
        let mut bs = NV_ENC_CREATE_BITSTREAM_BUFFER { version: CREATE_BITSTREAM_BUFFER_VER, ..Default::default() };
        call!(session.api, nvEncCreateBitstreamBuffer(session.enc, &mut bs))?;
        let mut enc = NvencEncoder {
            info: EncoderInfo { kind: EncoderKind::Nvenc, codec: s.codec, device: gl.renderer.clone(), hardware: true, zero_copy: true },
            session,
            gl,
            texture,
            registered: reg.registeredResource,
            bitstream: bs.bitstreamBuffer,
            width: s.width,
            height: s.height,
            param_sets: Vec::new(),
            force_idr: false,
            frame: 0,
        };
        let mut buf = vec![0u8; 1024];
        let mut size = 0u32;
        let mut payload = NV_ENC_SEQUENCE_PARAM_PAYLOAD {
            version: SEQUENCE_PARAM_PAYLOAD_VER,
            inBufferSize: buf.len() as u32,
            spsppsBuffer: buf.as_mut_ptr().cast(),
            outSPSPPSPayloadSize: &mut size,
            ..Default::default()
        };
        call!(enc.session.api, nvEncGetSequenceParams(enc.session.enc, &mut payload))?;
        buf.truncate(size as usize);
        enc.param_sets = buf;
        Ok(enc)
    }
}

impl VideoEncoder for NvencEncoder {
    fn info(&self) -> &EncoderInfo {
        &self.info
    }

    fn parameter_sets(&self) -> &[u8] {
        &self.param_sets
    }

    fn force_keyframe(&mut self) {
        self.force_idr = true;
    }

    fn encode(&mut self, frame: &VideoFrame, crop: Option<Rect>, pts_ns: u64, out: &mut EncodedFrame) -> Result<(), EncodeError> {
        let (src, w, h) = self.gl.frame_texture(frame)?;
        self.gl.blit(src, w, h, crop, self.texture, self.width, self.height)?;
        self.gl.flush();
        let mut map = NV_ENC_MAP_INPUT_RESOURCE { version: MAP_INPUT_RESOURCE_VER, registeredResource: self.registered, ..Default::default() };
        call!(self.session.api, nvEncMapInputResource(self.session.enc, &mut map))?;
        let mut flags = 0;
        if self.force_idr {
            flags |= _NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_FORCEIDR;
            self.force_idr = false;
        }
        let mut pic = NV_ENC_PIC_PARAMS {
            version: PIC_PARAMS_VER,
            inputWidth: self.width,
            inputHeight: self.height,
            inputPitch: self.width,
            encodePicFlags: flags,
            frameIdx: self.frame as u32,
            inputTimeStamp: pts_ns,
            inputBuffer: map.mappedResource,
            outputBitstream: self.bitstream,
            bufferFmt: map.mappedBufferFmt,
            pictureStruct: _NV_ENC_PIC_STRUCT_NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        self.frame += 1;
        let encoded = call!(self.session.api, nvEncEncodePicture(self.session.enc, &mut pic));
        let result = encoded.and_then(|_| {
            let mut lock = NV_ENC_LOCK_BITSTREAM { version: LOCK_BITSTREAM_VER, outputBitstream: self.bitstream, ..Default::default() };
            call!(self.session.api, nvEncLockBitstream(self.session.enc, &mut lock))?;
            // SAFETY: while locked, the driver's buffer holds bitstreamSizeInBytes bytes at bitstreamBufferPtr.
            let data = unsafe { std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, lock.bitstreamSizeInBytes as usize) };
            out.data.clear();
            out.data.extend_from_slice(data);
            out.keyframe = lock.pictureType == _NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_IDR || lock.pictureType == _NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_I;
            call!(self.session.api, nvEncUnlockBitstream(self.session.enc, self.bitstream))
        });
        call!(self.session.api, nvEncUnmapInputResource(self.session.enc, map.mappedResource))?;
        result.map_err(|e| EncodeError(format!("{e} ({})", self.session.last_error())))
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        // SAFETY: the resources belong to this session and are not mapped (encode unmaps before returning).
        unsafe {
            if let Some(f) = self.session.api.f.nvEncUnregisterResource {
                f(self.session.enc, self.registered);
            }
            if let Some(f) = self.session.api.f.nvEncDestroyBitstreamBuffer {
                f(self.session.enc, self.bitstream);
            }
        }
        self.gl.delete_texture(self.texture);
    }
}
