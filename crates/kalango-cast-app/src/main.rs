//! Kalango Cast's app: the studio window of the recording engine (`kalango-castd`).

mod app;
mod client;
mod daemon;
mod model;
mod motion;
mod style;

fn main() -> gtk::glib::ExitCode {
    // other programs of the same user must not be able to read this process (the consent secret lives in its memory)
    // SAFETY: prctl with PR_SET_DUMPABLE and a plain integer argument.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    app::run()
}
