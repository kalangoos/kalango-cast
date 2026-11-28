//! `kalango-castd`'s control channel: JSON-RPC 2.0, one request per line, over a Unix socket at
//! `$XDG_RUNTIME_DIR/kalango/cast.sock` (mode 0600 in a 0700 folder). Every connection's peer is checked with
//! `SO_PEERCRED`: only processes of the same user are served.
//!
//! Methods: `capabilities`, `sources.list`, `record.start` {source, path, encoder?, codec?, fps?, bitrate_kbps?,
//! desktop_audio?, mic?, overwrite?}, `record.stop`, `record.status`, `consent.approve` {token},
//! `consent.deny` {token}, `consent.handler.register`.
//!
//! **Consent.** Being the same user is not consent: any program of the person's could otherwise start recording the
//! screen and the microphone. With `consent = "ask"` (the default; `$XDG_CONFIG_HOME/kalango/cast.toml`), `record.start`
//! answers error -32001 `consent_required` and gives a one-time approval code only to the daemon's own trusted
//! channel (the [`Hook`], which `kalango-castd` prints on the person's terminal); the person runs
//! `kalango-cast approve CODE`, and the same `record.start` is then accepted once. This stops clients that cannot see
//! the terminal (agents through kalangod, scripts); it does **not** stop a hostile program of the same user that can
//! read the terminal or the daemon's memory.
//!
//! **A UI as the consent handler.** A trusted UI (`kalango-cast-app`) can register its connection with
//! `consent.handler.register` (the peer's uid is checked again: only the daemon's own user). From then on the
//! approval code and the recording's lifecycle are pushed to that connection as JSON-RPC notifications (`method`
//! "event", `params.type` = `consent_requested` {what, token, ttl_s} | `recording_started` | `recording_stopped` |
//! `recording_failed`), the code is **no longer printed on the terminal**, and `consent.approve`/`consent.deny` are
//! accepted **only from that connection**. **Registration needs a secret**: only the app that spawned the daemon has
//! it (it hands it over on a socketpair after the daemon says it is ready, see [`receive_secret`]), and `register`
//! must present it (`params.secret`, constant-time compare). A daemon that was not given a secret (started by systemd
//! or by hand) has **no handler at all**: consent stays on the terminal/journal path. The peer's executable is not a
//! credential and is not looked at. Nothing is handed over, and the terminal notice is not silenced, before
//! authentication succeeds.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cast_core::{tr, trf, Source};
use serde_json::{json, Value};

use crate::recorder::{AudioChoice, FailNotify, RecordOptions, Recorder, StartError};
use crate::stats::Status;

/// The longest request line served (bytes, without the newline).
pub const MAX_LINE: usize = 1 << 20;
/// The most connections served at once.
pub const MAX_CONNECTIONS: usize = 16;
/// How long an approval request, and an approval, stay valid.
const CONSENT_TTL: Duration = Duration::from_secs(120);
/// Wrong approval codes after which the request is cancelled.
const MAX_WRONG_CODES: u32 = 5;

/// Separates the message from the request id inside a `consent_required` error until `handle_from` moves the id into
/// the error's `data`.
const ID_MARK: char = '\u{0}';

/// The error code of `record.start` when the person has not approved.
pub const CONSENT_REQUIRED: i64 = -32001;

/// The socket's default path.
pub fn default_socket() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(dir).join("kalango").join("cast.sock"))
}

/// The uid of a connected peer (`SO_PEERCRED`).
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into the ucred we pass; the fd is a connected Unix socket.
    let r = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) };
    (r == 0).then_some(cred.uid)
}

/// The pid and uid of a connected peer (`SO_PEERCRED`).
pub fn peer_cred(stream: &UnixStream) -> Option<(u32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into the ucred we pass; the fd is a connected Unix socket.
    let r = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) };
    (r == 0).then_some((cred.pid as u32, cred.uid))
}

/// Compares two secrets without stopping at the first different byte (the length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Overwrites a buffer with zeros in a way the compiler may not remove.
pub fn zeroize(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference to a byte.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// The daemon's half of the secret handshake on the socketpair end `fd` the parent left us (`--secret-fd N`): says
/// "ready" with one byte, then reads the secret the parent sends (up to 256 bytes, until it closes its writing side),
/// zeroizes its buffers and closes the descriptor. The caller must already have set `PR_SET_DUMPABLE` to 0: the
/// parent sends the secret only after reading the ready byte, so nothing secret exists before that point.
pub fn receive_secret(fd: i32, timeout: Duration) -> std::io::Result<String> {
    use std::os::fd::FromRawFd;
    // SAFETY: the parent passed this descriptor to us for this one purpose and nothing else in the process uses it;
    // the UnixStream takes ownership and closes it when dropped.
    let mut sock = unsafe { UnixStream::from_raw_fd(fd) };
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    sock.write_all(&[READY])?;
    let mut buf = [0u8; 256];
    let mut len = 0;
    let result = loop {
        match sock.read(&mut buf[len..]) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                len += n;
                if len == buf.len() {
                    break Err(std::io::Error::other("the handler secret is too long"));
                }
            }
            Err(e) => break Err(e),
        }
    };
    let secret = result.and_then(|()| {
        let text = std::str::from_utf8(&buf[..len]).map_err(std::io::Error::other)?.trim().to_string();
        if text.len() < 16 {
            return Err(std::io::Error::other("the handler secret is too short"));
        }
        Ok(text)
    });
    zeroize(&mut buf);
    secret
}

/// The byte the daemon sends when it is ready to receive the secret.
pub const READY: u8 = b'R';

/// Only a peer of the daemon's own user is served (an unknown peer is not).
pub fn peer_allowed(peer: Option<u32>, me: u32) -> bool {
    peer == Some(me)
}

fn my_uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// Creates the folder (0700, ours) and binds the socket (0600). A stale socket is replaced; anything else at that
/// path is left alone and refused; a folder owned by someone else or open to others is refused.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("socket path"))?;
    if !dir.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let meta = std::fs::metadata(dir)?;
    if meta.uid() != my_uid() || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!("{}: {}", dir.display(), tr("the folder must be yours and closed to others (0700)"))));
    }
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(std::io::Error::other(format!("{}: {}", path.display(), tr("exists and is not a socket: not removing it"))));
        }
        // a live daemon answers; a stale socket does not
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::other(tr("kalango-castd is already running")));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Whether a client may start recording without the person approving it first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Consent {
    /// Every `record.start` needs a one-time approval (the default).
    #[default]
    Ask,
    /// Any client of the same user may record (the person chose this).
    Allow,
}

/// `consent = "ask" | "allow"` from a config file's text; anything else (or nothing) is `Ask`.
pub fn consent_from_config(text: &str) -> Consent {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "consent" {
                return match v.trim().trim_matches('"') {
                    "allow" => Consent::Allow,
                    _ => Consent::Ask,
                };
            }
        }
    }
    Consent::Ask
}

/// The person's choice, from `$XDG_CONFIG_HOME/kalango/cast.toml` (`~/.config/...`); `Ask` when absent.
pub fn load_consent() -> Consent {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.and_then(|b| std::fs::read_to_string(b.join("kalango").join("cast.toml")).ok()).map(|t| consent_from_config(&t)).unwrap_or_default()
}

/// What the daemon tells its trusted channel (the [`Hook`]): the approval code (never sent to clients), and the
/// recording's lifecycle, for an indicator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A request is held for approval. `request_id` names it (not secret): the client whose `record.start` created it
    /// is told the same id in the `consent_required` error's `data`, so a UI can tell its own request from another's.
    ConsentRequested {
        what: String,
        token: String,
        request_id: String,
    },
    /// A held request was replaced by a different one before it was answered (its code is dead).
    ConsentSuperseded {
        request_id: String,
    },
    RecordingStarted {
        path: String,
    },
    RecordingStopped {
        path: String,
    },
    RecordingFailed {
        path: String,
        error: String,
    },
}

/// A callback for [`Event`]s. `kalango-castd` prints them on its terminal; a shell can show an indicator.
pub type Hook = Box<dyn Fn(&Event) + Send + Sync>;

/// Limits of the control channel.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub line: usize,
    pub connections: usize,
    /// How long an approval request, and an approval, stay valid.
    pub ttl: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { line: MAX_LINE, connections: MAX_CONNECTIONS, ttl: CONSENT_TTL }
    }
}

struct Pending {
    token: String,
    /// The request's public name (see [`Event::ConsentRequested`]).
    id: String,
    /// What the person was asked to approve (kept to hand it to a handler that registers later).
    what: String,
    /// The request that was held (its parameters, as text): the approval is for this one only.
    fingerprint: String,
    created: Instant,
    approved: bool,
    wrong: u32,
    /// This approval was given back once already (after a start that failed for the environment): never twice.
    restored: bool,
}

/// What a used-up approval was, to give it back after an environment failure.
#[derive(Clone, Copy)]
struct Approval {
    /// When the person gave it (the first approval's clock; a restore does not renew it).
    created: Instant,
    restored: bool,
}

/// The write side of a connection, shared between its own thread (replies) and the daemon (pushed events).
type SharedOut = Arc<Mutex<UnixStream>>;

/// What a request knows about the connection it came on.
pub struct ConnCtx {
    id: u64,
    uid: Option<u32>,
    out: SharedOut,
}

/// The connection registered as the consent handler.
struct HandlerSlot {
    conn: u64,
    out: SharedOut,
}

/// How long a pushed event may block on a handler that does not read before it is dropped as dead.
const PUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// A recording the daemon holds: a real [`Recorder`], or a stand-in in tests.
pub trait Running: Send {
    fn status(&self) -> Status;
    /// Stops the capture and leaves the writer to `stop` (a recording that failed).
    fn halt_capture(&mut self);
    fn stop(self: Box<Self>) -> Result<(cast_mux::Summary, Status), String>;
}

impl Running for Recorder {
    fn status(&self) -> Status {
        Recorder::status(self)
    }
    fn halt_capture(&mut self) {
        Recorder::halt_capture(self)
    }
    fn stop(self: Box<Self>) -> Result<(cast_mux::Summary, Status), String> {
        Recorder::stop(*self)
    }
}

/// How the daemon starts a recording (the real one by default; tests plug a slow or failing one in).
pub type Starter = Arc<dyn Fn(RecordOptions, FailNotify) -> Result<Box<dyn Running>, StartError> + Send + Sync>;

/// The one recording's state. Starting is a state of its own so the slow part runs without the lock: `record.status`
/// and `record.stop` answer while a start is under way.
#[derive(Default)]
enum Slot {
    #[default]
    Idle,
    Starting {
        id: u64,
        cancel: Arc<AtomicBool>,
        /// A failure that came before the recording was published.
        failed: Option<String>,
    },
    Running {
        id: u64,
        rec: Box<dyn Running>,
        /// The `recording_failed` event went out already.
        notified: bool,
    },
}

/// What the recording's own threads and the trusted channels share (a recording that fails reaches the person from
/// its writer thread, so this lives behind an `Arc`).
struct Core {
    slot: Mutex<Slot>,
    handler: Mutex<Option<HandlerSlot>>,
    hook: Option<Hook>,
    ttl: Duration,
    next_id: AtomicU64,
}

