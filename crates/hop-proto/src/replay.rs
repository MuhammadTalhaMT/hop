const WINDOW: u64 = 64;

/// Sliding window of recently seen sequence numbers.
///
/// Accepts each sequence number exactly once. Tolerates mild reordering
/// (within `WINDOW`) because the network may legitimately deliver frames
/// out of order, but refuses anything already seen or older than the
/// window, which is what defeats a capture-and-replay attack.
#[derive(Debug, Default)]
pub struct ReplayWindow {
    highest: u64,
    /// Bit i set means `highest - 1 - i` has been seen.
    seen: u64,
    started: bool,
}

impl ReplayWindow {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&mut self, seq: u64) -> bool {
        if !self.started {
            self.started = true;
            self.highest = seq;
            return true;
        }

        if seq > self.highest {
            let shift = seq - self.highest;
            self.seen = if shift >= WINDOW {
                0
            } else {
                // Record that the old highest was seen, then shift.
                ((self.seen << 1) | 1) << (shift - 1)
            };
            self.highest = seq;
            return true;
        }

        if seq == self.highest {
            return false;
        }

        let age = self.highest - seq;
        if age > WINDOW {
            return false;
        }
        let bit = 1u64 << (age - 1);
        if self.seen & bit != 0 {
            return false;
        }
        self.seen |= bit;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_increasing_sequence() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(1));
        assert!(w.accept(2));
        assert!(w.accept(3));
    }

    #[test]
    fn rejects_exact_replay() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(5));
        assert!(
            !w.accept(5),
            "the same sequence number must never be accepted twice"
        );
    }

    #[test]
    fn accepts_out_of_order_within_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(10));
        assert!(w.accept(8), "slightly reordered delivery is legitimate");
        assert!(!w.accept(8));
    }

    #[test]
    fn rejects_far_past() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(1000));
        assert!(!w.accept(1), "older than the window must be refused");
    }

    #[test]
    fn handles_large_forward_jump() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(1));
        assert!(w.accept(u64::from(u32::MAX)));
        assert!(!w.accept(1));
    }
}
