//! What a recording is doing, readable from any thread (atomics written by the capture, encode and mux threads),
//! and the process/GPU usage around it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use cast_core::monotonic_ns;
use serde::Serialize;

/// Counters of a running recording.
#[derive(Default)]
pub struct Counters {
    pub started_ns: AtomicU64,
    pub frames: AtomicU64,
    /// Frames the compositor sent empty/corrupted, or that failed to encode.
    pub dropped: AtomicU64,
    pub encode_errors: AtomicU64,
    pub video_bytes: AtomicU64,
    pub audio_packets: AtomicU64,
    pub audio_bytes: AtomicU64,
    /// Silence inserted / samples skipped to keep the sound on the clock (gaps, drift).
    pub audio_corrections: AtomicU64,
    /// Sum and max of the encode call (GPU import/blit + encode + bitstream), ns.
    pub encode_ns_sum: AtomicU64,
    pub encode_ns_max: AtomicU64,
    /// Sum and max from the frame's arrival to its packet being written to the file, ns.
    pub disk_ns_sum: AtomicU64,
    pub disk_ns_max: AtomicU64,
    pub disk_samples: AtomicU64,
    /// Media time of the last frame, ns.
    pub last_pts_ns: AtomicU64,
    pub dmabuf: AtomicBool,
    pub fragments: AtomicU64,
    /// The file can no longer be written (full disk, I/O error): capture and encoding stop, the error is in `Status`.
    pub failed: AtomicBool,
    /// Audio packets dropped because the writer's queue was full.
    pub audio_dropped: AtomicU64,
    /// Frames in the last full second, and the second it was counted in.
    pub fps_window: Mutex<(u64, u64, f64)>,
}

impl Counters {
    pub fn add_max(sum: &AtomicU64, max: &AtomicU64, v: u64) {
        sum.fetch_add(v, Ordering::Relaxed);
        max.fetch_max(v, Ordering::Relaxed);
    }

    /// Counts a frame for the per-second frame rate (called on the capture thread).
    pub fn tick_frame(&self, now_ns: u64) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut w) = self.fps_window.lock() {
            let (count, since, fps) = *w;
            if since == 0 {
                *w = (1, now_ns, fps);
            } else if now_ns - since >= 1_000_000_000 {
                *w = (1, now_ns, count as f64 * 1e9 / (now_ns - since) as f64);
            } else {
                *w = (count + 1, since, fps);
            }
        }
    }
}

/// What `record.status` answers.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub recording: bool,
    pub path: String,
    pub encoder: String,
    pub codec: String,
    pub device: String,
    pub zero_copy: bool,
    pub dmabuf: bool,
    pub width: u32,
    pub height: u32,
    pub duration_s: f64,
    pub frames: u64,
    /// Frames per second over the last second.
    pub fps: f64,
    pub dropped: u64,
    pub audio_dropped: u64,
    pub bitrate_kbps: f64,
    pub audio_tracks: u32,
    pub audio_packets: u64,
    pub audio_corrections: u64,
    pub encode_ms_avg: f64,
    pub encode_ms_max: f64,
    /// From the frame's arrival to its packet in the file.
    pub latency_ms_avg: f64,
    pub latency_ms_max: f64,
    pub fragments: u64,
    /// This process's CPU use since the recording started, % of one core.
    pub cpu_percent: f64,
    pub rss_mb: f64,
    /// The whole GPU's use (NVML), when available.
    pub gpu_percent: Option<u32>,
    pub gpu_encoder_percent: Option<u32>,
    pub error: Option<String>,
    /// The sound was asked for but could not start: the recording goes on without it (and says so).
    pub audio_error: Option<String>,
    /// The recording's media clock origin (CLOCK_MONOTONIC ns): media time 0 in the file.
    pub origin_ns: u64,
}

/// This process's CPU time (user + system), ns.
pub fn cpu_time_ns() -> u64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else { return 0 };
    // fields after the command name (which may hold spaces): utime is the 14th, stime the 15th
    let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else { return 0 };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    ticks * 1_000_000_000 / hz
}

/// This process's resident memory, MB.
pub fn rss_mb() -> f64 {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else { return 0.0 };
    let pages: u64 = statm.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    (pages * page) as f64 / (1024.0 * 1024.0)
}

/// NVIDIA's management library (loaded at run time): the GPU's and its encoder's utilisation.
pub struct Nvml {
    _lib: libloading::Library,
    device: *mut std::ffi::c_void,
    utilization: unsafe extern "C" fn(*mut std::ffi::c_void, *mut [u32; 2]) -> i32,
    encoder: unsafe extern "C" fn(*mut std::ffi::c_void, *mut u32, *mut u32) -> i32,
}

// SAFETY: NVML is thread-safe (documented); the device handle is an opaque, immutable pointer.
unsafe impl Send for Nvml {}
// SAFETY: as above.
unsafe impl Sync for Nvml {}

impl Nvml {
    pub fn open() -> Option<Nvml> {
        type Init = unsafe extern "C" fn() -> i32;
        type ByIndex = unsafe extern "C" fn(u32, *mut *mut std::ffi::c_void) -> i32;
        // SAFETY: NVML supports dlopen (nvidia-smi does it).
        let lib = unsafe { libloading::Library::new("libnvidia-ml.so.1") }.ok()?;
        // SAFETY: nvml.h's functions with their exact signatures.
        unsafe {
            let init = *lib.get::<Init>(b"nvmlInit_v2\0").ok()?;
            let by_index = *lib.get::<ByIndex>(b"nvmlDeviceGetHandleByIndex_v2\0").ok()?;
            let utilization = *lib.get(b"nvmlDeviceGetUtilizationRates\0").ok()?;
            let encoder = *lib.get(b"nvmlDeviceGetEncoderUtilization\0").ok()?;
            if init() != 0 {
                return None;
            }
            let mut device = std::ptr::null_mut();
            if by_index(0, &mut device) != 0 {
                return None;
            }
            Some(Nvml { _lib: lib, device, utilization, encoder })
        }
    }

    /// (GPU %, encoder %) over NVML's last sampling period.
    pub fn sample(&self) -> (Option<u32>, Option<u32>) {
        let mut u = [0u32; 2];
        let (mut e, mut period) = (0u32, 0u32);
        // SAFETY: the device handle is valid; the outputs are written.
        let gpu = (unsafe { (self.utilization)(self.device, &mut u) } == 0).then_some(u[0]);
        // SAFETY: as above.
        let enc = (unsafe { (self.encoder)(self.device, &mut e, &mut period) } == 0).then_some(e);
        (gpu, enc)
    }
}

pub fn now_s(since_ns: u64) -> f64 {
    monotonic_ns().saturating_sub(since_ns) as f64 / 1e9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_usage_is_readable() {
        // the kernel counts CPU in clock ticks (10 ms): spin until at least one has been charged
        let start = std::time::Instant::now();
        let mut x = 0u64;
        while cpu_time_ns() == 0 && start.elapsed() < std::time::Duration::from_secs(5) {
            for i in 0..1_000_000u64 {
                x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(i));
            }
        }
        assert!(x != 1);
        assert!(cpu_time_ns() > 0);
        assert!(rss_mb() > 0.5);
    }

    #[test]
    fn frame_rate_is_counted_per_second() {
        let c = Counters::default();
        for i in 0..=60u64 {
            c.tick_frame(1_000_000_000 + i * 16_666_667);
        }
        // the 61st frame closed the first second: 60 frames in 1.0 s
        let fps = c.fps_window.lock().unwrap().2;
        assert!((fps - 60.0).abs() < 0.1, "{fps}");
    }
}
