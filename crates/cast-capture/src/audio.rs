//! Sound capture: the desktop (the default output's monitor, or a named sink's) and the microphone (the default
//! input, or a named source), each a PipeWire capture stream of 48 kHz stereo float, all on one thread.
//!
//! Each buffer's first sample is placed on the monotonic clock from the stream's own timing
//! (`pw_stream_get_time_n`: the graph cycle's time, minus the delay the samples spent before reaching us).

use std::cell::RefCell;
use std::rc::Rc;
use std::thread::JoinHandle;

use pipewire as pw;
use pw::spa;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::ParamType;
use spa::pod::serialize::PodSerializer;
use spa::pod::{Pod, Value};
use spa::utils::{Direction, SpaTypes};

use crate::guard::Guarded;
use crate::CaptureError;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioTarget {
    /// What a sink plays (None: the default output).
    Desktop { sink: Option<String> },
    /// A source (None: the default input).
    Mic { source: Option<String> },
}

/// Receives the sound of every stream, on the audio thread.
pub trait AudioSink: Send {
    /// `pcm`: `frames` × [`CHANNELS`] interleaved samples of stream `index`, the first one captured at `first_ns`
    /// (monotonic).
    fn samples(&mut self, index: usize, pcm: &[f32], frames: usize, first_ns: i64);
    /// A callback panicked (in this sink or below it): the capture stops and nothing else reaches the sink.
    fn failed(&mut self, _why: &str) {}
}

impl crate::guard::Failing for Box<dyn AudioSink> {
    fn failed(&mut self, why: &str) {
        AudioSink::failed(&mut **self, why);
    }
}

pub struct AudioCapture {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start(targets: Vec<AudioTarget>, sink: Box<dyn AudioSink>) -> Result<AudioCapture, CaptureError> {
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), CaptureError>>();
        let thread = std::thread::Builder::new()
            .name("cast-audio".into())
            .spawn(move || {
                // a panic anywhere in the thread is an error of the capture, never an unwind into nothing
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(targets, sink, quit_rx, &ready_tx)))
                    .unwrap_or_else(|p| Err(CaptureError(format!("audio thread panicked: {}", crate::guard::panic_text(&*p)))));
                if let Err(e) = r {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| CaptureError(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(AudioCapture { quit: quit_tx, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(CaptureError("audio thread".into())),
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct StreamData {
    index: usize,
    sink: Rc<RefCell<Guarded<Box<dyn AudioSink>>>>,
    quit: Rc<dyn Fn()>,
}

fn format_pod() -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    let mut pos = [0; spa::param::audio::MAX_CHANNELS];
    pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);
    let obj = spa::pod::Object { type_: SpaTypes::ObjectParamFormat.as_raw(), id: ParamType::EnumFormat.as_raw(), properties: info.into() };
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj)).map(|(c, _)| c.into_inner()).unwrap_or_default()
}

fn run(
    targets: Vec<AudioTarget>,
    sink: Box<dyn AudioSink>,
    quit: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::Sender<Result<(), CaptureError>>,
) -> Result<(), CaptureError> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |_| ml.quit());
    let sink = Rc::new(RefCell::new(Guarded::new(sink)));
    let quit_loop: Rc<dyn Fn()> = {
        let ml = mainloop.clone();
        Rc::new(move || ml.quit())
    };
    let pod = format_pod();
    let mut streams = Vec::new();
    for (index, target) in targets.into_iter().enumerate() {
        let (name, target_name, capture_sink) = match &target {
            AudioTarget::Desktop { sink } => ("kalango-cast-desktop", sink.clone(), true),
            AudioTarget::Mic { source } => ("kalango-cast-mic", source.clone(), false),
        };
        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Production",
            *pw::keys::NODE_NAME => name,
            *pw::keys::APP_NAME => "Kalango Cast",
        };
        if capture_sink {
            props.insert("stream.capture.sink", "true");
        }
        if let Some(t) = target_name {
            props.insert("target.object", t);
        }
        let stream = pw::stream::StreamBox::new(&core, name, props)?;
        let listener = stream
            .add_local_listener_with_user_data(StreamData { index, sink: sink.clone(), quit: quit_loop.clone() })
            .process(|stream, data| {
                // SAFETY: null or a buffer owned by us until queued back below.
                let buf = unsafe { stream.dequeue_raw_buffer() };
                if buf.is_null() {
                    return;
                }
                // SAFETY: pw_time is plain data; get_time_n fills up to the size we pass.
                let mut t: pw::sys::pw_time = unsafe { std::mem::zeroed() };
                // SAFETY: the stream is valid during its own callback.
                unsafe { pw::sys::pw_stream_get_time_n(stream.as_raw_ptr(), &mut t, std::mem::size_of::<pw::sys::pw_time>()) };
                // SAFETY: buf is valid until queued.
                unsafe { deliver(data, buf, &t) };
                // SAFETY: given back once.
                unsafe { stream.queue_raw_buffer(buf) };
            })
            .register()?;
        let mut params = [Pod::from_bytes(&pod).ok_or_else(|| CaptureError("pod".into()))?];
        stream.connect(
            Direction::Input,
            None,
            // (no RT_PROCESS: the callbacks run on this thread's loop, where the shared sink lives)
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;
        streams.push((stream, listener));
    }
    let _ = ready.send(Ok(()));
    mainloop.run();
    for (s, _) in &streams {
        let _ = s.disconnect();
    }
    Ok(())
}

