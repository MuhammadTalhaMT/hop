use std::time::Duration;

/// Exponential backoff with a ceiling. Never gives up: `next_delay` keeps
/// returning the capped delay indefinitely, because the peer may be down
/// for hours and must still be picked up when it returns.
#[derive(Debug)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    current: Option<Duration>,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            current: None,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = match self.current {
            None => self.initial,
            // `checked_mul` guards against overflow if doubling would
            // exceed what a Duration can represent; saturating to `max`
            // in that case still upholds the cap.
            Some(previous) => previous.checked_mul(2).unwrap_or(self.max).min(self.max),
        };
        self.current = Some(delay);
        delay
    }

    /// Call after a successful connection so the next outage retries fast.
    pub fn reset(&mut self) {
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn backoff() -> Backoff {
        Backoff::new(Duration::from_millis(100), Duration::from_secs(5))
    }

    #[test]
    fn starts_at_the_initial_delay() {
        assert_eq!(backoff().next_delay(), Duration::from_millis(100));
    }

    #[test]
    fn doubles_each_attempt() {
        let mut b = backoff();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(400));
    }

    #[test]
    fn never_exceeds_the_cap() {
        let mut b = backoff();
        for _ in 0..50 {
            assert!(b.next_delay() <= Duration::from_secs(5));
        }
    }

    #[test]
    fn keeps_retrying_forever_at_the_cap() {
        // It must never give up: an unattended machine has to reconnect
        // after an outage of any length.
        let mut b = backoff();
        for _ in 0..50 {
            b.next_delay();
        }
        assert_eq!(b.next_delay(), Duration::from_secs(5));
    }

    #[test]
    fn reset_returns_to_the_initial_delay() {
        let mut b = backoff();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }
}
