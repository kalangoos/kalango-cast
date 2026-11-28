//! kalango-castd: Kalango Cast's recording service (JSON-RPC over a Unix socket; see `cast_engine::rpc`).
//!
//! `kalango-castd [--socket PATH] [--bus DBUS_ADDRESS] [--secret-fd N]`
//!
//! `--secret-fd N`: the app that spawned us keeps the other end of the socketpair N. We make ourselves undumpable first,
//! say "ready" on it, and only then does the app send a random secret; a consent handler must present it to register.
//! Without `--secret-fd` there is no consent handler at all: consent stays on this terminal (or the journal).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use cast_core::{tr, trf};
use cast_engine::rpc::{self, Daemon, Event};

/// Writes to the terminal and ignores a failure (a closed or full stderr must not panic the daemon).
macro_rules! say {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

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

/// What the trusted channel says about an event, one line.
fn event_line(e: &Event) -> String {
    match e {
        Event::ConsentRequested { what, token, .. } => {
            trf("Recording requested: {what}. To allow it, run: kalango-cast approve {token}", &[("what", what.clone()), ("token", token.clone())])
        }
        Event::ConsentSuperseded { .. } => tr("The held recording request was replaced by a different one; its code no longer works").to_string(),
        Event::RecordingStarted { path } => trf("RECORDING STARTED: {path}", &[("path", path.clone())]),
        Event::RecordingStopped { path } => trf("recording stopped: {path}", &[("path", path.clone())]),
        Event::RecordingFailed { path, error } => trf("RECORDING FAILED: {path}: {error}", &[("path", path.clone()), ("error", error.clone())]),
    }
}

/// Prints an event; a terminal that cannot take it (closed, full) is ignored, never a panic in the daemon's thread.
fn print_event(out: &mut dyn Write, e: &Event) {
    let _ = writeln!(out, "kalango-castd: {}", event_line(e));
}

fn main() {
    // the very first thing, before arguments or any secret: other programs of the same user must not be able to read
    // our memory or environment through /proc or ptrace
    // SAFETY: prctl with PR_SET_DUMPABLE and a plain integer argument.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    let mut socket = rpc::default_socket();
    let mut bus = None;
    let mut secret_fd: Option<i32> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--bus" => bus = args.next(),
            "--secret-fd" => secret_fd = args.next().and_then(|v| v.parse().ok()),
            "-h" | "--help" => {
                println!("kalango-castd [--socket {}] [--bus {}]", tr("PATH"), tr("DBUS_ADDRESS"));
                return;
            }
            other => {
                say!("{}: {other}", tr("unknown option"));
                std::process::exit(2);
            }
        }
    }
    let Some(socket) = socket else {
        say!("{}", tr("XDG_RUNTIME_DIR is not set: pass --socket"));
        std::process::exit(1);
    };
    let secret = secret_fd.map(|fd| match rpc::receive_secret(fd, std::time::Duration::from_secs(10)) {
        Ok(s) => s,
        Err(e) => {
            say!("kalango-castd: --secret-fd {fd}: {e}");
            std::process::exit(2);
        }
    });
    let set = block_signals();
    let listener = match rpc::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            say!("{}: {e}", socket.display());
            std::process::exit(1);
        }
    };
    // The trusted channel: what the person sees on this terminal. The approval code is shown here and nowhere else.
    let hook = Box::new(|e: &Event| print_event(&mut std::io::stderr(), e));
    let mut daemon = Daemon::with_bus(bus).with_consent(rpc::load_consent()).with_hook(hook);
    if let Some(s) = secret {
        daemon = daemon.with_handler_secret(s);
    }
    let daemon = Arc::new(daemon);
    let d = daemon.clone();
    std::thread::spawn(move || d.serve(listener));
    say!("kalango-castd: {} {}", tr("listening on"), socket.display());
    let mut sig = 0;
    // SAFETY: waits for one of the signals blocked in `set`.
    unsafe { libc::sigwait(&set, &mut sig) };
    say!("kalango-castd: {}", tr("stopping"));
    daemon.shutdown();
    let _ = std::fs::remove_file(&socket);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_on_a_full_terminal_does_not_panic() {
        let mut full = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
        print_event(&mut full, &Event::RecordingFailed { path: "/a.mp4".into(), error: "x".into() });
        let mut ok = Vec::new();
        print_event(&mut ok, &Event::RecordingStopped { path: "/a.mp4".into() });
        assert!(String::from_utf8(ok).unwrap().contains("/a.mp4"));
    }
}
