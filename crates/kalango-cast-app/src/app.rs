//! The GTK4 window: builds the widgets, renders the [`Model`] into them, and runs the effects (socket calls on worker
//! threads whose answers come back to the main loop as [`Msg`]s).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_channel::Sender;
use cast_core::{tr, trf, Source};
use gtk::prelude::*;
use gtk::{gdk, glib};
use serde_json::json;

use crate::client::{self, CallError, EventLink};
use crate::daemon::{self, Service};
use crate::model::{format_timer, CaptureState, DaemonState, Effect, Model, Msg, Phase, Profile, SourceList, StartConfig};
use crate::motion::{self, Spring};
use crate::style;

/// The app's identity: the application id, the `.desktop` file's name and the icon's name.
pub const APP_ID: &str = "os.kalango.Cast";

static LOGO: &[u8] = include_bytes!("../../../assets/logo-kalango-cast.png");

/// Which source row is chosen (a monitor's connector or a window's id).
#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceKey {
    Monitor(String),
    Window(u64),
}

impl SourceKey {
    fn source(&self) -> Source {
        match self {
            SourceKey::Monitor(c) => Source::Monitor { connector: c.clone() },
            SourceKey::Window(id) => Source::Window { id: *id },
        }
    }
}

/// The springs the window animates.
struct Anim {
    /// The recording dot's size (pops in with a little overshoot when a recording starts).
    dot: Spring,
    cpu: Spring,
    gpu: Spring,
    dialog_opacity: Spring,
}

struct Widgets {
    window: gtk::ApplicationWindow,
    dot: gtk::DrawingArea,
    state_label: gtk::Label,
    timer_label: gtk::Label,
    service_chip: gtk::Label,
    record_btn: gtk::Button,
    banner: gtk::Revealer,
    banner_box: gtk::Box,
    banner_label: gtk::Label,
    banner_dismiss: gtk::Button,
    sidebar: gtk::ScrolledWindow,
    /// The sidebar's content: lives in `sidebar` when the window is wide and above the cards when it is narrow.
    side: gtk::Box,
    narrow_slot: gtk::Box,
    profile_btns: Vec<(Profile, gtk::ToggleButton)>,
    sources_list: gtk::ListBox,
    sources_empty: gtk::Label,
    preview_title: gtk::Label,
    preview_body: gtk::Label,
    stat_values: Vec<gtk::Label>,
    cpu_bar: gtk::LevelBar,
    gpu_bar: gtk::LevelBar,
    stats_hint: gtk::Label,
    folder_label: gtk::Label,
    folder_btn: gtk::Button,
    desktop_switch: gtk::Switch,
    mic_switch: gtk::Switch,
    file_label: gtk::Label,
}

struct UiState {
    selected: Option<SourceKey>,
    rendered_sources: Option<SourceList>,
    folder: PathBuf,
    last_config: Option<StartConfig>,
    dialog: Option<gtk::Window>,
    /// The request the open dialog shows (a different one means a new dialog).
    dialog_request: Option<String>,
    dialog_countdown: Option<gtk::Label>,
    dialog_buttons: Vec<gtk::Button>,
    /// A row selection made by the code (a list rebuild) must not count as the person's choice.
    rebuilding: bool,
}

struct App {
    model: RefCell<Model>,
    w: Widgets,
    st: RefCell<UiState>,
    anim: RefCell<Anim>,
    animating: Cell<bool>,
    tx: Sender<Msg>,
    link: EventLink,
    socket: PathBuf,
    service: Arc<Mutex<Option<Service>>>,
    secret: Arc<Mutex<Option<String>>>,
    sources_busy: Arc<AtomicBool>,
    status_busy: Arc<AtomicBool>,
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(44)
        .css_classes(classes.to_vec())
        .build()
}

fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(spacing).build()
}

fn hbox(spacing: i32) -> gtk::Box {
    gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(spacing).build()
}

fn card(title: &str) -> (gtk::Box, gtk::Box) {
    let outer = vbox(10);
    outer.add_css_class("card");
    outer.set_hexpand(true);
    outer.set_valign(gtk::Align::Start);
    outer.append(&label(title, &["card-title"]));
    let body = vbox(8);
    outer.append(&body);
    (outer, body)
}

/// The person's Videos folder (`~/Videos`; the folder the engine requires to be theirs).
pub fn default_folder() -> PathBuf {
    glib::user_special_dir(glib::UserDirectory::Videos).unwrap_or_else(|| glib::home_dir().join("Videos"))
}

/// Whether the folder exists and is the person's own (the engine refuses anything else), with the reason if not.
pub fn check_folder(dir: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir)
        .map_err(|e| trf("The folder {dir} cannot be used: {why}", &[("dir", dir.display().to_string()), ("why", e.to_string())]))?;
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != me {
        return Err(trf("The folder {dir} is not yours: pick one of your own (for example Videos).", &[("dir", dir.display().to_string())]));
    }
    Ok(())
}

pub fn run() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        build(app);
    });
    app.run()
}

