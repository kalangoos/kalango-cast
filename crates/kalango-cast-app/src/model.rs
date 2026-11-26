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
    },
    /// The daemon pushed a consent request to us, its registered handler.
    ConsentRequested(ConsentRequest),
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
    pub consent: Option<ConsentRequest>,
    /// A request another program made (an agent, a script): the daemon handed us the code because we are its
    /// consent handler, so the person must be able to answer it here (nobody else can).
    pub external: Option<ConsentRequest>,
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
            consent: None,
            external: None,
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

    /// The consent dialog is up: waiting for the person, with the held request known. The flag says whether it is
    /// our own start (true) or another program's request (false).
    pub fn consent_dialog(&self) -> Option<(&ConsentRequest, bool)> {
        if self.phase == Phase::AwaitingConsent {
            self.consent.as_ref().map(|c| (c, true))
        } else {
            self.external.as_ref().map(|c| (c, false))
        }
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
                    self.consent = None;
                    return vec![Effect::StartRecording];
                }
            }
            Msg::Started { at, status } => {
                if self.phase == Phase::Starting {
                    self.phase = Phase::Recording;
                    self.started_at = Some(at);
                    self.consent = None;
                    self.stats = Stats::from_json(&status);
                }
            }
            Msg::StartFailed { code, message } => {
                if self.phase == Phase::Starting {
                    if code == CONSENT_REQUIRED {
                        self.phase = Phase::AwaitingConsent;
                    } else {
                        self.fail(message);
                    }
                }
            }
            Msg::ConsentRequested(req) => match self.phase {
                Phase::Starting | Phase::AwaitingConsent => self.consent = Some(req),
                Phase::Idle | Phase::Error(_) => self.external = Some(req),
                // a recording is running: the daemon refuses a second one before it asks anyone
                Phase::Recording | Phase::Stopping => {}
            },
            Msg::HandlerUnavailable(why) => self.handler_problem = Some(why),
            Msg::HandlerRegistered => self.handler_problem = None,
            Msg::Allow => {
                // our own start: the same record.start is re-sent when the daemon confirms the approval
                if let Some((req, _)) = self.consent_dialog() {
                    return vec![Effect::Approve(req.token.clone())];
                }
            }
            Msg::Approved => {
                if self.phase == Phase::AwaitingConsent {
                    self.phase = Phase::Starting;
                    self.consent = None;
                    return vec![Effect::StartRecording];
                } else if self.external.take().is_some() {
                    // the other program's request is approved; it starts it, not us
                    self.notice = Some(tr("Allowed: the program that asked can start recording now.").to_string());
                }
            }
            Msg::ApproveFailed(why) => {
                if self.phase == Phase::AwaitingConsent {
                    self.fail(why);
                } else if self.external.take().is_some() {
                    self.notice = Some(why);
                }
            }
            Msg::Deny => {
                if self.phase == Phase::AwaitingConsent {
                    if let Some(req) = self.consent.take() {
                        self.phase = Phase::Idle;
                        self.notice = Some(tr("You did not allow the recording: nothing was recorded.").to_string());
                        return vec![Effect::Deny(req.token)];
                    }
                } else if let Some(req) = self.external.take() {
                    self.notice = Some(tr("You did not allow the recording: nothing was recorded.").to_string());
                    return vec![Effect::Deny(req.token)];
                }
            }
            Msg::Tick(now) => {
                if self.external.as_ref().is_some_and(|c| c.expired(now)) {
                    self.external = None;
                }
                if self.phase == Phase::AwaitingConsent && self.consent.as_ref().is_some_and(|c| c.expired(now)) {
                    self.consent = None;
                    self.phase = Phase::Idle;
                    self.notice = Some(tr("The request expired without an answer: nothing was recorded.").to_string());
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
        self.consent = None;
    }
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

    fn request(now: Instant) -> ConsentRequest {
        ConsentRequest {
            what: "source: Monitor { connector: \"DP-1\" }\nfile: \"a.mp4\"".into(),
            token: "t0k3n".into(),
            received: now,
            ttl: Duration::from_secs(120),
        }
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
    fn idle_to_consent_to_recording_to_stopping_to_idle() {
        let t0 = Instant::now();
        let mut m = ready();
        assert_eq!(m.update(Msg::StartClicked), vec![Effect::StartRecording]);
        assert_eq!(m.phase, Phase::Starting);
        // the daemon's push can come before the reply...
        m.update(Msg::ConsentRequested(request(t0)));
        assert!(m.consent_dialog().is_none(), "no dialog until the daemon has said consent_required");
        m.update(Msg::StartFailed { code: CONSENT_REQUIRED, message: "consent_required".into() });
        assert_eq!(m.phase, Phase::AwaitingConsent);
        assert_eq!(m.consent_dialog().unwrap().0.token, "t0k3n");
        assert!(!m.can_start(), "no second start while waiting");
        // allow: approve first, then the same start again
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
    fn another_programs_request_is_answered_here_without_starting_our_own_recording() {
        let t0 = Instant::now();
        let mut m = ready();
        // an agent asked the daemon to record; the daemon gave us the code (we are the handler), nobody else can answer
        m.update(Msg::ConsentRequested(request(t0)));
        let (req, own) = m.consent_dialog().expect("the dialog is up");
        assert!(!own);
        assert_eq!(req.token, "t0k3n");
        assert_eq!(m.update(Msg::Allow), vec![Effect::Approve("t0k3n".into())]);
        assert!(m.update(Msg::Approved).is_empty(), "approving another program's request does not start our recording");
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.consent_dialog().is_none());
        assert!(m.notice.is_some());
        // denying
        m.update(Msg::ConsentRequested(request(t0)));
        assert_eq!(m.update(Msg::Deny), vec![Effect::Deny("t0k3n".into())]);
        assert!(m.consent_dialog().is_none());
        // an unanswered one goes away
        m.update(Msg::ConsentRequested(request(t0)));
        m.update(Msg::Tick(t0 + Duration::from_secs(121)));
        assert!(m.consent_dialog().is_none());
        // while recording nothing is shown
        let mut r = ready();
        r.update(Msg::StartClicked);
        r.update(Msg::Started { at: t0, status: json!({}) });
        r.update(Msg::ConsentRequested(request(t0)));
        assert!(r.consent_dialog().is_none());
    }

    #[test]
    fn the_daemons_push_may_also_come_after_the_reply() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::StartFailed { code: CONSENT_REQUIRED, message: "consent_required".into() });
        assert_eq!(m.phase, Phase::AwaitingConsent);
        assert!(m.consent_dialog().is_none(), "waiting for the description");
        m.update(Msg::ConsentRequested(request(t0)));
        assert!(m.consent_dialog().is_some());
    }

    #[test]
    fn denying_records_nothing_and_tells_the_daemon() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::ConsentRequested(request(t0)));
        m.update(Msg::StartFailed { code: CONSENT_REQUIRED, message: String::new() });
        assert_eq!(m.update(Msg::Deny), vec![Effect::Deny("t0k3n".into())]);
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.notice.is_some());
        assert!(m.can_start());
        // Allow/Deny without a request do nothing
        assert!(m.update(Msg::Allow).is_empty());
        assert!(m.update(Msg::Deny).is_empty());
    }

    #[test]
    fn an_unanswered_request_expires_back_to_idle() {
        let t0 = Instant::now();
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::ConsentRequested(request(t0)));
        m.update(Msg::StartFailed { code: CONSENT_REQUIRED, message: String::new() });
        m.update(Msg::Tick(t0 + Duration::from_secs(60)));
        assert_eq!(m.phase, Phase::AwaitingConsent);
        assert_eq!(m.consent_dialog().unwrap().0.seconds_left(t0 + Duration::from_secs(60)), 60);
        m.update(Msg::Tick(t0 + Duration::from_secs(121)));
        assert_eq!(m.phase, Phase::Idle);
        assert!(m.consent.is_none());
    }

    #[test]
    fn errors_are_shown_and_can_be_dismissed() {
        let mut m = ready();
        m.update(Msg::StartClicked);
        m.update(Msg::StartFailed { code: -32000, message: "folder not yours".into() });
        assert_eq!(m.phase, Phase::Error("folder not yours".into()));
        assert!(m.can_start(), "trying again is allowed from an error");
        m.update(Msg::DismissError);
        assert_eq!(m.phase, Phase::Idle);
        // an approval the daemon refuses is an error, not a silent start
        m.update(Msg::StartClicked);
        m.update(Msg::StartFailed { code: CONSENT_REQUIRED, message: String::new() });
        m.update(Msg::ConsentRequested(request(Instant::now())));
        m.update(Msg::Allow);
        m.update(Msg::ApproveFailed("Invalid or expired approval code".into()));
        assert_eq!(m.phase, Phase::Error("Invalid or expired approval code".into()));
        assert!(m.consent.is_none());
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
