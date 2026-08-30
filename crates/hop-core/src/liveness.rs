use std::time::{Duration, Instant};

/// Tracks whether the peer is still there.
///
/// Deliberately separates "when did we last hear from them" from "when did
/// we last speak", so our own outbound heartbeats can never make a silent
/// peer look alive. That distinction is what detects a half-open socket.
#[derive(Debug)]
pub struct Liveness {
    interval: Duration,
    timeout: Duration,
    last_inbound: Option<Instant>,
    last_outbound: Option<Instant>,
}

impl Liveness {
    /// `now` seeds both clocks, so a freshly opened connection is neither
    /// considered dead nor immediately due for a heartbeat.
    pub fn new(now: Instant, interval: Duration, timeout: Duration) -> Self {
        Self {
            interval,
            timeout,
            last_inbound: Some(now),
            last_outbound: Some(now),
        }
    }

    /// Call whenever anything is received from the peer.
    pub fn record_activity(&mut self, now: Instant) {
        self.last_inbound = Some(now);
    }

    pub fn record_heartbeat_sent(&mut self, now: Instant) {
        self.last_outbound = Some(now);
    }

    pub fn should_send_heartbeat(&self, now: Instant) -> bool {
        match self.last_outbound {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.interval,
        }
    }

    pub fn is_dead(&self, now: Instant) -> bool {
        match self.last_inbound {
            None => false,
            Some(last) => now.saturating_duration_since(last) > self.timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn setup() -> (Liveness, Instant) {
        let start = Instant::now();
        let l = Liveness::new(start, Duration::from_secs(1), Duration::from_secs(3));
        (l, start)
    }

    #[test]
    fn fresh_connection_is_alive() {
        let (l, start) = setup();
        assert!(!l.is_dead(start));
    }

    #[test]
    fn silence_past_the_timeout_is_death() {
        let (l, start) = setup();
        assert!(!l.is_dead(start + Duration::from_secs(2)));
        assert!(l.is_dead(start + Duration::from_secs(4)));
    }

    #[test]
    fn activity_keeps_it_alive() {
        let (mut l, start) = setup();
        l.record_activity(start + Duration::from_secs(2));
        assert!(!l.is_dead(start + Duration::from_secs(4)));
    }

    #[test]
    fn heartbeat_is_due_after_the_interval() {
        let (l, start) = setup();
        assert!(!l.should_send_heartbeat(start));
        assert!(l.should_send_heartbeat(start + Duration::from_millis(1100)));
    }

    #[test]
    fn sending_a_heartbeat_resets_the_interval() {
        let (mut l, start) = setup();
        let later = start + Duration::from_millis(1100);
        l.record_heartbeat_sent(later);
        assert!(!l.should_send_heartbeat(later));
    }

    #[test]
    fn sending_heartbeats_does_not_mask_a_dead_peer() {
        // We must not treat our own outbound traffic as proof the peer is
        // alive, or a half-open socket would look healthy forever.
        let (mut l, start) = setup();
        for i in 1..=5 {
            l.record_heartbeat_sent(start + Duration::from_secs(i));
        }
        assert!(l.is_dead(start + Duration::from_secs(5)));
    }

    #[test]
    fn heartbeat_is_due_exactly_at_the_interval() {
        // The interval is inclusive: at exactly one interval a heartbeat is
        // due, so a peer never waits longer than the configured period.
        let (l, start) = setup();
        assert!(l.should_send_heartbeat(start + Duration::from_secs(1)));
    }

    #[test]
    fn exactly_at_the_timeout_is_not_yet_dead() {
        // The timeout is exclusive: a peer heard from exactly at the limit
        // is still alive, and only silence BEYOND it counts as death.
        let (l, start) = setup();
        assert!(!l.is_dead(start + Duration::from_secs(3)));
        assert!(l.is_dead(start + Duration::from_millis(3001)));
    }
}
