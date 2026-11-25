//! One recording: screencast → encoder → MP4, with the sound, on one clock.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cast_capture::{AudioCapture, AudioSink, AudioTarget, ScreenCast, StreamFormat, VideoCapture, VideoSink};
use cast_core::i18n::n_;
use cast_core::{fourcc, monotonic_ns, tr, trf, AudioTimeline, EncoderKind, MediaClock, Placement, Rect, Source, VideoCodec, VideoFrame};
use cast_encode::opus::{OpusEncoder, FRAME_SAMPLES};
use cast_encode::{EncodedFrame, EncoderInfo, GlContext, Settings, VideoEncoder};
use cast_mux::{avc, AudioTrack, Summary, VideoTrack, Writer};
use serde::{Deserialize, Serialize};

use crate::stats::{self, Counters, Nvml, Status};

/// A sound to record: the default device, a named one, or none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum AudioChoice {
    #[default]
    Default,
    Named(String),
    #[serde(skip)]
    Off,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordOptions {
    pub source: Source,
    pub path: PathBuf,
    #[serde(default)]
    pub encoder: Option<EncoderKind>,
    #[serde(default = "default_codec")]
    pub codec: VideoCodec,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default)]
    pub bitrate_kbps: u32,
    #[serde(default)]
    pub desktop_audio: Option<AudioChoice>,
    #[serde(default)]
    pub mic: Option<AudioChoice>,
    /// The compositor's session bus (None: this process's session bus).
    #[serde(default)]
    pub dbus_address: Option<String>,
    #[serde(default = "default_cursor")]
    pub cursor: bool,
    /// Replace an existing recording of ours at `path` (never any other file; see `cast_mux::open_output`).
    #[serde(default)]
    pub overwrite: bool,
}

fn default_codec() -> VideoCodec {
    VideoCodec::H264
}

fn default_fps() -> u32 {
    60
}

fn default_cursor() -> bool {
    true
}

impl RecordOptions {
    pub fn new(source: Source, path: PathBuf) -> Self {
        RecordOptions {
            source,
            path,
            encoder: None,
            codec: VideoCodec::H264,
            fps: 60,
            bitrate_kbps: 0,
            desktop_audio: Some(AudioChoice::Default),
            mic: Some(AudioChoice::Default),
            dbus_address: None,
            cursor: true,
            overwrite: false,
        }
    }

    fn audio_targets(&self) -> Vec<(AudioTarget, &'static str)> {
        let mut t = Vec::new();
        let named = |c: &AudioChoice| match c {
            AudioChoice::Named(n) => Some(n.clone()),
            _ => None,
        };
        if let Some(c) = self.desktop_audio.as_ref().filter(|c| **c != AudioChoice::Off) {
            t.push((AudioTarget::Desktop { sink: named(c) }, n_("Desktop")));
        }
        if let Some(c) = self.mic.as_ref().filter(|c| **c != AudioChoice::Off) {
            t.push((AudioTarget::Mic { source: named(c) }, n_("Microphone")));
        }
        t
    }
}

/// Buffers cycle between the encode/audio threads and the mux thread instead of being allocated per packet.
struct Pool {
    take: Receiver<Vec<u8>>,
    capacity: usize,
}

impl Pool {
    /// A pool and the sender that gives buffers back to it.
    fn new(capacity: usize) -> (Pool, Sender<Vec<u8>>) {
        let (tx, rx) = mpsc::channel();
        (Pool { take: rx, capacity }, tx)
    }

    fn get(&self) -> Vec<u8> {
        self.take.try_recv().unwrap_or_else(|_| Vec::with_capacity(self.capacity))
    }
}

enum MuxMsg {
    /// The video track is known (the encoder opened): create the file.
    Start(VideoTrack),
    Video {
        data: Vec<u8>,
        pts_ns: u64,
        key: bool,
        arrived_ns: u64,
    },
    Audio {
        track: usize,
        data: Vec<u8>,
        pts_ns: u64,
    },
    Finish,
}

