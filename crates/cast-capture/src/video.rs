//! The screencast's PipeWire video stream.
//!
//! The stream runs on its own thread with its own PipeWire loop. That thread first builds the sink (the engine
//! creates the GPU context and the encoder there: GL contexts belong to a thread), which says which DMA-BUF
//! modifiers it can import; those are offered first (the compositor picks one and allocates GPU buffers), then a
//! shared-memory format as the fallback. Each frame is handed to the sink during PipeWire's `process` callback,
//! with the buffer's fds (never mapped) or its mapped memory, and given back to the compositor right after.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use cast_core::{fourcc, monotonic_ns, DmaBufFrame, DmaBufPlane, ShmFrame, VideoFrame};
use pipewire as pw;
use pw::spa;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::param::ParamType;
use spa::pod::serialize::PodSerializer;
use spa::pod::{ChoiceValue, Pod, Property, PropertyFlags, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes};

use crate::guard::Guarded;
use crate::CaptureError;

/// The negotiated stream format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamFormat {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    /// Some for DMA-BUF (the modifier the compositor chose), None for shared memory.
    pub modifier: Option<u64>,
}

/// Receives the frames, on the capture thread.
pub trait VideoSink {
    /// The format was (re)negotiated: the buffers that come next are of this kind.
    fn format(&mut self, format: &StreamFormat);
    /// A frame, received at `received_ns` (monotonic). Its buffer goes back to the compositor when this returns.
    fn frame(&mut self, frame: &VideoFrame, received_ns: u64);
    /// A frame the compositor sent empty or marked corrupted, or that came while the sink was busy.
    fn skipped(&mut self) {}
    /// A callback panicked (in this sink or below it): the capture stops and nothing else reaches the sink.
    fn failed(&mut self, _why: &str) {}
}

impl crate::guard::Failing for Box<dyn VideoSink> {
    fn failed(&mut self, why: &str) {
        VideoSink::failed(&mut **self, why);
    }
}

/// Counters readable from any thread.
#[derive(Default, Debug)]
pub struct VideoCounters {
    pub frames: AtomicU64,
    pub skipped: AtomicU64,
    pub dmabuf: AtomicBool,
}

/// A running video capture thread.
pub struct VideoCapture {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<Result<(), CaptureError>>>,
    pub counters: Arc<VideoCounters>,
}

impl VideoCapture {
    /// Starts capturing PipeWire node `node_id`. `make_sink` runs first on the new thread and returns the sink and
    /// the DMA-BUF modifiers it imports for XRGB8888/ARGB8888 (empty: shared memory only).
    pub fn start<F>(node_id: u32, make_sink: F) -> Result<VideoCapture, CaptureError>
    where
        F: FnOnce() -> Result<(Box<dyn VideoSink>, Vec<u64>), String> + Send + 'static,
    {
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let counters = Arc::new(VideoCounters::default());
        let c2 = counters.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), CaptureError>>();
        let thread = std::thread::Builder::new()
            .name("cast-video".into())
            .spawn(move || {
                // a panic anywhere in the thread ends the capture with an error instead of unwinding into nothing
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(node_id, make_sink, quit_rx, c2, ready_tx)))
                    .unwrap_or_else(|p| Err(CaptureError(format!("video thread panicked: {}", crate::guard::panic_text(&*p)))))
            })
            .map_err(|e| CaptureError(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(VideoCapture { quit: quit_tx, thread: Some(thread), counters }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(thread.join().ok().and_then(|r| r.err()).unwrap_or_else(|| CaptureError("video thread".into()))),
        }
    }

    /// Stops the stream and waits for the thread (the sink is dropped there, with its GPU context).
    pub fn stop(mut self) -> Result<(), CaptureError> {
        let _ = self.quit.send(());
        self.thread.take().map(|t| t.join().unwrap_or_else(|_| Err(CaptureError("video thread panicked".into())))).unwrap_or(Ok(()))
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serialize(obj: spa::pod::Object) -> Vec<u8> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj)).map(|(c, _)| c.into_inner()).unwrap_or_default()
}

