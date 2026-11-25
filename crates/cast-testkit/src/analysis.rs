//! A/V sync from a clap recording: the times of the flashes (the frame's mean brightness jumps) and of the beeps
//! (the sound rises from silence) in the file, decoded with ffmpeg, compared with what the emitter logged.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// One clap as the emitter logged it: when the flash was presented and when the beep's first sample played (both
/// monotonic ns).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Truth {
    pub flash_ns: u64,
    pub beep_ns: u64,
}

/// The per-frame mean luma (0–255) and presentation time (s) of the first video stream.
pub fn video_luma(file: &Path) -> Result<Vec<(f64, f64)>, String> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(file)
        .args(["-map", "0:v:0", "-vf", "signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-", "-f", "null", "-"])
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut frames = Vec::new();
    let mut t = None;
    for line in text.lines() {
        if let Some(p) = line.split_whitespace().find_map(|w| w.strip_prefix("pts_time:")) {
            t = p.parse::<f64>().ok();
        } else if let Some(v) = line.strip_prefix("lavfi.signalstats.YAVG=") {
            if let (Some(t), Ok(v)) = (t, v.trim().parse::<f64>()) {
                frames.push((t, v));
            }
        }
    }
    if frames.is_empty() {
        return Err(format!("no video frames decoded: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(frames)
}

/// The start time (s) of a stream, from ffprobe.
pub fn stream_start(file: &Path, stream: &str) -> Result<f64, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", stream, "-show_entries", "stream=start_time", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    String::from_utf8_lossy(&out.stdout).trim().parse().map_err(|e| format!("start_time of {stream}: {e}"))
}

/// The first audio stream decoded to 48 kHz mono float, and its start time (s).
pub fn audio_pcm(file: &Path, stream: usize) -> Result<(Vec<f32>, f64), String> {
    let start = stream_start(file, &format!("a:{stream}"))?;
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(file)
        .args(["-map", &format!("0:a:{stream}"), "-ac", "1", "-ar", "48000", "-f", "f32le", "-"])
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    let pcm = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    Ok((pcm, start))
}

/// Times (s) where the brightness jumps from dark (< 60) to bright (> 160).
pub fn flash_onsets(luma: &[(f64, f64)]) -> Vec<f64> {
    let mut out = Vec::new();
    let mut dark = true;
    for &(t, y) in luma {
        if dark && y > 160.0 {
            out.push(t);
            dark = false;
        } else if y < 60.0 {
            dark = true;
        }
    }
    out
}

/// Times (s) where the sound rises above `threshold` after at least 200 ms below it.
pub fn beep_onsets(pcm: &[f32], start_s: f64, rate: f64, threshold: f32) -> Vec<f64> {
    let quiet_needed = (0.2 * rate) as usize;
    let mut quiet = quiet_needed;
    let mut out = Vec::new();
    for (i, s) in pcm.iter().enumerate() {
        if s.abs() > threshold {
            if quiet >= quiet_needed {
                out.push(start_s + i as f64 / rate);
            }
            quiet = 0;
        } else {
            quiet += 1;
        }
    }
    out
}

/// The sync report: for each clap seen in the file and in the log, (measured offset − true offset) in ms; positive:
/// the sound is late in the file.
#[derive(Clone, Debug, Serialize)]
pub struct SyncReport {
    pub claps: usize,
    pub errors_ms: Vec<f64>,
    pub mean_ms: f64,
    pub max_abs_ms: f64,
}

/// Pairs each flash with the beep closest to it (within 300 ms), and each with the logged clap closest in time
/// (the file's media time + the recording's origin = monotonic time).
pub fn sync_report(flashes: &[f64], beeps: &[f64], truth: &[Truth]) -> SyncReport {
    let mut errors = Vec::new();
    for (k, &f) in flashes.iter().enumerate() {
        let Some(&b) = beeps.iter().min_by(|x, y| (*x - f).abs().total_cmp(&(*y - f).abs())) else { continue };
        if (b - f).abs() > 0.3 {
            continue;
        }
        // the k-th flash of the file is a logged clap: match by order of the claps after the first seen one
        let measured = b - f;
        let Some(t) = truth.get(k) else { continue };
        let expected = (t.beep_ns as f64 - t.flash_ns as f64) / 1e9;
        errors.push((measured - expected) * 1000.0);
    }
    let mean = if errors.is_empty() { 0.0 } else { errors.iter().sum::<f64>() / errors.len() as f64 };
    let max = errors.iter().fold(0.0f64, |m, e| m.max(e.abs()));
    SyncReport { claps: errors.len(), errors_ms: errors, mean_ms: mean, max_abs_ms: max }
}

/// Aligns the logged claps with the file's flashes: drops logged claps from before the recording began (their
/// flash is earlier than the file's first one by more than half the clap period).
pub fn align_truth(truth: &[Truth], flashes: &[f64], origin_ns: u64) -> Vec<Truth> {
    let Some(&first) = flashes.first() else { return vec![] };
    let first_ns = origin_ns as f64 + first * 1e9;
    truth.iter().copied().skip_while(|t| (t.flash_ns as f64) < first_ns - 400e6).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onsets_are_found_after_quiet_and_darkness() {
        let luma = [(0.0, 16.0), (0.016, 16.0), (0.033, 235.0), (0.05, 235.0), (0.066, 16.0), (1.0, 16.0), (1.016, 200.0)];
        assert_eq!(flash_onsets(&luma), vec![0.033, 1.016]);
        let mut pcm = vec![0.0f32; 48_000];
        for s in &mut pcm[4800..7200] {
            *s = 0.5;
        }
        for s in &mut pcm[30_000..31_000] {
            *s = -0.5;
        }
        let b = beep_onsets(&pcm, 2.0, 48_000.0, 0.1);
        assert_eq!(b.len(), 2);
        assert!((b[0] - 2.1).abs() < 1e-9 && (b[1] - 2.625).abs() < 1e-9);
    }

    #[test]
    fn the_report_subtracts_the_true_offset() {
        // the emitter beeped 5 ms after the flash; the file shows 12 ms: the recording adds 7 ms
        let truth = [Truth { flash_ns: 1_000_000_000, beep_ns: 1_005_000_000 }, Truth { flash_ns: 2_000_000_000, beep_ns: 2_005_000_000 }];
        let r = sync_report(&[0.5, 1.5], &[0.512, 1.512], &truth);
        assert_eq!(r.claps, 2);
        assert!((r.mean_ms - 7.0).abs() < 1e-6 && (r.max_abs_ms - 7.0).abs() < 1e-6);
    }
}
