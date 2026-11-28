//! Kalango Cast's shared core: the one media clock every stream is placed on, the types the capture, encode, mux and
//! engine crates exchange, and the UI/log translations (`tr`).

pub mod clock;
pub mod i18n;
pub mod text;
pub mod types;

pub use clock::{monotonic_ns, AudioTimeline, MediaClock, Placement};
pub use i18n::{tr, trf};
pub use types::*;
