//! End to end against a real niri: a nested session on DISPLAY=:0 (niri with KalangoOS's window-screencast patch,
//! `KALANGO_CAST_NIRI`), its ScreenCast D-Bus API, PipeWire, the GPU encoder and the MP4 writer.
//!
//! One test, in order (one session for all): 3 s of a monitor with desktop + microphone sound (stand-in devices),
//! the clap A/V sync measurement, a window, an area, and the daemon's JSON-RPC.
//!
//! `#[ignore]`d (CI has no GPU nor display): on a developer machine it is the acceptance test, and it fails if
//! `KALANGO_CAST_NIRI` is not set:
//! `KALANGO_CAST_NIRI=/path/to/niri cargo test -p cast-testkit --test niri_session -- --ignored --nocapture`.
//! (It runs the daemon with `consent = "allow"`: no person is there to approve.)

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use cast_core::Source;
use cast_engine::recorder::{AudioChoice, RecordOptions, Recorder};
use cast_mux::inspect::inspect_file;
use cast_testkit::analysis::{self, Truth};
use cast_testkit::{have, NestedNiri, NullSink};

fn dir() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("niri-session");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffprobe_dims(file: &Path) -> (u32, u32) {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.trim().split(',').map(|v| v.parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

fn options(session: &NestedNiri, source: Source, path: PathBuf) -> RecordOptions {
    let mut o = RecordOptions::new(source, path);
    o.dbus_address = Some(session.dbus_address.clone());
    o.desktop_audio = Some(AudioChoice::Off);
    o.mic = Some(AudioChoice::Off);
    o
}

fn record_for(o: RecordOptions, seconds: f64) -> (cast_mux::Summary, cast_engine::Status) {
    let r = Recorder::start(o).expect("recording starts");
    std::thread::sleep(Duration::from_secs_f64(seconds));
    r.stop().expect("recording stops")
}

#[test]
#[ignore = "needs a niri that screencasts in a window and a display: set KALANGO_CAST_NIRI and pass --ignored"]
fn niri_session_end_to_end() {
    let niri = cast_testkit::niri_binary().expect("KALANGO_CAST_NIRI points at the patched niri (tests/niri/README.md)");
    assert!(have("ffprobe") && have("ffmpeg"), "ffmpeg/ffprobe are needed to check the files");
    let d = dir();
    let session = NestedNiri::start(&niri, &d.join("session"), 1280, 800).expect("nested niri");
    let clap_bin = env!("CARGO_BIN_EXE_kalango-cast-clap");

    // sources: the winit monitor, and the window of the clap emitter once it is up
    let desk = NullSink::create("kalango-cast-test-desktop").expect("null sink");
    let mic = NullSink::create_source("kalango-cast-test-mic").expect("null source");
    let truth_path = d.join("truth.json");
    let mut emitter = session
        .command(clap_bin)
        .args(["--clap", "--seconds", "45", "--sink", &desk.name, "--truth"])
        .arg(&truth_path)
        .stdout(Stdio::piped())
        .spawn()
        .expect("clap emitter");
    let mut line = String::new();
    BufReader::new(emitter.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    let cast = cast_capture::ScreenCast::connect(Some(&session.dbus_address)).unwrap();
    let sources = cast.sources().unwrap();
    eprintln!("sources: {sources:?}");
    let monitor = sources.monitors.first().expect("a monitor").connector.clone();

    // 1. 3 s of the monitor, with desktop sound and microphone
    let mp4 = d.join("monitor.mp4");
    let mut o = options(&session, Source::Monitor { connector: monitor.clone() }, mp4.clone());
    o.desktop_audio = Some(AudioChoice::Named(desk.name.clone()));
    o.mic = Some(AudioChoice::Named(mic.name.clone()));
    let (summary, status) = record_for(o, 3.0);
    eprintln!("monitor: {summary:?}\n{status:?}");
    assert_eq!(status.encoder, "nvenc", "this machine's encoder");
    assert!(status.dmabuf, "frames came as DMA-BUF");
    assert!(status.zero_copy);
    let i = inspect_file(&mp4).unwrap();
    let v = i.video().expect("video track");
    assert!(v.samples > 100, "frames: {}", v.samples);
    assert!((v.end_s() - 3.0).abs() < 0.35, "video ends at {}", v.end_s());
    let audio: Vec<_> = i.audio().collect();
    assert_eq!(audio.len(), 2, "desktop and microphone tracks");
    for a in &audio {
        assert!(a.samples >= 100, "{}: {} packets", a.name, a.samples);
        assert!((a.end_s() - 3.0).abs() < 0.35, "{} ends at {}", a.name, a.end_s());
    }
    assert!((summary.duration_ns as f64 / 1e9 - 3.0).abs() < 0.35);
    assert_eq!(ffprobe_dims(&mp4), (1280, 800));

    // 2. the clap: flashes and beeps, A/V offset in the file vs the emitter's log
    let clap_mp4 = d.join("clap.mp4");
    let mut o = options(&session, Source::Monitor { connector: monitor.clone() }, clap_mp4.clone());
    o.desktop_audio = Some(AudioChoice::Named(desk.name.clone()));
    let (_, clap_status) = record_for(o, 10.0);

    // 3. a window (the emitter's)
    let window = sources.windows.iter().find(|w| w.app_id == "kalango-cast-clap").expect("emitter window").id;
    let win_mp4 = d.join("window.mp4");
    let (ws, _) = record_for(options(&session, Source::Window { id: window }, win_mp4.clone()), 2.0);
    assert!(ws.video_frames > 30, "window frames: {}", ws.video_frames);

    // 4. an area (cropped on the GPU)
    let area_mp4 = d.join("area.mp4");
    let area = Source::Area { connector: monitor.clone(), x: 100, y: 50, width: 640, height: 360 };
    let (as_, _) = record_for(options(&session, area, area_mp4.clone()), 2.0);
    assert!(as_.video_frames > 30);
    assert_eq!(ffprobe_dims(&area_mp4), (640, 360));

    // 5. the daemon: record.start / status / stop over the socket
    let sock = d.join("rt").join("kalango").join("cast.sock");
    let listener = cast_engine::rpc::bind(&sock).unwrap();
    let daemon = Arc::new(cast_engine::rpc::Daemon::with_bus(Some(session.dbus_address.clone())).with_consent(cast_engine::rpc::Consent::Allow));
    let dm = daemon.clone();
    std::thread::spawn(move || dm.serve(listener));
    let rpc_mp4 = d.join("rpc.mp4");
    let r = cast_engine::rpc::request(
        &sock,
        "record.start",
        serde_json::json!({"source": {"kind": "monitor", "connector": monitor}, "path": rpc_mp4, "desktop_audio": false, "mic": false}),
    )
    .unwrap();
    assert!(r.get("result").is_some(), "{r}");
    std::thread::sleep(Duration::from_millis(1500));
    let st = cast_engine::rpc::request(&sock, "record.status", serde_json::json!({})).unwrap();
    assert_eq!(st["result"]["recording"], true, "{st}");
    assert!(st["result"]["frames"].as_u64().unwrap() > 0);
    let again = cast_engine::rpc::request(
        &sock,
        "record.start",
        serde_json::json!({"source": {"kind": "monitor", "connector": "winit"}, "path": d.join("x.mp4")}),
    )
    .unwrap();
    assert!(again.get("error").is_some(), "one recording at a time");
    let stop = cast_engine::rpc::request(&sock, "record.stop", serde_json::json!({})).unwrap();
    assert!(stop["result"]["video_frames"].as_u64().unwrap() > 30, "{stop}");

    // the clap analysis (the emitter writes its log when it exits)
    let _ = emitter.kill();
    let _ = emitter.wait();
    drop(desk);
    drop(mic);
    let truth: Vec<Truth> = std::fs::read_to_string(&truth_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let luma = analysis::video_luma(&clap_mp4).unwrap();
    let flashes = analysis::flash_onsets(&luma);
    let (pcm, start) = analysis::audio_pcm(&clap_mp4, 0).unwrap();
    let beeps = analysis::beep_onsets(&pcm, start, 48_000.0, 0.1);
    let truth = analysis::align_truth(&truth, &flashes, clap_status.origin_ns);
    let report = analysis::sync_report(&flashes, &beeps, &truth);
    eprintln!("A/V sync: {} claps, flashes {flashes:?}, beeps {beeps:?}\n{report:?}", report.claps);
    std::fs::write(d.join("sync.json"), serde_json::to_string_pretty(&report).unwrap()).unwrap();
    assert!(report.claps >= 6, "claps measured: {}", report.claps);
    assert!(report.max_abs_ms < 20.0, "A/V offset up to {:.1} ms", report.max_abs_ms);
}