fn video_format_pod(format: VideoFormat, modifiers: Option<&[u64]>) -> Vec<u8> {
    let mut props = vec![
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(FormatProperties::VideoFormat, Id, format),
    ];
    if let Some(mods) = modifiers {
        let alts: Vec<i64> = mods.iter().map(|m| *m as i64).collect();
        props.push(Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            value: Value::Choice(ChoiceValue::Long(Choice(ChoiceFlags::empty(), ChoiceEnum::Enum { default: alts[0], alternatives: alts }))),
        });
    }
    props.push(pw::spa::pod::property!(
        FormatProperties::VideoSize,
        Choice,
        Range,
        Rectangle,
        Rectangle { width: 1920, height: 1080 },
        Rectangle { width: 1, height: 1 },
        Rectangle { width: 16384, height: 16384 }
    ));
    props.push(pw::spa::pod::property!(
        FormatProperties::VideoFramerate,
        Choice,
        Range,
        Fraction,
        Fraction { num: 0, denom: 1 },
        Fraction { num: 0, denom: 1 },
        Fraction { num: 1000, denom: 1 }
    ));
    serialize(spa::pod::Object { type_: SpaTypes::ObjectParamFormat.as_raw(), id: ParamType::EnumFormat.as_raw(), properties: props })
}

fn buffers_pod(dmabuf: bool) -> Vec<u8> {
    let types = if dmabuf { 1 << spa::sys::SPA_DATA_DmaBuf } else { (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr) };
    serialize(spa::pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_BUFFERS_buffers,
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: 4, min: 2, max: 16 }))),
            },
            Property { key: spa::sys::SPA_PARAM_BUFFERS_dataType, flags: PropertyFlags::empty(), value: Value::Int(types) },
        ],
    })
}

fn meta_header_pod() -> Vec<u8> {
    serialize(spa::pod::Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_META_type,
                flags: PropertyFlags::empty(),
                value: Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
            },
            Property {
                key: spa::sys::SPA_PARAM_META_size,
                flags: PropertyFlags::empty(),
                value: Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            },
        ],
    })
}

struct State {
    sink: Guarded<Box<dyn VideoSink>>,
    inner: Inner,
}

struct Inner {
    format: Option<StreamFormat>,
    counters: Arc<VideoCounters>,
    planes: Vec<DmaBufPlane>,
}

fn drm_fourcc(f: VideoFormat) -> Option<u32> {
    match f {
        VideoFormat::BGRx => Some(fourcc::XRGB8888),
        VideoFormat::BGRA => Some(fourcc::ARGB8888),
        VideoFormat::RGBx => Some(fourcc::XBGR8888),
        VideoFormat::RGBA => Some(fourcc::ABGR8888),
        _ => None,
    }
}

fn run<F>(
    node_id: u32,
    make_sink: F,
    quit: pw::channel::Receiver<()>,
    counters: Arc<VideoCounters>,
    ready: std::sync::mpsc::Sender<Result<(), CaptureError>>,
) -> Result<(), CaptureError>
where
    F: FnOnce() -> Result<(Box<dyn VideoSink>, Vec<u64>), String>,
{
    let fail = |e: CaptureError| {
        let _ = ready.send(Err(CaptureError(e.0.clone())));
        Err(e)
    };
    pw::init();
    let (sink, modifiers) = match make_sink() {
        Ok(x) => x,
        Err(e) => return fail(CaptureError(e)),
    };
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => return fail(e.into()),
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => return fail(e.into()),
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => return fail(e.into()),
    };
    let ml = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |_| ml.quit());
    let stream = match pw::stream::StreamBox::new(
        &core,
        "kalango-cast-video",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(e) => return fail(e.into()),
    };
    let state = State { sink: Guarded::new(sink), inner: Inner { format: None, counters, planes: Vec::with_capacity(4) } };
    let quit_loop: std::rc::Rc<dyn Fn()> = {
        let ml = mainloop.clone();
        std::rc::Rc::new(move || ml.quit())
    };
    let (quit_a, quit_b) = (quit_loop.clone(), quit_loop);
    let listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(move |stream, st, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let has_modifier = param.as_object().ok().and_then(|o| o.find_prop(spa::utils::Id(FormatProperties::VideoModifier.as_raw()))).is_some();
            let Some(fourcc) = drm_fourcc(info.format()) else { return };
            let format =
                StreamFormat { width: info.size().width, height: info.size().height, fourcc, modifier: has_modifier.then(|| info.modifier()) };
            st.inner.counters.dmabuf.store(has_modifier, Ordering::Relaxed);
            st.inner.format = Some(format);
            st.sink.call(&*quit_a, |sink| sink.format(&format));
            if st.sink.poisoned() {
                return;
            }
            let buffers = buffers_pod(has_modifier);
            let meta = meta_header_pod();
            let mut params = [Pod::from_bytes(&buffers).expect("pod"), Pod::from_bytes(&meta).expect("pod")];
            let _ = stream.update_params(&mut params);
        })
        .process(move |stream, st| {
            // SAFETY: dequeue returns null or a buffer owned by the stream until we queue it back below.
            let buf = unsafe { stream.dequeue_raw_buffer() };
            if buf.is_null() {
                return;
            }
            let now = monotonic_ns();
            // SAFETY: buf is a valid pw_buffer; its spa_buffer and datas stay valid until queued back. A panic in the
            // sink is caught here (it must not unwind into PipeWire's C frames).
            st.sink.call(&*quit_b, |sink| unsafe { deliver(&mut st.inner, &mut **sink, buf, now) });
            // SAFETY: the buffer came from this stream's dequeue and is given back once.
            unsafe { stream.queue_raw_buffer(buf) };
        })
        .register();
    let _listener = match listener {
        Ok(l) => l,
        Err(e) => return fail(e.into()),
    };
    let mut pods: Vec<Vec<u8>> = Vec::new();
    if !modifiers.is_empty() {
        pods.push(video_format_pod(VideoFormat::BGRx, Some(&modifiers)));
        pods.push(video_format_pod(VideoFormat::BGRA, Some(&modifiers)));
    }
    pods.push(video_format_pod(VideoFormat::BGRx, None));
    pods.push(video_format_pod(VideoFormat::BGRA, None));
    let mut params: Vec<&Pod> = pods.iter().filter_map(|p| Pod::from_bytes(p)).collect();
    if let Err(e) =
        stream.connect(Direction::Input, Some(node_id), pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS, &mut params)
    {
        return fail(e.into());
    }
    let _ = ready.send(Ok(()));
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

