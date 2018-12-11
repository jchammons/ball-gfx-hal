use log::{trace, warn};
use std::time::{Duration, Instant};

/// State used to attempt to generate regular interval ticks.
///
/// This will try to take into account how late or early the tick is
/// actually processed, and schedule the next one relative to the
/// intended time, unless it is more than a full tick interval late.
pub struct Interval {
    interval: Duration,
    next: Instant,
}

impl Interval {
    pub fn new(interval: Duration) -> Interval {
        Interval {
            interval,
            next: Instant::now() + interval,
        }
    }

    /// Gets the interval duration.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Reports a processed tick, and returns the interval since the
    /// last tick, and the delay before the next tick should occur.
    pub fn next(&mut self, tick: Instant) -> (Duration, Duration) {
        let interval = if tick > self.next {
            let late = tick.duration_since(self.next);
            if late > self.interval {
                warn!("got tick one full interval late");
                // Skipped a tick, so give up and schedule the next
                // one at the next interval from now.
                self.interval
            } else {
                // Less than one full tick late, so take the offset
                // into account.
                self.interval - late
            }
        } else {
            let early = self.next.duration_since(tick);
            if early > self.interval {
                warn!("got tick one full interval early");
                // One full tick early, so give up and schedule the
                // next one at the next interval from now.
                self.interval
            } else {
                // Less than one full tick early, so take the offset
                // into account.
                self.interval + early
            }
        };
        let tick_length = tick - (self.next - self.interval);
        trace!("scheduling next tick in {} secs", interval.as_float_secs());
        self.next = tick + interval;
        (tick_length, interval)
    }
}
