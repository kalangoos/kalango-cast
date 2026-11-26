//! Kalango Cast's app: the studio window of the recording engine (`kalango-castd`).

mod app;
mod client;
mod daemon;
mod model;
mod motion;
mod style;

fn main() -> gtk::glib::ExitCode {
    app::run()
}