fn build(application: &gtk::Application) {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(true);
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(style::CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
    }

    let (tx, rx) = async_channel::unbounded::<Msg>();
    let Some(socket) = client::default_socket() else {
        eprintln!("kalango-cast-app: XDG_RUNTIME_DIR is not set");
        application.quit();
        return;
    };
    let secret = Arc::new(Mutex::new(None));
    let link = client::spawn_events(socket.clone(), tx.clone(), secret.clone());
    let w = build_widgets(application);
    let folder = default_folder();
    let app = Rc::new(App {
        model: RefCell::new(Model::default()),
        w,
        st: RefCell::new(UiState {
            selected: None,
            rendered_sources: None,
            folder,
            last_config: None,
            dialog: None,
            dialog_request: None,
            dialog_countdown: None,
            dialog_buttons: Vec::new(),
            rebuilding: false,
        }),
        anim: RefCell::new(Anim {
            dot: Spring::new(0.0, motion::ELASTIC),
            cpu: Spring::new(0.0, motion::SMOOTH),
            gpu: Spring::new(0.0, motion::SMOOTH),
            dialog_opacity: Spring::new(0.0, motion::FAST),
        }),
        animating: Cell::new(false),
        tx,
        link,
        socket,
        service: Arc::new(Mutex::new(None)),
        secret,
        sources_busy: Arc::new(AtomicBool::new(false)),
        status_busy: Arc::new(AtomicBool::new(false)),
    });
    app.wire();
    app.render();
    app.start_service();
    {
        let app = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                app.dispatch(msg);
            }
        });
    }
    app.w.window.present();
}

fn build_widgets(application: &gtk::Application) -> Widgets {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Kalango Cast")
        .default_width(1120)
        .default_height(720)
        .css_classes(["cast"])
        .build();
    window.set_size_request(360, 420);

    // title bar
    let header = gtk::HeaderBar::new();
    let texture = gdk::Texture::from_bytes(&glib::Bytes::from_static(LOGO)).ok();
    let logo = match &texture {
        Some(t) => gtk::Image::from_paintable(Some(t)),
        None => gtk::Image::from_icon_name("camera-video-symbolic"),
    };
    logo.set_pixel_size(22);
    header.pack_start(&logo);
    window.set_titlebar(Some(&header));
    gtk::Window::set_default_icon_name(APP_ID);

    // the indicator bar: always visible, outside every scroller
    let dot = gtk::DrawingArea::builder().content_width(16).content_height(16).valign(gtk::Align::Center).build();
    let state_label = label("", &["state"]);
    state_label.set_valign(gtk::Align::Center);
    state_label.set_hexpand(false);
    let timer_label = label("00:00", &["timer"]);
    timer_label.set_valign(gtk::Align::Center);
    timer_label.set_hexpand(false);
    let service_chip = label("", &["chip"]);
    service_chip.set_valign(gtk::Align::Center);
    service_chip.set_hexpand(false);
    let record_btn = gtk::Button::builder().label(tr("Record")).css_classes(["record"]).valign(gtk::Align::Center).build();
    let topbar = hbox(10);
    topbar.add_css_class("topbar");
    topbar.append(&dot);
    topbar.append(&state_label);
    topbar.append(&timer_label);
    let spacer = gtk::Box::builder().hexpand(true).build();
    topbar.append(&spacer);
    topbar.append(&service_chip);
    topbar.append(&record_btn);

    // banner: errors and the result of the last action
    let banner_label = label("", &[]);
    banner_label.set_hexpand(true);
    let banner_dismiss = gtk::Button::builder().label(tr("Dismiss")).css_classes(["flat-icon"]).build();
    let banner_box = hbox(8);
    banner_box.add_css_class("banner");
    banner_box.append(&banner_label);
    banner_box.append(&banner_dismiss);
    let banner = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::SlideDown).transition_duration(200).child(&banner_box).build();

    // sidebar: scenes (profiles) and sources
    let side = vbox(10);
    side.set_margin_top(14);
    side.set_margin_bottom(14);
    side.set_margin_start(14);
    side.set_margin_end(14);
    side.append(&label(tr("SCENES"), &["section-title"]));
    let mut profile_btns: Vec<(Profile, gtk::ToggleButton)> = Vec::new();
    for p in Profile::ALL {
        let inner = vbox(2);
        inner.append(&label(tr(p.title()), &["scene-title"]));
        inner.append(&label(tr(p.blurb()), &["dim"]));
        let b = gtk::ToggleButton::builder().child(&inner).css_classes(["scene"]).build();
        if let Some((_, first)) = profile_btns.first() {
            b.set_group(Some(first));
        }
        side.append(&b);
        profile_btns.push((p, b));
    }
    side.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let sources_head = hbox(6);
    let sources_title = label(tr("SOURCES"), &["section-title"]);
    sources_title.set_hexpand(true);
    sources_head.append(&sources_title);
    side.append(&sources_head);
    let sources_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).css_classes(["sources"]).build();
    let sources_empty = label("", &["dim"]);
    side.append(&sources_list);
    side.append(&sources_empty);
    let sidebar =
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).min_content_width(300).child(&side).css_classes(["sidebar"]).build();

    // main area: cards that flow into as many columns as fit
    let (preview_card, preview_body_box) = card(tr("Preview"));
    let preview_title = label("", &["card-title"]);
    let preview_body = label("", &["dim"]);
    preview_body_box.append(&preview_title);
    preview_body_box.append(&preview_body);
    preview_card.set_size_request(320, -1);

    let (stats_card, stats_body) = card(tr("Recording health"));
    let grid = gtk::Grid::builder().row_spacing(6).column_spacing(16).build();
    let rows = [tr("Frames per second"), tr("Dropped frames"), tr("Encoder"), tr("Bitrate"), tr("Picture size")];
    let mut stat_values = Vec::new();
    for (i, name) in rows.iter().enumerate() {
        let n = label(name, &["dim"]);
        let v = label("—", &[]);
        v.set_hexpand(true);
        v.set_halign(gtk::Align::End);
        v.set_xalign(1.0);
        grid.attach(&n, 0, i as i32, 1, 1);
        grid.attach(&v, 1, i as i32, 1, 1);
        stat_values.push(v);
    }
    let cpu_value = label("—", &[]);
    cpu_value.set_halign(gtk::Align::End);
    cpu_value.set_hexpand(true);
    cpu_value.set_xalign(1.0);
    let cpu_bar = gtk::LevelBar::for_interval(0.0, 100.0);
    let gpu_value = label("—", &[]);
    gpu_value.set_halign(gtk::Align::End);
    gpu_value.set_hexpand(true);
    gpu_value.set_xalign(1.0);
    let gpu_bar = gtk::LevelBar::for_interval(0.0, 100.0);
    let n = rows.len() as i32;
    grid.attach(&label(tr("CPU"), &["dim"]), 0, n, 1, 1);
    grid.attach(&cpu_value, 1, n, 1, 1);
    grid.attach(&cpu_bar, 0, n + 1, 2, 1);
    grid.attach(&label(tr("GPU"), &["dim"]), 0, n + 2, 1, 1);
    grid.attach(&gpu_value, 1, n + 2, 1, 1);
    grid.attach(&gpu_bar, 0, n + 3, 2, 1);
    stat_values.push(cpu_value);
    stat_values.push(gpu_value);
    let stats_hint = label("", &["dim"]);
    stats_body.append(&grid);
    stats_body.append(&stats_hint);
    stats_card.set_size_request(320, -1);

    let (out_card, out_body) = card(tr("Output"));
    out_body.append(&label(tr("Save in"), &["dim"]));
    let folder_label = label("", &["mono"]);
    folder_label.set_wrap_mode(gtk::pango::WrapMode::Char);
    let folder_btn = gtk::Button::builder().label(tr("Choose folder…")).halign(gtk::Align::Start).build();
    out_body.append(&folder_label);
    out_body.append(&folder_btn);
    let file_label = label("", &["dim"]);
    out_body.append(&file_label);
    out_body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let desktop_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    let mic_switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
    for (text, sw) in [(tr("Computer sound"), &desktop_switch), (tr("Microphone"), &mic_switch)] {
        let row = hbox(8);
        let l = label(text, &[]);
        l.set_hexpand(true);
        row.append(&l);
        row.append(sw);
        out_body.append(&row);
    }
    out_card.set_size_request(320, -1);

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(1)
        .max_children_per_line(3)
        .column_spacing(14)
        .row_spacing(14)
        .homogeneous(false)
        .valign(gtk::Align::Start)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(16)
        .margin_end(16)
        .build();
    for c in [&preview_card, &stats_card, &out_card] {
        flow.append(c);
    }
    let narrow_slot = vbox(0);
    let main_column = vbox(0);
    main_column.append(&narrow_slot);
    main_column.append(&flow);
    let main_scroll =
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).hexpand(true).vexpand(true).child(&main_column).build();

    let body = hbox(0);
    body.append(&sidebar);
    body.append(&main_scroll);
    body.set_vexpand(true);
    let root = vbox(0);
    root.append(&topbar);
    root.append(&banner);
    root.append(&body);
    window.set_child(Some(&root));

    Widgets {
        window,
        dot,
        state_label,
        timer_label,
        service_chip,
        record_btn,
        banner,
        banner_box,
        banner_label,
        banner_dismiss,
        sidebar,
        side,
        narrow_slot,
        profile_btns,
        sources_list,
        sources_empty,
        preview_title,
        preview_body,
        stat_values,
        cpu_bar,
        gpu_bar,
        stats_hint,
        folder_label,
        folder_btn,
        desktop_switch,
        mic_switch,
        file_label,
    }
}