/// A keyframe every 2 s of media time, which is also the MP4 fragment length.
const KEYFRAME_NS: u64 = 2_000_000_000;

/// Is a keyframe due for a frame at `pts_ns`, the last one having been at `last_key_pts_ns`?
fn keyframe_due(last_key_pts_ns: Option<u64>, pts_ns: u64) -> bool {
    last_key_pts_ns.is_none_or(|k| pts_ns >= k + KEYFRAME_NS)
}

/// Hands a message to the writer thread without ever blocking (the capture callbacks must not wait on the disk):
/// false when the queue is full or the writer is gone, and the caller counts a drop.
fn offer(tx: &SyncSender<MuxMsg>, msg: MuxMsg) -> bool {
    tx.try_send(msg).is_ok()
}

/// What the video thread tells the recorder once the encoder is up (or failed).
type Opened = Result<(EncoderInfo, u32, u32), String>;

pub struct Recorder {
    options: RecordOptions,
    counters: Arc<Counters>,
    video: Option<VideoCapture>,
    audio: Option<AudioCapture>,
    mux_tx: SyncSender<MuxMsg>,
    mux: Option<JoinHandle<Result<Summary, String>>>,
    _stream: cast_capture::Stream,
    info: EncoderInfo,
    size: (u32, u32),
    audio_tracks: u32,
    cpu_start: u64,
    nvml: Option<Nvml>,
    error: Arc<Mutex<Option<String>>>,
}

struct VideoState {
    kind: EncoderKind,
    codec: VideoCodec,
    fps: u32,
    bitrate_kbps: u32,
    crop: Option<Rect>,
    gl: Option<GlContext>,
    encoder: Option<Box<dyn VideoEncoder>>,
    clock: MediaClock,
    out: EncodedFrame,
    pool: Pool,
    mux: SyncSender<MuxMsg>,
    counters: Arc<Counters>,
    opened: Option<Sender<Opened>>,
    error: Arc<Mutex<Option<String>>>,
    last_key_pts: Option<u64>,
}

impl VideoState {
    fn fail(&mut self, e: String) {
        if let Some(tx) = self.opened.take() {
            let _ = tx.send(Err(e.clone()));
        }
        if let Ok(mut slot) = self.error.lock() {
            slot.get_or_insert(e);
        }
    }
}

impl VideoSink for VideoState {
    fn format(&mut self, f: &StreamFormat) {
        if self.encoder.is_some() {
            // a renegotiation (a window resized): the new buffers are new fds; the encoder keeps its size and
            // the GPU blit scales into it
            return;
        }
        let (mut w, mut h) = (f.width, f.height);
        if let Some(c) = self.crop.and_then(|c| c.clipped(f.width, f.height)) {
            (w, h) = (c.width, c.height);
        }
        let settings =
            Settings { codec: self.codec, width: w & !1, height: h & !1, fps: self.fps, bitrate_kbps: self.bitrate_kbps, keyframe_interval_s: 2 };
        match cast_encode::open(self.kind, &settings, self.gl.take()) {
            Ok(enc) => {
                let Some(config) = avc::codec_config(self.codec, enc.parameter_sets()) else {
                    return self.fail(tr("The encoder gave no parameter sets").into());
                };
                let track = VideoTrack { codec: self.codec, width: settings.width, height: settings.height, config };
                let _ = self.mux.send(MuxMsg::Start(track));
                if let Some(tx) = self.opened.take() {
                    let _ = tx.send(Ok((enc.info().clone(), settings.width, settings.height)));
                }
                self.encoder = Some(enc);
            }
            Err(e) => self.fail(e.0),
        }
    }

