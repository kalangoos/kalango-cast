//! NVENC on a real NVIDIA GPU (headless: EGL device platform, no display server).
//!
//! Ignored by default; run with `KALANGO_CAST_GPU=1 cargo test -p cast-encode --test nvenc -- --ignored --nocapture`.
//! The frames are synthetic shared-memory frames (uploaded to a GL texture), so this measures the encode path
//! (upload, GPU blit, NVENC) and not the DMA-BUF import, which needs a compositor.

use std::time::Instant;

use cast_core::{fourcc, EncoderKind, ShmFrame, VideoCodec, VideoFrame};
use cast_encode::{EncodedFrame, GlContext, Settings};

fn process_cpu_ns() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = stat.rsplit_once(')').map(|(_, r)| r.to_string()).unwrap_or_default();
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    ticks * 10_000_000 // 100 ticks per second on Linux
}

fn fill(buf: &mut [u8], w: usize, h: usize, n: usize) {
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            buf[i] = ((x + n * 3) % 256) as u8;
            buf[i + 1] = ((y + n) % 256) as u8;
            buf[i + 2] = ((x ^ y) % 256) as u8;
            buf[i + 3] = 255;
        }
    }
}

fn run(codec: VideoCodec) {
    assert!(std::env::var_os("KALANGO_CAST_GPU").is_some(), "set KALANGO_CAST_GPU=1");
    let (w, h, frames) = (1920usize, 1080usize, 300usize);
    let gl = GlContext::new(None).expect("headless GL context on the GPU");
    let settings = Settings { codec, width: w as u32, height: h as u32, fps: 60, bitrate_kbps: 0, keyframe_interval_s: 2 };
    let mut enc = cast_encode::open(EncoderKind::Nvenc, &settings, Some(gl)).expect("NVENC session");
    assert!(!enc.parameter_sets().is_empty(), "parameter sets");
    let mut buf = vec![0u8; w * h * 4];
    let mut out = EncodedFrame::default();
    let mut lat = Vec::with_capacity(frames);
    let mut bytes = 0usize;
    let mut keys = 0;
    let mut fill_s = 0.0;
    let cpu0 = process_cpu_ns();
    let t0 = Instant::now();
    for n in 0..frames {
        let tf = Instant::now();
        fill(&mut buf, w, h, n);
        fill_s += tf.elapsed().as_secs_f64();
        let frame = VideoFrame::Shm(ShmFrame { width: w as u32, height: h as u32, stride: (w * 4) as u32, fourcc: fourcc::XRGB8888, data: &buf });
        if n == 120 {
            enc.force_keyframe();
        }
        let t = Instant::now();
        enc.encode(&frame, None, (n as u64) * 16_666_667, &mut out).expect("encode");
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(!out.data.is_empty());
        if n == 0 {
            assert!(out.keyframe, "the first frame is a keyframe");
        }
        keys += usize::from(out.keyframe);
        bytes += out.data.len();
    }
    let wall = t0.elapsed().as_secs_f64();
    let cpu = (process_cpu_ns() - cpu0) as f64 / 1e9;
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "NVENC {codec:?} {w}x{h}: {frames} frames in {wall:.2}s = {:.1} fps overall ({:.2}s of it filling synthetic frames on the CPU); encode() p50 {:.2} ms p99 {:.2} ms max {:.2} ms; process CPU {:.0}% of one core; {keys} keyframes; {:.1} Mbit/s",
        frames as f64 / wall,
        fill_s,
        lat[frames / 2],
        lat[frames * 99 / 100],
        lat[frames - 1],
        100.0 * cpu / wall,
        bytes as f64 * 8.0 / (frames as f64 / 60.0) / 1e6,
    );
    assert!(keys >= 2, "the forced keyframe came out");
}

#[test]
#[ignore = "needs an NVIDIA GPU: set KALANGO_CAST_GPU=1 and pass --ignored"]
fn nvenc_encodes_h264_on_the_gpu() {
    run(VideoCodec::H264);
}

#[test]
#[ignore = "needs an NVIDIA GPU: set KALANGO_CAST_GPU=1 and pass --ignored"]
fn nvenc_encodes_hevc_on_the_gpu() {
    run(VideoCodec::Hevc);
}