impl App {
    fn wire(self: &Rc<Self>) {
        let app = self.clone();
        self.w.record_btn.connect_clicked(move |_| app.on_record_clicked());
        let app = self.clone();
        self.w.banner_dismiss.connect_clicked(move |_| app.dispatch(Msg::DismissError));
        for (p, b) in &self.w.profile_btns {
            let (app, p) = (self.clone(), *p);
            b.connect_toggled(move |b| {
                if b.is_active() {
                    app.dispatch(Msg::SelectProfile(p));
                }
            });
        }
        let app = self.clone();
        self.w.sources_list.connect_row_selected(move |_, row| {
            if app.st.borrow().rebuilding {
                return;
            }
            if let Some(row) = row {
                app.st.borrow_mut().selected = parse_key(row.widget_name().as_str());
                app.render_output();
            }
        });
        let app = self.clone();
        self.w.folder_btn.connect_clicked(move |_| app.choose_folder());
        // a hidden-from-the-list sound choice is the person's: the profile only sets the starting point
        // sizes: the sidebar gives way on narrow windows (the main cards then have the whole width)
        let (sidebar, side, slot) = (self.w.sidebar.clone(), self.w.side.clone(), self.w.narrow_slot.clone());
        let narrow_now = Rc::new(Cell::new(false));
        // the surface tells us when its size changes (no frame clock needed): wide → the sidebar, narrow → above the cards
        self.w.window.connect_realize(move |w| {
            let Some(surface) = w.surface() else { return };
            let narrow_now = narrow_now.clone();
            let (sidebar, side, slot) = (sidebar.clone(), side.clone(), slot.clone());
            surface.connect_layout(move |_, width, _| {
                let narrow = width < 700;
                if narrow == narrow_now.get() {
                    return;
                }
                narrow_now.set(narrow);
                if narrow {
                    sidebar.set_child(None::<&gtk::Widget>);
                    slot.append(&side);
                    sidebar.set_visible(false);
                } else {
                    slot.remove(&side);
                    sidebar.set_child(Some(&side));
                    sidebar.set_visible(true);
                }
            });
        });
        // draw the dot from the spring
        let app = self.clone();
        self.w.dot.set_draw_func(move |_, cr, width, height| {
            let recording = app.model.borrow().is_recording();
            let k = app.anim.borrow().dot.value().clamp(0.0, 1.6);
            let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
            let radius = f64::from(width.min(height)) / 2.0 * 0.62;
            if recording {
                cr.set_source_rgb(0xE0 as f64 / 255.0, 0x6C as f64 / 255.0, 0x75 as f64 / 255.0);
                cr.arc(cx, cy, radius * k, 0.0, std::f64::consts::TAU);
                let _ = cr.fill();
            } else {
                cr.set_source_rgb(0x7A as f64 / 255.0, 0x7B as f64 / 255.0, 0x88 as f64 / 255.0);
                cr.set_line_width(1.5);
                cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
                let _ = cr.stroke();
            }
        });
        // the clock: the timer, the consent countdown, and the status poll while recording
        let app = self.clone();
        glib::timeout_add_local(Duration::from_millis(500), move || {
            app.on_second();
            glib::ControlFlow::Continue
        });
        let app = self.clone();
        glib::timeout_add_seconds_local(4, move || {
            app.refresh_sources_if_idle();
            glib::ControlFlow::Continue
        });
        // closing: finish a running recording first, stop a daemon we started
        let app = self.clone();
        self.w.window.connect_close_request(move |_| {
            app.shutdown();
            glib::Propagation::Proceed
        });
        // a signal (the script stopping us) does the same
        for sig in [libc::SIGTERM, libc::SIGINT] {
            let app = self.clone();
            glib::unix_signal_add_local(sig, move || {
                app.shutdown();
                app.w.window.close();
                glib::ControlFlow::Break
            });
        }
        // the profile's engine settings are the starting point of the two sound switches
        self.apply_profile_switches();
    }

