//! A recording cut short (a crash, a full disk) is still a playable file up to its last complete fragment.

use std::path::{Path, PathBuf};
use std::process::Command;

use cast_core::VideoCodec;
use cast_mux::inspect::{inspect, inspect_file};
use cast_mux::{avc, AudioTrack, Config, VideoTrack, Writer};

/// A real H.264 High SPS/PPS (1920×1080) so that readers accept the sample description.
const PARAMS: &[u8] =
    &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0x84, 0, 0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cast-mux");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    // a fresh file each run (the writer refuses to replace anything it was not asked to)
    let _ = std::fs::remove_file(&path);
    path
}

fn config() -> Config {
    Config {
        video: Some(VideoTrack { codec: VideoCodec::H264, width: 1920, height: 1080, config: avc::avcc(PARAMS).unwrap() }),
        audio: vec![
            AudioTrack { channels: 2, pre_skip: 312, input_sample_rate: 48_000, name: "Desktop".into() },
            AudioTrack { channels: 2, pre_skip: 312, input_sample_rate: 48_000, name: "Microphone".into() },
        ],
        fragment_ns: 2_000_000_000,
        sync: true,
    }
}

/// Writes `seconds` of 60 fps video (a keyframe every 2 s) and two 20 ms Opus tracks; returns the writer unfinished.
fn record(path: &Path, seconds: u64) -> Writer {
    let mut w = Writer::create(path, &config(), true).unwrap();
    let mut sample = Vec::new();
    let frames = seconds * 60;
    let mut audio_at = 0u64;
    for i in 0..frames {
        let pts = i * 1_000_000_000 / 60;
        let key = i % 120 == 0;
        let nal_type = if key { 0x65 } else { 0x41 };
        let mut au = vec![0, 0, 0, 1, nal_type];
        au.extend(std::iter::repeat_n((i % 251) as u8, if key { 20_000 } else { 3_000 }));
        let is_key = avc::annexb_to_sample(VideoCodec::H264, &au, &mut sample);
        assert_eq!(is_key, key);
        w.write_video(&sample, pts, key).unwrap();
        // the audio packets due by this frame
        while audio_at <= pts {
            for track in 0..2 {
                w.write_audio(track, &[0xfc, 0xff, 0xfe, track as u8], audio_at, 960).unwrap();
            }
            audio_at += 20_000_000;
        }
    }
    w
}

fn ffprobe_packets(path: &Path, stream: &str) -> Option<(bool, u64, f64)> {
    // a missing ffprobe is a failure, not a skip: the tests would pass without checking the file
    assert!(Command::new("ffprobe").arg("-version").output().is_ok_and(|o| o.status.success()), "ffprobe is required (package ffmpeg)");
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_packets",
            "-select_streams",
            stream,
            "-show_entries",
            "stream=nb_read_packets:format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let packets = lines.next()?.trim().trim_end_matches(',').parse().ok()?;
    let duration = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0.0);
    Some((out.status.success(), packets, duration))
}

#[test]
fn a_finished_recording_has_every_sample_and_its_duration() {
    let path = scratch("finished.mp4");
    let summary = record(&path, 10).finish().unwrap();
    assert_eq!(summary.video_frames, 600);
    assert!((summary.duration_ns as f64 / 1e9 - 10.0).abs() < 0.03, "{summary:?}");
    let i = inspect_file(&path).unwrap();
    assert_eq!(i.brand, "isom");
    assert!(i.has_mfra);
    assert_eq!(i.tail_bytes, 0);
    assert_eq!(i.fragments, 5, "one fragment per 2 s GOP");
    let v = i.video().unwrap();
    assert_eq!((v.codec.as_str(), v.samples, v.sync_samples), ("avc1", 600, 5));
    assert!((v.duration_s() - 10.0).abs() < 0.02, "{}", v.duration_s());
    let audio: Vec<_> = i.audio().collect();
    assert_eq!(audio.len(), 2);
    assert_eq!(audio[0].name, "Desktop");
    assert_eq!(audio[1].codec, "Opus");
    assert!(audio.iter().all(|a| a.samples == 500), "10 s of 20 ms packets");
    assert!((i.movie_duration_ms as f64 - 10_000.0).abs() < 30.0, "{}", i.movie_duration_ms);
    if let Some((ok, packets, duration)) = ffprobe_packets(&path, "v:0") {
        assert!(ok);
        assert_eq!(packets, 600);
        assert!((duration - 10.0).abs() < 0.05, "ffprobe duration {duration}");
        let (_, audio_packets, _) = ffprobe_packets(&path, "a:0").unwrap();
        assert_eq!(audio_packets, 500);
    }
}

/// The crash: the file ends in the middle of a fragment's media data, and that fragment's index was still the
/// unpublished `free` box (what the writer leaves until the data is synced).
#[test]
fn a_file_cut_mid_fragment_keeps_every_complete_fragment() {
    let path = scratch("whole.mp4");
    record(&path, 9).finish().unwrap();
    let data = std::fs::read(&path).unwrap();
    // the 4th moof, and a cut halfway into its mdat
    let mut moofs = Vec::new();
    let mut i = 0;
    while i + 8 <= data.len() {
        let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        if &data[i + 4..i + 8] == b"moof" {
            moofs.push((i, size));
        }
        i += size;
    }
    assert!(moofs.len() >= 4, "{} fragments", moofs.len());
    let (at, size) = moofs[3];
    let mdat_size = u32::from_be_bytes(data[at + size..at + size + 4].try_into().unwrap()) as usize;
    let cut = at + size + mdat_size / 2;

    for (name, publish) in [("torn-free.mp4", false), ("torn-moof.mp4", true)] {
        let mut torn = data[..cut].to_vec();
        if !publish {
            torn[at + 4..at + 8].copy_from_slice(b"free");
        }
        let torn_path = scratch(name);
        std::fs::write(&torn_path, &torn).unwrap();
        let i = inspect(&torn);
        assert_eq!(i.fragments, 3, "{name}: three complete fragments survive");
        assert!(i.tail_bytes > 0, "{name}: the torn fragment is the tail");
        let v = i.video().unwrap();
        assert_eq!(v.samples, 360, "{name}: 3 × 2 s at 60 fps");
        assert!((v.duration_s() - 6.0).abs() < 0.02);
        assert!(i.audio().all(|a| a.samples >= 290), "{name}: the audio of those fragments");
        if let Some((_, packets, _)) = ffprobe_packets(&torn_path, "v:0") {
            // ffprobe reads all complete samples (and, from a published torn fragment, the whole samples in it)
            assert!(packets >= 360, "{name}: ffprobe read {packets} packets");
        }
    }
}

/// The process died without `finish`: no `mfra`, no final duration, but every flushed fragment is there.
#[test]
fn an_unfinished_recording_is_readable() {
    let path = scratch("unfinished.mp4");
    let w = record(&path, 7);
    drop(w);
    let i = inspect_file(&path).unwrap();
    assert!(!i.has_mfra);
    assert_eq!(i.movie_duration_ms, 0);
    assert_eq!(i.fragments, 3);
    // the 4th keyframe (at 6 s) flushed the 3rd GOP; the 4th was still in memory
    assert_eq!(i.video().unwrap().samples, 360);
    assert_eq!(i.tail_bytes, 0);
    if let Some((ok, packets, _)) = ffprobe_packets(&path, "v:0") {
        assert!(ok);
        assert_eq!(packets, 360);
    }
}
