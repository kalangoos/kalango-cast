//! One media clock for every stream of a recording.
//!
//! Every timestamp Kalango Cast handles is a `CLOCK_MONOTONIC` instant in nanoseconds: PipeWire's graph times
//! (`pw_stream_get_time_n`) and the moment a video frame reaches us are both on that clock. A [`MediaClock`] has one
//! origin (the instant the recording started) and maps any monotonic instant to media time: nanoseconds since that
//! origin, the same for video and audio, so they stay in sync by construction.
//!
//! Audio is placed by counting samples ([`AudioTimeline`]): sample `n` is at `start + n / rate`. That is exact and
//! jitter-free, but the sound card's clock drifts from the monotonic one and a buffer can be lost (an xrun), so each
//! buffer's own timestamp is compared with where the count says it should be; past a tolerance, silence is inserted
//! (a gap) or samples are skipped (the count ran ahead), and the track stays on the shared clock.

/// Now, on `CLOCK_MONOTONIC`, in nanoseconds.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime only writes the timespec we pass, which lives for the whole call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Nanoseconds to ticks of a timescale (ticks per second), rounded to the nearest tick.
pub fn ns_to_ticks(ns: u64, timescale: u32) -> u64 {
    ((ns as u128 * timescale as u128 + 500_000_000) / 1_000_000_000) as u64
}

/// Ticks of a timescale to nanoseconds, rounded to the nearest nanosecond.
pub fn ticks_to_ns(ticks: u64, timescale: u32) -> u64 {
    ((ticks as u128 * 1_000_000_000 + timescale as u128 / 2) / timescale as u128) as u64
}

/// The recording's clock: media time is nanoseconds since `origin` (a monotonic instant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaClock {
    origin_ns: u64,
}

impl MediaClock {
    /// A clock whose origin is now.
    pub fn starting_now() -> Self {
        Self { origin_ns: monotonic_ns() }
    }

    pub fn with_origin(origin_ns: u64) -> Self {
        Self { origin_ns }
    }

    pub fn origin_ns(&self) -> u64 {
        self.origin_ns
    }

    /// The media time of a monotonic instant; negative before the origin (such data is dropped or trimmed).
    pub fn media_ns(&self, monotonic_ns: u64) -> i64 {
        monotonic_ns as i64 - self.origin_ns as i64
    }

    /// The media time now.
    pub fn now(&self) -> i64 {
        self.media_ns(monotonic_ns())
    }
}

/// What to do with an audio buffer so that its samples land at their media time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Append the whole buffer.
    Append,
    /// Insert this many samples of silence, then append the buffer (a gap: lost buffers or a slow clock).
    Silence(u64),
    /// Drop this many samples from the buffer's start, append the rest (the count ran ahead, or the buffer started
    /// before the recording). It can be the whole buffer.
    Skip(u64),
}

/// The most silence one buffer may be preceded by: a bogus timestamp (a jump of hours) must not become hours of
/// silence in the file; a real gap that long is rare, and the next buffers keep filling it in steps.
pub const MAX_SILENCE_NS: u64 = 10_000_000_000;

/// Places an audio stream's buffers on the media clock by counting samples, correcting drift and gaps past
/// `tolerance_ns`.
#[derive(Clone, Debug)]
pub struct AudioTimeline {
    rate: u32,
    tolerance_ns: u64,
    start_ns: Option<i64>,
    samples: u64,
}

impl AudioTimeline {
    pub fn new(rate: u32, tolerance_ns: u64) -> Self {
        Self { rate, tolerance_ns, start_ns: None, samples: 0 }
    }

    /// The media time of the track's first sample (None before the first placed buffer).
    pub fn start_ns(&self) -> Option<i64> {
        self.start_ns
    }

    /// Samples placed so far (written or inserted as silence).
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// The media time of the next sample to be written.
    pub fn next_ns(&self) -> Option<i64> {
        self.start_ns.map(|s| s + self.samples_to_ns(self.samples) as i64)
    }

    fn samples_to_ns(&self, n: u64) -> u64 {
        (n as u128 * 1_000_000_000 / self.rate as u128) as u64
    }

    fn ns_to_samples(&self, ns: u64) -> u64 {
        ((ns as u128 * self.rate as u128 + 500_000_000) / 1_000_000_000) as u64
    }

