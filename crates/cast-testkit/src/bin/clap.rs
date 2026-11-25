//! kalango-cast-clap: a fullscreen Wayland window for Kalango Cast's tests.
//!
//! `--clap`: black, with a white flash (3 frames) and a 1 kHz beep (50 ms, played into `--sink`) every second; each
//! clap's flash presentation time (wp_presentation) and the time its beep's first sample plays (PipeWire's stream
//! timing) go to `--truth FILE` (JSON) at the end.
//! `--animate`: a moving pattern every frame (benchmarks: the compositor sends a frame per refresh).
//!
//! `kalango-cast-clap (--clap|--animate) [--seconds N] [--sink NAME] [--truth FILE]`; prints `ready` once the first
//! frame was presented.

use std::f32::consts::TAU;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cast_core::monotonic_ns;
use cast_testkit::analysis::Truth;
use pipewire as pw;
use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const FLASH_FRAMES: u32 = 3;
const BEEP_MS: u64 = 50;
const ANIMATION_FRAMES: usize = 8;

#[derive(Default)]
struct Beep {
    /// A beep to start at this monotonic time.
    armed: Option<u64>,
    /// When each beep's first sample played.
    played: Vec<u64>,
}

struct App {
    clap: bool,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    presentation: Option<wp_presentation::WpPresentation>,
    surface: Option<wl_surface::WlSurface>,
    xdg: Option<xdg_surface::XdgSurface>,
    size: (i32, i32),
    configured: bool,
    buffers: Vec<wl_buffer::WlBuffer>,
    _pools: Vec<(OwnedFd, *mut u8, usize)>,
    frame: u64,
    next_clap_ns: u64,
    flash_left: u32,
    flashes: Vec<u64>,
    beep: Arc<Mutex<Beep>>,
    ready: bool,
    closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(app: &mut Self, reg: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_compositor" => app.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => app.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => app.wm = Some(reg.bind(name, 1, qh, ())),
                "wp_presentation" => app.presentation = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(app: &mut Self, s: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            s.ack_configure(serial);
            if !app.configured {
                app.configured = true;
                app.make_buffers(qh);
                app.draw(qh);
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(app: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 && !app.configured => app.size = (width, height),
            xdg_toplevel::Event::Close => app.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(app: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            app.draw(qh);
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, bool> for App {
    fn event(
        app: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        flash: &bool,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation_feedback::Event::Presented { tv_sec_hi, tv_sec_lo, tv_nsec, .. } = event {
            let ns = (((tv_sec_hi as u64) << 32) | tv_sec_lo as u64) * 1_000_000_000 + tv_nsec as u64;
            if *flash {
                app.flashes.push(ns);
            }
            if !app.ready {
                app.ready = true;
                println!("ready");
            }
        }
    }
}

wayland_client::delegate_noop!(App: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(App: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(App: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(App: ignore wp_presentation::WpPresentation);

impl App {
    fn make_buffers(&mut self, qh: &QueueHandle<Self>) {
        let (w, h) = self.size;
        let stride = w * 4;
        let len = (stride * h) as usize;
        let count = if self.clap { 2 } else { ANIMATION_FRAMES };
        let Some(shm) = &self.shm else { return };
        for i in 0..count {
            // SAFETY: memfd_create with a static NUL-terminated name.
            let raw = unsafe { libc::memfd_create(c"kalango-cast-clap".as_ptr(), libc::MFD_CLOEXEC) };
            assert!(raw >= 0, "memfd_create");
            // SAFETY: a fresh fd we own.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            // SAFETY: sizes the memfd we just made.
            unsafe { libc::ftruncate(raw, len as libc::off_t) };
            // SAFETY: maps the whole memfd read/write, shared with the compositor.
            let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, raw, 0) } as *mut u8;
            assert!(ptr as isize != -1, "mmap");
            // SAFETY: ptr maps len bytes, written only here before the buffer is used.
            let px = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, len / 4) };
            if self.clap {
                px.fill(if i == 0 { 0xff00_0000 } else { 0xffff_ffff });
            } else {
                // diagonal colour bands that move between the frames, plus a sweeping bar
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        let p = x + y + i * 48;
                        let r = ((p * 3) & 0xff) as u32;
                        let g = ((y * 255 / h as usize) & 0xff) as u32;
                        let b = (((x ^ y) + i * 16) & 0xff) as u32;
                        px[y * w as usize + x] = 0xff00_0000 | r << 16 | g << 8 | b;
                    }
                }
                let bar = (i * w as usize / ANIMATION_FRAMES)..((i + 1) * w as usize / ANIMATION_FRAMES);
                for y in 0..h as usize {
                    for x in bar.clone() {
                        px[y * w as usize + x] = 0xffff_ffff;
                    }
                }
            }
            let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
            self.buffers.push(pool.create_buffer(0, w, h, stride, wl_shm::Format::Xrgb8888, qh, ()));
            pool.destroy();
            self._pools.push((fd, ptr, len));
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let Some(surface) = self.surface.clone() else { return };
        let now = monotonic_ns();
        let mut flash = false;
        let buffer = if self.clap {
            if self.flash_left == 0 && now >= self.next_clap_ns && self.ready {
                self.flash_left = FLASH_FRAMES;
                self.next_clap_ns = now + 1_000_000_000;
                flash = true;
                // the beep for this flash: armed for about when the frame shows
                if let Ok(mut b) = self.beep.lock() {
                    b.armed = Some(now + 16_000_000);
                }
            }
            if self.flash_left > 0 {
                self.flash_left -= 1;
                &self.buffers[1]
            } else {
                &self.buffers[0]
            }
        } else {
            &self.buffers[self.frame as usize % self.buffers.len()]
        };
        surface.attach(Some(buffer), 0, 0);
        surface.damage_buffer(0, 0, self.size.0, self.size.1);
        surface.frame(qh, ());
        if let Some(p) = &self.presentation {
            p.feedback(&surface, qh, flash);
        }
        surface.commit();
        self.frame += 1;
    }
}

/// Plays the beeps into a sink; each beep's first sample time is logged.
fn beep_thread(sink: Option<String>, beep: Arc<Mutex<Beep>>, stop: pw::channel::Receiver<()>) {
    pw::init();
    let ml = pw::main_loop::MainLoopRc::new(None).expect("pw loop");
    let ctx = pw::context::ContextRc::new(&ml, None).expect("pw context");
    let core = ctx.connect_rc(None).expect("pw core");
    let ml2 = ml.clone();
    let _q = stop.attach(ml.loop_(), move |_| ml2.quit());
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => "kalango-cast-clap",
    };
    if let Some(s) = sink {
        props.insert("target.object", s);
    }
    let stream = pw::stream::StreamBox::new(&core, "kalango-cast-clap", props).expect("stream");
    struct St {
        beep: Arc<Mutex<Beep>>,
        left: usize,
        phase: f32,
    }
    let _l = stream
        .add_local_listener_with_user_data(St { beep, left: 0, phase: 0.0 })
        .process(|stream, st| {
            // SAFETY: null or a buffer we own until queued back.
            let buf = unsafe { stream.dequeue_raw_buffer() };
            if buf.is_null() {
                return;
            }
            // SAFETY: pw_time is plain data, filled by the call.
            let mut t: pw::sys::pw_time = unsafe { std::mem::zeroed() };
            // SAFETY: the stream is valid during its callback.
            unsafe { pw::sys::pw_stream_get_time_n(stream.as_raw_ptr(), &mut t, std::mem::size_of::<pw::sys::pw_time>()) };
            // SAFETY: buf is valid; datas[0] is mapped (MAP_BUFFERS) with maxsize bytes.
            unsafe {
                let sb = (*buf).buffer;
                let d = &mut *(*sb).datas;
                if d.data.is_null() {
                    stream.queue_raw_buffer(buf);
                    return;
                }
                let max_frames = d.maxsize as usize / 8;
                let req = (*buf).requested as usize;
                let frames = if req > 0 { req.min(max_frames) } else { max_frames.min(1024) };
                let out = std::slice::from_raw_parts_mut(d.data as *mut f32, frames * 2);
                let rate_ns = |ticks: i64| if t.rate.denom > 0 { ticks * t.rate.num as i64 * 1_000_000_000 / t.rate.denom as i64 } else { 0 };
                // when this buffer's first sample plays: the graph's delay, the resampler's and the queued data
                let play = t.now + rate_ns(t.delay) + t.buffered as i64 * 1_000_000_000 / 48_000 + (t.queued as i64 / 8) * 1_000_000_000 / 48_000;
                let mut start_at = usize::MAX;
                if let Ok(mut b) = st.beep.lock() {
                    if let Some(at) = b.armed {
                        let end = play + frames as i64 * 1_000_000_000 / 48_000;
                        if (at as i64) < end {
                            let off = ((at as i64 - play).max(0) * 48_000 / 1_000_000_000) as usize;
                            start_at = off.min(frames - 1);
                            b.played.push((play + start_at as i64 * 1_000_000_000 / 48_000) as u64);
                            b.armed = None;
                            st.left = (48_000 * BEEP_MS / 1000) as usize;
                            st.phase = 0.0;
                        }
                    }
                }
                for i in 0..frames {
                    let s = if st.left > 0 && (start_at == usize::MAX || i >= start_at) {
                        st.left -= 1;
                        st.phase += TAU * 1000.0 / 48_000.0;
                        st.phase.sin() * 0.5
                    } else {
                        0.0
                    };
                    out[2 * i] = s;
                    out[2 * i + 1] = s;
                }
                let c = &mut *d.chunk;
                c.offset = 0;
                c.stride = 8;
                c.size = (frames * 8) as u32;
                stream.queue_raw_buffer(buf);
            }
        })
        .register()
        .expect("listener");
    let mut info = pw::spa::param::audio::AudioInfoRaw::new();
    info.set_format(pw::spa::param::audio::AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(2);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pw::spa::pod::Value::Object(obj))
        .expect("pod")
        .0
        .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&bytes).expect("pod")];
    stream
        .connect(
            pw::spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .expect("connect");
    ml.run();
}

fn main() {
    let mut clap = false;
    let mut seconds = 30.0;
    let mut sink = None;
    let mut truth_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--clap" => clap = true,
            "--animate" => clap = false,
            "--seconds" => seconds = args.next().and_then(|v| v.parse().ok()).unwrap_or(30.0),
            "--sink" => sink = args.next(),
            "--truth" => truth_path = args.next(),
            _ => {
                eprintln!("kalango-cast-clap (--clap|--animate) [--seconds N] [--sink NAME] [--truth FILE]");
                std::process::exit(2);
            }
        }
    }
    let beep = Arc::new(Mutex::new(Beep::default()));
    let (stop_tx, stop_rx) = pw::channel::channel::<()>();
    let audio = clap.then(|| {
        let b = beep.clone();
        let s = sink.clone();
        std::thread::spawn(move || beep_thread(s, b, stop_rx))
    });