    fn frame(&mut self, frame: &VideoFrame, received_ns: u64) {
        if self.counters.failed.load(Ordering::Acquire) {
            return;
        }
        let Some(enc) = self.encoder.as_mut() else { return };
        let pts = self.clock.media_ns(received_ns);
        if pts < 0 {
            return;
        }
        let t0 = monotonic_ns();
        // a keyframe every 2 s of media time (frames come at a variable rate: a still screen sends few, so the
        // encoders' own GOP is off and the keyframes, which start the MP4 fragments, go by time)
        if keyframe_due(self.last_key_pts, pts as u64) {
            enc.force_keyframe();
        }
        if let Err(e) = enc.encode(frame, self.crop, pts as u64, &mut self.out) {
            self.counters.encode_errors.fetch_add(1, Ordering::Relaxed);
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            if self.counters.encode_errors.load(Ordering::Relaxed) == 1 {
                eprintln!("{}: {e}", tr("encode failed"));
            }
            enc.force_keyframe();
            return;
        }
        let t1 = monotonic_ns();
        Counters::add_max(&self.counters.encode_ns_sum, &self.counters.encode_ns_max, t1 - t0);
        if self.out.data.is_empty() {
            return;
        }
        let mut sample = self.pool.get();
        let key = avc::annexb_to_sample(self.codec, &self.out.data, &mut sample) || self.out.keyframe;
        if key {
            self.last_key_pts = Some(pts as u64);
        }
        let bytes = sample.len() as u64;
        if offer(&self.mux, MuxMsg::Video { data: sample, pts_ns: pts as u64, key, arrived_ns: received_ns }) {
            self.counters.video_bytes.fetch_add(bytes, Ordering::Relaxed);
            self.counters.last_pts_ns.store(pts as u64, Ordering::Relaxed);
            self.counters.tick_frame(t1);
        } else {
            // the writer cannot keep up: drop the frame and restart from a keyframe, so the next frames never
            // refer to the one that was lost
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            self.last_key_pts = None;
        }
    }

