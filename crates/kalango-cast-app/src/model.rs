//! The app's view-model: a pure state machine (no GTK, no sockets) that the window renders and the workers feed.
//!
//! The recording's phases are `Idle → Starting → AwaitingConsent → Starting → Recording → Stopping → Idle`, with
//! `Error` reachable from any of them. [`Model::update`] takes a [`Msg`] (what the person or the daemon did) and
//! returns the [`Effect`]s the shell must run (socket calls); it never does I/O itself, so it is tested alone.

use std::time::{Duration, Instant};

use cast_core::i18n::n_;
use cast_core::{tr, trf, Source};
use serde_json::{json, Value};

/// The JSON-RPC error code of `record.start` when the person has not approved yet (`cast_engine::rpc`).
pub const CONSENT_REQUIRED: i64 = -32001;

/// What a recording is doing right now.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    /// `record.start` was sent (or is being re-sent after an approval).
    Starting,
    /// The daemon answered `consent_required`: nothing records until the person decides.
    AwaitingConsent,
    Recording,
    Stopping,
    Error(String),
}

/// A request the daemon holds for the person's approval, as it described it (escaped by the engine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsentRequest {
    /// The request's public name: the daemon tells the client that made it the same id in its `consent_required`
    /// error, which is how a request is known to be ours.
    pub request_id: String,
    pub what: String,
    pub token: String,
    pub received: Instant,
    pub ttl: Duration,
}

impl ConsentRequest {
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.received) >= self.ttl
    }

    pub fn seconds_left(&self, now: Instant) -> u64 {
        self.ttl.saturating_sub(now.saturating_duration_since(self.received)).as_secs()
    }
}

/// Whether the service is there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    Connecting,
    Ready,
    Unavailable(String),
}

/// Whether the session can be captured (`sources.list` worked).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureState {
    Unknown,
    Available,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    pub connector: String,
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub refresh_hz: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceList {
    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<WindowInfo>,
}

impl SourceList {
    /// Reads `sources.list`'s answer.
    pub fn from_json(v: &Value) -> SourceList {
        let monitors = v["monitors"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        Some(MonitorInfo {
                            connector: m["connector"].as_str()?.to_string(),
                            name: m["name"].as_str().unwrap_or("").to_string(),
                            width: m["width"].as_i64().unwrap_or(0),
                            height: m["height"].as_i64().unwrap_or(0),
                            refresh_hz: m["refresh_hz"].as_f64().unwrap_or(0.0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let windows = v["windows"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|w| {
                        Some(WindowInfo {
                            id: w["id"].as_u64()?,
                            title: w["title"].as_str().unwrap_or("").to_string(),
                            app_id: w["app_id"].as_str().unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        SourceList { monitors, windows }
    }

    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty() && self.windows.is_empty()
    }
}

/// A scene profile: it only sets the engine's configuration, nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Profile {
    Gamer,
    Work,
    Podcast,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::Gamer, Profile::Work, Profile::Podcast];

    /// The English source string (translate with `tr` when shown).
    pub fn title(self) -> &'static str {
        match self {
            Profile::Gamer => n_("Gamer"),
            Profile::Work => n_("Work"),
            Profile::Podcast => n_("Podcast"),
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Profile::Gamer => n_("Smooth motion for games: 60 fps, high bitrate, computer sound and voice chat."),
            Profile::Work => n_("Lessons and presentations: 30 fps, lighter file, computer sound and microphone."),
            Profile::Podcast => n_("Voice first: 30 fps, small file, microphone only."),
        }
    }

    pub fn preset(self) -> Preset {
        match self {
            Profile::Gamer => Preset { fps: 60, bitrate_kbps: 15_000, desktop_audio: true, mic: true },
            Profile::Work => Preset { fps: 30, bitrate_kbps: 6_000, desktop_audio: true, mic: true },
            Profile::Podcast => Preset { fps: 30, bitrate_kbps: 3_000, desktop_audio: false, mic: true },
        }
    }
}

/// The engine settings a profile sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub desktop_audio: bool,
    pub mic: bool,
}

/// What the person chose to record: becomes `record.start`'s parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct StartConfig {
    pub source: Source,
    pub path: String,
    pub preset: Preset,
}

impl StartConfig {
    pub fn to_params(&self) -> Value {
        json!({
            "source": serde_json::to_value(&self.source).unwrap_or(Value::Null),
            "path": self.path,
            "codec": "h264",
            "fps": self.preset.fps,
            "bitrate_kbps": self.preset.bitrate_kbps,
            "desktop_audio": self.preset.desktop_audio,
            "mic": self.preset.mic,
        })
    }
}

/// What `record.status` says, the fields the window shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub fps: f64,
    pub dropped: u64,
    pub encoder: String,
    pub codec: String,
    pub zero_copy: bool,
    pub width: u64,
    pub height: u64,
    pub bitrate_kbps: f64,
    pub cpu_percent: f64,
    pub gpu_percent: Option<u64>,
    pub duration_s: f64,
    pub path: String,
}

impl Stats {
    pub fn from_json(v: &Value) -> Stats {
        Stats {
            fps: v["fps"].as_f64().unwrap_or(0.0),
            dropped: v["dropped"].as_u64().unwrap_or(0),
            encoder: v["encoder"].as_str().unwrap_or("").to_string(),
            codec: v["codec"].as_str().unwrap_or("").to_string(),
            zero_copy: v["zero_copy"].as_bool().unwrap_or(false),
            width: v["width"].as_u64().unwrap_or(0),
            height: v["height"].as_u64().unwrap_or(0),
            bitrate_kbps: v["bitrate_kbps"].as_f64().unwrap_or(0.0),
            cpu_percent: v["cpu_percent"].as_f64().unwrap_or(0.0),
            gpu_percent: v["gpu_percent"].as_u64(),
            duration_s: v["duration_s"].as_f64().unwrap_or(0.0),
            path: v["path"].as_str().unwrap_or("").to_string(),
        }
    }
}

/// The preview area's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub title: String,
    pub body: String,
}

