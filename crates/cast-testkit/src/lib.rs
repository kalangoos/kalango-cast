//! Test tools for Kalango Cast (not shipped):
//!
//! - [`NestedNiri`]: a niri session in a window of the current X display (DISPLAY=:0), with its own D-Bus session bus,
//!   so recordings are tested against the real compositor's ScreenCast. Needs a niri build that can screencast in a
//!   window (KalangoOS's patch, `tests/niri/`), given by `KALANGO_CAST_NIRI`.
//! - [`NullSink`]: a PipeWire null sink for test sound (nothing reaches the speakers, nothing is captured from a real
//!   microphone).
//! - [`analysis`]: finds the flashes and beeps of a clap recording (via ffmpeg) and the A/V offset.
//! - `kalango-cast-clap`: the Wayland client that flashes and beeps (or animates, for benchmarks).
//! - `kalango-cast-bench`: Kalango Cast vs the shell's gst-launch recorder.

pub mod analysis;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The niri binary to test with (`KALANGO_CAST_NIRI`).
pub fn niri_binary() -> Option<PathBuf> {
    std::env::var_os("KALANGO_CAST_NIRI").map(PathBuf::from).filter(|p| p.is_file())
}

/// A niri session in a window (winit backend) on DISPLAY=:0, its own session bus and Wayland socket.
pub struct NestedNiri {
    child: Child,
    pub dir: PathBuf,
    pub dbus_address: String,
    pub wayland_display: String,
    pub xdg_runtime_dir: String,
}

const CONFIG: &str = r#"
input {
    keyboard {
        xkb {
            layout "us"
        }
    }
}
prefer-no-csd
debug {
    dbus-interfaces-in-non-session-instances
}
animations {
    off
}
hotkey-overlay {
    skip-at-startup
}
layout {
    gaps 0
    focus-ring {
        off
    }
    border {
        off
    }
}
spawn-at-startup "sh" "-c" "env > @ENV@.part && mv @ENV@.part @ENV@"
"#;

impl NestedNiri {
    /// Starts niri (`width` × `height` window) and waits for its session to be up.
    pub fn start(niri: &Path, dir: &Path, width: u32, height: u32) -> std::io::Result<NestedNiri> {
        std::fs::create_dir_all(dir)?;
        let env_file = dir.join("session.env");
        let _ = std::fs::remove_file(&env_file);
        let config = dir.join("niri.kdl");
        std::fs::write(&config, CONFIG.replace("@ENV@", &env_file.display().to_string()))?;
        let log = std::fs::File::create(dir.join("niri.log"))?;
        let mut cmd = Command::new("dbus-run-session");
        cmd.arg("--")
            .arg(niri)
            .arg("--config")
            .arg(&config)
            .env("DISPLAY", std::env::var("KALANGO_CAST_DISPLAY").unwrap_or_else(|_| ":0".into()))
            .env_remove("WAYLAND_DISPLAY")
            .env("KALANGO_NIRI_WINIT_SIZE", format!("{width}x{height}"))
            .env("RUST_LOG", "niri=info")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // its own process group: stopping the session stops everything in it
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut n =
            NestedNiri { child, dir: dir.to_path_buf(), dbus_address: String::new(), wayland_display: String::new(), xdg_runtime_dir: String::new() };
        loop {
            if let Ok(text) = std::fs::read_to_string(&env_file) {
                for line in text.lines() {
                    if let Some(v) = line.strip_prefix("DBUS_SESSION_BUS_ADDRESS=") {
                        n.dbus_address = v.into();
                    } else if let Some(v) = line.strip_prefix("WAYLAND_DISPLAY=") {
                        n.wayland_display = v.into();
                    } else if let Some(v) = line.strip_prefix("XDG_RUNTIME_DIR=") {
                        n.xdg_runtime_dir = v.into();
                    }
                }
                if !n.dbus_address.is_empty() && !n.wayland_display.is_empty() {
                    return Ok(n);
                }
            }
            if let Ok(Some(status)) = n.child.try_wait() {
                return Err(std::io::Error::other(format!("niri exited ({status}); see {}", dir.join("niri.log").display())));
            }
            if Instant::now() > deadline {
                return Err(std::io::Error::other(format!("niri did not start; see {}", dir.join("niri.log").display())));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A command that runs as a client of this session (its Wayland socket and bus).
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        c.env("WAYLAND_DISPLAY", &self.wayland_display).env("DBUS_SESSION_BUS_ADDRESS", &self.dbus_address).env_remove("DISPLAY");
        c
    }
}

impl Drop for NestedNiri {
    fn drop(&mut self) {
        // SAFETY: signals our own child's process group (created with process_group(0)).
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // SAFETY: as above.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

/// A PipeWire null audio sink that lives as long as this value (its monitor is a capture source).
pub struct NullSink {
    pub name: String,
    quit: pipewire::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NullSink {
    pub fn create(name: &str) -> Result<NullSink, String> {
        Self::with_class(name, "Audio/Sink")
    }

    /// A virtual source (a stand-in microphone that only gives silence).
    pub fn create_source(name: &str) -> Result<NullSink, String> {
        Self::with_class(name, "Audio/Source/Virtual")
    }

    fn with_class(name: &str, class: &'static str) -> Result<NullSink, String> {
        let (tx, rx) = pipewire::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let node_name = name.to_string();
        let thread = std::thread::spawn(move || {
            pipewire::init();
            let run = || -> Result<(), pipewire::Error> {
                let ml = pipewire::main_loop::MainLoopRc::new(None)?;
                let ctx = pipewire::context::ContextRc::new(&ml, None)?;
                let core = ctx.connect_rc(None)?;
                let props = pipewire::properties::properties! {
                    "factory.name" => "support.null-audio-sink",
                    "node.name" => node_name.as_str(),
                    "node.description" => "Kalango Cast test",
                    "media.class" => class,
                    "audio.position" => "[ FL FR ]",
                    "audio.rate" => "48000",
                    "object.linger" => "false",
                };
                let node: pipewire::node::Node = core.create_object("adapter", &props)?;
                let ml2 = ml.clone();
                let _q = rx.attach(ml.loop_(), move |_| ml2.quit());
                // give the graph a moment to publish the node before saying it is there
                let _ = core.sync(0);
                let _ = ready_tx.send(Ok(()));
                ml.run();
                drop(node);
                Ok(())
            };
            if let Err(e) = run() {
                let _ = ready_tx.send(Err(e.to_string()));
            }
        });
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                std::thread::sleep(Duration::from_millis(300));
                Ok(NullSink { name: name.into(), quit: tx, thread: Some(thread) })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err("null sink: timeout".into()),
        }
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Is a program on PATH?
pub fn have(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}
