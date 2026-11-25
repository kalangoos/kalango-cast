//! The compositor's ScreenCast API (`org.gnome.Mutter.ScreenCast`, implemented by niri), as the KalangoOS shell
//! uses it (crates/kalango-shell/src/screen_share.rs): CreateSession → RecordMonitor / RecordWindow → wait for the
//! stream's `PipeWireStreamAdded` (the PipeWire node) → Start. The list of sources comes from
//! `org.gnome.Mutter.DisplayConfig` (monitors) and `org.gnome.Shell.Introspect` (windows), which niri also serves.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use serde::Serialize;
use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

use crate::CaptureError;

const MUTTER: &str = "org.gnome.Mutter.ScreenCast";
const MUTTER_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SESSION_IFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const STREAM_IFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";

/// niri's cursor modes: 0 hidden, 1 drawn into the frames, 2 as metadata.
pub const CURSOR_EMBEDDED: u32 = 1;

#[derive(Clone, Debug, Serialize)]
pub struct Monitor {
    pub connector: String,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub refresh_hz: f64,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub id: u64,
    pub title: String,
    pub app_id: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Sources {
    pub monitors: Vec<Monitor>,
    pub windows: Vec<Window>,
}

/// A started screencast: its PipeWire node, and its place and size (logical pixels) as the compositor says.
#[derive(Debug)]
pub struct Stream {
    pub node_id: u32,
    pub position: (i32, i32),
    pub size: (i32, i32),
    session: OwnedObjectPath,
    conn: Connection,
}

impl Stream {
    /// Stops the compositor's session (also done on drop).
    pub fn stop(&self) {
        let _ = self.conn.call_method(Some(MUTTER), self.session.as_str(), Some(SESSION_IFACE), "Stop", &());
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A connection to the session bus of the compositor.
pub struct ScreenCast {
    conn: Connection,
}

type MonitorSpec = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type MonitorInfo = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, HashMap<String, OwnedValue>);
type CurrentState = (u32, Vec<MonitorInfo>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>);

impl ScreenCast {
    /// The session bus (`DBUS_SESSION_BUS_ADDRESS`), or the bus at `address`.
    pub fn connect(address: Option<&str>) -> Result<ScreenCast, CaptureError> {
        let conn = match address {
            Some(a) => zbus::blocking::connection::Builder::address(a)?.build()?,
            None => Connection::session()?,
        };
        Ok(ScreenCast { conn })
    }

    /// Monitors and windows that can be recorded.
    pub fn sources(&self) -> Result<Sources, CaptureError> {
        let reply = self.conn.call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )?;
        let (_, monitors, logical, _): CurrentState = reply.body().deserialize()?;
        let mut out = Sources::default();
        for ((connector, _, _, _), modes, props) in monitors {
            let current = modes.iter().find(|m| m.6.get("is-current").and_then(|v| v.downcast_ref::<bool>().ok()).unwrap_or(false)).or(modes.first());
            let (width, height, refresh_hz) = current.map(|m| (m.1, m.2, m.3)).unwrap_or_default();
            let name = props.get("display-name").and_then(|v| v.downcast_ref::<&str>().ok().map(String::from)).unwrap_or_else(|| connector.clone());
            let place = logical.iter().find(|l| l.5.iter().any(|s| s.0 == connector));
            let (x, y, scale) = place.map(|l| (l.0, l.1, l.2)).unwrap_or((0, 0, 1.0));
            out.monitors.push(Monitor { connector, name, width, height, refresh_hz, x, y, scale });
        }
        // windows: optional (a compositor without Introspect still records monitors)
        if let Ok(reply) = self.conn.call_method(
            Some("org.gnome.Shell.Introspect"),
            "/org/gnome/Shell/Introspect",
            Some("org.gnome.Shell.Introspect"),
            "GetWindows",
            &(),
        ) {
            if let Ok(windows) = reply.body().deserialize::<HashMap<u64, HashMap<String, OwnedValue>>>() {
                let text = |m: &HashMap<String, OwnedValue>, k: &str| {
                    m.get(k).and_then(|v| v.downcast_ref::<&str>().ok().map(String::from)).unwrap_or_default()
                };
                let mut list: Vec<_> = windows.iter().map(|(id, p)| Window { id: *id, title: text(p, "title"), app_id: text(p, "app-id") }).collect();
                list.sort_by_key(|w| w.id);
                out.windows = list;
            }
        }
        Ok(out)
    }