/// Everything that can happen to the model.
#[derive(Clone, Debug)]
pub enum Msg {
    DaemonReady,
    DaemonUnavailable(String),
    Sources(Result<SourceList, String>),
    SelectProfile(Profile),
    StartClicked,
    /// `record.start` was accepted: the recording runs.
    Started {
        at: Instant,
        status: Value,
    },
    /// `record.start` was refused or failed (`code` is the JSON-RPC error code).
    StartFailed {
        code: i64,
        message: String,
        request_id: Option<String>,
    },
    /// The daemon pushed a consent request to us, its registered handler.
    ConsentRequested(ConsentRequest),
    /// The daemon replaced a held request with a different one before it was answered.
    ConsentSuperseded {
        request_id: String,
    },
    /// We could not register as the consent handler (the code then goes to the daemon's terminal).
    HandlerUnavailable(String),
    /// We are the registered consent handler.
    HandlerRegistered,
    Allow,
    Deny,
    Approved,
    ApproveFailed(String),
    Tick(Instant),
    StopClicked,
    Stopped {
        path: String,
        duration_s: f64,
    },
    StopFailed(String),
    Status(Value),
    /// The daemon said the recording failed.
    RecordingFailed {
        path: String,
        error: String,
    },
    DismissError,
}

/// What the shell must do after an update.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    StartRecording,
    Approve(String),
    Deny(String),
    StopRecording,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub phase: Phase,
    pub daemon: DaemonState,
    pub capture: CaptureState,
    pub sources: SourceList,
    pub profile: Profile,
    /// The requests the daemon holds for the person, oldest first: ours (see `own_request`) and other programs'
    /// (an agent, a script). The daemon hands us every code because we are its consent handler, so the person must
    /// be able to answer them all here (nobody else can).
    pub requests: Vec<ConsentRequest>,
    /// The id the daemon gave our own `record.start` when it held it for consent.
    pub own_request: Option<String>,
    /// The request the person allowed, until the daemon confirms.
    pub approving: Option<ConsentRequest>,
    /// When we started waiting for an answer to our own request (the first tick that saw it).
    pub awaiting_since: Option<Instant>,
    /// Set when we are not the daemon's consent handler: the approval is on the daemon's terminal.
    pub handler_problem: Option<String>,
    pub stats: Stats,
    pub started_at: Option<Instant>,
    /// A one-line result of the last action (saved file, denied, expired...), shown until the next start.
    pub notice: Option<String>,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            phase: Phase::Idle,
            daemon: DaemonState::Connecting,
            capture: CaptureState::Unknown,
            sources: SourceList::default(),
            profile: Profile::Work,
            requests: Vec::new(),
            own_request: None,
            approving: None,
            awaiting_since: None,
            handler_problem: None,
            stats: Stats::default(),
            started_at: None,
            notice: None,
        }
    }
}

