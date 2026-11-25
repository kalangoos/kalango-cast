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

const KEYFRAME_NS: u64 = 2_000_000_000;

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
        let Some(enc) = self.encoder.as_mut() else { return };
        let pts = self.clock.media_ns(received_ns);
        if pts < 0 {
            return;
        }
        let t0 = monotonic_ns();
        // a keyframe every 2 s of media time (frames come at a variable rate: a still screen sends few, so the
        // encoders' own GOP is off and the keyframes, which start the MP4 fragments, go by time)
        if self.last_key_pts.is_none_or(|k| pts as u64 >= k + KEYFRAME_NS) {
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
        self.counters.video_bytes.fetch_add(sample.len() as u64, Ordering::Relaxed);
        self.counters.last_pts_ns.store(pts as u64, Ordering::Relaxed);
        self.counters.tick_frame(t1);
        let _ = self.mux.send(MuxMsg::Video { data: sample, pts_ns: pts as u64, key, arrived_ns: received_ns });
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
                    self.counters.audio_packets.fetch_add(1, Ordering::Relaxed);
                    self.counters.audio_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    let _ = self.mux.send(MuxMsg::Audio { track: index, data, pts_ns: pts });
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

fn mux_thread(
    path: PathBuf,
    audio: Vec<AudioTrack>,
    rx: Receiver<MuxMsg>,
    recycle_video: Sender<Vec<u8>>,
    recycle_audio: Sender<Vec<u8>>,
    counters: Arc<Counters>,
) -> Result<Summary, String> {
    let mut writer: Option<Writer> = None;
    // audio that comes before the video track is known (the encoder opens on the first format)
    let mut early: VecDeque<(usize, Vec<u8>, u64)> = VecDeque::new();
    let mut fragments_seen = 0;
    for msg in rx {
        match msg {
            MuxMsg::Start(video) => {
                let config = cast_mux::Config { video: Some(video), audio: audio.clone(), fragment_ns: 2_000_000_000, sync: true };
                let mut w = Writer::create(&path, &config).map_err(|e| format!("{}: {e}", path.display()))?;
                for (track, data, pts) in early.drain(..) {
                    w.write_audio(track, &data, pts, FRAME_SAMPLES as u32).map_err(|e| e.to_string())?;
                    let _ = recycle_audio.send(data);
                }
                writer = Some(w);
            }
            MuxMsg::Video { data, pts_ns, key, arrived_ns } => {
                if let Some(w) = writer.as_mut() {
                    w.write_video(&data, pts_ns, key).map_err(|e| e.to_string())?;
                    let d = monotonic_ns().saturating_sub(arrived_ns);
                    Counters::add_max(&counters.disk_ns_sum, &counters.disk_ns_max, d);
                    counters.disk_samples.fetch_add(1, Ordering::Relaxed);
                    if key {
                        fragments_seen += 1;
                        counters.fragments.store(fragments_seen, Ordering::Relaxed);
                    }
                }
                let _ = recycle_video.send(data);
            }
            MuxMsg::Audio { track, data, pts_ns } => match writer.as_mut() {
                Some(w) => {
                    w.write_audio(track, &data, pts_ns, FRAME_SAMPLES as u32).map_err(|e| e.to_string())?;
                    let _ = recycle_audio.send(data);
                }
                None if early.len() < 2000 => early.push_back((track, data, pts_ns)),
                None => {}
            },
            MuxMsg::Finish => break,
        }
    }
    match writer {
        Some(w) => w.finish().map_err(|e| e.to_string()),
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
        let path = options.path.clone();
        let c2 = counters.clone();
        let mux = std::thread::Builder::new()
            .name("cast-mux".into())
            .spawn(move || mux_thread(path, audio_tracks, mux_rx, recycle_video, recycle_audio, c2))
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
            recording: self.video.is_some(),
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