    fn shutdown(&self) {
        if self.model.borrow().is_recording() {
            // blocking on purpose: the file must be finished before the window is gone
            let _ = client::call(&self.socket, "record.stop", json!({}), Duration::from_secs(15));
        }
        if let Some(mut s) = self.service.lock().ok().and_then(|mut s| s.take()) {
            s.stop();
        }
    }

    fn apply_profile_switches(&self) {
        let preset = self.model.borrow().profile.preset();
        self.w.desktop_switch.set_active(preset.desktop_audio);
        self.w.mic_switch.set_active(preset.mic);
    }

    /// Starts (or finds) the daemon without blocking the window.
    fn start_service(self: &Rc<Self>) {
        self.dispatch_quiet_connecting();
        let (tx, slot, socket, secret) = (self.tx.clone(), self.service.clone(), self.socket.clone(), self.secret.clone());
        let _ = std::thread::Builder::new().name("cast-service".into()).spawn(move || {
            match Service::ensure_running(&socket, daemon::find_binary().as_deref()) {
                Ok(svc) => {
                    eprintln!(
                        "kalango-cast-app: {}",
                        if svc.started_by_app() { "started kalango-castd (stopped when the app exits)" } else { "using the running kalango-castd" }
                    );
                    // the handler secret is kept in memory only, ready before the first registration attempt
                    if let Ok(mut s) = secret.lock() {
                        *s = svc.secret().map(str::to_string);
                    }
                    if let Ok(mut s) = slot.lock() {
                        *s = Some(svc);
                    }
                    let _ = tx.send_blocking(Msg::DaemonReady);
                }
                Err(e) => {
                    let _ = tx.send_blocking(Msg::DaemonUnavailable(e));
                }
            }
        });
    }

    fn dispatch_quiet_connecting(&self) {
        self.model.borrow_mut().daemon = DaemonState::Connecting;
    }

    fn dispatch(self: &Rc<Self>, msg: Msg) {
        let daemon_ready = matches!(msg, Msg::DaemonReady);
        let profile_changed = matches!(msg, Msg::SelectProfile(_));
        let effects = self.model.borrow_mut().update(msg);
        if daemon_ready {
            self.refresh_sources();
        }
        if profile_changed {
            self.apply_profile_switches();
        }
        self.render();
        for e in effects {
            self.run(e);
        }
    }

    fn run(self: &Rc<Self>, effect: Effect) {
        let (tx, socket) = (self.tx.clone(), self.socket.clone());
        match effect {
            Effect::StartRecording => {
                let Some(cfg) = self.st.borrow().last_config.clone() else { return };
                let _ = std::thread::Builder::new().name("cast-start".into()).spawn(move || {
                    let msg = match client::call(&socket, "record.start", cfg.to_params(), Duration::from_secs(40)) {
                        Ok(status) => Msg::Started { at: Instant::now(), status },
                        Err(CallError::Rpc { code, message, request_id }) => Msg::StartFailed { code, message, request_id },
                        Err(CallError::Transport(m)) => {
                            let _ = tx.send_blocking(Msg::DaemonUnavailable(m.clone()));
                            return;
                        }
                    };
                    let _ = tx.send_blocking(msg);
                });
            }
            Effect::Approve(token) => {
                if let Err(e) = self.link.approve(&token) {
                    let _ = tx.send_blocking(Msg::ApproveFailed(e));
                }
            }
            Effect::Deny(token) => {
                let _ = self.link.deny(&token);
            }
            Effect::StopRecording => {
                let _ = std::thread::Builder::new().name("cast-stop".into()).spawn(move || {
                    let msg = match client::call(&socket, "record.stop", json!({}), Duration::from_secs(30)) {
                        Ok(v) => {
                            Msg::Stopped { path: v["path"].as_str().unwrap_or("").to_string(), duration_s: v["duration_s"].as_f64().unwrap_or(0.0) }
                        }
                        Err(e) => Msg::StopFailed(e.message()),
                    };
                    let _ = tx.send_blocking(msg);
                });
            }
        }
    }

