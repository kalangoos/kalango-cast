//! `kalango-castd`'s control channel: JSON-RPC 2.0, one request per line, over a Unix socket at
//! `$XDG_RUNTIME_DIR/kalango/cast.sock` (mode 0600 in a 0700 folder). Every connection's peer is checked with
//! `SO_PEERCRED`: only processes of the same user are served (kalangod, which runs agents' requests after the
//! person's approval, talks to it as that user).
//!
//! Methods: `capabilities`, `sources.list`, `record.start` {source, path, encoder?, codec?, fps?, bitrate_kbps?,
//! desktop_audio?, mic?, overwrite?}, `record.stop`, `record.status`.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cast_core::{tr, Source};
use serde_json::{json, Value};

use crate::recorder::{AudioChoice, RecordOptions, Recorder};

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

fn my_uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// Creates the folder (0700, ours) and binds the socket (0600). A stale socket is replaced; a folder owned by
/// someone else or open to others is refused.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("socket path"))?;
    if !dir.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let meta = std::fs::metadata(dir)?;
    if meta.uid() != my_uid() || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!("{}: {}", dir.display(), tr("the folder must be yours and closed to others (0700)"))));
    }
    if path.exists() {
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

/// The daemon's state: at most one recording.
#[derive(Default)]
pub struct Daemon {
    recording: Mutex<Option<Recorder>>,
    pub dbus_address: Option<String>,
}

impl Daemon {
    /// A daemon that talks to the session bus at `dbus_address` (the default bus when `None`).
    pub fn with_bus(dbus_address: Option<String>) -> Self {
        Self { recording: Mutex::new(None), dbus_address }
    }
}

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

impl Daemon {
    /// Answers one JSON-RPC request (a notification without id gets no answer).
    pub fn handle(&self, line: &str) -> Option<Value> {
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(error(&Value::Null, -32700, format!("{}: {e}", tr("Invalid JSON")))),
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));
        let result = self.call(method, &params);
        let id = id?;
        Some(match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err((code, msg)) => error(&id, code, msg),
        })
    }

    fn call(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        let app = |e: String| (-32000, e);
        match method {
            "capabilities" => Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "encoders": cast_encode::probe(),
                "audio": {"codec": "opus", "sample_rate": 48000, "channels": 2},
                "container": "mp4 (fragmented)",
            })),
            "sources.list" => {
                let cast = cast_capture::ScreenCast::connect(self.dbus_address.as_deref()).map_err(|e| app(e.0))?;
                let s = cast.sources().map_err(|e| app(e.0))?;
                serde_json::to_value(s).map_err(|e| app(e.to_string()))
            }
            "record.start" => {
                let mut slot = self.recording.lock().map_err(|_| app("lock".into()))?;
                if slot.is_some() {
                    return Err(app(tr("A recording is already running").into()));
                }
                let source: Source =
                    serde_json::from_value(params.get("source").cloned().unwrap_or(Value::Null)).map_err(|e| (-32602, format!("source: {e}")))?;
                let path = PathBuf::from(params.get("path").and_then(|p| p.as_str()).ok_or((-32602, "path".to_string()))?);
                if path.exists() && !params.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(false) {
                    return Err(app(tr("The file already exists").into()));
                }
                let mut o = RecordOptions::new(source, path);
                if let Some(e) = params.get("encoder").and_then(|v| v.as_str()) {
                    o.encoder = Some(cast_core::EncoderKind::parse(e).ok_or((-32602, format!("encoder: {e}")))?);
                }
                if let Some(c) = params.get("codec") {
                    o.codec = serde_json::from_value(c.clone()).map_err(|e| (-32602, format!("codec: {e}")))?;
                }
                if let Some(f) = params.get("fps").and_then(|v| v.as_u64()) {
                    o.fps = f.clamp(1, 240) as u32;
                }
                if let Some(b) = params.get("bitrate_kbps").and_then(|v| v.as_u64()) {
                    o.bitrate_kbps = b.min(500_000) as u32;
                }
                o.desktop_audio = audio_choice(params.get("desktop_audio"));
                o.mic = audio_choice(params.get("mic"));
                o.dbus_address = self.dbus_address.clone();
                let r = Recorder::start(o).map_err(app)?;
                let status = r.status();
                *slot = Some(r);
                serde_json::to_value(status).map_err(|e| app(e.to_string()))
            }
            "record.status" => {
                let slot = self.recording.lock().map_err(|_| app("lock".into()))?;
                match slot.as_ref() {
                    Some(r) => serde_json::to_value(r.status()).map_err(|e| app(e.to_string())),
                    None => Ok(json!({"recording": false})),
                }
            }
            "record.stop" => {
                let r = self.recording.lock().map_err(|_| app("lock".into()))?.take().ok_or_else(|| app(tr("No recording is running").into()))?;
                let (summary, status) = r.stop().map_err(app)?;
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

    /// Serves connections until the listener fails. Each connection gets a thread.
    pub fn serve(self: Arc<Self>, listener: UnixListener) {
        let me = my_uid();
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            if peer_uid(&conn) != Some(me) {
                // another user (or unknown): closed without a word
                continue;
            }
            let d = self.clone();
            std::thread::spawn(move || d.client(conn));
        }
    }

    fn client(&self, conn: UnixStream) {
        let Ok(mut out) = conn.try_clone() else { return };
        for line in BufReader::new(conn).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle(&line) {
                if writeln!(out, "{reply}").is_err() {
                    break;
                }
            }
        }
    }

    /// Stops a running recording (the daemon is exiting).
    pub fn shutdown(&self) {
        if let Some(r) = self.recording.lock().ok().and_then(|mut s| s.take()) {
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
    fn the_socket_is_private_and_checks_the_peer() {
        let dir = std::env::temp_dir().join(format!("rpc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
    }
}