/// `01:02:03` from seconds (hours shown once there is one).
pub fn format_timer(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

impl Model {
    pub fn is_recording(&self) -> bool {
        self.phase == Phase::Recording
    }

    /// The person can change what is recorded only while nothing is starting or running.
    pub fn can_configure(&self) -> bool {
        matches!(self.phase, Phase::Idle | Phase::Error(_))
    }

    /// The Record button can be pressed (the daemon is up and capture works).
    pub fn can_start(&self) -> bool {
        self.can_configure() && self.daemon == DaemonState::Ready && self.capture == CaptureState::Available
    }

    /// The recording's elapsed time as the indicator shows it.
    pub fn elapsed_seconds(&self, now: Instant) -> u64 {
        match (&self.phase, self.started_at) {
            (Phase::Recording | Phase::Stopping, Some(t)) => now.saturating_duration_since(t).as_secs(),
            _ => 0,
        }
    }

    /// The request the dialog shows, and whether it is our own start (true) or another program's (false). Ours only
    /// when its id is the one the daemon gave our `record.start`; everything else is labelled as another program's,
    /// with the daemon's exact lines. Nothing is shown while our own start is in flight (its id is not known yet), or
    /// while recording.
    pub fn consent_dialog(&self) -> Option<(&ConsentRequest, bool)> {
        if matches!(self.phase, Phase::Starting | Phase::Recording | Phase::Stopping) {
            return None;
        }
        let own = self.own_request.as_deref().filter(|_| self.phase == Phase::AwaitingConsent);
        own.and_then(|id| self.requests.iter().find(|r| r.request_id == id)).map(|r| (r, true)).or_else(|| self.requests.first().map(|r| (r, false)))
    }

    fn drop_request(&mut self, id: &str) {
        self.requests.retain(|r| r.request_id != id);
    }

    /// What the preview area says. There is no live picture yet (the engine records but does not stream frames to
    /// the window), so this is always text, and it says why capture works or does not.
    pub fn preview(&self) -> Preview {
        let (title, body) = match (&self.daemon, &self.capture, &self.phase) {
            (_, _, Phase::AwaitingConsent) => (tr("Waiting for your permission"), tr("Nothing is recorded until you allow it.").to_string()),
            (_, _, Phase::Recording | Phase::Stopping) => {
                let size = if self.stats.width > 0 { format!("{}×{}", self.stats.width, self.stats.height) } else { String::from("…") };
                (
                    tr("Recording"),
                    trf(
                        "{size} · {encoder}. The picture is not shown here yet: the engine records but does not send frames to this window.",
                        &[("size", size), ("encoder", if self.stats.encoder.is_empty() { "…".to_string() } else { self.stats.encoder.clone() })],
                    ),
                )
            }
            (DaemonState::Connecting, _, _) => (tr("Starting the recording service…"), String::new()),
            (DaemonState::Unavailable(why), _, _) => (tr("The recording service is not available"), why.clone()),
            (_, CaptureState::Unknown, _) => (tr("Checking whether this session can be captured…"), String::new()),
            (_, CaptureState::Unavailable(why), _) => (
                tr("Capture is not available in this session"),
                trf(
                    "{why}\n\nKalango Cast captures through the compositor's screen-cast, so it needs a KalangoOS (niri) session.",
                    &[("why", why.clone())],
                ),
            ),
            (_, CaptureState::Available, _) if self.sources.is_empty() => {
                (tr("No screen or window was found to record"), tr("Nothing is offered by the compositor right now.").to_string())
            }
            (_, CaptureState::Available, _) => (
                tr("Capture is available"),
                tr("No live picture yet: the engine records but does not send frames to this window. Pick a source and press Record.").to_string(),
            ),
        };
        Preview { title: title.to_string(), body }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::DaemonReady => self.daemon = DaemonState::Ready,
            Msg::DaemonUnavailable(why) => {
                self.daemon = DaemonState::Unavailable(why.clone());
                if matches!(self.phase, Phase::Recording | Phase::Starting | Phase::AwaitingConsent | Phase::Stopping) {
                    self.fail(trf("The recording service stopped answering: {why}", &[("why", why)]));
                }
            }
            Msg::Sources(Ok(list)) => {
                // the service answered: it is there
                self.daemon = DaemonState::Ready;
                self.capture = CaptureState::Available;
                self.sources = list;
            }
            Msg::Sources(Err(why)) => {
                self.capture = CaptureState::Unavailable(why);
                self.sources = SourceList::default();
            }
            Msg::SelectProfile(p) => {
                if self.can_configure() {
                    self.profile = p;
                }
            }
            Msg::StartClicked => {
                if self.can_start() {
                    self.phase = Phase::Starting;
                    self.notice = None;
                    self.own_request = None;
                    return vec![Effect::StartRecording];
                }
            }
            Msg::Started { at, status } => {
                if self.phase == Phase::Starting {
                    self.phase = Phase::Recording;
                    self.started_at = Some(at);
                    self.own_request = None;
                    self.stats = Stats::from_json(&status);
                    // the sound was asked for and could not start: say so while the video records
                    self.notice = status["audio_error"].as_str().map(str::to_string);
                }
            }
            Msg::StartFailed { code, message, request_id } => {
                if self.phase == Phase::Starting {
                    if code == CONSENT_REQUIRED {
                        self.phase = Phase::AwaitingConsent;
                        self.own_request = request_id;
                        self.awaiting_since = None;
                    } else {
                        self.fail(message);
                    }
                }
            }
            Msg::ConsentRequested(req) => {
                // a recording is running: the daemon refuses a second one before it asks anyone
                if !matches!(self.phase, Phase::Recording | Phase::Stopping) {
                    self.drop_request(&req.request_id);
                    self.requests.push(req);
                }
            }
            Msg::ConsentSuperseded { request_id } => {
                self.drop_request(&request_id);
                if self.phase == Phase::AwaitingConsent && self.own_request.as_deref() == Some(request_id.as_str()) {
                    self.phase = Phase::Idle;
                    self.own_request = None;
                    self.notice =
                        Some(tr("Your request was replaced by a different one before you answered: press Record to ask again.").to_string());
                }
            }
            Msg::HandlerUnavailable(why) => self.handler_problem = Some(why),
            Msg::HandlerRegistered => self.handler_problem = None,
            Msg::Allow => {
                if let Some((req, _)) = self.consent_dialog() {
                    let req = req.clone();
                    let token = req.token.clone();
                    // our own start: the same record.start is re-sent when the daemon confirms the approval
                    self.approving = Some(req);
                    return vec![Effect::Approve(token)];
                }
            }
            Msg::Approved => {
                if let Some(req) = self.approving.take() {
                    self.drop_request(&req.request_id);
                    if self.phase == Phase::AwaitingConsent && self.own_request.as_deref() == Some(req.request_id.as_str()) {
                        self.phase = Phase::Starting;
                        self.own_request = None;
                        return vec![Effect::StartRecording];
                    }
                    // the other program's request is approved; it starts it, not us
                    self.notice = Some(tr("Allowed: the program that asked can start recording now.").to_string());
                }
            }
            Msg::ApproveFailed(why) => {
                if let Some(req) = self.approving.take() {
                    self.drop_request(&req.request_id);
                    if self.phase == Phase::AwaitingConsent && self.own_request.as_deref() == Some(req.request_id.as_str()) {
                        self.fail(why);
                    } else {
                        self.notice = Some(why);
                    }
                }
            }
            Msg::Deny => {
                if let Some((req, own)) = self.consent_dialog() {
                    let req = req.clone();
                    self.drop_request(&req.request_id);
                    if own {
                        self.phase = Phase::Idle;
                        self.own_request = None;
                    }
                    self.notice = Some(tr("You did not allow the recording: nothing was recorded.").to_string());
                    return vec![Effect::Deny(req.token)];
                }
            }
            Msg::Tick(now) => {
                self.requests.retain(|r| !r.expired(now));
                if self.phase == Phase::AwaitingConsent {
                    let since = *self.awaiting_since.get_or_insert(now);
                    let waited = now.saturating_duration_since(since);
                    let gone = match &self.own_request {
                        // our request was held and has now expired or been answered elsewhere (it may still be
                        // on its way for a moment after the reply)
                        Some(id) => waited >= Duration::from_secs(5) && !self.requests.iter().any(|r| &r.request_id == id),
                        // we never learned which request is ours: give up when it would have expired anyway
                        None => waited >= Duration::from_secs(130),
                    };
                    if gone {
                        self.phase = Phase::Idle;
                        self.own_request = None;
                        self.awaiting_since = None;
                        self.notice = Some(tr("The request expired without an answer: nothing was recorded.").to_string());
                    }
                }
            }
            Msg::StopClicked => {
                if self.phase == Phase::Recording {
                    self.phase = Phase::Stopping;
                    return vec![Effect::StopRecording];
                }
            }
            Msg::Stopped { path, duration_s } => {
                if matches!(self.phase, Phase::Stopping | Phase::Recording) {
                    self.phase = Phase::Idle;
                    self.started_at = None;
                    self.notice = Some(trf("Saved {path} ({time})", &[("path", path), ("time", format_timer(duration_s as u64))]));
                }
            }
            Msg::StopFailed(why) => {
                if self.phase == Phase::Stopping {
                    self.fail(why);
                }
            }
            Msg::Status(v) => {
                if self.phase == Phase::Recording {
                    if let Some(err) = v["error"].as_str() {
                        self.fail(err.to_string());
                    } else if v["recording"].as_bool() == Some(false) {
                        self.phase = Phase::Idle;
                        self.started_at = None;
                        self.notice = Some(tr("The recording ended outside this app.").to_string());
                    } else {
                        self.stats = Stats::from_json(&v);
                    }
                }
            }
            Msg::RecordingFailed { path, error } => {
                if matches!(self.phase, Phase::Recording | Phase::Stopping | Phase::Starting) {
                    self.fail(trf("Recording failed ({path}): {error}", &[("path", path), ("error", error)]));
                }
            }
            Msg::DismissError => {
                if matches!(self.phase, Phase::Error(_)) {
                    self.phase = Phase::Idle;
                }
            }
        }
        Vec::new()
    }

    fn fail(&mut self, why: String) {
        self.phase = Phase::Error(why);
        self.started_at = None;
        self.own_request = None;
        self.approving = None;
    }
}