    let conn = Connection::connect_to_env().expect("Wayland");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut app = App {
        clap,
        compositor: None,
        shm: None,
        wm: None,
        presentation: None,
        surface: None,
        xdg: None,
        size: (1280, 800),
        configured: false,
        buffers: Vec::new(),
        _pools: Vec::new(),
        frame: 0,
        next_clap_ns: 0,
        flash_left: 0,
        flashes: Vec::new(),
        beep: beep.clone(),
        ready: false,
        closed: false,
    };
    queue.roundtrip(&mut app).expect("roundtrip");
    let (Some(comp), Some(wm)) = (app.compositor.clone(), app.wm.clone()) else {
        eprintln!("no wl_compositor / xdg_wm_base");
        std::process::exit(1);
    };
    let surface = comp.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    top.set_title("Kalango Cast clap".into());
    top.set_app_id("kalango-cast-clap".into());
    top.set_fullscreen(None);
    surface.commit();
    app.surface = Some(surface);
    app.xdg = Some(xdg);
    // the first clap a second after the window shows
    app.next_clap_ns = monotonic_ns() + 1_000_000_000;
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    let mut logged = 0;
    while Instant::now() < end && !app.closed {
        if queue.blocking_dispatch(&mut app).is_err() {
            break;
        }
        // the log is rewritten after every clap (whole: write then rename), so it is complete whenever this ends
        if let Some(p) = &truth_path {
            let played = beep.lock().map(|b| b.played.clone()).unwrap_or_default();
            let truth: Vec<Truth> = app.flashes.iter().zip(played.iter()).map(|(f, b)| Truth { flash_ns: *f, beep_ns: *b }).collect();
            if truth.len() != logged {
                logged = truth.len();
                let tmp = format!("{p}.part");
                if std::fs::write(&tmp, serde_json::to_string_pretty(&truth).unwrap_or_default()).is_ok() {
                    let _ = std::fs::rename(&tmp, p);
                }
            }
        }
    }
    let _ = stop_tx.send(());
    if let Some(a) = audio {
        let _ = a.join();
    }
}