/// Hands one buffer to the sink.
///
/// # Safety
/// `buf` must be a dequeued, not yet queued pw_buffer of the stream.
unsafe fn deliver(st: &mut Inner, sink: &mut dyn VideoSink, buf: *mut pw::sys::pw_buffer, now: u64) {
    // SAFETY: guaranteed by the caller.
    let spa_buf = unsafe { (*buf).buffer };
    if spa_buf.is_null() {
        return;
    }
    // SAFETY: spa_buf is valid; datas has n_datas entries.
    let (n, datas) = unsafe { ((*spa_buf).n_datas as usize, (*spa_buf).datas) };
    let Some(format) = st.format else { return };
    if n == 0 || datas.is_null() {
        return;
    }
    // SAFETY: datas[0] exists (n > 0); its chunk pointer is set by PipeWire.
    let d0 = unsafe { &*datas };
    // SAFETY: as above.
    let chunk = unsafe { &*d0.chunk };
    if chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 || (chunk.size == 0 && d0.type_ != spa::sys::SPA_DATA_DmaBuf) {
        st.counters.skipped.fetch_add(1, Ordering::Relaxed);
        sink.skipped();
        return;
    }
    match d0.type_ {
        spa::sys::SPA_DATA_DmaBuf => {
            st.planes.clear();
            for i in 0..n {
                // SAFETY: i < n_datas.
                let d = unsafe { &*datas.add(i) };
                // SAFETY: each data has its chunk.
                let c = unsafe { &*d.chunk };
                st.planes.push(DmaBufPlane { fd: d.fd as RawFd, offset: c.offset, stride: c.stride.max(0) as u32 });
            }
            let frame = VideoFrame::DmaBuf(DmaBufFrame {
                width: format.width,
                height: format.height,
                fourcc: format.fourcc,
                modifier: format.modifier.unwrap_or(cast_core::modifier::INVALID),
                planes: &st.planes,
            });
            st.counters.frames.fetch_add(1, Ordering::Relaxed);
            sink.frame(&frame, now);
        }
        spa::sys::SPA_DATA_MemFd | spa::sys::SPA_DATA_MemPtr => {
            if d0.data.is_null() {
                return;
            }
            let offset = chunk.offset as usize % d0.maxsize.max(1) as usize;
            let len = (chunk.size as usize).min(d0.maxsize as usize - offset);
            // SAFETY: PipeWire mapped maxsize bytes at data (MAP_BUFFERS); offset + len stay inside them.
            let data = unsafe { std::slice::from_raw_parts((d0.data as *const u8).add(offset), len) };
            let stride = if chunk.stride > 0 { chunk.stride as u32 } else { format.width * 4 };
            let frame = VideoFrame::Shm(ShmFrame { width: format.width, height: format.height, stride, fourcc: format.fourcc, data });
            st.counters.frames.fetch_add(1, Ordering::Relaxed);
            sink.frame(&frame, now);
        }
        _ => {
            st.counters.skipped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