impl Default for Core {
    fn default() -> Self {
        Core { slot: Mutex::new(Slot::Idle), handler: Mutex::new(None), hook: None, ttl: CONSENT_TTL, next_id: AtomicU64::new(0) }
    }
}

impl Core {
    /// Tells the trusted channels: the registered UI first (every event), the hook (the terminal) for everything
    /// except an approval code a UI took.
    fn emit(&self, e: Event) {
        let delivered = self.push_to_handler(&e);
        if delivered && matches!(e, Event::ConsentRequested { .. } | Event::ConsentSuperseded { .. }) {
            return;
        }
        if let Some(h) = &self.hook {
            h(&e);
        }
    }

    /// Sends an event to the consent handler; a handler that cannot take it is dropped (the terminal takes over).
    fn push_to_handler(&self, e: &Event) -> bool {
        let Ok(mut slot) = self.handler.lock() else { return false };
        let Some(h) = slot.as_ref() else { return false };
        let note = json!({"jsonrpc": "2.0", "method": "event", "params": event_json(e, self.ttl)});
        let sent = h.out.lock().map(|mut out| writeln!(out, "{note}").is_ok()).unwrap_or(false);
        if !sent {
            *slot = None;
        }
        sent
    }

    /// Recording `id` failed for good (its writer thread says so): tell the person now, and stop its capture.
    fn recording_failed(self: &Arc<Self>, id: u64, error: String) {
        let path = {
            let Ok(mut slot) = self.slot.lock() else { return };
            match &mut *slot {
                Slot::Starting { id: i, failed, .. } if *i == id => {
                    failed.get_or_insert(error.clone());
                    None
                }
                Slot::Running { id: i, rec, notified } if *i == id && !*notified => {
                    *notified = true;
                    Some(rec.status().path)
                }
                _ => None,
            }
        };
        let Some(path) = path else { return };
        self.emit(Event::RecordingFailed { path, error });
        let core = self.clone();
        // off the writer thread: stopping the capture waits for threads, and must not hold up the failed writer's drain
        let _ = std::thread::Builder::new().name("cast-halt".into()).spawn(move || {
            if let Ok(mut slot) = core.slot.lock() {
                if let Slot::Running { id: i, rec, .. } = &mut *slot {
                    if *i == id {
                        rec.halt_capture();
                    }
                }
            }
        });
    }
}

/// The daemon's state: at most one recording.
#[derive(Default)]
pub struct Daemon {
    core: Arc<Core>,
    starter: Option<Starter>,
    pending: Mutex<Option<Pending>>,
    pub dbus_address: Option<String>,
    pub consent: Consent,
    limits: Limits,
    active: AtomicUsize,
    next_conn: AtomicU64,
    /// The secret the app that spawned us passed over a pipe; a handler must present it.
    handler_secret: Option<String>,
}

impl Daemon {
    /// A daemon that talks to the session bus at `dbus_address` (the default bus when `None`), asking for consent.
    pub fn with_bus(dbus_address: Option<String>) -> Self {
        Self { dbus_address, ..Default::default() }
    }

    pub fn with_consent(mut self, consent: Consent) -> Self {
        self.consent = consent;
        self
    }

    pub fn with_hook(mut self, hook: Hook) -> Self {
        if let Some(core) = Arc::get_mut(&mut self.core) {
            core.hook = Some(hook);
        }
        self
    }

    /// Replaces how recordings are started (tests; the default starts a real [`Recorder`]).
    pub fn with_starter(mut self, starter: Starter) -> Self {
        self.starter = Some(starter);
        self
    }

    /// The secret a consent handler must present to register (the app that spawned this daemon holds the other copy).
    pub fn with_handler_secret(mut self, secret: String) -> Self {
        self.handler_secret = Some(secret);
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        if let Some(core) = Arc::get_mut(&mut self.core) {
            core.ttl = limits.ttl;
        }
        self
    }

    fn emit(&self, e: Event) {
        self.core.emit(e);
    }

    fn push_to_handler(&self, e: &Event) -> bool {
        self.core.push_to_handler(e)
    }

    /// Whether `ctx` may approve or deny: with a handler registered, only its connection; otherwise anyone who
    /// holds the code (the terminal flow).
    fn may_decide(&self, ctx: Option<&ConnCtx>) -> Result<(), RpcError> {
        let slot = self.core.handler.lock().map_err(|_| (-32000, "lock".to_string()))?;
        match (slot.as_ref(), ctx) {
            (None, _) => Ok(()),
            (Some(h), Some(c)) if h.conn == c.id => Ok(()),
            _ => Err((NOT_HANDLER, tr("Only the registered consent handler may decide").to_string())),
        }
    }

    /// Who may be the consent handler: only a connection that presents the secret the app that spawned this daemon
    /// gave it (constant-time compare). A daemon without a secret has no handler. A failure says nothing about why.
    fn authenticate(&self, params: &Value) -> Result<(), RpcError> {
        let refused = || (NOT_HANDLER, tr("The consent handler could not be authenticated").to_string());
        let secret = self.handler_secret.as_ref().ok_or_else(refused)?;
        let presented = params.get("secret").and_then(|v| v.as_str()).unwrap_or("");
        if ct_eq(presented.as_bytes(), secret.as_bytes()) {
            Ok(())
        } else {
            Err(refused())
        }
    }

    fn register_handler(&self, ctx: Option<&ConnCtx>, params: &Value) -> Result<Value, RpcError> {
        let ctx = ctx.ok_or((NOT_HANDLER, tr("A handler must be a socket connection").to_string()))?;
        if !peer_allowed(ctx.uid, my_uid()) {
            return Err((NOT_HANDLER, tr("Only the daemon's own user may handle consent").to_string()));
        }
        // before anything is handed over or the terminal is silenced
        self.authenticate(params)?;
        {
            let mut slot = self.core.handler.lock().map_err(|_| (-32000, "lock".to_string()))?;
            if let Some(h) = slot.as_ref() {
                if h.conn != ctx.id {
                    return Err((NOT_HANDLER, tr("A consent handler is already registered").to_string()));
                }
            }
            if let Ok(out) = ctx.out.lock() {
                let _ = out.set_write_timeout(Some(PUSH_TIMEOUT));
            }
            *slot = Some(HandlerSlot { conn: ctx.id, out: ctx.out.clone() });
        }
        // a request that was held before the UI registered is handed over now (the terminal's notice stands too)
        let waiting = self.pending.lock().ok().and_then(|p| {
            p.as_ref().filter(|p| !p.approved && p.created.elapsed() <= self.limits.ttl).map(|p| Event::ConsentRequested {
                what: p.what.clone(),
                token: p.token.clone(),
                request_id: p.id.clone(),
            })
        });
        if let Some(e) = waiting {
            self.push_to_handler(&e);
        }
        Ok(json!({"registered": true}))
    }

    /// The connection ended: it stops being the handler.
    fn connection_closed(&self, id: u64) {
        if let Ok(mut slot) = self.core.handler.lock() {
            if slot.as_ref().is_some_and(|h| h.conn == id) {
                *slot = None;
            }
        }
    }
}

/// The error code of a consent decision or registration by a connection that may not make it.
pub const NOT_HANDLER: i64 = -32003;

fn event_json(e: &Event, ttl: Duration) -> Value {
    match e {
        Event::ConsentRequested { what, token, request_id } => {
            json!({"type": "consent_requested", "request_id": request_id, "what": what, "token": token, "ttl_s": ttl.as_secs()})
        }
        Event::ConsentSuperseded { request_id } => json!({"type": "consent_superseded", "request_id": request_id}),
        Event::RecordingStarted { path } => json!({"type": "recording_started", "path": path}),
        Event::RecordingStopped { path } => json!({"type": "recording_stopped", "path": path}),
        Event::RecordingFailed { path, error } => json!({"type": "recording_failed", "path": path, "error": error}),
    }
}

type RpcError = (i64, String);

fn error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn audio_choice(v: Option<&Value>) -> Option<AudioChoice> {
    match v {
        None => Some(AudioChoice::Default),
        Some(Value::Bool(true)) => Some(AudioChoice::Default),
        Some(Value::Bool(false)) | Some(Value::Null) => Some(AudioChoice::Off),
        Some(Value::String(s)) => Some(AudioChoice::Named(s.clone())),
        _ => None,
    }
}

/// A character that could fake or hide text on the trusted channel (the terminal where the person approves):
/// controls (C0, DEL, C1), line/paragraph separators, zero-width and bidirectional formatting characters.
fn is_unsafe_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Rejects a client-supplied string that holds such characters.
fn plain(what: &str, s: &str) -> Result<(), RpcError> {
    if s.chars().any(is_unsafe_char) {
        return Err((-32602, format!("{what}: {}", tr("control or invisible characters are not allowed"))));
    }
    Ok(())
}

/// Validates `record.start`'s parameters into options.
pub fn parse_start(params: &Value) -> Result<RecordOptions, RpcError> {
    let bad = |m: String| (-32602, m);
    let source: Source = serde_json::from_value(params.get("source").cloned().unwrap_or(Value::Null)).map_err(|e| bad(format!("source: {e}")))?;
    let path_text = params.get("path").and_then(|p| p.as_str()).ok_or_else(|| bad("path".into()))?;
    plain("path", path_text)?;
    if let Source::Monitor { connector } | Source::Area { connector, .. } = &source {
        plain("source", connector)?;
    }
    let path = PathBuf::from(path_text);
    if !path.is_absolute() {
        return Err(bad(tr("The path must be absolute").into()));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(bad(tr("The path must not contain ..").into()));
    }
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")) {
        return Err(bad(tr("The path must end in .mp4").into()));
    }
    let mut o = RecordOptions::new(source, path);
    // a field of the wrong type is an error, never silently the default: what the person approves is what runs
    let typed = |name: &str, want: &str| bad(format!("{name}: {want}"));
    if let Some(v) = params.get("overwrite").filter(|v| !v.is_null()) {
        o.overwrite = v.as_bool().ok_or_else(|| typed("overwrite", "true or false"))?;
    }
    if let Some(v) = params.get("cursor").filter(|v| !v.is_null()) {
        o.cursor = v.as_bool().ok_or_else(|| typed("cursor", "true or false"))?;
    }
    if let Some(v) = params.get("encoder").filter(|v| !v.is_null()) {
        let e = v.as_str().ok_or_else(|| typed("encoder", "a name"))?;
        o.encoder = Some(cast_core::EncoderKind::parse(e).ok_or_else(|| bad(format!("encoder: {e}")))?);
    }
    if let Some(c) = params.get("codec") {
        o.codec = serde_json::from_value(c.clone()).map_err(|e| bad(format!("codec: {e}")))?;
    }
    if let Some(v) = params.get("fps").filter(|v| !v.is_null()) {
        o.fps = v.as_u64().ok_or_else(|| typed("fps", "a whole number"))?.min(u32::MAX as u64) as u32;
    }
    if let Some(v) = params.get("bitrate_kbps").filter(|v| !v.is_null()) {
        o.bitrate_kbps = v.as_u64().ok_or_else(|| typed("bitrate_kbps", "a whole number"))?.min(u32::MAX as u64) as u32;
    }
    o.normalize();
    o.desktop_audio = Some(audio_choice(params.get("desktop_audio")).ok_or_else(|| bad("desktop_audio".into()))?);
    o.mic = Some(audio_choice(params.get("mic")).ok_or_else(|| bad("mic".into()))?);
    for (what, c) in [("desktop_audio", &o.desktop_audio), ("mic", &o.mic)] {
        if let Some(AudioChoice::Named(n)) = c {
            plain(what, n)?;
        }
    }
    Ok(o)
}

