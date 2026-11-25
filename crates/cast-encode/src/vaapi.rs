//! VA-API (AMD, Intel): the capability probe. libva and libva-drm are loaded at run time; each render node is
//! opened and asked whether it has an encode entry point for H.264 / HEVC. Encoding through VA-API is phase 2 (this
//! machine class first: the NVIDIA path is the one implemented and tested here); the probe already tells the engine
//! and the studio what such a machine offers.

use std::ffi::{c_int, c_void, CStr};
use std::os::fd::AsRawFd;

use cast_core::{tr, EncoderKind, VideoCodec};
use libloading::Library;

use crate::Capability;

// va.h
const VA_PROFILE_H264_MAIN: c_int = 6;
const VA_PROFILE_H264_HIGH: c_int = 7;
const VA_PROFILE_HEVC_MAIN: c_int = 17;
const VA_ENTRYPOINT_ENC_SLICE: c_int = 6;
const VA_ENTRYPOINT_ENC_SLICE_LP: c_int = 8;

type GetDisplayDrm = unsafe extern "C" fn(c_int) -> *mut c_void;
type Initialize = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;
type Terminate = unsafe extern "C" fn(*mut c_void) -> c_int;
type QueryEntrypoints = unsafe extern "C" fn(*mut c_void, c_int, *mut c_int, *mut c_int) -> c_int;
type MaxEntrypoints = unsafe extern "C" fn(*mut c_void) -> c_int;
type QueryVendor = unsafe extern "C" fn(*mut c_void) -> *const std::ffi::c_char;

fn render_nodes() -> Vec<std::path::PathBuf> {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("renderD"))).collect())
        .unwrap_or_default();
    nodes.sort();
    nodes
}

pub fn probe() -> Capability {
    let unavailable = |reason: String| Capability {
        kind: EncoderKind::Vaapi,
        available: false,
        hardware: true,
        codecs: vec![],
        device: String::new(),
        reason: Some(reason),
    };
    // SAFETY: libva's libraries support being loaded at run time (it is how GStreamer and browsers use them).
    let (va, drm) = match unsafe { (Library::new("libva.so.2"), Library::new("libva-drm.so.2")) } {
        (Ok(a), Ok(b)) => (a, b),
        _ => return unavailable(tr("libva is not installed").into()),
    };
    // SAFETY: the va.h / va_drm.h functions with their exact signatures.
    let syms = unsafe {
        (|| -> Result<_, libloading::Error> {
            Ok((
                *drm.get::<GetDisplayDrm>(b"vaGetDisplayDRM\0")?,
                *va.get::<Initialize>(b"vaInitialize\0")?,
                *va.get::<Terminate>(b"vaTerminate\0")?,
                *va.get::<QueryEntrypoints>(b"vaQueryConfigEntrypoints\0")?,
                *va.get::<MaxEntrypoints>(b"vaMaxNumEntrypoints\0")?,
                *va.get::<QueryVendor>(b"vaQueryVendorString\0")?,
            ))
        })()
    };
    let Ok((get_display, initialize, terminate, query, max_entrypoints, vendor)) = syms else {
        return unavailable("libva: symbols".into());
    };
    let mut last_reason = tr("No GPU with a VA-API encoder").to_string();
    for node in render_nodes() {
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&node) else { continue };
        // SAFETY: a valid DRM fd that outlives the display (terminated below before the file closes).
        let dpy = unsafe { get_display(file.as_raw_fd()) };
        if dpy.is_null() {
            continue;
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: dpy is a VADisplay; the version ints are written.
        if unsafe { initialize(dpy, &mut major, &mut minor) } != 0 {
            last_reason = format!("{}: {}", node.display(), tr("no VA-API driver for this GPU"));
            // SAFETY: as above.
            unsafe { terminate(dpy) };
            continue;
        }
        // SAFETY: dpy is initialised.
        let max = unsafe { max_entrypoints(dpy) }.max(1) as usize;
        let mut codecs = Vec::new();
        for (codec, profiles) in
            [(VideoCodec::H264, &[VA_PROFILE_H264_HIGH, VA_PROFILE_H264_MAIN][..]), (VideoCodec::Hevc, &[VA_PROFILE_HEVC_MAIN][..])]
        {
            let encodes = profiles.iter().any(|&p| {
                let mut eps = vec![0 as c_int; max];
                let mut n = 0;
                // SAFETY: eps has room for vaMaxNumEntrypoints entries.
                let status = unsafe { query(dpy, p, eps.as_mut_ptr(), &mut n) };
                status == 0 && eps[..n.max(0) as usize].iter().any(|&e| e == VA_ENTRYPOINT_ENC_SLICE || e == VA_ENTRYPOINT_ENC_SLICE_LP)
            });
            if encodes {
                codecs.push(codec);
            }
        }
        // SAFETY: dpy is initialised; the vendor string is static.
        let device = unsafe { CStr::from_ptr(vendor(dpy)) }.to_string_lossy().into_owned();
        // SAFETY: done with this display.
        unsafe { terminate(dpy) };
        if !codecs.is_empty() {
            return Capability {
                kind: EncoderKind::Vaapi,
                available: true,
                hardware: true,
                codecs,
                device,
                reason: Some(tr("VA-API encoding is not available yet").into()),
            };
        }
        last_reason = format!("{}: {}", node.display(), tr("the VA-API driver has no encoder"));
    }
    unavailable(last_reason)
}
