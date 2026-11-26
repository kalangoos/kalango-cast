//! Simple springs for the app's motion (the Design System's four feels: fast, smooth, elastic, slow), implemented
//! here so the app does not depend on any other KalangoOS repository. A [`Spring`] chases a target with a damped
//! harmonic oscillator, stepped with a fixed small time step so a slow frame cannot blow it up.

/// How a spring feels.
#[derive(Clone, Copy, Debug)]
pub struct Feel {
    pub stiffness: f64,
    pub damping: f64,
}

/// Quick and crisp (hover, press).
pub const FAST: Feel = Feel { stiffness: 520.0, damping: 38.0 };
/// The everyday one (panels, meters).
pub const SMOOTH: Feel = Feel { stiffness: 260.0, damping: 28.0 };
/// A little overshoot (a dot popping in).
pub const ELASTIC: Feel = Feel { stiffness: 340.0, damping: 15.0 };
/// Calm and slow (large surfaces; not used by this window yet).
#[allow(dead_code)]
pub const SLOW: Feel = Feel { stiffness: 120.0, damping: 22.0 };

/// Below these distance and speed the spring is at rest.
const REST_DISTANCE: f64 = 0.0005;
const REST_SPEED: f64 = 0.005;
/// The physics step (s) and the longest frame time that is simulated (a frame after a stall counts as this long).
const STEP: f64 = 1.0 / 240.0;
const MAX_FRAME: f64 = 1.0 / 20.0;

#[derive(Clone, Copy, Debug)]
pub struct Spring {
    value: f64,
    velocity: f64,
    target: f64,
    feel: Feel,
}

impl Spring {
    pub fn new(value: f64, feel: Feel) -> Self {
        Spring { value, velocity: 0.0, target: value, feel }
    }

    pub fn value(&self) -> f64 {
        self.value
    }

    pub fn target(&self) -> f64 {
        self.target
    }

    pub fn set_target(&mut self, target: f64) {
        self.target = target;
    }

    /// Jumps to a value with no motion.
    pub fn snap(&mut self, value: f64) {
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
    }

    pub fn at_rest(&self) -> bool {
        (self.value - self.target).abs() < REST_DISTANCE && self.velocity.abs() < REST_SPEED
    }

    /// Advances by `dt` seconds; true while it still moves.
    pub fn step(&mut self, dt: f64) -> bool {
        if self.at_rest() {
            self.value = self.target;
            self.velocity = 0.0;
            return false;
        }
        let mut left = dt.clamp(0.0, MAX_FRAME);
        while left > 0.0 {
            let h = left.min(STEP);
            let accel = -self.feel.stiffness * (self.value - self.target) - self.feel.damping * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
            left -= h;
        }
        if self.at_rest() {
            self.value = self.target;
            self.velocity = 0.0;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &mut Spring, seconds: f64) -> f64 {
        let mut peak = s.value();
        let mut t = 0.0;
        while t < seconds {
            s.step(1.0 / 60.0);
            peak = peak.max(s.value());
            t += 1.0 / 60.0;
        }
        peak
    }

    #[test]
    fn a_spring_settles_on_its_target() {
        for feel in [FAST, SMOOTH, ELASTIC, SLOW] {
            let mut s = Spring::new(0.0, feel);
            s.set_target(1.0);
            assert!(!s.at_rest());
            run(&mut s, 4.0);
            assert!(s.at_rest(), "{feel:?}");
            assert_eq!(s.value(), 1.0);
        }
    }

    #[test]
    fn the_elastic_one_overshoots_and_the_smooth_one_does_not() {
        let mut e = Spring::new(0.0, ELASTIC);
        e.set_target(1.0);
        assert!(run(&mut e, 2.0) > 1.05, "elastic goes past the target");
        let mut m = Spring::new(0.0, SMOOTH);
        m.set_target(1.0);
        assert!(run(&mut m, 2.0) < 1.05, "smooth barely does");
    }

    #[test]
    fn a_long_stall_does_not_blow_it_up() {
        let mut s = Spring::new(0.0, ELASTIC);
        s.set_target(1.0);
        for _ in 0..10 {
            s.step(5.0);
        }
        assert!(s.value().is_finite() && s.value().abs() < 3.0);
    }

    #[test]
    fn at_rest_it_reports_no_motion_and_snap_jumps() {
        let mut s = Spring::new(0.5, FAST);
        assert!(!s.step(0.016));
        s.snap(2.0);
        assert_eq!((s.value(), s.target()), (2.0, 2.0));
        assert!(s.at_rest());
    }
}