/// A random one-time code from the kernel (128 bits, hex).
fn new_token() -> Result<String, RpcError> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| (-32000, e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// What an approval is for: every field of the parsed request, so a different request needs its own approval.
fn fingerprint(o: &RecordOptions) -> String {
    format!("{o:?}")
}

/// Takes what the windows say about their contents out of the list (their ids and the monitors stay).
fn redact_titles(s: &mut cast_capture::Sources) {
    for w in &mut s.windows {
        w.title.clear();
        w.app_id.clear();
    }
}

fn audio_text(c: &Option<AudioChoice>) -> String {
    match c {
        None | Some(AudioChoice::Off) => tr("off").into(),
        Some(AudioChoice::Default) => tr("default device").into(),
        Some(AudioChoice::Named(n)) => format!("{n:?}"),
    }
}

/// What the person is asked to approve, in words, built from the parsed request (what is shown is what runs). Names
/// the client chose are printed escaped (`{:?}` on the text only) and the path comes last, on its own line.
fn describe(o: &RecordOptions) -> String {
    let exists = std::fs::symlink_metadata(&o.path).is_ok();
    let mode = match (o.overwrite, exists) {
        (true, true) => tr("REPLACES an existing recording"),
        (true, false) => tr("new file (overwrite allowed, nothing there now)"),
        (false, true) => tr("refused: the file already exists"),
        (false, false) => tr("new file"),
    };
    let source = match &o.source {
        Source::Monitor { connector } => trf("monitor {name}", &[("name", format!("{connector:?}"))]),
        Source::Window { id } => trf("window {id}", &[("id", id.to_string())]),
        Source::Area { connector, x, y, width, height } => trf(
            "area {width}×{height} at {x},{y} of monitor {name}",
            &[
                ("width", width.to_string()),
                ("height", height.to_string()),
                ("x", x.to_string()),
                ("y", y.to_string()),
                ("name", format!("{connector:?}")),
            ],
        ),
    };
    let codec = match o.codec {
        cast_core::VideoCodec::H264 => "H.264",
        cast_core::VideoCodec::Hevc => "HEVC",
    };
    let bitrate = if o.bitrate_kbps == 0 { tr("automatic").to_string() } else { trf("{n} kbit/s", &[("n", o.bitrate_kbps.to_string())]) };
    let video = trf(
        "{codec}, {fps} fps, bitrate {bitrate}, encoder {encoder}, cursor {cursor}",
        &[
            ("codec", codec.to_string()),
            ("fps", o.fps.to_string()),
            ("bitrate", bitrate),
            ("encoder", o.encoder.map_or_else(|| tr("automatic").to_string(), |e| e.name().to_string())),
            ("cursor", if o.cursor { tr("shown") } else { tr("hidden") }.to_string()),
        ],
    );
    let folder = format!("{:?}", o.path.parent().unwrap_or(Path::new("/")));
    let file = format!("{:?}", o.path.file_name().unwrap_or_default());
    [
        (tr("Source"), source),
        (tr("Desktop sound"), audio_text(&o.desktop_audio)),
        (tr("Microphone"), audio_text(&o.mic)),
        (tr("Video"), video),
        (tr("Mode"), mode.to_string()),
        (tr("Folder"), folder),
        (tr("File"), file),
    ]
    .iter()
    .map(|(label, value)| format!("{label}: {value}"))
    .collect::<Vec<_>>()
    .join("\n")
}

impl Daemon {
    /// Answers one JSON-RPC request (a notification without id gets no answer).
    pub fn handle(&self, line: &str) -> Option<Value> {
        self.handle_from(None, line)
    }

    /// Like [`Daemon::handle`], for a request that came on a connection (`None`: no connection, as in tests).
    pub fn handle_from(&self, ctx: Option<&ConnCtx>, line: &str) -> Option<Value> {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(error(&Value::Null, -32700, format!("{}: {e}", tr("Invalid JSON")))),
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));
        let result = self.call(ctx, method, &params);
        let id = id?;
        Some(match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err((CONSENT_REQUIRED, msg)) if msg.contains(ID_MARK) => {
                let (message, request_id) = msg.split_once(ID_MARK).unwrap_or((&msg, ""));
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": CONSENT_REQUIRED, "message": message, "data": {"request_id": request_id}}})
            }
            Err((code, msg)) => error(&id, code, msg),
        })
    }

    /// Holds a request until the person approves it, or lets it through when it was approved (once) or consent is
    /// not asked for.
    fn check_consent(&self, fingerprint: String, what: String) -> Result<Option<Approval>, RpcError> {
        if self.consent == Consent::Allow {
            return Ok(None);
        }
        let mut slot = self.pending.lock().map_err(|_| (-32000, "lock".to_string()))?;
        if let Some(p) = slot.as_ref() {
            if p.created.elapsed() > self.limits.ttl {
                *slot = None;
            }
        }
        match slot.as_ref() {
            Some(p) if p.approved && p.fingerprint == fingerprint => {
                let used = Approval { created: p.created, restored: p.restored };
                *slot = None;
                return Ok(Some(used));
            }
            // asked already, still waiting: no new code, no new notice
            Some(p) if !p.approved && p.fingerprint == fingerprint => {}
            _ => {
                // a different request replaces the held one: never silently, the handler is told which one died
                if let Some(old) = slot.as_ref().filter(|p| !p.approved) {
                    self.emit(Event::ConsentSuperseded { request_id: old.id.clone() });
                }
                let token = new_token()?;
                let id = new_token()?[..16].to_string();
                *slot = Some(Pending {
                    token: token.clone(),
                    id: id.clone(),
                    what: what.clone(),
                    fingerprint,
                    created: Instant::now(),
                    approved: false,
                    wrong: 0,
                    restored: false,
                });
                self.emit(Event::ConsentRequested { what, token, request_id: id });
            }
        }
        // the id travels in the error's data (see `handle_from`)
        let id = slot.as_ref().map(|p| p.id.clone()).unwrap_or_default();
        Err((CONSENT_REQUIRED, format!("consent_required{ID_MARK}{id}")))
    }

    /// Gives back the approval `check_consent` used up, after a start that failed for the environment once the output
    /// file was open. At most once per approval, never past the first approval's own deadline (the clock is not reset),
    /// and never for a different request than the one approved.
    fn restore_approval(&self, fingerprint: String, what: String, used: Option<Approval>) {
        let Some(used) = used else { return };
        if used.restored || used.created.elapsed() > self.limits.ttl {
            return;
        }
        let (Ok(token), Ok(id)) = (new_token(), new_token()) else { return };
        if let Ok(mut slot) = self.pending.lock() {
            if slot.is_none() {
                *slot = Some(Pending {
                    token,
                    id: id[..16].to_string(),
                    what,
                    fingerprint,
                    created: used.created,
                    approved: true,
                    wrong: 0,
                    restored: true,
                });
            }
        }
    }

    fn approve(&self, params: &Value) -> Result<Value, RpcError> {
        let token = params.get("token").and_then(|t| t.as_str()).ok_or((-32602, "token".to_string()))?;
        let invalid = || (-32002, tr("Invalid or expired approval code").to_string());
        let mut slot = self.pending.lock().map_err(|_| (-32000, "lock".to_string()))?;
        let p = slot.as_mut().ok_or_else(invalid)?;
        if p.created.elapsed() > self.limits.ttl {
            *slot = None;
            return Err(invalid());
        }
        if p.token != token {
            p.wrong += 1;
            if p.wrong >= MAX_WRONG_CODES {
                *slot = None;
            }
            return Err(invalid());
        }
        p.approved = true;
        Ok(json!({"approved": true}))
    }

    /// The person said no: the held request is dropped (a new `record.start` asks again, with a new code).
    fn deny(&self, params: &Value) -> Result<Value, RpcError> {
        let token = params.get("token").and_then(|t| t.as_str()).ok_or((-32602, "token".to_string()))?;
        let mut slot = self.pending.lock().map_err(|_| (-32000, "lock".to_string()))?;
        match slot.as_ref() {
            Some(p) if p.token == token && p.created.elapsed() <= self.limits.ttl => {
                *slot = None;
                Ok(json!({"denied": true}))
            }
            _ => Err((-32002, tr("Invalid or expired approval code").to_string())),
        }
    }

    fn call(&self, ctx: Option<&ConnCtx>, method: &str, params: &Value) -> Result<Value, RpcError> {
        let app = |e: String| (-32000, e);
        match method {
            "capabilities" => Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "encoders": cast_encode::probe(),
                "audio": {"codec": "opus", "sample_rate": 48000, "channels": 2},
                "container": "mp4 (fragmented)",
                "consent": match self.consent { Consent::Ask => "ask", Consent::Allow => "allow" },
                "consent_handler": self.core.handler.lock().map(|h| h.is_some()).unwrap_or(false),
            })),
            "sources.list" => {
                let cast = cast_capture::ScreenCast::connect(self.dbus_address.as_deref()).map_err(|e| app(e.0))?;
                let mut s = cast.sources().map_err(|e| app(e.0))?;
                // window titles say what the person is doing: only the app that spawned this daemon (it holds the secret),
                // or a daemon that was told to allow everything, hands them out
                if self.consent != Consent::Allow && self.authenticate(params).is_err() {
                    redact_titles(&mut s);
                }
                serde_json::to_value(s).map_err(|e| app(e.to_string()))
            }
            "consent.approve" => {
                self.may_decide(ctx)?;
                self.approve(params)
            }
            "consent.deny" => {
                self.may_decide(ctx)?;
                self.deny(params)
            }
            "consent.handler.register" => self.register_handler(ctx, params),
            "record.start" => {
                let mut o = parse_start(params)?;
                o.dbus_address = self.dbus_address.clone();
                let (fp, what) = (fingerprint(&o), describe(&o));
                // a recording that failed is over: tell, then make room for a new one (stopped outside the lock)
                let failed = {
                    let mut slot = self.core.slot.lock().map_err(|_| app("lock".into()))?;
                    match std::mem::take(&mut *slot) {
                        Slot::Idle => None,
                        s @ Slot::Starting { .. } => {
                            *slot = s;
                            return Err(app(tr("A recording is already starting").into()));
                        }
                        Slot::Running { id, rec, notified } => {
                            if rec.status().error.is_none() {
                                *slot = Slot::Running { id, rec, notified };
                                return Err(app(tr("A recording is already running").into()));
                            }
                            Some((rec, notified))
                        }
                    }
                };
                if let Some((rec, notified)) = failed {
                    let st = rec.status();
                    if !notified {
                        self.emit(Event::RecordingFailed { path: st.path, error: st.error.unwrap_or_default() });
                    }
                    let _ = rec.stop();
                }
                let used = self.check_consent(fp.clone(), what.clone())?;
                // the slow part (the compositor, the encoder, up to 10 s) runs with no lock held
                let (id, cancel) = {
                    let mut slot = self.core.slot.lock().map_err(|_| app("lock".into()))?;
                    if !matches!(*slot, Slot::Idle) {
                        drop(slot);
                        let _ = (fp, what, used);
                        return Err(app(tr("A recording is already starting").into()));
                    }
                    let id = self.core.next_id.fetch_add(1, Ordering::Relaxed) + 1;
                    let cancel = Arc::new(AtomicBool::new(false));
                    *slot = Slot::Starting { id, cancel: cancel.clone(), failed: None };
                    (id, cancel)
                };
                let notify: FailNotify = {
                    let core = self.core.clone();
                    Arc::new(move |e| core.recording_failed(id, e))
                };
                let path = o.path.display().to_string();
                let started = match &self.starter {
                    Some(start) => start(o, notify),
                    None => Recorder::start_notify(o, notify).map(|r| Box::new(r) as Box<dyn Running>),
                };
                let mut slot = self.core.slot.lock().map_err(|_| app("lock".into()))?;
                let early_failure = match &mut *slot {
                    Slot::Starting { failed, .. } => failed.take(),
                    _ => None,
                };
                let cancelled = cancel.load(Ordering::Acquire);
                match started {
                    Ok(rec) if cancelled => {
                        *slot = Slot::Idle;
                        drop(slot);
                        let _ = rec.stop();
                        Err(app(tr("The start was cancelled").into()))
                    }
                    Ok(rec) => {
                        let status = rec.status();
                        *slot = Slot::Running { id, rec, notified: false };
                        drop(slot);
                        self.emit(Event::RecordingStarted { path });
                        if let Some(e) = early_failure {
                            self.core.recording_failed(id, e);
                        }
                        serde_json::to_value(status).map_err(|e| app(e.to_string()))
                    }
                    Err(e) => {
                        *slot = Slot::Idle;
                        drop(slot);
                        // the environment failed (the encoder, the compositor), not the request: the approval stands, so
                        // a retry does not ask the person again
                        if !cancelled && e.environment {
                            self.restore_approval(fp, what, used);
                        }
                        Err(app(e.message))
                    }
                }
            }
            "record.status" => {
                let slot = self.core.slot.lock().map_err(|_| app("lock".into()))?;
                match &*slot {
                    Slot::Idle => Ok(json!({"recording": false})),
                    Slot::Starting { .. } => Ok(json!({"recording": false, "starting": true})),
                    Slot::Running { rec, .. } => serde_json::to_value(rec.status()).map_err(|e| app(e.to_string())),
                }
            }
            "record.stop" => {
                let (rec, notified) = {
                    let mut slot = self.core.slot.lock().map_err(|_| app("lock".into()))?;
                    match std::mem::take(&mut *slot) {
                        Slot::Idle => return Err(app(tr("No recording is running").into())),
                        s @ Slot::Starting { .. } => {
                            // the start in progress ends when it can (it cannot be interrupted midway); its result is stopped
                            if let Slot::Starting { cancel, .. } = &s {
                                cancel.store(true, Ordering::Release);
                            }
                            *slot = s;
                            return Ok(json!({"cancelled": true}));
                        }
                        Slot::Running { rec, notified, .. } => (rec, notified),
                    }
                };
                let path = rec.status().path;
                let (summary, status) = match rec.stop() {
                    Ok(x) => x,
                    Err(e) => {
                        if !notified {
                            self.emit(Event::RecordingFailed { path, error: e.clone() });
                        }
                        return Err(app(e));
                    }
                };
                self.emit(Event::RecordingStopped { path });
                Ok(json!({
                    "path": status.path,
                    "duration_s": summary.duration_ns as f64 / 1e9,
                    "video_frames": summary.video_frames,
                    "audio_packets": summary.audio_packets,
                    "fragments": summary.fragments,
                    "bytes": summary.bytes,
                    "status": status,
                }))
            }
            _ => Err((-32601, format!("{}: {method}", tr("Unknown method")))),
        }
    }

    /// Serves connections until the listener fails. Each connection gets a thread, up to the limit.
    pub fn serve(self: Arc<Self>, listener: UnixListener) {
        let me = my_uid();
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            if !peer_allowed(peer_uid(&conn), me) {
                // another user (or unknown): closed without a word
                continue;
            }
            if self.active.fetch_add(1, Ordering::AcqRel) >= self.limits.connections {
                self.active.fetch_sub(1, Ordering::AcqRel);
                let _ = writeln!(conn, "{}", error(&Value::Null, -32000, tr("Too many connections")));
                continue;
            }
            let d = self.clone();
            let cred = peer_cred(&conn);
            let spawned = std::thread::Builder::new().name("cast-rpc".into()).spawn(move || {
                d.client(conn, cred);
                d.active.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                self.active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn client(&self, conn: UnixStream, cred: Option<(u32, u32)>) {
        let Ok(clone) = conn.try_clone() else { return };
        let out: SharedOut = Arc::new(Mutex::new(clone));
        let ctx = ConnCtx { id: self.next_conn.fetch_add(1, Ordering::Relaxed), uid: cred.map(|c| c.1), out: out.clone() };
        self.serve_connection(conn, &ctx);
        self.connection_closed(ctx.id);
    }

    fn serve_connection(&self, conn: UnixStream, ctx: &ConnCtx) {
        let mut reader = BufReader::new(conn);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            // at most one byte past the limit is read: a longer line is refused without being buffered
            match (&mut reader).take(self.limits.line as u64 + 1).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let ends = buf.last() == Some(&b'\n');
            if buf.len() - usize::from(ends) > self.limits.line {
                if let Ok(mut o) = ctx.out.lock() {
                    let _ = writeln!(o, "{}", error(&Value::Null, -32600, tr("Request too long")));
                }
                break;
            }
            let line = String::from_utf8_lossy(&buf);
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle_from(Some(ctx), &line) {
                let ok = ctx.out.lock().map(|mut o| writeln!(o, "{reply}").is_ok()).unwrap_or(false);
                if !ok {
                    break;
                }
            }
        }
    }

    /// Stops a running recording (the daemon is exiting), or cancels a start in progress.
    pub fn shutdown(&self) {
        let rec = {
            let Ok(mut slot) = self.core.slot.lock() else { return };
            match std::mem::take(&mut *slot) {
                Slot::Running { rec, .. } => Some(rec),
                s @ Slot::Starting { .. } => {
                    if let Slot::Starting { cancel, .. } = &s {
                        cancel.store(true, Ordering::Release);
                    }
                    *slot = s;
                    None
                }
                Slot::Idle => None,
            }
        };
        if let Some(r) = rec {
            let _ = r.stop();
        }
    }
}

/// A one-shot client: sends one request, returns the reply.
pub fn request(socket: &Path, method: &str, params: Value) -> std::io::Result<Value> {
    let mut s = UnixStream::connect(socket)?;
    writeln!(s, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kalango-cast-rpc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn start_params(path: &Path) -> Value {
        json!({"source": {"kind": "monitor", "connector": "DP-1"}, "path": path})
    }

    fn start(d: &Daemon, params: &Value) -> Value {
        d.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "record.start", "params": params}).to_string()).unwrap()
    }

    fn approve(d: &Daemon, token: &str) -> Value {
        d.handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "consent.approve", "params": {"token": token}}).to_string()).unwrap()
    }

    /// A daemon whose bus does not exist (a start that passes every check fails on the bus, which is not the
    /// consent error) and a log of the events its trusted channel receives.
    fn daemon(consent: Consent) -> (Daemon, Arc<Mutex<Vec<Event>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l2 = log.clone();
        let d = Daemon::with_bus(Some("unix:path=/nonexistent/kalango-cast-test-bus".into()))
            .with_consent(consent)
            .with_handler_secret(SECRET.into())
            .with_hook(Box::new(move |e| l2.lock().unwrap().push(e.clone())));
        (d, log)
    }

    fn token_of(log: &Mutex<Vec<Event>>) -> String {
        match log.lock().unwrap().last() {
            Some(Event::ConsentRequested { token, .. }) => token.clone(),
            other => panic!("no consent request: {other:?}"),
        }
    }

    /// A recording that is not real: counts what is done to it.
    #[derive(Default)]
    struct Fake {
        path: String,
        error: Arc<Mutex<Option<String>>>,
        halted: Arc<AtomicUsize>,
        stopped: Arc<AtomicUsize>,
    }

    impl Running for Fake {
        fn status(&self) -> Status {
            Status { recording: true, path: self.path.clone(), error: self.error.lock().unwrap().clone(), ..Status::default() }
        }
        fn halt_capture(&mut self) {
            self.halted.fetch_add(1, Ordering::SeqCst);
        }
        fn stop(self: Box<Self>) -> Result<(cast_mux::Summary, Status), String> {
            self.stopped.fetch_add(1, Ordering::SeqCst);
            Ok((cast_mux::Summary::default(), Status { path: self.path.clone(), ..Status::default() }))
        }
    }

    fn call(d: &Daemon, method: &str, params: Value) -> Value {
        d.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string()).unwrap()
    }

    fn slow_daemon(delay: Duration, fake: Fake, notify_out: Arc<Mutex<Option<FailNotify>>>) -> (Arc<Daemon>, Arc<Mutex<Vec<Event>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l2 = log.clone();
        let fake = Arc::new(Mutex::new(Some(fake)));
        let d = Daemon::default().with_consent(Consent::Allow).with_hook(Box::new(move |e| l2.lock().unwrap().push(e.clone()))).with_starter(
            Arc::new(move |_, notify| {
                std::thread::sleep(delay);
                *notify_out.lock().unwrap() = Some(notify);
                fake.lock().unwrap().take().map(|f| Box::new(f) as Box<dyn Running>).ok_or_else(|| StartError::environment("no encoder"))
            }),
        );
        (Arc::new(d), log)
    }

    #[test]
    fn status_and_stop_answer_while_a_start_is_under_way() {
        let stopped = Arc::new(AtomicUsize::new(0));
        let fake = Fake { path: "/home/me/Videos/a.mp4".into(), stopped: stopped.clone(), ..Fake::default() };
        let (d, _log) = slow_daemon(Duration::from_millis(800), fake, Arc::default());
        let d2 = d.clone();
        let starter = std::thread::spawn(move || call(&d2, "record.start", start_params(Path::new("/home/me/Videos/a.mp4"))));
        std::thread::sleep(Duration::from_millis(150));
        let t0 = Instant::now();
        let st = call(&d, "record.status", json!({}));
        assert!(t0.elapsed() < Duration::from_millis(100), "status waited {:?} for the start", t0.elapsed());
        assert_eq!(st["result"]["starting"], true, "{st}");
        let t0 = Instant::now();
        let stop = call(&d, "record.stop", json!({}));
        assert!(t0.elapsed() < Duration::from_millis(100), "stop waited {:?}", t0.elapsed());
        assert_eq!(stop["result"]["cancelled"], true, "{stop}");
        let other = call(&d, "record.start", start_params(Path::new("/home/me/Videos/b.mp4")));
        assert_eq!(other["error"]["message"], tr("A recording is already starting"), "{other}");
        // the cancelled start ends as an error and what it opened is stopped
        let r = starter.join().unwrap();
        assert_eq!(r["error"]["message"], tr("The start was cancelled"), "{r}");
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert_eq!(call(&d, "record.status", json!({}))["result"]["recording"], false);
    }

    #[test]
    fn a_recording_that_fails_midway_is_announced_at_once_and_its_capture_stops() {
        let halted = Arc::new(AtomicUsize::new(0));
        let error = Arc::new(Mutex::new(None));
        let fake = Fake { path: "/home/me/Videos/a.mp4".into(), halted: halted.clone(), error: error.clone(), ..Fake::default() };
        let out: Arc<Mutex<Option<FailNotify>>> = Arc::default();
        let (d, log) = slow_daemon(Duration::ZERO, fake, out.clone());
        let r = call(&d, "record.start", start_params(Path::new("/home/me/Videos/a.mp4")));
        assert!(r.get("error").is_none(), "{r}");
        // the writer thread says the disk is full; nobody calls record.stop
        *error.lock().unwrap() = Some("No space left on device".into());
        (out.lock().unwrap().clone().unwrap())("No space left on device".into());
        let failed: Vec<_> = log.lock().unwrap().iter().filter(|e| matches!(e, Event::RecordingFailed { .. })).cloned().collect();
        assert_eq!(failed, vec![Event::RecordingFailed { path: "/home/me/Videos/a.mp4".into(), error: "No space left on device".into() }]);
        let end = Instant::now() + Duration::from_secs(5);
        while halted.load(Ordering::SeqCst) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(halted.load(Ordering::SeqCst), 1, "the capture is stopped on the spot");
        // a second report, the next start and the stop do not announce it again
        (out.lock().unwrap().clone().unwrap())("again".into());
        let _ = call(&d, "record.stop", json!({}));
        let n = log.lock().unwrap().iter().filter(|e| matches!(e, Event::RecordingFailed { .. })).count();
        assert_eq!(n, 1);
    }

    #[test]
    fn discarding_a_failed_recording_at_the_next_start_tells_the_person_once() {
        let error = Arc::new(Mutex::new(Some("I/O error".to_string())));
        let fake = Fake { path: "/home/me/Videos/a.mp4".into(), error, ..Fake::default() };
        let (d, log) = slow_daemon(Duration::ZERO, fake, Arc::default());
        let _ = call(&d, "record.start", start_params(Path::new("/home/me/Videos/a.mp4")));
        // the next start: the failed one is discarded (and the starter, now empty, fails: no matter here)
        let _ = call(&d, "record.start", start_params(Path::new("/home/me/Videos/b.mp4")));
        let n = log.lock().unwrap().iter().filter(|e| matches!(e, Event::RecordingFailed { error, .. } if error == "I/O error")).count();
        assert_eq!(n, 1, "{:?}", log.lock().unwrap());
    }

    /// An approved request whose start fails with `error` after `delay`; returns the daemon and its params.
    fn approved_start(ttl: Duration, delay: Duration, error: StartError, name: &str) -> (Daemon, Value) {
        let (d, log) = daemon(Consent::Ask);
        let d = d.with_limits(Limits { ttl, ..Limits::default() }).with_starter(Arc::new(move |_, _| {
            std::thread::sleep(delay);
            Err(error.clone())
        }));
        let params = start_params(&scratch(name).join("a.mp4"));
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        assert_eq!(approve(&d, &token_of(&log))["result"]["approved"], true);
        (d, params)
    }

    fn message(r: &Value) -> String {
        r["error"]["message"].as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn an_environment_failure_gives_the_approval_back_once() {
        let (d, params) = approved_start(Duration::from_secs(60), Duration::ZERO, StartError::environment("no encoder"), "env-once");
        assert_eq!(message(&start(&d, &params)), "no encoder");
        assert_eq!(message(&start(&d, &params)), "no encoder", "the restored approval is used: no new question");
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED, "a second failure is not given back again");
        // a different request is never covered by it
        let other = start_params(&scratch("env-once").join("b.mp4"));
        assert_eq!(start(&d, &other)["error"]["code"], CONSENT_REQUIRED);
    }

    #[test]
    fn a_failure_of_the_request_or_the_file_does_not_give_the_approval_back() {
        let (d, params) = approved_start(Duration::from_secs(60), Duration::ZERO, StartError::request("The file already exists"), "request");
        assert_eq!(message(&start(&d, &params)), "The file already exists");
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED, "asks again");
    }

    #[test]
    fn a_restored_approval_keeps_the_first_deadline() {
        let ttl = Duration::from_millis(700);
        let (d, params) = approved_start(ttl, Duration::from_millis(400), StartError::environment("no encoder"), "deadline");
        assert_eq!(message(&start(&d, &params)), "no encoder");
        // 400 ms in the first start, 400 ms here: past the first approval's 700 ms, though not past 700 ms from the restore
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED, "the clock was not reset by the restore");
    }

    #[test]
    fn an_approval_that_expired_during_the_start_is_not_restored() {
        let ttl = Duration::from_millis(300);
        let (d, params) = approved_start(ttl, Duration::from_millis(600), StartError::environment("timeout"), "expired");
        assert_eq!(message(&start(&d, &params)), "timeout");
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
    }

    #[test]
    fn window_titles_only_go_to_the_app_that_holds_the_secret() {
        let mut s = cast_capture::Sources {
            monitors: vec![],
            windows: vec![cast_capture::screencast::Window { id: 3, title: "Bank - Firefox".into(), app_id: "firefox".into() }],
        };
        redact_titles(&mut s);
        assert_eq!((s.windows[0].id, s.windows[0].title.as_str(), s.windows[0].app_id.as_str()), (3, "", ""));
    }

    #[test]
    fn the_description_is_words_not_debug_output() {
        let t = describe(&opts(json!({})));
        assert!(!t.contains("Monitor {") && !t.contains("connector:"), "{t}");
        assert!(t.contains(&trf("monitor {name}", &[("name", "\"DP-1\"".into())])), "{t}");
        let t = describe(&opts(json!({"source": {"kind": "area", "connector": "DP-1", "x": 1, "y": 2, "width": 30, "height": 40}})));
        assert!(t.contains("30×40") && t.contains("1,2") && !t.contains("Area {"), "{t}");
        assert!(describe(&opts(json!({"source": {"kind": "window", "id": 9}}))).contains(&trf("window {id}", &[("id", "9".into())])));
    }

    #[test]
    fn errors_follow_json_rpc() {
        let d = Daemon::default();
        let r = d.handle(r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#).unwrap();
        assert_eq!(r["error"]["code"], -32601);
        assert_eq!(r["id"], 3);
        let r = d.handle("{not json").unwrap();
        assert_eq!(r["error"]["code"], -32700);
        assert!(d.handle(r#"{"jsonrpc":"2.0","method":"record.status"}"#).is_none(), "notifications get no reply");
        let r = d.handle(r#"{"jsonrpc":"2.0","id":1,"method":"record.status"}"#).unwrap();
        assert_eq!(r["result"]["recording"], false);
        let r = d.handle(r#"{"jsonrpc":"2.0","id":1,"method":"record.stop"}"#).unwrap();
        assert_eq!(r["error"]["code"], -32000);
        let r = d.handle(r#"{"jsonrpc":"2.0","id":1,"method":"record.start","params":{"source":{"kind":"tv"},"path":"/tmp/x.mp4"}}"#).unwrap();
        assert_eq!(r["error"]["code"], -32602, "{r}");
    }

    #[test]
    fn start_parameters_are_validated_and_clamped() {
        let p = |extra: Value| {
            let mut v = start_params(Path::new("/home/me/Videos/a.mp4"));
            for (k, val) in extra.as_object().unwrap() {
                v[k] = val.clone();
            }
            parse_start(&v)
        };
        let o = p(json!({})).unwrap();
        assert_eq!((o.fps, o.bitrate_kbps, o.overwrite), (60, 0, false));
        assert_eq!(p(json!({"fps": 0})).unwrap().fps, 1);
        assert_eq!(p(json!({"fps": 1})).unwrap().fps, 1);
        assert_eq!(p(json!({"fps": 240})).unwrap().fps, 240);
        assert_eq!(p(json!({"fps": 241})).unwrap().fps, 240);
        assert_eq!(p(json!({"fps": 100_000})).unwrap().fps, 240);
        assert_eq!(p(json!({"bitrate_kbps": 500_000})).unwrap().bitrate_kbps, 500_000);
        assert_eq!(p(json!({"bitrate_kbps": 500_001})).unwrap().bitrate_kbps, 500_000);
        assert!(p(json!({"overwrite": true})).unwrap().overwrite);
        assert_eq!(p(json!({"overwrite": "yes"})).unwrap_err().0, -32602, "only a real boolean overwrites");
        for wrong in [
            json!({"fps": "30"}),
            json!({"fps": 29.5}),
            json!({"fps": -1}),
            json!({"bitrate_kbps": "8000"}),
            json!({"encoder": 5}),
            json!({"cursor": "no"}),
        ] {
            assert_eq!(p(wrong.clone()).unwrap_err().0, -32602, "{wrong}: a field of the wrong type is refused, not defaulted");
        }
        assert!(!p(json!({"cursor": false})).unwrap().cursor);
        assert!(p(json!({})).unwrap().cursor);
        assert_eq!(p(json!({"encoder": "nvenc"})).unwrap().encoder, Some(cast_core::EncoderKind::Nvenc));
        assert_eq!(p(json!({"desktop_audio": false})).unwrap().desktop_audio, Some(AudioChoice::Off));
        assert_eq!(p(json!({"mic": "usb"})).unwrap().mic, Some(AudioChoice::Named("usb".into())));
        assert_eq!(p(json!({"mic": 5})).unwrap_err().0, -32602);
        assert_eq!(p(json!({"encoder": "toaster"})).unwrap_err().0, -32602);
    }

    #[test]
    fn paths_must_be_absolute_plain_mp4_files() {
        let with = |path: &str| parse_start(&json!({"source": {"kind": "window", "id": 3}, "path": path}));
        assert!(with("/home/me/a.mp4").is_ok());
        assert!(with("/home/me/A.MP4").is_ok());
        for bad in
            ["a.mp4", "Videos/a.mp4", "./a.mp4", "/home/me/../../etc/a.mp4", "/home/me/.bashrc", "/home/me/a.mp4.sh", "/home/me/mp4", "/home/me/a"]
        {
            assert_eq!(with(bad).unwrap_err().0, -32602, "{bad}");
        }
    }

    fn opts(extra: Value) -> RecordOptions {
        let mut v = start_params(Path::new("/home/me/Videos/a.mp4"));
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        parse_start(&v).unwrap()
    }

    #[test]
    fn text_that_could_fake_the_consent_line_is_refused() {
        for evil in [
            "/tmp/\u{1b}[2K\rapproved.mp4",
            "/tmp/a\rb.mp4",
            "/tmp/a\nb.mp4",
            "/tmp/a\u{202e}4pm.mp4",
            "/tmp/a\u{9b}2K.mp4",
            "/tmp/a\u{200b}.mp4",
            "/tmp/a\u{7f}.mp4",
            "/tmp/a\u{2028}.mp4",
            "/tmp/a\0.mp4",
        ] {
            assert_eq!(parse_start(&json!({"source": {"kind": "window", "id": 3}, "path": evil})).unwrap_err().0, -32602, "{evil:?}");
        }
        let src = |c: &str| parse_start(&json!({"source": {"kind": "monitor", "connector": c}, "path": "/tmp/a.mp4"}));
        assert!(src("DP-1").is_ok());
        assert_eq!(src("DP-1\u{1b}[2K").unwrap_err().0, -32602);
        let named = |m: &str| parse_start(&json!({"source": {"kind": "window", "id": 3}, "path": "/tmp/a.mp4", "mic": m}));
        assert!(named("usb mic").is_ok());
        assert_eq!(named("x\u{202e}y").unwrap_err().0, -32602);
    }

    #[test]
    fn the_description_escapes_what_the_client_chose_and_puts_the_path_last() {
        // even if a hostile value got past parsing, nothing raw reaches the terminal
        let mut o = opts(json!({}));
        o.path = PathBuf::from("/tmp/\u{1b}[2K\rAPPROVED\n\u{202e}.mp4");
        o.source = Source::Monitor { connector: "DP-1\u{1b}[31m".into() };
        o.mic = Some(AudioChoice::Named("m\u{9b}".into()));
        let text = describe(&o);
        assert!(!text.chars().any(|c| is_unsafe_char(c) && c != '\n'), "{text:?}");
        assert_eq!(text.lines().count(), 7, "one line per fact, the client's text cannot add lines: {text:?}");
        assert!(text.lines().last().unwrap().starts_with(&format!("{}: ", tr("File"))));
        assert!(text.lines().next().unwrap().starts_with(&format!("{}: ", tr("Source"))));
    }

    #[test]
    fn the_description_says_whether_it_replaces_a_recording() {
        let dir = scratch("describe");
        let existing = dir.join("old.mp4");
        std::fs::write(&existing, b"").unwrap();
        let on = |path: &Path, over: bool| {
            let mut v = start_params(path);
            v["overwrite"] = json!(over);
            describe(&parse_start(&v).unwrap())
        };
        assert!(on(&existing, true).contains(&format!("{}: {}", tr("Mode"), tr("REPLACES an existing recording"))));
        assert!(on(&existing, false).contains(tr("refused: the file already exists")));
        assert!(on(&dir.join("new.mp4"), false).contains(&format!("{}: {}", tr("Mode"), tr("new file"))));
        assert!(!on(&dir.join("new.mp4"), false).contains("REPLACES"));
        let t = describe(&opts(json!({"fps": 30, "bitrate_kbps": 8000, "codec": "hevc", "mic": false})));
        let line = |label: &str, value: String| format!("{}: {value}", tr(label));
        for part in [
            "30".to_string(),
            "8000".to_string(),
            "HEVC".to_string(),
            line("Microphone", tr("off").into()),
            line("Desktop sound", tr("default device").into()),
            line("Folder", "\"/home/me/Videos\"".into()),
            line("File", "\"a.mp4\"".into()),
        ] {
            assert!(t.contains(&part), "{part} in {t}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_description_says_the_encoder_and_the_cursor() {
        let t = describe(&opts(json!({"encoder": "nvenc", "cursor": false})));
        assert!(t.contains("nvenc") && t.contains(tr("hidden")), "{t}");
        let t = describe(&opts(json!({})));
        assert!(t.contains(tr("automatic")) && t.contains(tr("shown")), "{t}");
    }

    #[test]
    fn changing_any_field_changes_what_is_approved() {
        let base = fingerprint(&opts(json!({})));
        assert_eq!(base, fingerprint(&opts(json!({}))), "the same request, the same fingerprint");
        assert_eq!(base, fingerprint(&opts(json!({"fps": 60}))), "an explicit default is the same request");
        for (name, extra) in [
            ("overwrite", json!({"overwrite": true})),
            ("fps", json!({"fps": 30})),
            ("bitrate", json!({"bitrate_kbps": 1000})),
            ("codec", json!({"codec": "hevc"})),
            ("encoder", json!({"encoder": "openh264"})),
            ("mic", json!({"mic": false})),
            ("mic device", json!({"mic": "usb"})),
            ("desktop", json!({"desktop_audio": false})),
            ("desktop device", json!({"desktop_audio": "hdmi"})),
            ("path", json!({"path": "/home/me/Videos/b.mp4"})),
            ("folder", json!({"path": "/home/me/Other/a.mp4"})),
            ("source", json!({"source": {"kind": "window", "id": 9}})),
        ] {
            assert_ne!(base, fingerprint(&opts(extra)), "{name} must change the fingerprint");
        }
    }

    #[test]
    fn peers_of_other_users_are_not_served() {
        assert!(peer_allowed(Some(1000), 1000));
        assert!(!peer_allowed(Some(0), 1000));
        assert!(!peer_allowed(Some(1001), 1000));
        assert!(!peer_allowed(None, 1000));
    }

    #[test]
    fn a_recording_needs_the_persons_approval() {
        let dir = scratch("consent");
        let path = dir.join("a.mp4");
        let (d, log) = daemon(Consent::Ask);
        let d = d.with_starter(Arc::new(|_, _| Ok(Box::new(Fake { path: "/x/a.mp4".into(), ..Fake::default() }) as Box<dyn Running>)));
        let params = start_params(&path);
        let r = start(&d, &params);
        assert_eq!(r["error"]["code"], CONSENT_REQUIRED, "{r}");
        assert!(r["error"]["message"].as_str().unwrap().starts_with("consent_required"));
        let token = token_of(&log);
        assert!(!r.to_string().contains(&token), "the code goes to the trusted channel only, never to the client");
        assert!(!path.exists(), "nothing was started or created");
        // asking again does not produce a new code or a new notice
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        assert_eq!(log.lock().unwrap().len(), 1);
        // a wrong code is refused; the right one is accepted
        assert_eq!(approve(&d, "0000")["error"]["code"], -32002);
        assert_eq!(approve(&d, &token)["result"]["approved"], true);
        // the approved request goes through the consent step and starts
        let r = start(&d, &params);
        assert!(r.get("error").is_none(), "{r}");
        // and the approval was for one start only
        assert_eq!(call(&d, "record.stop", json!({}))["result"]["path"], "/x/a.mp4");
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        assert_ne!(token_of(&log), token, "a fresh code");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_approval_is_for_that_request_only() {
        let dir = scratch("fingerprint");
        let (d, log) = daemon(Consent::Ask);
        let a = start_params(&dir.join("a.mp4"));
        assert_eq!(start(&d, &a)["error"]["code"], CONSENT_REQUIRED);
        let token = token_of(&log);
        assert_eq!(approve(&d, &token)["result"]["approved"], true);
        // a different request (another file) is held, not let through on the first one's approval
        let b = start_params(&dir.join("b.mp4"));
        assert_eq!(start(&d, &b)["error"]["code"], CONSENT_REQUIRED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guessing_the_code_cancels_the_request() {
        let dir = scratch("guess");
        let (d, log) = daemon(Consent::Ask);
        let params = start_params(&dir.join("a.mp4"));
        start(&d, &params);
        let token = token_of(&log);
        for _ in 0..MAX_WRONG_CODES {
            assert_eq!(approve(&d, "guess")["error"]["code"], -32002);
        }
        assert_eq!(approve(&d, &token)["error"]["code"], -32002, "even the right code is too late");
        assert_eq!(approve(&Daemon::default(), "x")["error"]["code"], -32002, "nothing pending");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_code_cannot_approve_and_an_expired_approval_is_not_used() {
        let dir = scratch("ttl");
        let params = start_params(&dir.join("a.mp4"));
        let limits = Limits { ttl: Duration::from_millis(150), ..Limits::default() };
        // the code expires before it is used
        let (d, log) = daemon(Consent::Ask);
        let d = d.with_limits(limits);
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        let token = token_of(&log);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(approve(&d, &token)["error"]["code"], -32002, "the right code, too late");
        // the approval expires before the request is repeated: held again, with a new code
        let (d, log) = daemon(Consent::Ask);
        let d = d.with_limits(limits);
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        let token = token_of(&log);
        assert_eq!(approve(&d, &token)["result"]["approved"], true);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        assert_eq!(log.lock().unwrap().len(), 2, "a second request went to the person");
        assert_ne!(token_of(&log), token);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_folder_says_why_in_the_response() {
        // the folder of the output must be the user's own (unless the tests run as root)
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (d, _) = daemon(Consent::Allow);
        let r = start(&d, &start_params(Path::new("/usr/kalango-cast-test.mp4")));
        assert_eq!(r["error"]["code"], -32000, "{r}");
        let msg = r["error"]["message"].as_str().unwrap();
        assert!(msg.contains("/usr") && msg.contains("not yours"), "{msg}");
        assert!(!Path::new("/usr/kalango-cast-test.mp4").exists());
    }

    #[test]
    fn with_consent_allowed_no_approval_is_asked() {
        let dir = scratch("allow");
        let (d, log) = daemon(Consent::Allow);
        let r = start(&d, &start_params(&dir.join("a.mp4")));
        assert_ne!(r["error"]["code"], CONSENT_REQUIRED, "{r}");
        assert!(log.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_config_chooses_the_consent_mode() {
        assert_eq!(consent_from_config(""), Consent::Ask);
        assert_eq!(consent_from_config("consent = \"allow\"\n"), Consent::Allow);
        assert_eq!(consent_from_config("# c\n  consent=\"allow\"  # ok\n"), Consent::Allow);
        assert_eq!(consent_from_config("consent = \"ask\""), Consent::Ask);
        assert_eq!(consent_from_config("consent = \"whatever\""), Consent::Ask);
        assert_eq!(consent_from_config("other = \"allow\""), Consent::Ask);
    }

    #[test]
    fn the_socket_is_private_and_checks_the_peer() {
        let dir = scratch("sock");
        let sock = dir.join("kalango").join("cast.sock");
        let listener = bind(&sock).unwrap();
        assert_eq!(std::fs::metadata(&sock).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(sock.parent().unwrap()).unwrap().mode() & 0o777, 0o700);
        let d = Arc::new(Daemon::default());
        std::thread::spawn(move || d.serve(listener));
        let reply = request(&sock, "record.status", json!({})).unwrap();
        assert_eq!(reply["result"]["recording"], false);
        let c = UnixStream::connect(&sock).unwrap();
        assert_eq!(peer_uid(&c), Some(my_uid()));
        // a second daemon on the same socket is refused while the first answers
        assert!(bind(&sock).is_err());
        // a folder open to others is refused
        let open = dir.join("open");
        std::fs::create_dir_all(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(bind(&open.join("cast.sock")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_stale_sockets_are_removed() {
        let dir = scratch("stale");
        let folder = dir.join("kalango");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
        // a file that is not a socket is never deleted
        let path = folder.join("cast.sock");
        std::fs::write(&path, b"my notes").unwrap();
        assert!(bind(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"my notes");
        // neither is what a link points to
        std::fs::remove_file(&path).unwrap();
        let target = dir.join("precious");
        std::fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(bind(&path).is_err());
        assert!(target.exists());
        std::fs::remove_file(&path).unwrap();
        // a socket nobody listens on is stale: replaced
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(bind(&path).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn serve_with(limits: Limits, name: &str) -> (PathBuf, PathBuf) {
        let dir = scratch(name);
        let sock = dir.join("kalango").join("cast.sock");
        let listener = bind(&sock).unwrap();
        let d = Arc::new(Daemon::default().with_limits(limits));
        std::thread::spawn(move || d.serve(listener));
        (dir, sock)
    }

    fn read_reply(c: &UnixStream) -> Value {
        // a daemon that does not answer fails the test instead of hanging it
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut line = String::new();
        BufReader::new(c.try_clone().unwrap()).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }

    #[test]
    fn a_line_over_the_limit_is_refused_and_the_connection_closed() {
        let (dir, sock) = serve_with(Limits { line: 64, connections: 4, ..Limits::default() }, "line");
        // exactly at the limit is served (the content, without the newline)
        let ok = format!("{{\"id\":1,\"method\":\"record.status\"{}}}", " ".repeat(64 - 33));
        assert_eq!(ok.len(), 64);
        let mut c = UnixStream::connect(&sock).unwrap();
        writeln!(c, "{ok}").unwrap();
        assert_eq!(read_reply(&c)["result"]["recording"], false);
        // one byte more is refused
        let long = format!("{{\"id\":1,\"method\":\"record.status\"{}}}", " ".repeat(64 - 32));
        assert_eq!(long.len(), 65);
        writeln!(c, "{long}").unwrap();
        assert_eq!(read_reply(&c)["error"]["code"], -32600);
        let mut rest = String::new();
        assert_eq!(c.read_to_string(&mut rest).unwrap(), 0, "closed");
        // a line that never ends is cut at the limit, not buffered
        let mut c = UnixStream::connect(&sock).unwrap();
        c.write_all(&[b'a'; 4096]).unwrap();
        assert_eq!(read_reply(&c)["error"]["code"], -32600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connections_are_capped_and_freed() {
        let (dir, sock) = serve_with(Limits { connections: 2, ..Limits::default() }, "conns");
        let ping = |c: &mut UnixStream| {
            writeln!(c, r#"{{"id":1,"method":"record.status"}}"#).unwrap();
            read_reply(c)
        };
        let mut a = UnixStream::connect(&sock).unwrap();
        let mut b = UnixStream::connect(&sock).unwrap();
        assert_eq!(ping(&mut a)["result"]["recording"], false);
        assert_eq!(ping(&mut b)["result"]["recording"], false);
        // the third is told why and closed
        let mut c = UnixStream::connect(&sock).unwrap();
        let r = read_reply(&c);
        assert_eq!(r["error"]["code"], -32000, "{r}");
        let mut rest = String::new();
        assert_eq!(c.read_to_string(&mut rest).unwrap(), 0);
        // a slot is freed when a client leaves
        drop(a);
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let mut d = UnixStream::connect(&sock).unwrap();
            writeln!(d, r#"{{"id":1,"method":"record.status"}}"#).unwrap();
            if read_reply(&d).get("result").is_some() {
                break;
            }
            assert!(Instant::now() < end, "the slot was never freed");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fake connection: its context and the peer end where pushed events can be read.
    fn conn(id: u64, uid: u32) -> (ConnCtx, BufReader<UnixStream>) {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        (ConnCtx { id, uid: Some(uid), out: Arc::new(Mutex::new(a)) }, BufReader::new(b))
    }

    fn call_from(d: &Daemon, c: &ConnCtx, method: &str, params: Value) -> Value {
        d.handle_from(Some(c), &json!({"jsonrpc": "2.0", "id": 9, "method": method, "params": params}).to_string()).unwrap()
    }

    fn next_event(r: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
        assert_eq!(v["method"], "event");
        assert!(v.get("id").is_none(), "events are notifications");
        v["params"].clone()
    }

    #[test]
    fn a_registered_handler_gets_the_code_and_the_terminal_stays_quiet() {
        let dir = scratch("handler");
        let (d, log) = daemon(Consent::Ask);
        let me = my_uid();
        let (ui, mut ui_in) = conn(1, me);
        let (other, _other_in) = conn(2, me);
        assert_eq!(call_from(&d, &ui, "consent.handler.register", json!({"secret": SECRET}))["result"]["registered"], true);
        assert_eq!(call_from(&d, &ui, "capabilities", json!({}))["result"]["consent_handler"], true);
        let params = start_params(&dir.join("a.mp4"));
        let r = d.handle_from(Some(&other), &json!({"jsonrpc": "2.0", "id": 1, "method": "record.start", "params": params}).to_string()).unwrap();
        assert_eq!(r["error"]["code"], CONSENT_REQUIRED, "{r}");
        let ev = next_event(&mut ui_in);
        assert_eq!(ev["type"], "consent_requested");
        let token = ev["token"].as_str().unwrap().to_string();
        assert!(ev["what"].as_str().unwrap().contains("\"a.mp4\""), "the exact description, escaped, reaches the UI");
        assert_eq!(ev["ttl_s"], 120);
        assert!(!r.to_string().contains(&token), "never to the client that asked");
        assert!(log.lock().unwrap().is_empty(), "the code is not printed on the terminal while a UI handles consent");
        // a client that is not the handler cannot approve or deny, even holding the code
        assert_eq!(call_from(&d, &other, "consent.approve", json!({"token": token}))["error"]["code"], NOT_HANDLER);
        assert_eq!(call_from(&d, &other, "consent.deny", json!({"token": token}))["error"]["code"], NOT_HANDLER);
        assert_eq!(approve(&d, &token)["error"]["code"], NOT_HANDLER, "nor can a request without a connection");
        // the handler can
        assert_eq!(call_from(&d, &ui, "consent.approve", json!({"token": token}))["result"]["approved"], true);
        let r = d.handle_from(Some(&other), &json!({"jsonrpc": "2.0", "id": 1, "method": "record.start", "params": params}).to_string()).unwrap();
        assert_ne!(r["error"]["code"], CONSENT_REQUIRED, "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn denying_drops_the_request_and_the_next_start_asks_again() {
        let dir = scratch("deny");
        let (d, _log) = daemon(Consent::Ask);
        let (ui, mut ui_in) = conn(1, my_uid());
        call_from(&d, &ui, "consent.handler.register", json!({"secret": SECRET}));
        let params = start_params(&dir.join("a.mp4"));
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        let token = next_event(&mut ui_in)["token"].as_str().unwrap().to_string();
        assert_eq!(call_from(&d, &ui, "consent.deny", json!({"token": token}))["result"]["denied"], true);
        assert_eq!(call_from(&d, &ui, "consent.approve", json!({"token": token}))["error"]["code"], -32002, "the denied code is dead");
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        let again = next_event(&mut ui_in)["token"].as_str().unwrap().to_string();
        assert_ne!(again, token, "a fresh code");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_daemons_own_user_can_register_and_only_one_handler_at_a_time() {
        let (d, _log) = daemon(Consent::Ask);
        let me = my_uid();
        let (stranger, _s) = conn(1, me.wrapping_add(1));
        assert_eq!(call_from(&d, &stranger, "consent.handler.register", json!({"secret": SECRET}))["error"]["code"], NOT_HANDLER);
        let unknown = ConnCtx { uid: None, ..conn(3, me).0 };
        assert_eq!(call_from(&d, &unknown, "consent.handler.register", json!({"secret": SECRET}))["error"]["code"], NOT_HANDLER);
        assert_eq!(d.handle(r#"{"jsonrpc":"2.0","id":1,"method":"consent.handler.register"}"#).unwrap()["error"]["code"], NOT_HANDLER);
        assert_eq!(call_from(&d, &stranger, "capabilities", json!({}))["result"]["consent_handler"], false, "nothing registered");
        let (first, _f) = conn(4, me);
        let (second, _g) = conn(5, me);
        assert_eq!(call_from(&d, &first, "consent.handler.register", json!({"secret": SECRET}))["result"]["registered"], true);
        assert_eq!(
            call_from(&d, &second, "consent.handler.register", json!({"secret": SECRET}))["error"]["code"],
            NOT_HANDLER,
            "no hijacking a live handler"
        );
        assert_eq!(
            call_from(&d, &first, "consent.handler.register", json!({"secret": SECRET}))["result"]["registered"],
            true,
            "the same one may repeat"
        );
    }

    #[test]
    fn without_a_handler_or_after_it_leaves_the_terminal_gets_the_code() {
        let dir = scratch("fallback");
        let (d, log) = daemon(Consent::Ask);
        let params = start_params(&dir.join("a.mp4"));
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        let t1 = token_of(&log);
        assert_eq!(approve(&d, &t1)["result"]["approved"], true, "terminal flow unchanged: whoever holds the code");
        // a handler that registers and then goes away (its connection closes): back to the terminal
        let (ui, ui_in) = conn(1, my_uid());
        call_from(&d, &ui, "consent.handler.register", json!({"secret": SECRET}));
        d.connection_closed(1);
        drop(ui_in);
        let b = start_params(&dir.join("b.mp4"));
        assert_eq!(start(&d, &b)["error"]["code"], CONSENT_REQUIRED);
        assert_eq!(log.lock().unwrap().len(), 2);
        assert_ne!(token_of(&log), t1);
        // a handler whose connection is dead without being closed is dropped on the first push
        let (ui2, ui2_in) = conn(2, my_uid());
        call_from(&d, &ui2, "consent.handler.register", json!({"secret": SECRET}));
        drop(ui2_in);
        let c = start_params(&dir.join("c.mp4"));
        assert_eq!(start(&d, &c)["error"]["code"], CONSENT_REQUIRED);
        assert_eq!(log.lock().unwrap().len(), 4, "the dead handler did not swallow the code (the replaced request b is announced too)");
        assert!(matches!(log.lock().unwrap().last(), Some(Event::ConsentRequested { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_held_before_the_ui_registered_is_handed_over() {
        let dir = scratch("late");
        let (d, log) = daemon(Consent::Ask);
        assert_eq!(start(&d, &start_params(&dir.join("a.mp4")))["error"]["code"], CONSENT_REQUIRED);
        let token = token_of(&log);
        let (ui, mut ui_in) = conn(1, my_uid());
        call_from(&d, &ui, "consent.handler.register", json!({"secret": SECRET}));
        let ev = next_event(&mut ui_in);
        assert_eq!(ev["token"], token.as_str());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_handler_works_over_the_real_socket_and_lifecycle_events_follow() {
        let dir = scratch("handler-sock");
        let sock = dir.join("kalango").join("cast.sock");
        let listener = bind(&sock).unwrap();
        let d = Arc::new(daemon(Consent::Ask).0);
        let d2 = d.clone();
        std::thread::spawn(move || d2.serve(listener));
        let mut ui = UnixStream::connect(&sock).unwrap();
        ui.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut ui_in = BufReader::new(ui.try_clone().unwrap());
        writeln!(ui, "{}", json!({"jsonrpc":"2.0","id":1,"method":"consent.handler.register","params":{"secret": SECRET}})).unwrap();
        let mut line = String::new();
        ui_in.read_line(&mut line).unwrap();
        assert!(line.contains("registered"), "{line}");
        let params = start_params(&dir.join("a.mp4"));
        let r = request(&sock, "record.start", params.clone()).unwrap();
        assert_eq!(r["error"]["code"], CONSENT_REQUIRED);
        let ev = next_event(&mut ui_in);
        let token = ev["token"].as_str().unwrap().to_string();
        // another connection of the same user holding the code still cannot approve
        let r = request(&sock, "consent.approve", json!({"token": token})).unwrap();
        assert_eq!(r["error"]["code"], NOT_HANDLER, "{r}");
        writeln!(ui, "{}", json!({"jsonrpc":"2.0","id":2,"method":"consent.approve","params":{"token":token}})).unwrap();
        line.clear();
        ui_in.read_line(&mut line).unwrap();
        assert!(line.contains("approved"), "{line}");
        // the approved start reaches the engine (and fails on the missing bus: a failed start is no event)
        let r = request(&sock, "record.start", params).unwrap();
        assert_ne!(r["error"]["code"], CONSENT_REQUIRED, "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- authenticated registration -------------------------------------------------------------------------

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    /// A daemon the app spawned: it holds a secret, and no development mode.
    fn spawned_daemon() -> (Daemon, Arc<Mutex<Vec<Event>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l2 = log.clone();
        let d = Daemon::with_bus(Some("unix:path=/nonexistent/kalango-cast-test-bus".into()))
            .with_handler_secret(SECRET.into())
            .with_hook(Box::new(move |e| l2.lock().unwrap().push(e.clone())));
        (d, log)
    }

    fn reg(d: &Daemon, c: &ConnCtx, secret: Option<&str>) -> Value {
        let params = secret.map(|s| json!({"secret": s})).unwrap_or(json!({}));
        call_from(d, c, "consent.handler.register", params)
    }

    #[test]
    fn a_rogue_client_that_registers_first_gets_nothing_and_cannot_approve() {
        let dir = scratch("rogue");
        let (d, log) = spawned_daemon();
        let me = my_uid();
        let (rogue, mut rogue_in) = conn(1, me);
        // no secret, a wrong secret, an empty one: all refused, and the daemon is not a "handler" afterwards
        assert_eq!(reg(&d, &rogue, None)["error"]["code"], NOT_HANDLER);
        assert_eq!(reg(&d, &rogue, Some("wrong"))["error"]["code"], NOT_HANDLER);
        assert_eq!(reg(&d, &rogue, Some(""))["error"]["code"], NOT_HANDLER);
        assert_eq!(reg(&d, &rogue, Some("0123456789abcdef0123456789abcdeX"))["error"]["code"], NOT_HANDLER, "one wrong byte");
        assert_eq!(call_from(&d, &rogue, "capabilities", json!({}))["result"]["consent_handler"], false);
        // a request is held: the code goes to the terminal (not silenced), never to the rogue
        let params = start_params(&dir.join("a.mp4"));
        assert_eq!(start(&d, &params)["error"]["code"], CONSENT_REQUIRED);
        let token = token_of(&log);
        rogue.out.lock().unwrap().set_nonblocking(true).unwrap();
        rogue_in.get_ref().set_nonblocking(true).unwrap();
        let mut line = String::new();
        assert!(rogue_in.read_line(&mut line).is_err() || line.is_empty(), "nothing was pushed to the rogue: {line:?}");
        // the rogue cannot approve or deny with the code it somehow got; the terminal flow still works
        assert!(
            call_from(&d, &rogue, "consent.approve", json!({"token": token}))["result"]["approved"] == true,
            "no handler: the code is the credential, as on the terminal"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_app_with_the_secret_registers_and_a_rogue_cannot_take_over_afterwards() {
        let (d, _log) = spawned_daemon();
        let me = my_uid();
        let (rogue, _r) = conn(1, me);
        let (app, mut app_in) = conn(2, me);
        // the rogue came first and failed; the app then registers with the secret
        assert_eq!(reg(&d, &rogue, Some("nope"))["error"]["code"], NOT_HANDLER);
        assert_eq!(reg(&d, &app, Some(SECRET))["result"]["registered"], true);
        // a second registration is refused even with the right secret (one handler at a time)
        assert_eq!(reg(&d, &rogue, Some(SECRET))["error"]["code"], NOT_HANDLER);
        let dir = scratch("secret-app");
        assert_eq!(start(&d, &start_params(&dir.join("a.mp4")))["error"]["code"], CONSENT_REQUIRED);
        let ev = next_event(&mut app_in);
        let token = ev["token"].as_str().unwrap().to_string();
        assert_eq!(call_from(&d, &rogue, "consent.approve", json!({"token": token}))["error"]["code"], NOT_HANDLER);
        assert_eq!(call_from(&d, &app, "consent.approve", json!({"token": token}))["result"]["approved"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_secret_is_never_in_replies_or_events() {
        let (d, _log) = spawned_daemon();
        let (app, _a) = conn(1, my_uid());
        let r = reg(&d, &app, Some(SECRET));
        assert!(!r.to_string().contains(SECRET));
        assert!(!call_from(&d, &app, "capabilities", json!({})).to_string().contains(SECRET));
        let (bad, _b) = conn(2, my_uid());
        assert!(!reg(&d, &bad, Some("x")).to_string().contains(SECRET));
    }

    #[test]
    fn a_daemon_without_a_secret_has_no_handler_whatever_the_peer_presents_or_runs() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l2 = log.clone();
        // started by systemd or by hand: nobody gave it a secret, so nobody can be its handler
        let d = Daemon::with_bus(Some("unix:path=/nonexistent/kalango-cast-test-bus".into()))
            .with_hook(Box::new(move |e| l2.lock().unwrap().push(e.clone())));
        let (c, _c_in) = conn(1, my_uid());
        assert_eq!(reg(&d, &c, None)["error"]["code"], NOT_HANDLER);
        assert_eq!(reg(&d, &c, Some(SECRET))["error"]["code"], NOT_HANDLER);
        let dir = scratch("unspawned");
        assert_eq!(start(&d, &start_params(&dir.join("a.mp4")))["error"]["code"], CONSENT_REQUIRED);
        assert_eq!(log.lock().unwrap().len(), 1, "the terminal notice stands");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_daemon_says_ready_first_and_the_secret_is_read_after_it() {
        use std::os::fd::IntoRawFd;
        let (daemon_end, app_end) = UnixStream::pair().unwrap();
        let fd = daemon_end.into_raw_fd();
        let t = std::thread::spawn(move || receive_secret(fd, Duration::from_secs(5)));
        let mut app_end = app_end;
        app_end.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // the app waits for the ready byte; only then does the secret exist on the wire
        let mut b = [0u8; 1];
        app_end.read_exact(&mut b).unwrap();
        assert_eq!(b[0], READY);
        app_end.write_all(SECRET.as_bytes()).unwrap();
        app_end.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(t.join().unwrap().unwrap(), SECRET);
    }

    #[test]
    fn a_missing_short_or_endless_secret_is_an_error_and_the_wait_is_bounded() {
        use std::os::fd::IntoRawFd;
        // the app never answers: the daemon gives up
        let (d, _app) = UnixStream::pair().unwrap();
        let start = Instant::now();
        assert!(receive_secret(d.into_raw_fd(), Duration::from_millis(200)).is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        // too short
        let (d, mut app) = UnixStream::pair().unwrap();
        app.write_all(b"short").unwrap();
        drop(app);
        assert!(receive_secret(d.into_raw_fd(), Duration::from_secs(1)).is_err());
        // too long (never ends)
        let (d, mut app) = UnixStream::pair().unwrap();
        app.write_all(&[b'a'; 300]).unwrap();
        assert!(receive_secret(d.into_raw_fd(), Duration::from_secs(1)).is_err());
        let mut buf = *b"secret!!";
        zeroize(&mut buf);
        assert_eq!(buf, [0u8; 8]);
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn a_handler_that_dies_falls_back_to_the_terminal_even_with_a_secret() {
        let dir = scratch("secret-death");
        let (d, log) = spawned_daemon();
        let (app, app_in) = conn(1, my_uid());
        assert_eq!(reg(&d, &app, Some(SECRET))["result"]["registered"], true);
        d.connection_closed(1);
        drop(app_in);
        assert_eq!(start(&d, &start_params(&dir.join("a.mp4")))["error"]["code"], CONSENT_REQUIRED);
        assert_eq!(log.lock().unwrap().len(), 1, "the terminal has the code again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_error_and_the_pushed_request_carry_the_same_id_so_a_ui_can_tell_whose_it_is() {
        let dir = scratch("request-id");
        let (d, _log) = daemon(Consent::Ask);
        let (ui, mut ui_in) = conn(1, my_uid());
        call_from(&d, &ui, "consent.handler.register", json!({"secret": SECRET}));
        let r = start(&d, &start_params(&dir.join("a.mp4")));
        let ev = next_event(&mut ui_in);
        assert_eq!(r["error"]["code"], CONSENT_REQUIRED);
        assert_eq!(r["error"]["message"], "consent_required");
        let id = r["error"]["data"]["request_id"].as_str().unwrap();
        assert_eq!(id.len(), 16);
        assert_eq!(ev["request_id"], id);
        assert!(!r.to_string().contains(ev["token"].as_str().unwrap()), "the id is not the code");
        // asking again for the same thing: same id, no new event
        let again = start(&d, &start_params(&dir.join("a.mp4")));
        assert_eq!(again["error"]["data"]["request_id"], id);
        // a different request has a different id
        let other = start(&d, &start_params(&dir.join("b.mp4")));
        assert_ne!(other["error"]["data"]["request_id"], id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacing_a_held_request_is_announced_not_silent() {
        let dir = scratch("superseded");
        let (d, log) = daemon(Consent::Ask);
        let (ui, mut ui_in) = conn(1, my_uid());
        call_from(&d, &ui, "consent.handler.register", json!({"secret": SECRET}));
        start(&d, &start_params(&dir.join("a.mp4")));
        let first = next_event(&mut ui_in);
        let r = start(&d, &start_params(&dir.join("b.mp4")));
        let gone = next_event(&mut ui_in);
        assert_eq!(gone["type"], "consent_superseded");
        assert_eq!(gone["request_id"], first["request_id"]);
        let second = next_event(&mut ui_in);
        assert_eq!(second["type"], "consent_requested");
        assert_eq!(second["request_id"], r["error"]["data"]["request_id"]);
        // the dead request's code no longer approves
        let old = first["token"].as_str().unwrap();
        assert_eq!(call_from(&d, &ui, "consent.approve", json!({"token": old}))["error"]["code"], -32002);
        assert!(log.lock().unwrap().is_empty(), "with a handler the terminal hears nothing");
        // without a handler the terminal is told too
        let (d2, log2) = daemon(Consent::Ask);
        start(&d2, &start_params(&dir.join("a.mp4")));
        start(&d2, &start_params(&dir.join("b.mp4")));
        let l = log2.lock().unwrap();
        assert!(matches!(l[1], Event::ConsentSuperseded { .. }), "{l:?}");
        assert_eq!(l.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
