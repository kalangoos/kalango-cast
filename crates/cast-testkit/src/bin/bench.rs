//! kalango-cast-bench: Kalango Cast against the KalangoOS shell's current recorder (gst-launch: pipewiresrc →
//! videoconvert → vp8enc deadline=1 cpu-used=8 → webmmux, as crates/kalango-shell/src/screenshot_tool.rs runs it),
//! recording the same nested niri session showing a full-screen animation.
//!
//! `kalango-cast-bench --cast BIN --clap BIN [--size WxH] [--seconds N] [--dir DIR]` (niri from KALANGO_CAST_NIRI)
//!
//! Per recorder: CPU (% of one core, the recorder process only), peak RSS, GPU and NVENC use (nvidia-smi, minus the
//! same scene without recording), frames in the file vs the frames the screen showed, file size, and (Kalango Cast)
//! the frame-arrival → on-disk latency.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cast_testkit::{NestedNiri, NullSink};
use serde_json::{json, Value};

fn proc_cpu_ns(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Some(ticks * 1_000_000_000 / hz)
}

fn proc_rss_mb(pid: u32) -> Option<f64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb: f64 = status.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0)
}

/// Samples the GPU's and the encoder's use (%) every 200 ms while running.
struct GpuSampler {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<(f64, f64)>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GpuSampler {
    fn start() -> GpuSampler {
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let (s2, v2) = (stop.clone(), samples.clone());
        let thread = std::thread::spawn(move || {
            let Ok(mut child) = Command::new("nvidia-smi")
                .args(["--query-gpu=utilization.gpu,utilization.encoder", "--format=csv,noheader,nounits", "-lms", "200"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                return;
            };
            let out = child.stdout.take().map(BufReader::new);
            if let Some(out) = out {
                for line in out.lines().map_while(Result::ok) {
                    let v: Vec<f64> = line.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    if v.len() == 2 {
                        if let Ok(mut s) = v2.lock() {
                            s.push((v[0], v[1]));
                        }
                    }
                    if s2.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        });
        GpuSampler { stop, samples, thread: Some(thread) }
    }

    fn finish(mut self) -> (f64, f64) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let s = self.samples.lock().map(|s| s.clone()).unwrap_or_default();
        if s.is_empty() {
            return (f64::NAN, f64::NAN);
        }
        let n = s.len() as f64;
        (s.iter().map(|x| x.0).sum::<f64>() / n, s.iter().map(|x| x.1).sum::<f64>() / n)
    }
}

/// Watches a process: CPU time used and peak RSS, until it exits.
fn watch(child: &mut Child) -> (f64, f64, f64) {
    let pid = child.id();
    let t0 = Instant::now();
    let cpu0 = proc_cpu_ns(pid).unwrap_or(0);
    let mut cpu_last = cpu0;
    let mut rss_max = 0.0f64;
    loop {
        if let Some(c) = proc_cpu_ns(pid) {
            cpu_last = c;
        }
        if let Some(r) = proc_rss_mb(pid) {
            rss_max = rss_max.max(r);
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let wall = t0.elapsed().as_secs_f64();
    ((cpu_last - cpu0) as f64 / 1e9 / wall * 100.0, rss_max, wall)
}

fn count_frames(file: &Path) -> u64 {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-count_packets", "-select_streams", "v:0", "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"])
        .arg(file)
        .output();
    out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().trim_end_matches(',').parse().ok()).unwrap_or(0)
}

fn main() {
    let mut cast = None;
    let mut clap = None;
    let mut size = (1920u32, 1080u32);
    let mut seconds = 30.0f64;
    let mut dir = std::env::temp_dir().join("kalango-cast-bench");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cast" => cast = args.next().map(PathBuf::from),
            "--clap" => clap = args.next().map(PathBuf::from),
            "--size" => {
                let v = args.next().unwrap_or_default();
                if let Some((w, h)) = v.split_once('x') {
                    size = (w.parse().unwrap_or(1920), h.parse().unwrap_or(1080));
                }
            }
            "--seconds" => seconds = args.next().and_then(|v| v.parse().ok()).unwrap_or(30.0),
            "--dir" => dir = args.next().map(PathBuf::from).unwrap_or(dir),
            _ => {
                eprintln!("kalango-cast-bench --cast BIN --clap BIN [--size WxH] [--seconds N] [--dir DIR]");
                std::process::exit(2);
            }
        }
    }
    let (Some(cast), Some(clap)) = (cast, clap) else {
        eprintln!("--cast and --clap are needed");
        std::process::exit(2);
    };
    let niri = cast_testkit::niri_binary().expect("KALANGO_CAST_NIRI");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let session = NestedNiri::start(&niri, &dir.join("session"), size.0, size.1).expect("niri");
    let mut emitter = session
        .command(&clap)
        .args(["--animate", "--seconds", &format!("{}", seconds * 3.0 + 40.0)])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("clap");
    let mut ready = String::new();
    BufReader::new(emitter.stdout.take().expect("stdout")).read_line(&mut ready).expect("ready");
    std::thread::sleep(Duration::from_secs(2));

    // the scene alone (animation, no recorder): the GPU's baseline
    let g = GpuSampler::start();
    std::thread::sleep(Duration::from_secs(5));
    let (gpu_base, enc_base) = g.finish();
    eprintln!("baseline: GPU {gpu_base:.1} %, NVENC {enc_base:.1} %");

    let expected = (seconds * 60.0).round() as u64;
    let mut rows = Vec::new();

    // Kalango Cast (video only: the old recorder takes no desktop sound; a second run adds sound)
    for (label, audio) in [("Kalango Cast (NVENC H.264, fMP4)", false), ("Kalango Cast + som (desktop+mic, Opus)", true)] {
        let out = dir.join(if audio { "cast-audio.mp4" } else { "cast.mp4" });
        let sinks =
            audio.then(|| (NullSink::create("kalango-cast-bench-desk").expect("sink"), NullSink::create("kalango-cast-bench-mic").expect("sink")));
        let mut cmd = Command::new(&cast);
        cmd.args([
            "record",
            "--monitor",
            "winit",
            "--seconds",
            &seconds.to_string(),
            "--json",
            "--bus",
            &session.dbus_address,
            "--overwrite",
            "--out",
        ])
        .arg(&out);
        match &sinks {
            Some((d, m)) => {
                cmd.args(["--desktop-sink", &d.name, "--mic-source", &m.name]);
            }
            None => {
                cmd.args(["--no-desktop", "--no-mic"]);
            }
        }
        let g = GpuSampler::start();
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().expect("kalango-cast");
        let stdout = child.stdout.take();
        let (cpu, rss, wall) = watch(&mut child);
        let (gpu, enc) = g.finish();
        let mut text = String::new();
        if let Some(mut s) = stdout {
            use std::io::Read;
            let _ = s.read_to_string(&mut text);
        }
        let report: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
        drop(sinks);
        let frames = count_frames(&out);
        let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        rows.push(json!({
            "recorder": label, "cpu_percent": cpu, "rss_mb": rss, "gpu_percent": gpu - gpu_base, "nvenc_percent": enc - enc_base,
            "frames": frames, "expected": expected, "dropped": expected.saturating_sub(frames), "bytes": bytes, "wall_s": wall,
            "latency_ms_avg": report["status"]["latency_ms_avg"], "latency_ms_max": report["status"]["latency_ms_max"],
            "encode_ms_avg": report["status"]["encode_ms_avg"], "zero_copy": report["status"]["zero_copy"],
            "audio_tracks": report["audio"].as_array().map(|a| a.len()).unwrap_or(0),
        }));
        std::thread::sleep(Duration::from_secs(2));
    }

    // the shell's recorder: its own ScreenCast session (as the shell makes it), then the same gst-launch line
    let sc = cast_capture::ScreenCast::connect(Some(&session.dbus_address)).expect("screencast");
    let stream = sc.record_monitor("winit", cast_capture::screencast::CURSOR_EMBEDDED).expect("stream");
    let webm = dir.join("shell.webm");
    let g = GpuSampler::start();
    let mut gst = Command::new("gst-launch-1.0")
        .args(["-e", "pipewiresrc", &format!("path={}", stream.node_id), "do-timestamp=true", "!", "videoconvert", "!"])
        .args(["queue", "!", "vp8enc", "deadline=1", "cpu-used=8", "!", "webmmux", "name=m", "!", "filesink"])
        .arg(format!("location={}", webm.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("gst-launch-1.0");
    let pid = gst.id();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs_f64(seconds));
        // SAFETY: SIGINT to our own child (gst-launch -e finishes the file on it)
        unsafe { libc::kill(pid as i32, libc::SIGINT) };
    });
    let (cpu, rss, wall) = watch(&mut gst);
    let _ = stopper.join();
    let (gpu, enc) = g.finish();
    drop(stream);
    let frames = count_frames(&webm);
    let bytes = std::fs::metadata(&webm).map(|m| m.len()).unwrap_or(0);
    rows.push(json!({
        "recorder": "shell hoje (gst vp8enc, WebM)", "cpu_percent": cpu, "rss_mb": rss, "gpu_percent": gpu - gpu_base, "nvenc_percent": enc - enc_base,
        "frames": frames, "expected": expected, "dropped": expected.saturating_sub(frames), "bytes": bytes, "wall_s": wall,
        "latency_ms_avg": Value::Null, "latency_ms_max": Value::Null, "encode_ms_avg": Value::Null, "zero_copy": false, "audio_tracks": 0,
    }));
    let _ = emitter.kill();
    let _ = emitter.wait();

    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"size": format!("{}x{}", size.0, size.1), "seconds": seconds, "gpu_baseline": gpu_base, "nvenc_baseline": enc_base, "rows": rows})
        )
        .unwrap_or_default()
    );
    println!();
    println!("| Gravador | CPU (% de 1 núcleo) | RSS pico (MB) | GPU (+%) | NVENC (+%) | Quadros (de {expected}) | Perdidos | Arquivo (MB) | Latência chegada→disco (ms, média/máx) |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        let lat = match (r["latency_ms_avg"].as_f64(), r["latency_ms_max"].as_f64()) {
            (Some(a), Some(m)) => format!("{a:.1} / {m:.1}"),
            _ => "não medida".into(),
        };
        println!(
            "| {} | {:.1} | {:.0} | {:.1} | {:.1} | {} | {} | {:.1} | {} |",
            r["recorder"].as_str().unwrap_or(""),
            r["cpu_percent"].as_f64().unwrap_or(0.0),
            r["rss_mb"].as_f64().unwrap_or(0.0),
            r["gpu_percent"].as_f64().unwrap_or(f64::NAN),
            r["nvenc_percent"].as_f64().unwrap_or(f64::NAN),
            r["frames"],
            r["dropped"],
            r["bytes"].as_u64().unwrap_or(0) as f64 / 1e6,
            lat
        );
    }
}
