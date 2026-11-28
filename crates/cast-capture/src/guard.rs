//! A panic must never cross into PipeWire's C callbacks: that is a "non-unwinding panic" and aborts the whole process
//! (the daemon with every recording in it). Every callback that calls into a sink goes through [`Guarded::call`], which
//! turns a panic into a failure the sink is told about, stops the loop, and ignores everything after.

use std::panic::{catch_unwind, AssertUnwindSafe};

/// A sink that can be told its capture broke (a panic in its own code).
pub trait Failing {
    fn failed(&mut self, why: &str);
}

pub struct Guarded<S> {
    sink: S,
    poisoned: bool,
}

/// The text of a panic.
pub fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

impl<S: Failing> Guarded<S> {
    pub fn new(sink: S) -> Self {
        Guarded { sink, poisoned: false }
    }

    /// Whether a callback panicked (nothing reaches the sink any more).
    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    /// Runs `f` on the sink. A panic poisons the sink, tells it (`failed`, itself guarded) and calls `quit` (the
    /// capture loop stops); later calls do nothing.
    pub fn call(&mut self, quit: &dyn Fn(), f: impl FnOnce(&mut S)) {
        if self.poisoned {
            return;
        }
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| f(&mut self.sink))) {
            self.poisoned = true;
            let why = panic_text(&*p);
            let _ = catch_unwind(AssertUnwindSafe(|| self.sink.failed(&why)));
            quit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct Sink {
        frames: u32,
        told: Option<String>,
        panic_on_failed: bool,
    }

    impl Failing for Sink {
        fn failed(&mut self, why: &str) {
            self.told = Some(why.to_string());
            if self.panic_on_failed {
                panic!("failed() broke too");
            }
        }
    }

    #[test]
    fn a_panic_in_the_sink_is_told_to_it_stops_the_loop_and_later_calls_do_nothing() {
        let quits = Cell::new(0);
        let quit = || quits.set(quits.get() + 1);
        let mut g = Guarded::new(Sink::default());
        g.call(&quit, |s| s.frames += 1);
        assert_eq!((g.sink.frames, quits.get()), (1, 0));
        g.call(&quit, |_| panic!("encoder exploded"));
        assert!(g.poisoned());
        assert_eq!(g.sink.told.as_deref(), Some("encoder exploded"));
        assert_eq!(quits.get(), 1);
        g.call(&quit, |s| s.frames += 1);
        assert_eq!(g.sink.frames, 1, "nothing reaches a poisoned sink");
        assert_eq!(quits.get(), 1);
    }

    #[test]
    fn a_failed_that_panics_too_still_stops_the_loop() {
        let quits = Cell::new(0);
        let quit = || quits.set(quits.get() + 1);
        let mut g = Guarded::new(Sink { panic_on_failed: true, ..Sink::default() });
        g.call(&quit, |_| panic!("first"));
        assert_eq!(quits.get(), 1);
    }
}
