//! Is a websocket feed alive? Two ways it can die:
//!
//! 1. **Silent.** Nothing at all comes back, not even an answer to our pings:
//!    the connection is gone, even if the operating system hasn't noticed.
//! 2. **Alive but empty.** Pings get answered but the data (prices, book
//!    updates) has stopped. The bot then stops quoting on a stale price, and
//!    would wait forever if we only listened for pongs. So feeds that should
//!    never go quiet also give up after a while without data, and reconnect.

use std::time::{Duration, Instant};

/// Nothing at all for this long, not even a pong: the connection is dead.
pub const SILENT_AFTER: Duration = Duration::from_secs(3);

pub struct Liveness {
    last_heard: Instant,
    last_data: Instant,
    /// Reconnect after this long without data. `None`: quiet is normal here
    /// (our own fills can be minutes apart).
    data_timeout: Option<Duration>,
}

impl Liveness {
    /// A connection that just opened.
    pub fn new(now: Instant, data_timeout: Option<Duration>) -> Self {
        Self {
            last_heard: now,
            last_data: now,
            data_timeout,
        }
    }

    /// A message arrived. `data` is false for pongs and other housekeeping.
    pub fn heard(&mut self, now: Instant, data: bool) {
        self.last_heard = now;
        if data {
            self.last_data = now;
        }
    }

    /// Why the connection should be dropped and reopened, if it should.
    pub fn dead(&self, now: Instant) -> Option<String> {
        let silent = now.saturating_duration_since(self.last_heard);
        if silent > SILENT_AFTER {
            return Some(format!("nothing heard for {}s", silent.as_secs()));
        }
        let quiet = now.saturating_duration_since(self.last_data);
        match self.data_timeout {
            Some(timeout) if quiet > timeout => Some(format!(
                "pings answered but no data for {}s",
                quiet.as_secs()
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn data_keeps_it_alive() {
        let start = Instant::now();
        let mut live = Liveness::new(start, Some(secs(5)));
        for s in 1..=20 {
            live.heard(start + secs(s), true);
        }
        assert_eq!(live.dead(start + secs(20)), None);
    }

    #[test]
    fn pongs_without_data_are_not_enough() {
        // The case that stalled the dry run: answers to pings, but no prices.
        let start = Instant::now();
        let mut live = Liveness::new(start, Some(secs(5)));
        for s in 1..=6 {
            live.heard(start + secs(s), false);
        }
        assert_eq!(live.dead(start + secs(5)), None, "not yet");
        assert!(live.dead(start + secs(6)).is_some());
    }

    #[test]
    fn silence_is_death() {
        let start = Instant::now();
        let live = Liveness::new(start, None);
        assert_eq!(live.dead(start + secs(3)), None);
        assert!(live.dead(start + secs(4)).is_some());
    }

    #[test]
    fn a_feed_that_may_be_quiet_lives_on_pongs() {
        // Our own orders and fills: minutes without news is normal.
        let start = Instant::now();
        let mut live = Liveness::new(start, None);
        for s in 1..=600 {
            live.heard(start + secs(s), false);
        }
        assert_eq!(live.dead(start + secs(600)), None);
    }
}
