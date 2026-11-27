//! kalango-cast: Kalango Cast from the command line.
//!
//! ```text
//! kalango-cast record [--monitor CONECTOR | --window ID | --area CONECTOR:X,Y,LxA] [--seconds N] [--out ARQUIVO]
//!                     [--encoder nvenc|openh264] [--codec h264|hevc] [--fps N] [--bitrate KBIT]
//!                     [--no-desktop] [--no-mic] [--desktop-sink NOME] [--mic-source NOME] [--no-cursor]
//!                     [--bus ENDEREÇO] [--overwrite] [--json]
//! kalango-cast sources [--bus ENDEREÇO]
//! kalango-cast caps
//! kalango-cast inspect ARQUIVO
//! kalango-cast openh264 download
//! kalango-cast rpc MÉTODO [JSON] [--socket CAMINHO]
//! kalango-cast approve CÓDIGO [--socket CAMINHO]
//! ```
//! `record` runs the engine in this process (Ctrl+C stops early); `rpc` talks to kalango-castd.

use std::path::PathBuf;
use std::time::Duration;

use cast_core::{tr, trf, EncoderKind, Source, VideoCodec};
use cast_engine::recorder::{AudioChoice, RecordOptions, Recorder};

fn usage() -> ! {
    eprintln!("{}", tr("usage: kalango-cast record|sources|caps|inspect|openh264|rpc …  (see the source's header or the README)"));
    std::process::exit(2)
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("kalango-cast: {msg}");
    std::process::exit(1)
}

fn parse_area(s: &str) -> Option<Source> {
    let (connector, rect) = s.rsplit_once(':')?;
    let (xy, wh) = rect.split_once(',').and_then(|(x, rest)| rest.split_once(',').map(|(y, wh)| ((x, y), wh)))?;
    let (w, h) = wh.split_once('x')?;
    Some(Source::Area { connector: connector.into(), x: xy.0.parse().ok()?, y: xy.1.parse().ok()?, width: w.parse().ok()?, height: h.parse().ok()? })
}