/// The monotonic time of a buffer's first sample from the stream timing: the cycle's time minus the samples'
/// delay (the device and graph latency, in the graph's rate) and the buffer's own length.
pub fn first_sample_ns(now: i64, delay_ticks: i64, rate_num: u32, rate_denom: u32, frames: usize) -> i64 {
    let delay_ns = if rate_denom > 0 { (delay_ticks as i128 * rate_num as i128 * 1_000_000_000 / rate_denom as i128) as i64 } else { 0 };
    now - delay_ns - (frames as i64 * 1_000_000_000 / RATE as i64)
}

/// # Safety
/// `buf` must be a dequeued, not yet queued pw_buffer of a stream with the F32 stereo format.
unsafe fn deliver(data: &mut StreamData, buf: *mut pw::sys::pw_buffer, t: &pw::sys::pw_time) {
    // SAFETY: guaranteed by the caller.
    let spa_buf = unsafe { (*buf).buffer };
    if spa_buf.is_null() {
        return;
    }
    // SAFETY: spa_buf valid; datas has n_datas entries.
    let (n, datas) = unsafe { ((*spa_buf).n_datas, (*spa_buf).datas) };
    if n == 0 || datas.is_null() {
        return;
    }
    // SAFETY: datas[0] exists.
    let d = unsafe { &*datas };
    if d.data.is_null() || d.chunk.is_null() {
        return;
    }
    // SAFETY: the chunk is set by PipeWire.
    let c = unsafe { &*d.chunk };
    let offset = (c.offset as usize).min(d.maxsize as usize);
    let size = (c.size as usize).min(d.maxsize as usize - offset);
    let frames = size / (CHANNELS * 4);
    if frames == 0 {
        return;
    }
    // SAFETY: MAP_BUFFERS mapped maxsize bytes at data; offset + size stay inside; F32 samples are 4-byte aligned.
    let pcm = unsafe { std::slice::from_raw_parts((d.data as *const u8).add(offset) as *const f32, frames * CHANNELS) };
    let first = first_sample_ns(t.now, t.delay, t.rate.num, t.rate.denom, frames);
    let index = data.index;
    let quit = data.quit.clone();
    data.sink.borrow_mut().call(&*quit, |sink| sink.samples(index, pcm, frames, first));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_sample_is_before_the_cycle_by_the_delay_and_the_buffer() {
        // cycle at 10 s, 1024 ticks of delay at 1/48000, 480 frames: 21.33 ms + 10 ms earlier
        let t = first_sample_ns(10_000_000_000, 1024, 1, 48_000, 480);
        assert_eq!(t, 10_000_000_000 - 21_333_333 - 10_000_000);
        assert_eq!(first_sample_ns(5, 100, 1, 0, 0), 5, "no rate: no delay");
    }
}
