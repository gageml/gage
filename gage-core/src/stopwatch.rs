//! A stopwatch that can be paused and resumed. Scan tasks use it to
//! count time worked, holding the clock still while the task is
//! blocked on the agent pool.

use std::time::{Duration, Instant};

/// Elapsed time over running spells. The clock accumulates while
/// running and holds still while paused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stopwatch {
    /// Completed running spells
    elapsed: Duration,
    /// Start of the open running spell, `None` while paused
    running_since: Option<Instant>,
}

impl Stopwatch {
    /// A paused stopwatch with no elapsed time
    pub fn new() -> Self {
        Self::default()
    }

    /// A stopwatch running from now
    pub fn started() -> Self {
        Self {
            elapsed: Duration::ZERO,
            running_since: Some(Instant::now()),
        }
    }

    /// Start the clock; no effect when already running
    pub fn resume(&mut self) {
        self.set_running(true);
    }

    /// Run or pause the clock; no effect when already in that state
    pub fn set_running(&mut self, running: bool) {
        match (running, self.running_since) {
            (false, Some(since)) => {
                self.elapsed += since.elapsed();
                self.running_since = None;
            }
            (true, None) => self.running_since = Some(Instant::now()),
            _ => {}
        }
    }

    /// Time elapsed so far, including the open running spell
    pub fn elapsed(&self) -> Duration {
        self.elapsed + self.running_since.map(|s| s.elapsed()).unwrap_or_default()
    }

    /// Pause the clock and return the time elapsed
    pub fn stop(&mut self) -> Duration {
        self.set_running(false);
        self.elapsed
    }
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;

    use super::*;

    #[test]
    fn new_is_paused_at_zero() {
        let sw = Stopwatch::new();
        sleep(Duration::from_millis(5));
        assert_eq!(sw.elapsed(), Duration::ZERO);
        assert_eq!(sw, Stopwatch::default());
    }

    #[test]
    fn started_accumulates_until_stopped() {
        let mut sw = Stopwatch::started();
        sleep(Duration::from_millis(5));
        let total = sw.stop();
        assert!(total >= Duration::from_millis(5));
        sleep(Duration::from_millis(5));
        assert_eq!(sw.elapsed(), total);
    }

    #[test]
    fn paused_clock_holds_still_and_resumes() {
        let mut sw = Stopwatch::started();
        sw.set_running(false);
        let paused_at = sw.elapsed();
        sleep(Duration::from_millis(5));
        assert_eq!(sw.elapsed(), paused_at);
        sw.resume();
        sleep(Duration::from_millis(5));
        assert!(sw.elapsed() >= paused_at + Duration::from_millis(5));
    }

    #[test]
    fn set_running_is_idempotent() {
        let mut sw = Stopwatch::started();
        let before = sw.clone();
        sw.set_running(true);
        assert_eq!(sw, before);
        sw.set_running(false);
        let paused = sw.clone();
        sw.set_running(false);
        assert_eq!(sw, paused);
    }
}