    fn refresh_sources_if_idle(self: &Rc<Self>) {
        let (idle, ready) = {
            let m = self.model.borrow();
            (m.can_configure(), m.daemon == DaemonState::Ready)
        };
        if idle && ready {
            self.refresh_sources();
        }
    }

    fn refresh_sources(self: &Rc<Self>) {
        if self.sources_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let (tx, socket, busy) = (self.tx.clone(), self.socket.clone(), self.sources_busy.clone());
        // the daemon this app spawned hands window titles to whoever holds its secret (the app does)
        let params = match self.secret.lock().ok().and_then(|s| s.clone()) {
            Some(s) => json!({"secret": s}),
            None => json!({}),
        };
        let _ = std::thread::Builder::new().name("cast-sources".into()).spawn(move || {
            let msg = match client::call(&socket, "sources.list", params, Duration::from_secs(10)) {
                Ok(v) => Some(Msg::Sources(Ok(SourceList::from_json(&v)))),
                Err(CallError::Rpc { message, .. }) => Some(Msg::Sources(Err(message))),
                Err(CallError::Transport(m)) => {
                    let _ = tx.send_blocking(Msg::DaemonUnavailable(m));
                    None
                }
            };
            busy.store(false, Ordering::Release);
            if let Some(m) = msg {
                let _ = tx.send_blocking(m);
            }
        });
    }

    fn on_second(self: &Rc<Self>) {
        let now = Instant::now();
        let (recording, awaiting) = {
            let m = self.model.borrow();
            (m.is_recording(), m.phase == Phase::AwaitingConsent)
        };
        if awaiting {
            self.dispatch(Msg::Tick(now));
        }
        if recording {
            self.render_indicator();
            if !self.status_busy.swap(true, Ordering::AcqRel) {
                let (tx, socket, busy) = (self.tx.clone(), self.socket.clone(), self.status_busy.clone());
                let _ = std::thread::Builder::new().name("cast-status".into()).spawn(move || {
                    let msg = match client::call(&socket, "record.status", json!({}), Duration::from_secs(5)) {
                        Ok(v) => Msg::Status(v),
                        // the service answers but says the recording has a problem
                        Err(CallError::Rpc { message, .. }) => Msg::Status(json!({"recording": true, "error": message})),
                        Err(CallError::Transport(m)) => Msg::DaemonUnavailable(m),
                    };
                    busy.store(false, Ordering::Release);
                    let _ = tx.send_blocking(msg);
                });
            }
        }
        self.update_countdown(now);
    }

    fn on_record_clicked(self: &Rc<Self>) {
        let phase = self.model.borrow().phase.clone();
        if phase == Phase::Recording {
            self.dispatch(Msg::StopClicked);
            return;
        }
        if !self.model.borrow().can_start() {
            return;
        }
        match self.build_config() {
            Ok(cfg) => {
                self.st.borrow_mut().last_config = Some(cfg);
                self.dispatch(Msg::StartClicked);
            }
            Err(why) => {
                // shown as the error banner, like any failed start
                let _ = self.model.borrow_mut().update(Msg::StartClicked);
                self.dispatch(Msg::StartFailed { code: -32000, message: why, request_id: None });
            }
        }
    }