/// The most of the service's approval text the dialog will lay out (the service cuts every field itself; this is the
/// window not trusting that, for a daemon that is not ours).
pub const MAX_WHAT_CHARS: usize = 4000;

/// `what`, cut to [`MAX_WHAT_CHARS`] with a mark when it was cut.
pub fn clip_what(what: &str) -> String {
    if what.chars().nth(MAX_WHAT_CHARS).is_none() {
        return what.to_string();
    }
    let mut out: String = what.chars().take(MAX_WHAT_CHARS).collect();
    out.push('…');
    out
}

/// The two lines of a window's row in the source list: the compositor's words, shown without anything that could
/// hide or reorder text.
pub fn window_row(win: &WindowInfo) -> (String, String) {
    let (title, app) = (cast_core::text::escape_unsafe(&win.title), cast_core::text::escape_unsafe(&win.app_id));
    let title = if title.is_empty() { tr("(untitled window)").to_string() } else { title };
    let sub = if app.is_empty() { tr("Window").to_string() } else { trf("Window · {app}", &[("app", app)]) };
    (title, sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Model {
        let mut m = Model::default();
        m.update(Msg::DaemonReady);
        m.update(Msg::Sources(Ok(SourceList::default())));
        m
    }

    fn req(id: &str, token: &str, now: Instant) -> ConsentRequest {
        ConsentRequest {
            request_id: id.into(),
            what: format!("source: x\nfile: {id:?}"),
            token: token.into(),
            received: now,
            ttl: Duration::from_secs(120),
        }
    }

    fn consent_failed(id: Option<&str>) -> Msg {
        Msg::StartFailed { code: CONSENT_REQUIRED, message: "consent_required".into(), request_id: id.map(str::to_string) }
    }

    #[test]
    fn idle_to_consent_to_recording_to_stopping_to_idle() {
        let t0 = Instant::now();
        let mut m = ready();
        assert_eq!(m.update(Msg::StartClicked), vec![Effect::StartRecording]);
        assert_eq!(m.phase, Phase::Starting);
        // the daemon's push can come before the reply...
        m.update(Msg::ConsentRequested(req("mine", "t0k3n", t0)));
        assert!(m.consent_dialog().is_none(), "nothing is shown until our own start's reply says which request is ours");
        m.update(consent_failed(Some("mine")));
        assert_eq!(m.phase, Phase::AwaitingConsent);
        let (r, own) = m.consent_dialog().unwrap();
        assert!(own, "the id matches the one the daemon gave our record.start");
        assert_eq!(r.token, "t0k3n");
        assert!(!m.can_start(), "no second start while waiting");
        assert_eq!(m.update(Msg::Allow), vec![Effect::Approve("t0k3n".into())]);
        assert_eq!(m.update(Msg::Approved), vec![Effect::StartRecording]);
        assert_eq!(m.phase, Phase::Starting);
        assert!(m.consent_dialog().is_none());
        m.update(Msg::Started { at: t0, status: json!({"fps": 59.9, "encoder": "nvenc", "dropped": 2, "width": 1920, "height": 1080}) });
        assert!(m.is_recording());
        assert_eq!(m.stats.encoder, "nvenc");
        assert_eq!(m.elapsed_seconds(t0 + Duration::from_secs(75)), 75);
        assert!(!m.can_configure(), "the profile is locked while recording");
        m.update(Msg::SelectProfile(Profile::Gamer));
        assert_eq!(m.profile, Profile::Work);
        assert_eq!(m.update(Msg::StopClicked), vec![Effect::StopRecording]);
        assert_eq!(m.phase, Phase::Stopping);
        m.update(Msg::Stopped { path: "/home/a/Videos/x.mp4".into(), duration_s: 75.4 });
        assert_eq!(m.phase, Phase::Idle);
        assert_eq!(m.elapsed_seconds(t0 + Duration::from_secs(99)), 0);
        assert!(m.notice.as_deref().unwrap().contains("x.mp4"));
    }

    #[test]
    fn the_push_after_the_reply_is_still_ours_by_id() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(consent_failed(Some("mine")));
        assert!(m.consent_dialog().is_none(), "waiting for the description");
        m.update(Msg::ConsentRequested(req("mine", "a", t0)));
        assert!(m.consent_dialog().unwrap().1);
    }

    #[test]
    fn another_programs_request_arriving_during_our_start_is_labelled_external() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        // someone else's request is pushed while our start is in flight, and it is not the one our reply names
        m.update(Msg::ConsentRequested(req("theirs", "x", t0)));
        m.update(consent_failed(Some("mine")));
        let (r, own) = m.consent_dialog().unwrap();
        assert_eq!(r.request_id, "theirs");
        assert!(!own, "never shown as ours: the person must not approve it believing the app asked");
        // approving it does not start our recording
        assert_eq!(m.update(Msg::Allow), vec![Effect::Approve("x".into())]);
        assert!(m.update(Msg::Approved).is_empty());
        assert_eq!(m.phase, Phase::AwaitingConsent, "ours is still waiting");
        assert!(m.notice.is_some());
        // ours arrives and is ours
        m.update(Msg::ConsentRequested(req("mine", "m", t0)));
        assert!(m.consent_dialog().unwrap().1);
        // with the two held, ours is shown first
        m.update(Msg::ConsentRequested(req("theirs2", "y", t0)));
        assert_eq!(m.consent_dialog().unwrap().0.request_id, "mine");
    }

    #[test]
    fn a_reply_without_an_id_never_makes_a_request_ours() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::ConsentRequested(req("r", "t", t0)));
        m.update(consent_failed(None));
        assert!(!m.consent_dialog().unwrap().1);
    }

    #[test]
    fn another_programs_request_is_answered_here_without_starting_our_own_recording() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::ConsentRequested(req("agent", "t0k3n", t0)));
        let (r, own) = m.consent_dialog().expect("the dialog is up");
        assert!(!own);
        assert_eq!(r.token, "t0k3n");
        assert_eq!(m.update(Msg::Allow), vec![Effect::Approve("t0k3n".into())]);
        assert!(m.update(Msg::Approved).is_empty(), "approving another program's request does not start our recording");
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.consent_dialog().is_none());
        assert!(m.notice.is_some());
        m.update(Msg::ConsentRequested(req("agent2", "t2", t0)));
        assert_eq!(m.update(Msg::Deny), vec![Effect::Deny("t2".into())]);
        assert!(m.consent_dialog().is_none());
        m.update(Msg::ConsentRequested(req("agent3", "t3", t0)));
        m.update(Msg::Tick(t0 + Duration::from_secs(121)));
        assert!(m.consent_dialog().is_none(), "an unanswered one goes away");
        let mut r = ready();
        r.update(Msg::StartClicked);
        r.update(Msg::Started { at: t0, status: json!({}) });
        r.update(Msg::ConsentRequested(req("late", "t", t0)));
        assert!(r.consent_dialog().is_none(), "while recording nothing is shown");
    }

    #[test]
    fn a_replaced_request_disappears_and_ours_going_away_is_said() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::ConsentRequested(req("a", "ta", t0)));
        m.update(Msg::ConsentSuperseded { request_id: "a".into() });
        assert!(m.consent_dialog().is_none());
        m.update(Msg::ConsentRequested(req("b", "tb", t0)));
        assert_eq!(m.consent_dialog().unwrap().0.token, "tb");
        // ours is replaced by another program's request while we wait
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::ConsentRequested(req("mine", "tm", t0)));
        m.update(consent_failed(Some("mine")));
        assert!(m.consent_dialog().unwrap().1);
        m.update(Msg::ConsentSuperseded { request_id: "mine".into() });
        m.update(Msg::ConsentRequested(req("theirs", "tt", t0)));
        assert_eq!(m.phase, Phase::Idle, "our start is over: it must be asked again");
        assert!(m.notice.is_some());
        let (r, own) = m.consent_dialog().unwrap();
        assert_eq!(r.request_id, "theirs");
        assert!(!own, "the replacement is shown as another program's");
        assert!(m.can_start());
    }

    #[test]
    fn denying_records_nothing_and_tells_the_daemon() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::ConsentRequested(req("mine", "t0k3n", t0)));
        m.update(consent_failed(Some("mine")));
        assert_eq!(m.update(Msg::Deny), vec![Effect::Deny("t0k3n".into())]);
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.notice.is_some());
        assert!(m.can_start());
        assert!(m.update(Msg::Allow).is_empty());
        assert!(m.update(Msg::Deny).is_empty());
    }

    #[test]
    fn our_unanswered_request_expires_back_to_idle() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::ConsentRequested(req("mine", "t", t0)));
        m.update(consent_failed(Some("mine")));
        m.update(Msg::Tick(t0 + Duration::from_secs(1)));
        m.update(Msg::Tick(t0 + Duration::from_secs(60)));
        assert_eq!(m.phase, Phase::AwaitingConsent);
        assert_eq!(m.consent_dialog().unwrap().0.seconds_left(t0 + Duration::from_secs(60)), 60);
        m.update(Msg::Tick(t0 + Duration::from_secs(121)));
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.requests.is_empty());
        // and one that never got a description (no handler) does not lock the button forever
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(consent_failed(None));
        m.update(Msg::Tick(t0));
        m.update(Msg::Tick(t0 + Duration::from_secs(131)));
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn errors_are_shown_and_can_be_dismissed() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::StartFailed { code: -32000, message: "folder not yours".into(), request_id: None });
        assert_eq!(m.phase, Phase::Error("folder not yours".into()));
        assert!(m.can_start(), "trying again is allowed from an error");
        m.update(Msg::DismissError);
        assert_eq!(m.phase, Phase::Idle);
        m.update(Msg::StartClicked);
        m.update(Msg::ConsentRequested(req("mine", "t", t0)));
        m.update(consent_failed(Some("mine")));
        m.update(Msg::Allow);
        m.update(Msg::ApproveFailed("Invalid or expired approval code".into()));
        assert_eq!(m.phase, Phase::Error("Invalid or expired approval code".into()));
        assert!(m.requests.is_empty());
    }

    #[test]
    fn the_record_button_needs_the_daemon_and_a_working_capture() {
        let mut m = Model::default();
        assert!(!m.can_start());
        assert!(m.update(Msg::StartClicked).is_empty(), "nothing happens while the service is not there");
        assert_eq!(m.phase, Phase::Idle);
        m.update(Msg::DaemonReady);
        assert!(!m.can_start(), "the daemon alone is not enough");
        m.update(Msg::Sources(Err("no screencast".into())));
        assert!(!m.can_start());
        assert_eq!(m.capture, CaptureState::Unavailable("no screencast".into()));
        m.update(Msg::Sources(Ok(SourceList::default())));
        assert!(m.can_start());
    }

    #[test]
    fn the_approval_text_is_cut_to_what_fits_on_a_screen() {
        let huge = format!("Mode: x\nFile: {}", "y".repeat(1 << 20));
        let c = clip_what(&huge);
        assert!(c.chars().count() <= MAX_WHAT_CHARS + 1 && c.starts_with("Mode: x"));
        assert_eq!(clip_what("short"), "short");
    }

    #[test]
    fn a_window_title_cannot_hide_text_in_the_list() {
        let w = WindowInfo { id: 1, title: "Pay\u{E0041}\u{061C}ment".into(), app_id: "app\u{202E}".into() };
        let (title, sub) = window_row(&w);
        assert_eq!(title, "Pay\\u{e0041}\\u{61c}ment");
        assert!(sub.contains("app\\u{202e}"), "{sub}");
        assert!(!title.chars().chain(sub.chars()).any(cast_core::text::is_unsafe_char));
    }

    #[test]
    fn a_sound_that_could_not_start_is_shown_while_the_video_records() {
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::Started { at: Instant::now(), status: json!({"audio_error": "sound not recorded: no PipeWire"}) });
        assert_eq!(m.phase, Phase::Recording);
        assert_eq!(m.notice.as_deref(), Some("sound not recorded: no PipeWire"));
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::Started { at: Instant::now(), status: json!({"audio_error": null}) });
        assert!(m.notice.is_none());
    }

    #[test]
    fn a_failing_recording_goes_to_error_from_status_or_from_the_event() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::Started { at: t0, status: json!({}) });
        m.update(Msg::Status(json!({"recording": true, "fps": 30.0, "dropped": 1})));
        assert_eq!(m.stats.dropped, 1);
        m.update(Msg::Status(json!({"recording": true, "error": "disk full"})));
        assert_eq!(m.phase, Phase::Error("disk full".into()));
        assert!(m.started_at.is_none(), "the indicator goes off");
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::Started { at: t0, status: json!({}) });
        m.update(Msg::RecordingFailed { path: "/a.mp4".into(), error: "I/O".into() });
        assert!(matches!(m.phase, Phase::Error(ref e) if e.contains("I/O") && e.contains("/a.mp4")));
        // a stale event while idle changes nothing
        let mut m = ready();
        m.update(Msg::RecordingFailed { path: "/a.mp4".into(), error: "x".into() });
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn losing_the_daemon_while_recording_is_an_error_and_while_idle_blocks_start() {
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::Started { at: Instant::now(), status: json!({}) });
        m.update(Msg::DaemonUnavailable("connection refused".into()));
        assert!(matches!(m.phase, Phase::Error(_)));
        let mut m = ready();
        m.update(Msg::DaemonUnavailable("gone".into()));
        assert_eq!(m.phase, Phase::Idle);
        assert!(!m.can_start());
    }

    #[test]
    fn a_status_that_says_nothing_records_ends_the_recording_without_an_error() {
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::Started { at: Instant::now(), status: json!({}) });
        m.update(Msg::Status(json!({"recording": false})));
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.notice.is_some());
    }

    #[test]
    fn stopping_failures_are_errors() {
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::Started { at: Instant::now(), status: json!({}) });
        m.update(Msg::StopClicked);
        m.update(Msg::StopFailed("could not finish the file".into()));
        assert_eq!(m.phase, Phase::Error("could not finish the file".into()));
    }

    #[test]
    fn profiles_only_set_engine_settings_and_the_params_are_what_the_daemon_accepts() {
        let cfg = |p: Profile| StartConfig {
            source: Source::Monitor { connector: "DP-1".into() },
            path: "/home/x/Videos/a.mp4".into(),
            preset: p.preset(),
        };
        assert_eq!(Profile::Gamer.preset().fps, 60);
        assert!(!Profile::Podcast.preset().desktop_audio && Profile::Podcast.preset().mic);
        for p in Profile::ALL {
            let o = cast_engine::rpc::parse_start(&cfg(p).to_params()).unwrap_or_else(|e| panic!("{p:?}: {e:?}"));
            assert_eq!(o.fps, p.preset().fps);
            assert_eq!(o.bitrate_kbps, p.preset().bitrate_kbps);
            assert_eq!(o.desktop_audio == Some(cast_engine::AudioChoice::Default), p.preset().desktop_audio);
            assert_eq!(o.mic == Some(cast_engine::AudioChoice::Default), p.preset().mic);
        }
        let w = StartConfig { source: Source::Window { id: 7 }, ..cfg(Profile::Work) };
        assert!(cast_engine::rpc::parse_start(&w.to_params()).is_ok());
    }

    #[test]
    fn sources_and_stats_are_read_from_the_daemons_json() {
        let l = SourceList::from_json(
            &json!({"monitors": [{"connector": "DP-1", "name": "Dell", "width": 1920, "height": 1080, "refresh_hz": 60.0}], "windows": [{"id": 3, "title": "Editor", "app_id": "gedit"}, {"nope": 1}]}),
        );
        assert_eq!(l.monitors.len(), 1);
        assert_eq!(l.windows, vec![WindowInfo { id: 3, title: "Editor".into(), app_id: "gedit".into() }]);
        assert!(SourceList::from_json(&json!({})).is_empty());
        let s = Stats::from_json(&json!({"fps": 29.5, "cpu_percent": 4.0, "gpu_percent": 31, "zero_copy": true}));
        assert_eq!((s.fps, s.gpu_percent, s.zero_copy), (29.5, Some(31), true));
    }

    #[test]
    fn the_preview_never_pretends_a_picture_exists_and_says_why_capture_fails() {
        let mut m = Model::default();
        assert!(m.preview().title.contains("Starting") || m.preview().title.contains("Iniciando"));
        m.update(Msg::DaemonReady);
        m.update(Msg::Sources(Err("no ScreenCast service on the bus".into())));
        let p = m.preview();
        assert!(p.body.contains("no ScreenCast service on the bus"), "the real reason is shown: {p:?}");
        assert!(!p.body.is_empty());
        m.update(Msg::Sources(Ok(SourceList::default())));
        assert_ne!(m.preview().body, "", "an empty list is said, not hidden");
        m.update(Msg::Sources(Ok(SourceList {
            monitors: vec![MonitorInfo { connector: "DP-1".into(), name: String::new(), width: 1, height: 1, refresh_hz: 60.0 }],
            windows: vec![],
        })));
        m.update(Msg::StartClicked);
        m.update(Msg::Started { at: Instant::now(), status: json!({"width": 1920, "height": 1080, "encoder": "nvenc"}) });
        let p = m.preview();
        assert!(p.body.contains("1920×1080") && p.body.contains("nvenc"), "{p:?}");
        let mut gone = Model::default();
        gone.update(Msg::DaemonUnavailable("connection refused".into()));
        assert!(gone.preview().body.contains("connection refused"));
    }

    #[test]
    fn a_working_sources_call_means_the_daemon_is_there() {
        let mut m = Model::default();
        m.update(Msg::DaemonUnavailable("x".into()));
        m.update(Msg::Sources(Ok(SourceList::default())));
        assert_eq!(m.daemon, DaemonState::Ready);
    }

    #[test]
    fn the_timer_format() {
        assert_eq!(format_timer(0), "00:00");
        assert_eq!(format_timer(75), "01:15");
        assert_eq!(format_timer(3725), "01:02:05");
    }
}
