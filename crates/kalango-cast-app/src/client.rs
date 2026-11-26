//! The app's side of `kalango-castd`'s JSON-RPC socket: one-shot calls (a connection each, with time limits) and the
//! long-lived event connection, which registers the app as the daemon's consent handler and turns the daemon's
//! pushed notifications into model messages.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_channel::Sender;
use serde_json::{json, Value};

use crate::model::{ConsentRequest, Msg};

/// Why a call failed.
#[derive(Clone, Debug, PartialEq)]
pub enum CallError {
    /// The socket could not be reached or did not answer in time (the service is not there).
    Transport(String),
    /// The service answered with a JSON-RPC error.
    Rpc { code: i64, message: String, request_id: Option<String> },
}

impl CallError {
    pub fn message(&self) -> String {
        match self {
            CallError::Transport(m) => m.clone(),
            CallError::Rpc { message, .. } => message.clone(),
        }
    }
}

/// The socket's default path (`$XDG_RUNTIME_DIR/kalango/cast.sock`), as the daemon uses.
pub fn default_socket() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(dir).join("kalango").join("cast.sock"))
}

/// One request on its own connection, with `timeout` for connecting-to-answer.
pub fn call(socket: &Path, method: &str, params: Value, timeout: Duration) -> Result<Value, CallError> {
    let transport = |e: std::io::Error| CallError::Transport(e.to_string());
    let mut s = UnixStream::connect(socket).map_err(transport)?;
    s.set_read_timeout(Some(timeout)).map_err(transport)?;
    s.set_write_timeout(Some(timeout)).map_err(transport)?;
    writeln!(s, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).map_err(transport)?;
    let mut line = String::new();
    let n = BufReader::new(s).read_line(&mut line).map_err(transport)?;
    if n == 0 {
        return Err(CallError::Transport("the service closed the connection".into()));
    }
    parse_reply(&line)
}

/// Splits a JSON-RPC reply line into its result or its error.
pub fn parse_reply(line: &str) -> Result<Value, CallError> {
    let v: Value = serde_json::from_str(line).map_err(|e| CallError::Transport(format!("invalid reply: {e}")))?;
    if let Some(err) = v.get("error") {
        return Err(CallError::Rpc {
            code: err["code"].as_i64().unwrap_or(-32000),
            message: err["message"].as_str().unwrap_or("error").to_string(),
            request_id: err["data"]["request_id"].as_str().map(str::to_string),
        });
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// Turns a line the daemon pushed (or a reply on the event connection) into a model message.
/// `Some(msg)` for the ones the model cares about.
pub fn event_to_msg(line: &str, now: Instant) -> Option<Msg> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["method"] == "event" {
        let p = &v["params"];
        return match p["type"].as_str()? {
            "consent_superseded" => Some(Msg::ConsentSuperseded { request_id: p["request_id"].as_str()?.to_string() }),
            "consent_requested" => Some(Msg::ConsentRequested(ConsentRequest {
                request_id: p["request_id"].as_str()?.to_string(),
                what: p["what"].as_str()?.to_string(),
                token: p["token"].as_str()?.to_string(),
                received: now,
                ttl: Duration::from_secs(p["ttl_s"].as_u64().unwrap_or(120)),
            })),
            "recording_failed" => Some(Msg::RecordingFailed {
                path: p["path"].as_str().unwrap_or("").to_string(),
                error: p["error"].as_str().unwrap_or("").to_string(),
            }),
            // started/stopped are known from our own calls and from the status poll
            _ => None,
        };
    }
    match v["id"].as_u64()? {
        REGISTER_ID => Some(match parse_reply(line) {
            Ok(_) => Msg::HandlerRegistered,
            Err(e) => Msg::HandlerUnavailable(e.message()),
        }),
        APPROVE_ID => Some(match parse_reply(line) {
            Ok(_) => Msg::Approved,
            Err(e) => Msg::ApproveFailed(e.message()),
        }),
        _ => None,
    }
}

const REGISTER_ID: u64 = 1;
const APPROVE_ID: u64 = 2;
const DENY_ID: u64 = 3;

/// The registered connection: approvals and denials go out on it (the daemon accepts them from it only).
#[derive(Clone)]
pub struct EventLink {
    out: Arc<Mutex<Option<UnixStream>>>,
}

impl EventLink {
    fn send(&self, id: u64, method: &str, token: &str) -> Result<(), String> {
        let mut slot = self.out.lock().map_err(|_| "lock".to_string())?;
        let s = slot.as_mut().ok_or_else(|| cast_core::tr("not connected to the service").to_string())?;
        writeln!(s, "{}", json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {"token": token}})).map_err(|e| e.to_string())
    }

    pub fn approve(&self, token: &str) -> Result<(), String> {
        self.send(APPROVE_ID, "consent.approve", token)
    }

    pub fn deny(&self, token: &str) -> Result<(), String> {
        self.send(DENY_ID, "consent.deny", token)
    }
}