    fn create_session(&self) -> Result<OwnedObjectPath, CaptureError> {
        let props: HashMap<&str, Value> = HashMap::new();
        let reply = self.conn.call_method(Some(MUTTER), MUTTER_PATH, Some(MUTTER), "CreateSession", &(props,))?;
        Ok(reply.body().deserialize::<OwnedObjectPath>()?)
    }

    /// Records a monitor (by connector).
    pub fn record_monitor(&self, connector: &str, cursor_mode: u32) -> Result<Stream, CaptureError> {
        let session = self.create_session()?;
        let props: HashMap<&str, Value> = HashMap::from([("cursor-mode", Value::from(cursor_mode))]);
        let reply = self.conn.call_method(Some(MUTTER), session.as_str(), Some(SESSION_IFACE), "RecordMonitor", &(connector, props))?;
        self.start(session, reply.body().deserialize::<OwnedObjectPath>()?)
    }

    /// Records one window (by the compositor's window id).
    pub fn record_window(&self, window_id: u64, cursor_mode: u32) -> Result<Stream, CaptureError> {
        let session = self.create_session()?;
        let props: HashMap<&str, Value> = HashMap::from([("window-id", Value::from(window_id)), ("cursor-mode", Value::from(cursor_mode))]);
        let reply = self.conn.call_method(Some(MUTTER), session.as_str(), Some(SESSION_IFACE), "RecordWindow", &(props,))?;
        self.start(session, reply.body().deserialize::<OwnedObjectPath>()?)
    }

    /// Starts the session and waits for the stream's PipeWire node (6 s, as the shell does).
    fn start(&self, session: OwnedObjectPath, stream: OwnedObjectPath) -> Result<Stream, CaptureError> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(STREAM_IFACE)?
            .member("PipeWireStreamAdded")?
            .path(stream.as_str())?
            .build();
        let iter = MessageIterator::for_match_rule(rule, &self.conn, Some(4))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for msg in iter.flatten() {
                if let Ok(node) = msg.body().deserialize::<u32>() {
                    let _ = tx.send(node);
                    break;
                }
            }
        });
        self.conn.call_method(Some(MUTTER), session.as_str(), Some(SESSION_IFACE), "Start", &())?;
        let stop = || {
            let _ = self.conn.call_method(Some(MUTTER), session.as_str(), Some(SESSION_IFACE), "Stop", &());
        };
        let node_id = match rx.recv_timeout(Duration::from_secs(6)) {
            Ok(n) => n,
            Err(_) => {
                stop();
                return Err(CaptureError(cast_core::tr("The compositor did not give the stream.").into()));
            }
        };
        let params = self
            .conn
            .call_method(Some(MUTTER), stream.as_str(), Some("org.freedesktop.DBus.Properties"), "Get", &(STREAM_IFACE, "Parameters"))
            .ok()
            .and_then(|r| r.body().deserialize::<OwnedValue>().ok())
            .and_then(|v| HashMap::<String, OwnedValue>::try_from(v).ok())
            .unwrap_or_default();
        let pair = |k: &str| {
            let s = params.get(k)?.downcast_ref::<zbus::zvariant::Structure>().ok()?;
            let f = s.fields();
            Some((i32::try_from(f.first()?).ok()?, i32::try_from(f.get(1)?).ok()?))
        };
        let (position, size) = (pair("position").unwrap_or((0, 0)), pair("size").unwrap_or((0, 0)));
        Ok(Stream { node_id, position, size, session, conn: self.conn.clone() })
    }
}
