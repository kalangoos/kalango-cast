//! kalango-castd: Kalango Cast's recording service (JSON-RPC over a Unix socket; see `cast_engine::rpc`).
//!
//! `kalango-castd [--socket PATH] [--bus DBUS_ADDRESS]`

use std::path::PathBuf;
use std::sync::Arc;

use cast_core::{tr, trf};
use cast_engine::rpc::{self, Daemon, Event};

/// Blocks SIGINT/SIGTERM in this thread (and the threads it starts afterwards), to wait for them with sigwait.
fn block_signals() -> libc::sigset_t {
    // SAFETY: sigset operations on a local set; pthread_sigmask changes this thread's mask only.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn main() {
    let mut socket = rpc::default_socket();
    let mut bus = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--bus" => bus = args.next(),
            "-h" | "--help" => {
                println!("kalango-castd [--socket {}] [--bus {}]", tr("PATH"), tr("DBUS_ADDRESS"));
                return;
            }
            other => {
                eprintln!("{}: {other}", tr("unknown option"));
                std::process::exit(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("{}", tr("XDG_RUNTIME_DIR is not set: pass --socket"));
        std::process::exit(1);
    };
    let set = block_signals();
    let listener = match rpc::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{}: {e}", socket.display());
            std::process::exit(1);
        }
    };
    // The trusted channel: what the person sees on this terminal. The approval code is shown here and nowhere else.
    let hook = Box::new(|e: &Event| match e {
        Event::ConsentRequested { what, token } => {
            eprintln!(
                "kalango-castd: {}",
                trf(
                    "Recording requested: {what}. To allow it, run: kalango-cast approve {token}",
                    &[("what", what.clone()), ("token", token.clone())]
                )
            )
        }
        Event::RecordingStarted { path } => eprintln!("kalango-castd: {}", trf("RECORDING STARTED: {path}", &[("path", path.clone())])),
        Event::RecordingStopped { path } => eprintln!("kalango-castd: {}", trf("recording stopped: {path}", &[("path", path.clone())])),
        Event::RecordingFailed { path, error } => {
            eprintln!("kalango-castd: {}", trf("RECORDING FAILED: {path}: {error}", &[("path", path.clone()), ("error", error.clone())]))
        }
    });
    let daemon = Arc::new(Daemon::with_bus(bus).with_consent(rpc::load_consent()).with_hook(hook));
    let d = daemon.clone();
    std::thread::spawn(move || d.serve(listener));
    eprintln!("kalango-castd: {} {}", tr("listening on"), socket.display());
    let mut sig = 0;
    // SAFETY: waits for one of the signals blocked in `set`.
    unsafe { libc::sigwait(&set, &mut sig) };
    eprintln!("kalango-castd: {}", tr("stopping"));
    daemon.shutdown();
    let _ = std::fs::remove_file(&socket);
}