/// Starts the event connection thread: connects, registers as the consent handler, forwards what the daemon pushes;
/// reconnects (every 2 s) when the connection drops. Messages go to `tx` (the main loop).
pub fn spawn_events(socket: PathBuf, tx: Sender<Msg>, secret: Arc<Mutex<Option<String>>>) -> EventLink {
    let link = EventLink { out: Arc::new(Mutex::new(None)) };
    let shared = link.out.clone();
    let _ = std::thread::Builder::new().name("cast-events".into()).spawn(move || loop {
        match UnixStream::connect(&socket) {
            Ok(s) => {
                let Ok(mut w) = s.try_clone() else { continue };
                // the secret of the daemon this app spawned (memory only); none for a daemon someone else started
                let params = match secret.lock().ok().and_then(|s| s.clone()) {
                    Some(s) => json!({"secret": s}),
                    None => json!({}),
                };
                let registered =
                    writeln!(w, "{}", json!({"jsonrpc": "2.0", "id": REGISTER_ID, "method": "consent.handler.register", "params": params})).is_ok();
                if let Ok(mut slot) = shared.lock() {
                    *slot = Some(w);
                }
                if registered {
                    for line in BufReader::new(s).lines() {
                        let Ok(line) = line else { break };
                        if let Some(m) = event_to_msg(&line, Instant::now()) {
                            // a refused registration ends this connection (it is retried, with the secret if it came late)
                            let refused = matches!(m, Msg::HandlerUnavailable(_));
                            if tx.send_blocking(m).is_err() {
                                return;
                            }
                            if refused {
                                break;
                            }
                        }
                    }
                }
                if let Ok(mut slot) = shared.lock() {
                    *slot = None;
                }
                if tx.send_blocking(Msg::HandlerUnavailable(cast_core::tr("not connected to the service").to_string())).is_err() {
                    return;
                }
            }
            Err(_) => {
                if tx.is_closed() {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(700));
    });
    link
}

#[cfg(test)]
mod tests {
    use super::*;
    use cast_engine::rpc::{self, Consent, Daemon};
    use std::sync::Arc;

    fn serve(name: &str, hook_log: Arc<Mutex<Vec<String>>>) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kalango-cast-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sock = dir.join("kalango").join("cast.sock");
        let listener = rpc::bind(&sock).unwrap();
        let d = Arc::new(
            Daemon::with_bus(Some("unix:path=/nonexistent/kalango-cast-app-bus".into()))
                .with_consent(Consent::Ask)
                .with_dev_handler(true)
                .with_hook(Box::new(move |e| hook_log.lock().unwrap().push(format!("{e:?}")))),
        );
        std::thread::spawn(move || d.serve(listener));
        (dir, sock)
    }

    fn start_params(dir: &Path) -> Value {
        json!({"source": {"kind": "monitor", "connector": "DP-1"}, "path": dir.join("a.mp4")})
    }

    #[test]
    fn replies_are_split_into_results_and_errors() {
        assert_eq!(parse_reply(r#"{"jsonrpc":"2.0","id":1,"result":{"a":1}}"#).unwrap()["a"], 1);
        assert_eq!(
            parse_reply(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"consent_required","data":{"request_id":"abc123"}}}"#),
            Err(CallError::Rpc { code: -32001, message: "consent_required".into(), request_id: Some("abc123".into()) })
        );
        assert!(matches!(parse_reply("not json"), Err(CallError::Transport(_))));
    }

    #[test]
    fn pushed_lines_become_model_messages() {
        let now = Instant::now();
        let line = r#"{"jsonrpc":"2.0","method":"event","params":{"type":"consent_requested","request_id":"rid","what":"source: x","token":"abc","ttl_s":90}}"#;
        match event_to_msg(line, now) {
            Some(Msg::ConsentRequested(r)) => assert_eq!((r.what.as_str(), r.token.as_str(), r.ttl), ("source: x", "abc", Duration::from_secs(90))),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            event_to_msg(r#"{"method":"event","params":{"type":"recording_failed","path":"/a","error":"e"}}"#, now),
            Some(Msg::RecordingFailed { .. })
        ));
        assert!(event_to_msg(r#"{"method":"event","params":{"type":"recording_started","path":"/a"}}"#, now).is_none());
        assert!(matches!(event_to_msg(r#"{"id":1,"result":{"registered":true}}"#, now), Some(Msg::HandlerRegistered)));
        assert!(
            matches!(event_to_msg(r#"{"id":1,"error":{"code":-32003,"message":"taken"}}"#, now), Some(Msg::HandlerUnavailable(m)) if m == "taken")
        );
        assert!(matches!(event_to_msg(r#"{"id":2,"result":{"approved":true}}"#, now), Some(Msg::Approved)));
        assert!(matches!(event_to_msg(r#"{"id":2,"error":{"code":-32002,"message":"late"}}"#, now), Some(Msg::ApproveFailed(m)) if m == "late"));
        assert!(event_to_msg(r#"{"id":3,"result":{"denied":true}}"#, now).is_none());
        assert!(event_to_msg("garbage", now).is_none());
    }

    #[test]
    fn a_missing_service_is_a_transport_error() {
        let r = call(Path::new("/nonexistent/kalango/cast.sock"), "capabilities", json!({}), Duration::from_secs(1));
        assert!(matches!(r, Err(CallError::Transport(_))), "{r:?}");
    }

    /// The whole consent loop against the real daemon code: the UI registers, a start is held, the code reaches the
    /// UI only (not the terminal hook), the UI approves, and the same start then passes the consent step.
    #[test]
    fn the_app_registers_receives_the_code_and_approves() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (dir, sock) = serve("consent", log.clone());
        let (tx, rx) = async_channel::unbounded();
        let link = spawn_events(sock.clone(), tx, Arc::new(Mutex::new(None)));
        let next = || {
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(m) = rx.try_recv() {
                    return m;
                }
                assert!(Instant::now() < end, "no message from the daemon");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        assert!(matches!(next(), Msg::HandlerRegistered));
        let params = start_params(&dir);
        let r = call(&sock, "record.start", params.clone(), Duration::from_secs(5));
        assert!(matches!(r, Err(CallError::Rpc { code: -32001, .. })), "{r:?}");
        let Msg::ConsentRequested(req) = next() else { panic!("expected the consent request") };
        assert!(req.what.contains("file: \"a.mp4\""));
        assert!(log.lock().unwrap().is_empty(), "the code stayed off the terminal");
        // another client cannot approve with the code; we can
        let other = call(&sock, "consent.approve", json!({"token": req.token}), Duration::from_secs(5));
        assert!(matches!(other, Err(CallError::Rpc { code: -32003, .. })), "{other:?}");
        link.approve(&req.token).unwrap();
        assert!(matches!(next(), Msg::Approved));
        let r = call(&sock, "record.start", params, Duration::from_secs(5));
        assert!(!matches!(r, Err(CallError::Rpc { code: -32001, .. })), "approved: {r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_daemon_with_a_secret_only_accepts_the_app_that_holds_it() {
        let dir = std::env::temp_dir().join(format!("kalango-cast-app-{}-secret", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sock = dir.join("kalango").join("cast.sock");
        let listener = rpc::bind(&sock).unwrap();
        let d = Arc::new(
            Daemon::with_bus(Some("unix:path=/nonexistent/kalango-cast-app-bus".into()))
                .with_dev_handler(true)
                .with_handler_secret("s3cret-s3cret-s3cret-s3cret".into()),
        );
        std::thread::spawn(move || d.serve(listener));
        // an app without the secret (a rogue, or one that started before the daemon's secret was known) is refused
        let (tx, rx) = async_channel::unbounded();
        let slot = Arc::new(Mutex::new(None));
        let _link = spawn_events(sock.clone(), tx, slot.clone());
        assert!(matches!(rx.recv_blocking().unwrap(), Msg::HandlerUnavailable(_)));
        // the secret arrives: the retry registers
        *slot.lock().unwrap() = Some("s3cret-s3cret-s3cret-s3cret".into());
        loop {
            match rx.recv_blocking().unwrap() {
                Msg::HandlerRegistered => break,
                Msg::HandlerUnavailable(_) => continue,
                other => panic!("{other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn denying_goes_out_on_the_registered_connection() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (dir, sock) = serve("deny", log);
        let (tx, rx) = async_channel::unbounded();
        let link = spawn_events(sock.clone(), tx, Arc::new(Mutex::new(None)));
        assert!(matches!(rx.recv_blocking().unwrap(), Msg::HandlerRegistered));
        let params = start_params(&dir);
        let _ = call(&sock, "record.start", params.clone(), Duration::from_secs(5));
        let Msg::ConsentRequested(req) = rx.recv_blocking().unwrap() else { panic!() };
        link.deny(&req.token).unwrap();
        // the denied code is dead: approving it now fails
        std::thread::sleep(Duration::from_millis(200));
        link.approve(&req.token).unwrap();
        assert!(matches!(rx.recv_blocking().unwrap(), Msg::ApproveFailed(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