    /// Places a buffer whose first sample is at `first_ns` (media time) and that has `frames` samples per channel;
    /// the caller then does what the answer says. The count advances as if it did.
    pub fn place(&mut self, first_ns: i64, frames: u64) -> Placement {
        let Some(start) = self.start_ns else {
            if first_ns >= 0 {
                self.start_ns = Some(first_ns);
                self.samples = frames;
                return Placement::Append;
            }
            // the buffer began before the recording: keep only its samples from the origin on
            let skip = self.ns_to_samples(first_ns.unsigned_abs()).min(frames);
            if skip < frames {
                self.start_ns = Some(0);
                self.samples = frames - skip;
            }
            return Placement::Skip(skip);
        };
        let expected = start + self.samples_to_ns(self.samples) as i64;
        let diff = first_ns - expected;
        if diff > self.tolerance_ns as i64 {
            let gap = self.ns_to_samples((diff as u64).min(MAX_SILENCE_NS));
            self.samples += gap + frames;
            Placement::Silence(gap)
        } else if diff < -(self.tolerance_ns as i64) {
            let skip = self.ns_to_samples(diff.unsigned_abs()).min(frames);
            self.samples += frames - skip;
            Placement::Skip(skip)
        } else {
            self.samples += frames;
            Placement::Append
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bogus_timestamp_does_not_become_hours_of_silence() {
        let mut t = AudioTimeline::new(48_000, 10_000_000);
        assert_eq!(t.place(0, 480), Placement::Append);
        // the next buffer claims to start an hour later
        assert_eq!(t.place(3_600_000_000_000, 480), Placement::Silence(480_000), "10 s at most");
        assert_eq!(t.samples(), 480 + 480_000 + 480);
        // a gap of 1 s is still filled whole
        let mut t = AudioTimeline::new(48_000, 10_000_000);
        t.place(0, 480);
        assert_eq!(t.place(1_010_000_000, 480), Placement::Silence(48_000));
    }

    #[test]
    fn media_time_is_relative_to_the_origin() {
        let c = MediaClock::with_origin(5_000_000_000);
        assert_eq!(c.media_ns(5_000_000_000), 0);
        assert_eq!(c.media_ns(5_016_666_667), 16_666_667);
        assert_eq!(c.media_ns(4_990_000_000), -10_000_000, "before the origin is negative");
    }

    #[test]
    fn monotonic_never_goes_back() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(b >= a);
        let c = MediaClock::starting_now();
        assert!(c.now() >= 0);
    }

    #[test]
    fn tick_conversions_round_to_nearest() {
        assert_eq!(ns_to_ticks(16_666_667, 90_000), 1500);
        assert_eq!(ns_to_ticks(1_000_000_000, 48_000), 48_000);
        assert_eq!(ns_to_ticks(20_833, 48_000), 1, "20.8 µs is one sample at 48 kHz");
        assert_eq!(ticks_to_ns(1500, 90_000), 16_666_667);
        assert_eq!(ticks_to_ns(960, 48_000), 20_000_000);
        // a long recording does not overflow (u128 inside)
        let ten_hours = 36_000 * 1_000_000_000u64;
        assert_eq!(ns_to_ticks(ten_hours, 90_000), 36_000 * 90_000);
    }

    #[test]
    fn audio_follows_the_sample_count_while_on_time() {
        let mut t = AudioTimeline::new(48_000, 10_000_000);
        assert_eq!(t.place(2_000_000, 480), Placement::Append);
        assert_eq!(t.start_ns(), Some(2_000_000));
        // 480 samples = 10 ms later, with 1 ms of jitter: still appended, the count decides
        assert_eq!(t.place(13_000_000, 480), Placement::Append);
        assert_eq!(t.samples(), 960);
        assert_eq!(t.next_ns(), Some(22_000_000));
    }

    #[test]
    fn a_gap_inserts_silence() {
        let mut t = AudioTimeline::new(48_000, 10_000_000);
        t.place(0, 480);
        // the next buffer should start at 10 ms but comes at 40 ms: 30 ms (1440 samples) were lost
        assert_eq!(t.place(40_000_000, 480), Placement::Silence(1440));
        assert_eq!(t.next_ns(), Some(50_000_000));
    }

    #[test]
    fn a_count_running_ahead_skips_samples() {
        let mut t = AudioTimeline::new(48_000, 10_000_000);
        t.place(0, 4800);
        // expected at 100 ms, the buffer says 80 ms: drop its first 20 ms (960 samples)
        assert_eq!(t.place(80_000_000, 4800), Placement::Skip(960));
        assert_eq!(t.next_ns(), Some(180_000_000));
    }

    #[test]
    fn a_buffer_from_before_the_origin_is_trimmed() {
        let mut t = AudioTimeline::new(48_000, 10_000_000);
        // starts 5 ms before the recording: its first 240 samples go
        assert_eq!(t.place(-5_000_000, 480), Placement::Skip(240));
        assert_eq!(t.start_ns(), Some(0));
        assert_eq!(t.samples(), 240);
        // a buffer entirely before the origin leaves the track unstarted
        let mut u = AudioTimeline::new(48_000, 10_000_000);
        assert_eq!(u.place(-50_000_000, 480), Placement::Skip(480));
        assert_eq!(u.start_ns(), None);
    }

    #[test]
    fn slow_drift_is_corrected_once_past_the_tolerance() {
        // a card running 0.1 % fast: its buffers' timestamps fall behind the count by 10 µs per 10 ms buffer
        let mut t = AudioTimeline::new(48_000, 10_000_000);
        let mut skipped = 0;
        for i in 0..2000u64 {
            let ts = (i * 10_000_000) as i64 - (i * 10_000) as i64;
            if let Placement::Skip(n) = t.place(ts, 480) {
                skipped += n;
            }
        }
        // 20 s of 0.1 % drift is 20 ms: corrected, and the track ends within the tolerance of the clock
        assert!(skipped > 0);
        let end_true: i64 = 2000 * 10_000_000 - 2000 * 10_000;
        let end_track = t.next_ns().unwrap();
        assert!((end_track - end_true).abs() <= 10_000_000, "{end_track} vs {end_true}");
    }
}
