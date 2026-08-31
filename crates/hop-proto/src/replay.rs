const WINDOW: u64 = 64;

/// Sliding window of recently seen sequence numbers.
///
/// Accepts each sequence number exactly once. Tolerates mild reordering
/// (within `WINDOW`) because the network may legitimately deliver frames
/// out of order, but refuses anything already seen or older than the
/// window, which is what defeats a capture-and-replay attack within a
/// single session.
///
/// A fresh `ReplayWindow` is created every time a `Transport` is
/// constructed, so its protection resets at the start of each connection
/// and lasts only for that connection's lifetime. It has nothing to say
/// about a frame captured during one session and replayed at the start of
/// a later one, a fresh window accepts sequence numbers starting from
/// scratch. That gap is what [`crate::SessionId`] closes: binding a frame
/// to the session it was sealed under means a later session's fresh
/// window never even sees a valid tag to accept. Until Plan B's handshake
/// exists, every session uses [`crate::SessionId::ZERO`], so in a running
/// system this window's per-connection reset is, for now, the only replay
/// protection actually in effect.
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

    /// Must only be called with a sequence number that has already been
    /// authenticated by [`crate::open`] (i.e. its AEAD tag has already
    /// verified). Checking the window before authentication would let an
    /// attacker forge a single frame carrying `seq = u64::MAX`, without
    /// ever knowing the key, to pin `highest` at the maximum and
    /// permanently reject every genuine frame afterward, a one-packet
    /// denial of service. Callers must authenticate first, then call
    /// `accept`, never the other way around.
    pub fn accept(&mut self, seq: u64) -> bool {
        if !self.started {
            self.started = true;
            self.highest = seq;
            return true;
        }

        if seq > self.highest {
            let shift = seq - self.highest;
            // `>` and not `>=`: age == WINDOW is still inside the accept
            // region on the past-side check below (`age > WINDOW` rejects,
            // so age == WINDOW is accepted and representable at bit
            // WINDOW - 1). A jump of exactly WINDOW must therefore still
            // record the old highest at that bit instead of zeroing the
            // mask, or a replay of the old highest right after the jump
            // would wrongly be accepted. Do not "simplify" this to `>=`.
            self.seen = if shift > WINDOW {
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

    #[test]
    fn refuses_replay_at_the_forward_jump_boundary() {
        // A jump of exactly WINDOW leaves the previous highest at the oldest
        // still-accepted age. Forgetting it there would let one captured
        // frame replay after 63 dropped frames, which is the whole attack
        // this type exists to stop.
        let mut w = ReplayWindow::new();
        assert!(w.accept(100));
        assert!(w.accept(164));
        assert!(!w.accept(100), "replay at shift == WINDOW must be refused");
    }

    #[test]
    fn window_edge_accepts_once_then_refuses() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(200));
        assert!(w.accept(263));
        assert!(w.accept(201), "age 62 is inside the window");
        assert!(!w.accept(201));
    }

    #[test]
    fn refuses_just_outside_the_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(500));
        assert!(!w.accept(435), "age 65 is outside the window");
    }
}