fn record(args: &[String]) {
    let mut source = None;
    let mut seconds = 10.0f64;
    let mut out = None;
    let mut encoder = None;
    let mut codec = VideoCodec::H264;
    let mut fps = 60;
    let mut bitrate = 0;
    let mut desktop = Some(AudioChoice::Default);
    let mut mic = Some(AudioChoice::Default);
    let mut bus = None;
    let mut json = false;
    let mut cursor = true;
    let mut overwrite = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().unwrap_or_else(|| usage());
        match a.as_str() {
            "--monitor" => source = Some(Source::Monitor { connector: val() }),
            "--window" => source = Some(Source::Window { id: val().parse().unwrap_or_else(|_| usage()) }),
            "--area" => source = Some(parse_area(&val()).unwrap_or_else(|| usage())),
            "--seconds" => seconds = val().parse().ok().filter(|s: &f64| s.is_finite() && *s > 0.0).unwrap_or_else(|| usage()),
            "--out" => out = Some(PathBuf::from(val())),
            "--encoder" => encoder = Some(EncoderKind::parse(&val()).unwrap_or_else(|| usage())),
            "--codec" => {
                codec = match val().as_str() {
                    "h264" => VideoCodec::H264,
                    "hevc" | "h265" => VideoCodec::Hevc,
                    _ => usage(),
                }
            }
            "--fps" => fps = val().parse().unwrap_or_else(|_| usage()),
            "--bitrate" => bitrate = val().parse().unwrap_or_else(|_| usage()),
            "--no-desktop" => desktop = Some(AudioChoice::Off),
            "--no-mic" => mic = Some(AudioChoice::Off),
            "--desktop-sink" => desktop = Some(AudioChoice::Named(val())),
            "--mic-source" => mic = Some(AudioChoice::Named(val())),
            "--no-cursor" => cursor = false,
            "--overwrite" => overwrite = true,
            "--bus" => bus = Some(val()),
            "--json" => json = true,
            _ => usage(),
        }
    }
    let source = match source {
        Some(s) => s,
        None => {
            let cast = cast_capture::ScreenCast::connect(bus.as_deref()).unwrap_or_else(|e| fail(e));
            let first = cast.sources().unwrap_or_else(|e| fail(e)).monitors.into_iter().next().unwrap_or_else(|| fail(tr("No monitor to record")));
            Source::Monitor { connector: first.connector }
        }
    };
    let out = out.unwrap_or_else(|| PathBuf::from(format!("kalango-cast-{}.mp4", std::process::id())));
    let out = if out.is_absolute() { out } else { std::env::current_dir().unwrap_or_default().join(out) };
    let mut o = RecordOptions::new(source, out.clone());
    o.overwrite = overwrite;
    o.encoder = encoder;
    o.codec = codec;
    o.fps = fps;
    o.bitrate_kbps = bitrate;
    o.desktop_audio = desktop;
    o.mic = mic;
    o.dbus_address = bus;
    o.cursor = cursor;
    o.normalize();

    // Ctrl+C / SIGTERM end the recording cleanly (the file is finished, not cut)
    // SAFETY: sigset operations on a local set; blocks the signals for this thread and the ones started later.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    let rec = Recorder::start(o).unwrap_or_else(|e| fail(e));
    let s = rec.status();
    if !json {
        eprintln!(
            "{}",
            trf(
                "Recording {w}×{h} with {encoder} ({device}{zc}) to {path}",
                &[
                    ("w", s.width.to_string()),
                    ("h", s.height.to_string()),
                    ("encoder", s.encoder.clone()),
                    ("device", s.device.clone()),
                    ("zc", if s.zero_copy { format!(", {}", tr("zero-copy")) } else { String::new() }),
                    ("path", s.path.clone()),
                ]
            )
        );
    }
    let start = std::time::Instant::now();
    loop {
        let left = seconds - start.elapsed().as_secs_f64();
        if left <= 0.0 {
            break;
        }
        let wait = Duration::from_secs_f64(left.min(1.0));
        let ts = libc::timespec { tv_sec: wait.as_secs() as libc::time_t, tv_nsec: wait.subsec_nanos() as libc::c_long };
        // SAFETY: waits for a blocked signal of `set` or the timeout.
        let sig = unsafe { libc::sigtimedwait(&set, std::ptr::null_mut(), &ts) };
        if sig > 0 {
            break;
        }
        if !json {
            let s = rec.status();
            eprintln!(
                "{}",
                trf(
                    "{t} s · {fps} fps · {frames} frames · {dropped} dropped · encode {enc} ms · latency {lat} ms · {kbps} kbit/s · CPU {cpu} %",
                    &[
                        ("t", format!("{:.0}", s.duration_s)),
                        ("fps", format!("{:.1}", s.fps)),
                        ("frames", s.frames.to_string()),
                        ("dropped", s.dropped.to_string()),
                        ("enc", format!("{:.2}", s.encode_ms_avg)),
                        ("lat", format!("{:.1}", s.latency_ms_avg)),
                        ("kbps", format!("{:.0}", s.bitrate_kbps)),
                        ("cpu", format!("{:.1}", s.cpu_percent)),
                    ]
                )
            );
        }
    }
    let (summary, status) = rec.stop().unwrap_or_else(|e| fail(e));
    let inspection = cast_mux::inspect::inspect_file(&out).ok();
    if json {
        let audio: Vec<_> = inspection
            .iter()
            .flat_map(|i| i.audio())
            .map(|a| serde_json::json!({"name": a.name, "packets": a.samples, "start_s": a.start_s(), "duration_s": a.duration_s()}))
            .collect();
        let video = inspection
            .as_ref()
            .and_then(|i| i.video())
            .map(|v| serde_json::json!({"frames": v.samples, "keyframes": v.sync_samples, "start_s": v.start_s(), "duration_s": v.duration_s()}));
        println!(
            "{}",
            serde_json::json!({
                "path": out,
                "duration_s": summary.duration_ns as f64 / 1e9,
                "bytes": summary.bytes,
                "fragments": summary.fragments,
                "video": video,
                "audio": audio,
                "status": status,
            })
        );
        return;
    }
    eprintln!(
        "{}",
        trf(
            "Done: {path} · {dur} s · {frames} frames · {dropped} dropped · {mb} MB · {frags} fragments · encode {enc} ms (max {encmax}) · latency {lat} ms (max {latmax}) · CPU {cpu} % · RSS {rss} MB",
            &[
                ("path", out.display().to_string()),
                ("dur", format!("{:.2}", summary.duration_ns as f64 / 1e9)),
                ("frames", summary.video_frames.to_string()),
                ("dropped", status.dropped.to_string()),
                ("mb", format!("{:.1}", summary.bytes as f64 / 1e6)),
                ("frags", summary.fragments.to_string()),
                ("enc", format!("{:.2}", status.encode_ms_avg)),
                ("encmax", format!("{:.2}", status.encode_ms_max)),
                ("lat", format!("{:.1}", status.latency_ms_avg)),
                ("latmax", format!("{:.1}", status.latency_ms_max)),
                ("cpu", format!("{:.1}", status.cpu_percent)),
                ("rss", format!("{:.0}", status.rss_mb)),
            ]
        )
    );
    if let Some(i) = inspection {
        for a in i.audio() {
            eprintln!("  {} · {} × 20 ms · {:.2}–{:.2} s", a.name, a.samples, a.start_s(), a.end_s());
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { usage() };
    let rest = &args[1..];
    match cmd.as_str() {
        "record" => record(rest),
        "sources" => {
            let bus = rest.iter().position(|a| a == "--bus").and_then(|i| rest.get(i + 1)).cloned();
            let cast = cast_capture::ScreenCast::connect(bus.as_deref()).unwrap_or_else(|e| fail(e));
            let s = cast.sources().unwrap_or_else(|e| fail(e));
            println!("{}", serde_json::to_string_pretty(&s).unwrap_or_default());
        }
        "caps" => println!("{}", serde_json::to_string_pretty(&cast_encode::probe()).unwrap_or_default()),
        "inspect" => {
            let path = rest.first().unwrap_or_else(|| usage());
            let i = cast_mux::inspect::inspect_file(std::path::Path::new(path)).unwrap_or_else(|e| fail(e));
            for t in &i.tracks {
                println!("{} {} {} · {} · {:.3}–{:.3} s", t.id, t.handler, t.codec, t.samples, t.start_s(), t.end_s());
            }
            println!("{} {} · mfra {} · tail {}", i.fragments, tr("fragments"), i.has_mfra, i.tail_bytes);
        }
        "openh264" if rest.first().map(String::as_str) == Some("download") => {
            let dir = cast_encode::openh264::download::cache_dir().unwrap_or_else(|| fail("HOME"));
            eprintln!("{}", tr("Downloading OpenH264 from Cisco (openh264.org); Cisco licenses the H.264 patents for its binary."));
            match cast_encode::openh264::download::fetch(&dir) {
                Ok(p) => eprintln!("{}: {}", tr("Verified and installed"), p.display()),
                Err(e) => fail(e),
            }
        }
        "approve" => {
            let token = rest.first().unwrap_or_else(|| usage());
            let socket = rest
                .iter()
                .position(|a| a == "--socket")
                .and_then(|i| rest.get(i + 1))
                .map(PathBuf::from)
                .or_else(cast_engine::rpc::default_socket)
                .unwrap_or_else(|| fail("XDG_RUNTIME_DIR"));
            match cast_engine::rpc::request(&socket, "consent.approve", serde_json::json!({"token": token})) {
                Ok(v) if v.get("error").is_none() => eprintln!("{}", tr("Approved: the held recording may start now (ask the program to try again)")),
                Ok(v) => fail(v["error"]["message"].as_str().unwrap_or("error")),
                Err(e) => fail(format!("{}: {e}", socket.display())),
            }
        }
        "rpc" => {
            let method = rest.first().unwrap_or_else(|| usage());
            let params = rest
                .get(1)
                .filter(|a| !a.starts_with("--"))
                .map(|p| serde_json::from_str(p).unwrap_or_else(|e| fail(e)))
                .unwrap_or(serde_json::json!({}));
            let socket = rest
                .iter()
                .position(|a| a == "--socket")
                .and_then(|i| rest.get(i + 1))
                .map(PathBuf::from)
                .or_else(cast_engine::rpc::default_socket)
                .unwrap_or_else(|| fail("XDG_RUNTIME_DIR"));
            match cast_engine::rpc::request(&socket, method, params) {
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
                Err(e) => fail(format!("{}: {e}", socket.display())),
            }
        }
        _ => usage(),
    }
}