    fn skipped(&mut self) {
        self.counters.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

struct AudioTrackState {
    timeline: AudioTimeline,
    encoder: OpusEncoder,
    /// Samples waiting to fill a 20 ms frame.
    pending: Vec<f32>,
    /// Media time of the next packet.
    packets: u64,
}

struct AudioState {
    clock: MediaClock,
    tracks: Vec<AudioTrackState>,
    pool: Pool,
    mux: SyncSender<MuxMsg>,
    counters: Arc<Counters>,
}

impl AudioState {
    fn push(&mut self, index: usize, samples: &[f32]) {
        let Some(t) = self.tracks.get_mut(index) else { return };
        let ch = cast_capture::audio::CHANNELS;
        let frame_len = FRAME_SAMPLES * ch;
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (frame_len - t.pending.len()).min(rest.len());
            t.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if t.pending.len() == frame_len {
                let mut data = self.pool.get();
                if t.encoder.encode_frame(&t.pending, &mut data).is_ok() {
                    let start = t.timeline.start_ns().unwrap_or(0).max(0) as u64;
                    let pts = start + t.packets * 20_000_000;
                    t.packets += 1;
                    let bytes = data.len() as u64;
                    if offer(&self.mux, MuxMsg::Audio { track: index, data, pts_ns: pts }) {
                        self.counters.audio_packets.fetch_add(1, Ordering::Relaxed);
                        self.counters.audio_bytes.fetch_add(bytes, Ordering::Relaxed);
                    } else {
                        self.counters.audio_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                t.pending.clear();
            }
        }
    }
}

/// Zeros for inserted silence (one 20 ms stereo frame).
static SILENCE: [f32; FRAME_SAMPLES * 2] = [0.0; FRAME_SAMPLES * 2];

impl AudioSink for AudioState {
    fn samples(&mut self, index: usize, pcm: &[f32], frames: usize, first_ns: i64) {
        if self.counters.failed.load(Ordering::Acquire) {
            return;
        }
        let Some(t) = self.tracks.get_mut(index) else { return };
        let media = first_ns - self.clock.origin_ns() as i64;
        let ch = cast_capture::audio::CHANNELS;
        match t.timeline.place(media, frames as u64) {
            Placement::Append => self.push(index, pcm),
            Placement::Silence(n) => {
                self.counters.audio_corrections.fetch_add(1, Ordering::Relaxed);
                let mut left = n as usize * ch;
                while left > 0 {
                    let k = left.min(SILENCE.len());
                    self.push(index, &SILENCE[..k]);
                    left -= k;
                }
                self.push(index, pcm);
            }
            Placement::Skip(n) => {
                let skip = (n as usize * ch).min(pcm.len());
                if self.tracks[index].timeline.start_ns().is_some() && n > 0 && self.tracks[index].packets > 0 {
                    self.counters.audio_corrections.fetch_add(1, Ordering::Relaxed);
                }
                self.push(index, &pcm[skip..]);
            }
        }
    }
}

/// Opens the writer once the video track is known.
type Opener = Box<dyn FnOnce(&cast_mux::Config) -> std::io::Result<Writer> + Send>;

/// The file cannot be written: remember why (visible in `Status::error`), tell the capture threads to stop, and drop
/// the writer without finishing it (what is on disk stays playable up to its last complete fragment).
fn fail_mux(e: String, failure: &mut Option<String>, error: &Mutex<Option<String>>, counters: &Counters, writer: &mut Option<Writer>) {
    counters.failed.store(true, Ordering::Release);
    if let Ok(mut slot) = error.lock() {
        slot.get_or_insert_with(|| e.clone());
    }
    failure.get_or_insert(e);
    *writer = None;
}

fn mux_thread(
    open: Opener,
    audio: Vec<AudioTrack>,
    rx: Receiver<MuxMsg>,
    recycle_video: Sender<Vec<u8>>,
    recycle_audio: Sender<Vec<u8>>,
    counters: Arc<Counters>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<Summary, String> {
    let mut open = Some(open);
    let mut writer: Option<Writer> = None;
    let mut failure: Option<String> = None;
    // audio that comes before the video track is known (the encoder opens on the first format)
    let mut early: VecDeque<(usize, Vec<u8>, u64)> = VecDeque::new();
    let mut fragments_seen = 0;
    // After a failure the loop keeps draining, so the capture side never blocks and buffers keep cycling.
    for msg in rx {
        if failure.is_some() {
            match msg {
                MuxMsg::Finish => break,
                MuxMsg::Video { data, .. } => {
                    let _ = recycle_video.send(data);
                }
                MuxMsg::Audio { data, .. } => {
                    let _ = recycle_audio.send(data);
                }
                MuxMsg::Start(_) => {}
            }
            continue;
        }
        match msg {
            MuxMsg::Start(video) => {
                let config = cast_mux::Config { video: Some(video), audio: audio.clone(), fragment_ns: KEYFRAME_NS, sync: true };
                let Some(open) = open.take() else { continue };
                match open(&config) {
                    Ok(mut w) => {
                        let mut result = Ok(());
                        for (track, data, pts) in early.drain(..) {
                            if result.is_ok() {
                                result = w.write_audio(track, &data, pts, FRAME_SAMPLES as u32);
                            }
                            let _ = recycle_audio.send(data);
                        }
                        match result {
                            Ok(()) => writer = Some(w),
                            Err(e) => fail_mux(e.to_string(), &mut failure, &error, &counters, &mut writer),
                        }
                    }
                    Err(e) => fail_mux(e.to_string(), &mut failure, &error, &counters, &mut writer),
                }
            }
            MuxMsg::Video { data, pts_ns, key, arrived_ns } => {
                if let Some(w) = writer.as_mut() {
                    match w.write_video(&data, pts_ns, key) {
                        Ok(()) => {
                            let d = monotonic_ns().saturating_sub(arrived_ns);
                            Counters::add_max(&counters.disk_ns_sum, &counters.disk_ns_max, d);
                            counters.disk_samples.fetch_add(1, Ordering::Relaxed);
                            if key {
                                fragments_seen += 1;
                                counters.fragments.store(fragments_seen, Ordering::Relaxed);
                            }
                        }
                        Err(e) => fail_mux(e.to_string(), &mut failure, &error, &counters, &mut writer),
                    }
                }
                let _ = recycle_video.send(data);
            }
            MuxMsg::Audio { track, data, pts_ns } => match writer.as_mut() {
                Some(w) => {
                    if let Err(e) = w.write_audio(track, &data, pts_ns, FRAME_SAMPLES as u32) {
                        fail_mux(e.to_string(), &mut failure, &error, &counters, &mut writer);
                    }
                    let _ = recycle_audio.send(data);
                }
                None if early.len() < 2000 => early.push_back((track, data, pts_ns)),
                None => {}
            },
            MuxMsg::Finish => break,
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    match writer {
        Some(w) => w.finish().map_err(|e| {
            let e = e.to_string();
            fail_mux(e.clone(), &mut None, &error, &counters, &mut None);
            e
        }),
        None => Err(tr("No video frame arrived").into()),
    }
}

impl Recorder {
    /// Starts recording; returns once the encoder is running (or with why it could not start).
    pub fn start(options: RecordOptions) -> Result<Recorder, String> {
        if !options.path.is_absolute() {
            return Err(tr("The path must be absolute").into());
        }
        if options.path.parent().is_none_or(|p| !p.is_dir()) {
            return Err(trf("The folder {path} does not exist", &[("path", options.path.parent().unwrap_or(&options.path).display().to_string())]));
        }
        // the file is opened (safely, exclusively) before anything else is started: a path that cannot be written
        // fails here, not minutes into a recording
        let file = cast_mux::open_output(&options.path, options.overwrite).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                tr("The file already exists").to_string()
            } else {
                format!("{}: {e}", options.path.display())
            }
        })?;
        let path = options.path.clone();
        Self::start_with_file(options, file).inspect_err(|_| {
            // nothing was recorded: do not leave an empty file behind
            if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
                let _ = std::fs::remove_file(&path);
            }
        })
    }

    fn start_with_file(options: RecordOptions, file: std::fs::File) -> Result<Recorder, String> {
        let caps = cast_encode::probe();
        let kind = cast_encode::choose(&caps, options.encoder, options.codec).ok_or_else(|| {
            let why: Vec<String> = caps.iter().filter_map(|c| c.reason.as_ref().map(|r| format!("{}: {r}", c.kind.name()))).collect();
            format!("{} ({})", tr("No encoder available for this codec"), why.join("; "))
        })?;
        let cast = ScreenCast::connect(options.dbus_address.as_deref()).map_err(|e| e.0)?;
        let cursor = if options.cursor { cast_capture::screencast::CURSOR_EMBEDDED } else { 0 };
        let stream = match &options.source {
            Source::Monitor { connector } | Source::Area { connector, .. } => cast.record_monitor(connector, cursor),
            Source::Window { id } => cast.record_window(*id, cursor),
        }
        .map_err(|e| e.0)?;

        let clock = MediaClock::starting_now();
        let counters = Arc::new(Counters::default());
        counters.started_ns.store(clock.origin_ns(), Ordering::Relaxed);
        let error = Arc::new(Mutex::new(None));
        let targets = options.audio_targets();
        let mut audio_tracks = Vec::new();
        let mut audio_states = Vec::new();
        for (_, name) in &targets {
            let enc = OpusEncoder::new(2, 128_000).map_err(|e| e.0)?;
            audio_tracks.push(AudioTrack { channels: 2, pre_skip: enc.pre_skip, input_sample_rate: 48_000, name: tr(name).to_string() });
            audio_states.push(AudioTrackState {
                timeline: AudioTimeline::new(48_000, 10_000_000),
                encoder: enc,
                pending: Vec::with_capacity(FRAME_SAMPLES * 2),
                packets: 0,
            });
        }

        let (mux_tx, mux_rx) = mpsc::sync_channel::<MuxMsg>(512);
        let (video_pool, recycle_video) = Pool::new(256 << 10);
        let (audio_pool, recycle_audio) = Pool::new(2 << 10);
        let c2 = counters.clone();
        let e2 = error.clone();
        let open: Opener = Box::new(move |config| Writer::from_file(file, config));
        let mux = std::thread::Builder::new()
            .name("cast-mux".into())
            .spawn(move || mux_thread(open, audio_tracks, mux_rx, recycle_video, recycle_audio, c2, e2))
            .map_err(|e| e.to_string())?;

        let (opened_tx, opened_rx) = mpsc::channel::<Opened>();
        let (codec, fps, bitrate_kbps, crop) = (options.codec, options.fps, options.bitrate_kbps, options.source.crop());
        let (vmux, vcounters, verror) = (mux_tx.clone(), counters.clone(), error.clone());
        let video = VideoCapture::start(stream.node_id, move || {
            // the GPU context lives on this (the capture) thread; without one, only shared memory frames
            let gl = GlContext::new(None).ok();
            if kind == EncoderKind::Nvenc && gl.is_none() {
                return Err(tr("NVENC needs the GPU's OpenGL context").into());
            }
            let mut modifiers = gl.as_ref().map(|g| g.dmabuf_modifiers(fourcc::XRGB8888)).unwrap_or_default();
            modifiers.dedup();
            let st = VideoState {
                kind,
                codec,
                fps,
                bitrate_kbps,
                crop,
                gl,
                encoder: None,
                clock,
                out: EncodedFrame { data: Vec::with_capacity(1 << 20), keyframe: false },
                pool: video_pool,
                mux: vmux,
                counters: vcounters,
                opened: Some(opened_tx),
                error: verror,
                last_key_pts: None,
            };
            Ok((Box::new(st) as Box<dyn VideoSink>, modifiers))
        })
        .map_err(|e| e.0)?;

        let (info, w, h) = match opened_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                let _ = video.stop();
                let _ = mux_tx.send(MuxMsg::Finish);
                let _ = mux.join();
                return Err(e);
            }
            Err(_) => {
                let _ = video.stop();
                let _ = mux_tx.send(MuxMsg::Finish);
                let _ = mux.join();
                return Err(tr("The screencast sent no frames").into());
            }
        };
        counters.dmabuf.store(video.counters.dmabuf.load(Ordering::Relaxed), Ordering::Relaxed);

        let audio = if targets.is_empty() {
            None
        } else {
            let state = AudioState { clock, tracks: audio_states, pool: audio_pool, mux: mux_tx.clone(), counters: counters.clone() };
            match AudioCapture::start(targets.iter().map(|(t, _)| t.clone()).collect(), Box::new(state)) {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("{}: {e}", tr("sound not recorded"));
                    None
                }
            }
        };
        Ok(Recorder {
            audio_tracks: if audio.is_some() { targets.len() as u32 } else { 0 },
            options,
            counters,
            video: Some(video),
            audio,
            mux_tx,
            mux: Some(mux),
            _stream: stream,
            info,
            size: (w, h),
            cpu_start: stats::cpu_time_ns(),
            nvml: Nvml::open(),
            error,
        })
    }

    pub fn status(&self) -> Status {
        let c = &self.counters;
        let frames = c.frames.load(Ordering::Relaxed);
        let started = c.started_ns.load(Ordering::Relaxed);
        let elapsed = stats::now_s(started).max(1e-3);
        let disk_n = c.disk_samples.load(Ordering::Relaxed).max(1);
        let (gpu, enc) = self.nvml.as_ref().map(|n| n.sample()).unwrap_or((None, None));
        Status {
            recording: self.video.is_some() && !c.failed.load(Ordering::Acquire),
            path: self.options.path.display().to_string(),
            encoder: self.info.kind.name().into(),
            codec: format!("{:?}", self.info.codec).to_lowercase(),
            device: self.info.device.clone(),
            zero_copy: self.info.zero_copy && c.dmabuf.load(Ordering::Relaxed),
            dmabuf: c.dmabuf.load(Ordering::Relaxed),
            width: self.size.0,
            height: self.size.1,
            duration_s: elapsed,
            frames,
            fps: c.fps_window.lock().map(|w| w.2).unwrap_or(0.0),
            dropped: c.dropped.load(Ordering::Relaxed),
            audio_dropped: c.audio_dropped.load(Ordering::Relaxed),
            bitrate_kbps: (c.video_bytes.load(Ordering::Relaxed) + c.audio_bytes.load(Ordering::Relaxed)) as f64 * 8.0 / elapsed / 1000.0,
            audio_tracks: self.audio_tracks,
            audio_packets: c.audio_packets.load(Ordering::Relaxed),
            audio_corrections: c.audio_corrections.load(Ordering::Relaxed),
            encode_ms_avg: c.encode_ns_sum.load(Ordering::Relaxed) as f64 / frames.max(1) as f64 / 1e6,
            encode_ms_max: c.encode_ns_max.load(Ordering::Relaxed) as f64 / 1e6,
            latency_ms_avg: c.disk_ns_sum.load(Ordering::Relaxed) as f64 / disk_n as f64 / 1e6,
            latency_ms_max: c.disk_ns_max.load(Ordering::Relaxed) as f64 / 1e6,
            fragments: c.fragments.load(Ordering::Relaxed),
            cpu_percent: (stats::cpu_time_ns().saturating_sub(self.cpu_start)) as f64 / 1e9 / elapsed * 100.0,
            rss_mb: stats::rss_mb(),
            gpu_percent: gpu,
            gpu_encoder_percent: enc,
            error: self.error.lock().ok().and_then(|e| e.clone()),
            origin_ns: started,
        }
    }

    /// Stops capturing, writes the last fragment and the index; returns what the file holds.
    pub fn stop(mut self) -> Result<(Summary, Status), String> {
        if let Some(v) = self.video.take() {
            let _ = v.stop();
        }
        if let Some(a) = self.audio.take() {
            a.stop();
        }
        self._stream.stop();
        let status = Status { recording: false, ..self.status() };
        let _ = self.mux_tx.send(MuxMsg::Finish);
        let summary = self.mux.take().map(|m| m.join().unwrap_or_else(|_| Err("mux thread panicked".into()))).unwrap_or(Err(String::new()))?;
        Ok((summary, status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;
    use std::time::{Duration, Instant};

    const PARAMS: &[u8] =
        &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0x84, 0, 0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];

    fn track() -> VideoTrack {
        VideoTrack { codec: VideoCodec::H264, width: 1920, height: 1080, config: avc::avcc(PARAMS).unwrap() }
    }

    fn video(pts_ms: u64, key: bool) -> MuxMsg {
        MuxMsg::Video { data: vec![if key { 0x65 } else { 0x41 }; 64], pts_ns: pts_ms * 1_000_000, key, arrived_ns: monotonic_ns() }
    }

    #[test]
    fn keyframes_come_every_two_seconds_of_media_time() {
        assert!(keyframe_due(None, 0), "the first frame");
        assert!(!keyframe_due(Some(0), 1_999_999_999));
        assert!(keyframe_due(Some(0), 2_000_000_000));
        assert!(!keyframe_due(Some(5_000_000_000), 6_999_999_999));
        assert!(keyframe_due(Some(5_000_000_000), 7_000_000_000));
        assert_eq!(KEYFRAME_NS, 2_000_000_000);
    }

    #[test]
    fn a_full_queue_drops_instead_of_blocking_the_capture_callback() {
        let (tx, rx) = mpsc::sync_channel::<MuxMsg>(2);
        let tx2 = tx.clone();
        // on its own thread: an offer that blocked would hang here, and the test says so instead
        let h = std::thread::spawn(move || (0..100).filter(|i| offer(&tx2, video(*i, false))).count());
        let accepted = wait_for("the offers to return without waiting", || h.is_finished().then(|| h.is_finished()));
        assert!(accepted);
        assert_eq!(h.join().unwrap(), 2, "only the queue's capacity is taken");
        drop(rx);
        assert!(!offer(&tx, video(0, true)), "a writer that is gone is a drop too");
    }

    struct Rig {
        tx: SyncSender<MuxMsg>,
        mux: JoinHandle<Result<Summary, String>>,
        counters: Arc<Counters>,
        error: Arc<Mutex<Option<String>>>,
        /// The memfd the writer writes to (to seal it: a disk that stops accepting data).
        file: Arc<Mutex<Option<std::fs::File>>>,
        opened: Receiver<()>,
    }

    fn rig(open_dev_full: bool) -> Rig {
        let (tx, rx) = mpsc::sync_channel::<MuxMsg>(2);
        let (rv, _) = mpsc::channel();
        let (ra, _) = mpsc::channel();
        let counters = Arc::new(Counters::default());
        let error = Arc::new(Mutex::new(None));
        let file = Arc::new(Mutex::new(None));
        let (otx, opened) = mpsc::channel();
        let slot = file.clone();
        let open: Opener = Box::new(move |config| {
            let f = if open_dev_full {
                std::fs::OpenOptions::new().write(true).open("/dev/full")?
            } else {
                // SAFETY: memfd_create with a static NUL-terminated name; the fd is owned by the File made from it.
                let fd = unsafe { libc::memfd_create(c"cast-test".as_ptr(), libc::MFD_ALLOW_SEALING) };
                assert!(fd >= 0);
                // SAFETY: fd is a fresh valid descriptor nothing else owns.
                unsafe { std::fs::File::from_raw_fd(fd) }
            };
            *slot.lock().unwrap() = Some(f.try_clone()?);
            let w = Writer::from_file(f, config);
            let _ = otx.send(());
            w
        });
        let (c2, e2) = (counters.clone(), error.clone());
        let mux = std::thread::spawn(move || mux_thread(open, vec![], rx, rv, ra, c2, e2));
        Rig { tx, mux, counters, error, file, opened }
    }

    fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Floods the queue from another thread; fails the test if the writer thread stops draining.
    fn flood_must_not_block(tx: SyncSender<MuxMsg>) {
        let h = std::thread::spawn(move || {
            for i in 0..200 {
                tx.send(video(10_000 + i, false)).unwrap();
                tx.send(MuxMsg::Audio { track: 0, data: vec![1; 8], pts_ns: i * 20_000_000 }).unwrap();
            }
        });
        wait_for("the writer to keep draining after a failure", || h.is_finished().then_some(()));
        h.join().unwrap();
    }

    #[test]
    fn a_write_error_midway_is_reported_and_the_queue_keeps_draining() {
        let r = rig(false);
        r.tx.send(MuxMsg::Start(track())).unwrap();
        r.opened.recv_timeout(Duration::from_secs(5)).unwrap();
        // the disk stops accepting data (a sealed memfd refuses writes, like a full disk)
        {
            let f = r.file.lock().unwrap();
            let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
            // SAFETY: F_ADD_SEALS on a memfd we own, with a plain int argument.
            assert_eq!(unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(f.as_ref().unwrap()), libc::F_ADD_SEALS, seals) }, 0);
        }
        // a keyframe after 2.1 s closes the first fragment, which must now be written
        r.tx.send(video(0, true)).unwrap();
        r.tx.send(video(2_100, true)).unwrap();
        let why = wait_for("the error", || r.error.lock().unwrap().clone());
        assert!(!why.is_empty());
        assert!(r.counters.failed.load(Ordering::Acquire), "capture is told to stop");
        flood_must_not_block(r.tx.clone());
        r.tx.send(MuxMsg::Finish).unwrap();
        let res = r.mux.join().unwrap();
        assert_eq!(res.unwrap_err(), why, "stop() reports the same error");
    }

    #[test]
    fn a_disk_that_refuses_the_file_header_fails_the_recording_visibly() {
        let r = rig(true);
        r.tx.send(MuxMsg::Start(track())).unwrap();
        let why = wait_for("the error", || r.error.lock().unwrap().clone());
        assert!(why.to_lowercase().contains("space"), "{why}");
        flood_must_not_block(r.tx.clone());
        r.tx.send(MuxMsg::Finish).unwrap();
        assert!(r.mux.join().unwrap().is_err());
    }
}
