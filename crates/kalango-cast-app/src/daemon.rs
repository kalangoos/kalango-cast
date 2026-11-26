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

/// The descriptor number the daemon finds its secret pipe on.
const SECRET_FD: i32 = 3;

/// A random secret (256 bits, hex) from the kernel.
fn new_secret() -> std::io::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Starts `cmd` with a fresh secret in an inherited pipe: the child finds it on descriptor 3 (`--secret-fd 3` is added
/// to its arguments). The secret is in neither the environment, nor the arguments, nor any file; the parent keeps it
/// in memory only and closes its end of the pipe. Returns the child and the secret.
pub fn spawn_with_secret(cmd: &mut Command) -> std::io::Result<(Child, String)> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let secret = new_secret()?;
    let mut fds = [0i32; 2];
    // SAFETY: pipe2 fills the two descriptors we pass.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors are fresh and ours; OwnedFd closes them.
    let (read_end, write_end) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // a small secret fits the pipe's buffer: written now, so the daemon can read it whenever it starts
    let mut w = std::fs::File::from(write_end);
    w.write_all(secret.as_bytes())?;
    drop(w);
    // keep the read end clear of descriptor 3 so the dup2 below always clears close-on-exec
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor >= 10 for the one we own.
    let high = unsafe { libc::fcntl(read_end.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if high < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `high` is a fresh descriptor we own.
    let high = unsafe { OwnedFd::from_raw_fd(high) };
    drop(read_end);
    cmd.arg("--secret-fd").arg(SECRET_FD.to_string());
    let src = high.as_raw_fd();
    // SAFETY: the closure runs between fork and exec and only calls dup2, which is async-signal-safe; dup2 gives the
    // new descriptor 3 without close-on-exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(src, SECRET_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(high);
    Ok((child, secret))
}

/// The service as the app sees it; dropping it stops the daemon if the app started it.
pub struct Service {
    child: Option<Child>,
    /// The consent-handler secret of the daemon we spawned (memory only; never logged or written).
    secret: Option<String>,
}

impl Service {
    /// The secret to register as the consent handler with (only for a daemon this app spawned).
    pub fn secret(&self) -> Option<&str> {
        self.secret.as_deref()
    }

    /// Whether this app started the daemon (and so stops it on exit).
    pub fn started_by_app(&self) -> bool {
        self.child.is_some()
    }

    /// Uses the daemon on `socket` if there is one, otherwise starts `binary` on it and waits until it answers.
    pub fn ensure_running(socket: &Path, binary: Option<&Path>) -> Result<Service, String> {
        if is_running(socket) {
            return Ok(Service { child: None, secret: None });
        }
        let binary = binary.ok_or_else(|| tr("kalango-castd was not found: install it next to this app or set KALANGO_CASTD").to_string())?;
        let mut cmd = Command::new(binary);
        cmd.arg("--socket").arg(socket).stdin(Stdio::null()).stdout(Stdio::null());
        // its messages go to a file the person owns, next to the socket's folder (never to a terminal we do not own)
        match log_file(socket) {
            Some(f) => cmd.stderr(Stdio::from(f)),
            None => cmd.stderr(Stdio::null()),
        };
        let (mut child, secret) = spawn_with_secret(&mut cmd).map_err(|e| format!("{}: {e}", binary.display()))?;
        let end = Instant::now() + START_TIMEOUT;
        loop {
            if is_running(socket) {
                return Ok(Service { child: Some(child), secret: Some(secret) });
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

    #[test]
    fn the_secret_reaches_the_child_on_fd_3_and_is_nowhere_else() {
        let dir = scratch("secret-fd");
        let out = dir.join("seen");
        // a stand-in child: copies what it finds on descriptor 3 to a file, then waits so /proc can be read
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("cat <&3 > \"$0\"; sleep 30").arg(&out);
        let (mut child, secret) = spawn_with_secret(&mut cmd).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(&out).map(|s| s.is_empty()).unwrap_or(true) {
            assert!(Instant::now() < end, "the child never read the secret");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), secret);
        assert_eq!(secret.len(), 64);
        let pid = child.id();
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
        let has = |hay: &[u8]| hay.windows(secret.len()).any(|w| w == secret.as_bytes());
        assert!(!has(&cmdline), "not in the arguments");
        assert!(!has(&environ), "not in the environment");
        assert!(String::from_utf8_lossy(&cmdline).contains("--secret-fd"));
        // and two spawns never share a secret
        let (mut c2, s2) = spawn_with_secret(Command::new("/bin/sleep").arg("30")).unwrap();
        assert_ne!(secret, s2);
        let _ = (child.kill(), child.wait(), c2.kill(), c2.wait());
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
        // the real daemon: the secret is in neither its arguments nor its environment
        let secret = s.secret().unwrap().to_string();
        let pid = s.child.as_ref().unwrap().id();
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
        assert!(!cmdline.windows(secret.len()).any(|w| w == secret.as_bytes()));
        // (it set PR_SET_DUMPABLE=0, so its environ is not even readable by the same user)
        assert!(std::fs::read(format!("/proc/{pid}/environ")).is_err(), "environ is closed to other processes");
        s.stop();
        assert!(!is_running(&sock), "stopped with the app");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