    fn build_config(&self) -> Result<StartConfig, String> {
        let key = self.st.borrow().selected.clone().ok_or_else(|| tr("Choose a screen or a window to record first.").to_string())?;
        let folder = self.st.borrow().folder.clone();
        if !folder.exists() && folder == default_folder() {
            std::fs::create_dir_all(&folder)
                .map_err(|e| trf("The folder {dir} cannot be used: {why}", &[("dir", folder.display().to_string()), ("why", e.to_string())]))?;
        }
        check_folder(&folder)?;
        let stamp = glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format("%Y-%m-%d %H-%M-%S").ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "recording".into());
        let path = folder.join(format!("Cast {stamp}.mp4"));
        let mut preset = self.model.borrow().profile.preset();
        preset.desktop_audio = self.w.desktop_switch.is_active();
        preset.mic = self.w.mic_switch.is_active();
        Ok(StartConfig { source: key.source(), path: path.display().to_string(), preset })
    }

    fn choose_folder(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title(tr("Choose where recordings are saved")).modal(true).build();
        dialog.set_initial_folder(Some(&gtk::gio::File::for_path(&self.st.borrow().folder)));
        let app = self.clone();
        dialog.select_folder(Some(&self.w.window), gtk::gio::Cancellable::NONE, move |res| {
            if let Ok(file) = res {
                if let Some(path) = file.path() {
                    app.st.borrow_mut().folder = path;
                    app.render_output();
                }
            }
        });
    }

    // ---- rendering --------------------------------------------------------------------------------------------

    fn render(self: &Rc<Self>) {
        self.render_indicator();
        self.render_banner();
        self.render_sources();
        self.render_preview_and_stats();
        self.render_output();
        self.render_dialog();
        self.render_locks();
        self.kick();
    }

    fn render_indicator(&self) {
        let m = self.model.borrow();
        let now = Instant::now();
        let w = &self.w;
        let (state, recording) = match &m.phase {
            Phase::Recording => (tr("REC"), true),
            Phase::Starting => (tr("Starting…"), false),
            Phase::AwaitingConsent => (tr("Waiting for your permission"), false),
            Phase::Stopping => (tr("Finishing the file…"), true),
            Phase::Error(_) => (tr("Stopped with an error"), false),
            Phase::Idle => (tr("Ready"), false),
        };
        w.state_label.set_label(state);
        w.timer_label.set_visible(recording);
        w.timer_label.set_label(&format_timer(m.elapsed_seconds(now)));
        if recording {
            w.timer_label.add_css_class("recording");
        } else {
            w.timer_label.remove_css_class("recording");
        }
        let (chip, class) = match &m.daemon {
            DaemonState::Connecting => (tr("Service starting…").to_string(), ""),
            DaemonState::Ready => (tr("Service ready").to_string(), "ok"),
            DaemonState::Unavailable(_) => (tr("Service unavailable").to_string(), "bad"),
        };
        w.service_chip.set_label(&chip);
        for c in ["ok", "bad"] {
            w.service_chip.remove_css_class(c);
        }
        if !class.is_empty() {
            w.service_chip.add_css_class(class);
        }
        if let DaemonState::Unavailable(why) = &m.daemon {
            w.service_chip.set_tooltip_text(Some(why));
        } else {
            w.service_chip.set_tooltip_text(None);
        }
        // the record button
        let (text, enabled, stop) = match &m.phase {
            Phase::Recording => (tr("Stop"), true, true),
            Phase::Stopping => (tr("Stopping…"), false, true),
            Phase::Starting | Phase::AwaitingConsent => (tr("Record"), false, false),
            _ => (tr("Record"), m.can_start(), false),
        };
        w.record_btn.set_label(text);
        w.record_btn.set_sensitive(enabled);
        // a disabled button says why (the preview's headline is the reason: no service, no capture...)
        let reason = m.preview().title;
        w.record_btn.set_tooltip_text(if enabled || !matches!(m.phase, Phase::Idle | Phase::Error(_)) { None } else { Some(&reason) });
        if stop {
            w.record_btn.add_css_class("stop");
        } else {
            w.record_btn.remove_css_class("stop");
        }
        drop(m);
        // the dot pops in when a recording starts, and shrinks away when it ends
        let mut a = self.anim.borrow_mut();
        if recording && a.dot.target() < 0.5 {
            a.dot.snap(0.2);
            a.dot.set_target(1.0);
        } else if !recording && a.dot.target() > 0.5 {
            a.dot.snap(0.0);
        }
        drop(a);
        w.dot.queue_draw();
    }

    fn render_banner(&self) {
        let m = self.model.borrow();
        let w = &self.w;
        let (text, error) = match (&m.phase, &m.notice) {
            (Phase::Error(why), _) => (Some(why.clone()), true),
            (_, Some(n)) => (Some(n.clone()), false),
            _ => (None, false),
        };
        if let Some(t) = &text {
            w.banner_label.set_label(t);
        }
        if error {
            w.banner_box.add_css_class("error");
        } else {
            w.banner_box.remove_css_class("error");
        }
        w.banner_dismiss.set_visible(error);
        w.banner.set_reveal_child(text.is_some());
    }

    fn render_sources(self: &Rc<Self>) {
        let m = self.model.borrow();
        let w = &self.w;
        let changed = self.st.borrow().rendered_sources.as_ref() != Some(&m.sources);
        if changed {
            self.st.borrow_mut().rebuilding = true;
            while let Some(row) = w.sources_list.row_at_index(0) {
                w.sources_list.remove(&row);
            }
            let mut keys = Vec::new();
            for mon in &m.sources.monitors {
                let title = if mon.name.is_empty() { mon.connector.clone() } else { format!("{} · {}", mon.connector, mon.name) };
                let sub = trf(
                    "Whole screen · {w}×{h} · {hz} Hz",
                    &[("w", mon.width.to_string()), ("h", mon.height.to_string()), ("hz", format!("{:.0}", mon.refresh_hz))],
                );
                add_source_row(&w.sources_list, &format!("m:{}", mon.connector), &title, &sub);
                keys.push(SourceKey::Monitor(mon.connector.clone()));
            }
            for win in &m.sources.windows {
                let title = if win.title.is_empty() { tr("(untitled window)").to_string() } else { win.title.clone() };
                let sub = if win.app_id.is_empty() { tr("Window").to_string() } else { trf("Window · {app}", &[("app", win.app_id.clone())]) };
                add_source_row(&w.sources_list, &format!("w:{}", win.id), &title, &sub);
                keys.push(SourceKey::Window(win.id));
            }
            let keep = self.st.borrow().selected.clone().filter(|k| keys.contains(k));
            let pick = keep.or_else(|| keys.first().cloned());
            self.st.borrow_mut().selected = pick.clone();
            if let Some(k) = pick {
                if let Some(i) = keys.iter().position(|x| *x == k) {
                    if let Some(row) = w.sources_list.row_at_index(i as i32) {
                        w.sources_list.select_row(Some(&row));
                    }
                }
            }
            let mut st = self.st.borrow_mut();
            st.rebuilding = false;
            st.rendered_sources = Some(m.sources.clone());
        }
        let empty = match (&m.capture, m.sources.is_empty()) {
            (CaptureState::Available, true) => tr("No screens or windows are offered by the compositor.").to_string(),
            (CaptureState::Unavailable(_), _) => tr("No sources: capture is not available in this session.").to_string(),
            (CaptureState::Unknown, _) => tr("Looking for screens and windows…").to_string(),
            _ => String::new(),
        };
        w.sources_empty.set_label(&empty);
        w.sources_empty.set_visible(!empty.is_empty());
        w.sources_list.set_visible(!m.sources.is_empty());
    }

    fn render_preview_and_stats(&self) {
        let m = self.model.borrow();
        let w = &self.w;
        let p = m.preview();
        w.preview_title.set_label(&p.title);
        w.preview_body.set_label(&p.body);
        let live = m.is_recording() || m.phase == Phase::Stopping;
        let s = &m.stats;
        let dash = "—".to_string();
        let vals = [
            if live { format!("{:.1}", s.fps) } else { dash.clone() },
            if live { s.dropped.to_string() } else { dash.clone() },
            if live && !s.encoder.is_empty() {
                format!("{} · {}{}", s.encoder, s.codec, if s.zero_copy { " · zero-copy" } else { "" })
            } else {
                dash.clone()
            },
            if live { format!("{:.0} kbit/s", s.bitrate_kbps) } else { dash.clone() },
            if live && s.width > 0 { format!("{}×{}", s.width, s.height) } else { dash.clone() },
            if live { format!("{:.0} %", s.cpu_percent) } else { dash.clone() },
            match (live, s.gpu_percent) {
                (true, Some(g)) => format!("{g} %"),
                (true, None) => tr("not available").to_string(),
                _ => dash.clone(),
            },
        ];
        for (l, v) in w.stat_values.iter().zip(vals) {
            l.set_label(&v);
        }
        w.stats_hint.set_label(if live { "" } else { tr("The numbers appear while recording.") });
        w.stats_hint.set_visible(!live);
        let mut a = self.anim.borrow_mut();
        a.cpu.set_target(if live { s.cpu_percent.clamp(0.0, 100.0) } else { 0.0 });
        a.gpu.set_target(if live { f64::from(s.gpu_percent.unwrap_or(0) as u32) } else { 0.0 });
    }

    fn render_output(&self) {
        let st = self.st.borrow();
        self.w.folder_label.set_label(&tilde(&st.folder));
        let name = match &st.selected {
            Some(SourceKey::Monitor(c)) => trf("Records the screen {name}.", &[("name", c.clone())]),
            Some(SourceKey::Window(_)) => tr("Records the chosen window.").to_string(),
            None => tr("No source chosen yet.").to_string(),
        };
        self.w.file_label.set_label(&name);
    }

    fn render_locks(&self) {
        let (can, profile) = {
            let m = self.model.borrow();
            (m.can_configure(), m.profile)
        };
        for (p, b) in &self.w.profile_btns {
            b.set_sensitive(can);
            // (set_active fires the toggle handler, which updates the model: no model borrow is held here)
            if *p == profile && !b.is_active() {
                b.set_active(true);
            }
        }
        self.w.sources_list.set_sensitive(can);
        self.w.folder_btn.set_sensitive(can);
        self.w.desktop_switch.set_sensitive(can);
        self.w.mic_switch.set_sensitive(can);
    }

    // ---- the consent dialog -----------------------------------------------------------------------------------

    fn render_dialog(self: &Rc<Self>) {
        let want = self.model.borrow().consent_dialog().map(|(r, own)| (r.request_id.clone(), r.what.clone(), own));
        let showing = self.st.borrow().dialog_request.clone();
        // the dialog shows exactly one request: when it changes (answered, replaced, expired) the dialog does too
        if showing.is_some() && showing != want.as_ref().map(|w| w.0.clone()) {
            if let Some(d) = self.st.borrow_mut().dialog.take() {
                d.destroy();
            }
            let mut st = self.st.borrow_mut();
            st.dialog_request = None;
            st.dialog_countdown = None;
            st.dialog_buttons.clear();
            self.anim.borrow_mut().dialog_opacity.snap(0.0);
        }
        if let Some((id, what, own)) = want {
            if self.st.borrow().dialog_request.is_none() {
                self.st.borrow_mut().dialog_request = Some(id);
                self.open_dialog(&what, own);
            }
        }
    }

    fn open_dialog(self: &Rc<Self>, what: &str, own: bool) {
        let dialog = gtk::Window::builder()
            .transient_for(&self.w.window)
            .modal(true)
            .resizable(false)
            .title(tr("Allow recording?"))
            .default_width(520)
            .css_classes(["cast-dialog"])
            .build();
        let body = vbox(12);
        body.set_margin_top(18);
        body.set_margin_bottom(18);
        body.set_margin_start(20);
        body.set_margin_end(20);
        body.append(&label(tr("Allow recording the screen/microphone?"), &["card-title"]));
        if !own {
            // not ours: the person must not approve it believing this app asked
            body.append(&label(tr("Request from another program"), &["card-title"]));
        }
        let intro = if own {
            tr("Kalango Cast asks the recording service to do this. It describes it exactly like this:")
        } else {
            tr("Another program asked the recording service to do this. It describes it exactly like this:")
        };
        body.append(&label(intro, &["dim"]));
        // the service's own words, escaped by the service; shown as plain text (no markup), never edited here
        let what_label = gtk::Label::builder()
            .label(what)
            .selectable(false)
            .use_markup(false)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::Char)
            .xalign(0.0)
            .halign(gtk::Align::Fill)
            .css_classes(["what", "mono"])
            .build();
        body.append(&what_label);
        let countdown = label("", &["dim"]);
        body.append(&countdown);
        body.append(&label(tr("While recording, this window shows a red dot and a timer at the top. Nothing is recorded if you deny."), &["warn"]));
        let buttons = hbox(8);
        buttons.set_halign(gtk::Align::End);
        let deny = gtk::Button::with_label(tr("Deny"));
        let allow = gtk::Button::builder().label(tr("Allow")).css_classes(["record"]).build();
        buttons.append(&deny);
        buttons.append(&allow);
        body.append(&buttons);
        dialog.set_child(Some(&body));
        dialog.set_default_widget(Some(&allow));
        let app = self.clone();
        deny.connect_clicked(move |_| app.dispatch(Msg::Deny));
        let app = self.clone();
        let allow_btn = allow.clone();
        let deny_btn = deny.clone();
        allow.connect_clicked(move |_| {
            allow_btn.set_sensitive(false);
            deny_btn.set_sensitive(false);
            app.dispatch(Msg::Allow);
        });
        // Esc and closing the window mean no
        let key = gtk::EventControllerKey::new();
        let app = self.clone();
        key.connect_key_pressed(move |_, k, _, _| {
            if k == gdk::Key::Escape {
                app.dispatch(Msg::Deny);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        dialog.add_controller(key);
        let app = self.clone();
        dialog.connect_close_request(move |_| {
            app.dispatch(Msg::Deny);
            glib::Propagation::Stop
        });
        dialog.set_opacity(0.0);
        dialog.present();
        {
            let mut st = self.st.borrow_mut();
            st.dialog = Some(dialog);
            st.dialog_countdown = Some(countdown);
            st.dialog_buttons = vec![deny, allow];
        }
        self.anim.borrow_mut().dialog_opacity.snap(0.0);
        self.anim.borrow_mut().dialog_opacity.set_target(1.0);
        self.update_countdown(Instant::now());
        self.kick();
    }

    fn update_countdown(&self, now: Instant) {
        let m = self.model.borrow();
        if let (Some((req, _)), Some(l)) = (m.consent_dialog(), self.st.borrow().dialog_countdown.as_ref()) {
            l.set_label(&trf("Expires in {n} s", &[("n", req.seconds_left(now).to_string())]));
        }
    }

    // ---- motion -----------------------------------------------------------------------------------------------

    /// Runs the frame clock only while something moves (a resting window costs no frames).
    fn kick(self: &Rc<Self>) {
        if self.animating.replace(true) {
            return;
        }
        let app = self.clone();
        let last = Cell::new(0i64);
        self.w.window.add_tick_callback(move |_, clock| {
            let t = clock.frame_time();
            let dt = if last.get() == 0 { 1.0 / 60.0 } else { (t - last.get()) as f64 / 1e6 };
            last.set(t);
            if app.step_animation(dt) {
                glib::ControlFlow::Continue
            } else {
                app.animating.set(false);
                glib::ControlFlow::Break
            }
        });
    }

    fn step_animation(&self, dt: f64) -> bool {
        let mut a = self.anim.borrow_mut();
        let moving = [a.dot.step(dt), a.cpu.step(dt), a.gpu.step(dt), a.dialog_opacity.step(dt)].into_iter().any(|m| m);
        self.w.cpu_bar.set_value(a.cpu.value().clamp(0.0, 100.0));
        self.w.gpu_bar.set_value(a.gpu.value().clamp(0.0, 100.0));
        self.w.dot.queue_draw();
        if let Some(d) = &self.st.borrow().dialog {
            d.set_opacity(a.dialog_opacity.value().clamp(0.0, 1.0));
        }
        moving
    }
}

/// A path for display: the home folder shown as `~`.
fn tilde(path: &std::path::Path) -> String {
    match path.strip_prefix(glib::home_dir()) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn add_source_row(list: &gtk::ListBox, key: &str, title: &str, sub: &str) {
    let inner = vbox(2);
    inner.append(&label(title, &[]));
    inner.append(&label(sub, &["dim"]));
    let row = gtk::ListBoxRow::builder().child(&inner).build();
    row.set_widget_name(key);
    list.append(&row);
}

fn parse_key(name: &str) -> Option<SourceKey> {
    if let Some(c) = name.strip_prefix("m:") {
        return Some(SourceKey::Monitor(c.to_string()));
    }
    name.strip_prefix("w:").and_then(|id| id.parse().ok()).map(SourceKey::Window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_keys_round_trip_through_row_names() {
        assert_eq!(parse_key("m:DP-1"), Some(SourceKey::Monitor("DP-1".into())));
        assert_eq!(parse_key("w:42"), Some(SourceKey::Window(42)));
        assert_eq!(parse_key("w:x"), None);
        assert_eq!(SourceKey::Window(7).source(), Source::Window { id: 7 });
        assert_eq!(SourceKey::Monitor("DP-1".into()).source(), Source::Monitor { connector: "DP-1".into() });
    }

    #[test]
    fn paths_are_shown_with_the_home_as_tilde() {
        let home = glib::home_dir();
        assert_eq!(tilde(&home.join("Videos")), "~/Videos");
        assert_eq!(tilde(&home), "~");
        assert_eq!(tilde(std::path::Path::new("/srv/x")), "/srv/x");
    }

    #[test]
    fn a_folder_that_is_not_the_persons_is_refused_with_the_reason() {
        assert!(check_folder(std::path::Path::new("/tmp")).is_err(), "/tmp is the root's");
        assert!(check_folder(std::path::Path::new("/nonexistent/kalango-cast")).is_err());
        let dir = std::env::temp_dir().join(format!("kalango-cast-app-{}-folder", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(check_folder(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
