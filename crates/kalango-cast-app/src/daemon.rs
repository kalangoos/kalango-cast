//! The daemon's lifecycle from the app: if `kalango-castd` is not answering on the person's socket, start it as our own
//! child process (no shell: the program and its arguments are passed as they are, the socket is the user-owned one
//! in `$XDG_RUNTIME_DIR`), and stop it when the app exits, but only if the app started it.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cast_core::{tr, trf};
use serde_json::json;

use crate::client;

/// The environment variable that points at the daemon's program (for development and tests).
pub const CASTD_ENV: &str = "KALANGO_CASTD";

/// How long to wait for a daemon we just started to answer.
const START_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a daemon gets to finish its recording after SIGTERM before it is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(8);

/// Whether a daemon answers on `socket`.
pub fn is_running(socket: &Path) -> bool {
    client::call(socket, "capabilities", json!({}), Duration::from_secs(2)).is_ok()
}

/// Looks for the daemon's program: `$KALANGO_CASTD`, then next to this program, then the directories of `$PATH`.
pub fn find_binary() -> Option<PathBuf> {
    let is_exe = |p: &Path| p.is_file();
    if let Some(p) = std::env::var_os(CASTD_ENV).filter(|p| !p.is_empty()).map(PathBuf::from) {
        return is_exe(&p).then_some(p);
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        let p = dir.join("kalango-castd");
        if is_exe(&p) {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join("kalango-castd")).find(|p| is_exe(p)))
}

/// The service as the app sees it; dropping it stops the daemon if the app started it.
pub struct Service {
    child: Option<Child>,
}

impl Service {
    /// Whether this app started the daemon (and so stops it on exit).
    pub fn started_by_app(&self) -> bool {
        self.child.is_some()
    }

    /// Uses the daemon on `socket` if there is one, otherwise starts `binary` on it and waits until it answers.
    pub fn ensure_running(socket: &Path, binary: Option<&Path>) -> Result<Service, String> {
        if is_running(socket) {
            return Ok(Service { child: None });
        }
        let binary = binary.ok_or_else(|| tr("kalango-castd was not found: install it next to this app or set KALANGO_CASTD").to_string())?;
        let mut cmd = Command::new(binary);
        cmd.arg("--socket").arg(socket).stdin(Stdio::null()).stdout(Stdio::null());
        // its messages go to a file the person owns, next to the socket's folder (never to a terminal we do not own)
        match log_file(socket) {
            Some(f) => cmd.stderr(Stdio::from(f)),
            None => cmd.stderr(Stdio::null()),
        };
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", binary.display()))?;
        let end = Instant::now() + START_TIMEOUT;
        loop {
            if is_running(socket) {
                return Ok(Service { child: Some(child) });
            }
            if let Ok(Some(status)) = child.try_wait() {
                let how = match (status.code(), status.signal()) {
                    (Some(c), _) => trf("exit code {code}", &[("code", c.to_string())]),
                    (None, Some(s)) => trf("signal {signal}", &[("signal", s.to_string())]),
                    _ => String::new(),
                };
                return Err(trf(
                    "kalango-castd exited right away ({how}); see {log}",
                    &[("how", how), ("log", log_path(socket).display().to_string())],
                ));
            }
            if Instant::now() >= end {
                let _ = child.kill();
                let _ = child.wait();
                return Err(tr("kalango-castd did not answer in time").to_string());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stops the daemon if the app started it: SIGTERM (it finishes the recording and removes its socket), then a
    /// kill if it does not leave in time.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        // SAFETY: kill(2) with the pid of our own live child and a plain signal number.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let end = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < end {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}

fn log_path(socket: &Path) -> PathBuf {
    socket.parent().map(|d| d.join("castd.log")).unwrap_or_else(|| PathBuf::from("castd.log"))
}

/// The daemon's log, opened for appending in the socket's folder (0700, ours), mode 0600.
fn log_file(socket: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = socket.parent()?;
    // 0700 and ours, as the daemon requires of the socket's folder (it refuses a folder open to others)
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).ok()?;
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(log_path(socket)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cast_engine::rpc::{self, Daemon};
    use std::sync::Arc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kalango-cast-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_daemon_that_is_already_running_is_used_and_never_stopped() {
        let dir = scratch("running");
        let sock = dir.join("kalango").join("cast.sock");
        let listener = rpc::bind(&sock).unwrap();
        let d = Arc::new(Daemon::default());
        std::thread::spawn(move || d.serve(listener));
        let mut s = Service::ensure_running(&sock, None).unwrap();
        assert!(!s.started_by_app());
        s.stop();
        assert!(is_running(&sock), "not ours: still there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_daemon_and_without_a_program_the_reason_is_given() {
        let dir = scratch("none");
        let sock = dir.join("kalango").join("cast.sock");
        let e = Service::ensure_running(&sock, None).err().unwrap();
        assert!(e.contains("kalango-castd"), "{e}");
        let e = Service::ensure_running(&sock, Some(&dir.join("not-there"))).err().unwrap();
        assert!(e.contains("not-there"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_that_exits_at_once_is_reported_with_its_log() {
        let dir = scratch("exits");
        let sock = dir.join("kalango").join("cast.sock");
        // `false` ignores its arguments and exits 1: a daemon that cannot start
        let false_bin = ["/bin/false", "/usr/bin/false"].into_iter().map(PathBuf::from).find(|p| p.exists()).expect("false");
        let e = Service::ensure_running(&sock, Some(&false_bin)).err().unwrap();
        assert!(e.contains("castd.log"), "the reason points at the log: {e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real program, started as a child and stopped when the service is dropped. Needs a built `kalango-castd`:
    /// `KALANGO_CASTD=target/debug/kalango-castd cargo test -p kalango-cast-app -- --ignored`.
    #[test]
    #[ignore = "needs KALANGO_CASTD (a built kalango-castd)"]
    fn the_real_daemon_is_started_as_a_child_and_stopped_with_the_app() {
        let bin = find_binary().expect("set KALANGO_CASTD");
        let dir = scratch("real");
        let sock = dir.join("kalango").join("cast.sock");
        let mut s = Service::ensure_running(&sock, Some(&bin)).unwrap();
        assert!(s.started_by_app());
        assert!(is_running(&sock));
        s.stop();
        assert!(!is_running(&sock), "stopped with the app");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
