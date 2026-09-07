//! Not forwarding the operating system's own cursor moves as if they
//! were the user's.
//!
//! When hop moves the local cursor itself, which it does when focus
//! leaves this machine and the cursor has to stop sitting on whatever it
//! was hovering, macOS does not simply move it. It folds the
//! displacement into the delta fields of the NEXT motion event the tap
//! sees, so that an application tracking relative motion stays
//! consistent with where the cursor actually is. SDL and GLFW both carry
//! a version of this type for the same reason.
//!
//! hop reads exactly those delta fields and sends them to the peer, so
//! without this the warp arrives on the other machine as a real hand
//! movement. It is not a small one either: it is the vector from the
//! crossing point to the middle of this screen, which on a crossing to a
//! PC above the Mac cancelled the crossing position exactly and dumped
//! the cursor in the centre of the PC's taskbar on every single
//! crossing.
//!
//! The fix is to swallow one motion sample, not to subtract the known
//! displacement. Subtracting is only correct if the OS reports the
//! displacement exactly once, in full, on exactly the next event; if it
//! ever does not, subtraction injects the negated warp as a fresh lie
//! and throws the cursor the other way. Swallowing is right under every
//! behaviour: either the polluted sample is dropped, or one real sample
//! is, and one sample at 125 Hz or more is not perceptible.

/// Tracks that a warp has happened and the next motion event is not to
/// be trusted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WarpDebt {
    pending: bool,
}

impl WarpDebt {
    pub fn new() -> Self {
        Self { pending: false }
    }

    /// Records that hop just moved the cursor itself.
    pub fn note_warp(&mut self) {
        self.pending = true;
    }

    /// Whether this motion event should be dropped rather than
    /// forwarded. Clears the debt, so exactly one event is ever
    /// swallowed per warp.
    pub fn absorb(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }

    /// Whether a warp is still waiting to be absorbed.
    pub fn is_pending(&self) -> bool {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_swallowed_when_hop_has_not_warped() {
        let mut debt = WarpDebt::new();
        assert!(!debt.absorb());
        assert!(!debt.absorb());
    }

    #[test]
    fn exactly_one_event_is_swallowed_per_warp() {
        // One, not two: the second motion event after a crossing is the
        // user's hand, and dropping that would make movement stutter
        // every time focus changed machines.
        let mut debt = WarpDebt::new();
        debt.note_warp();
        assert!(debt.absorb(), "the polluted event must be dropped");
        assert!(!debt.absorb(), "the next one is the user's real movement");
    }

    #[test]
    fn two_warps_before_any_motion_still_swallow_only_one() {
        // The debt is a flag, not a count, and deliberately so: the
        // displacement the OS reports is the total since the last event,
        // so two warps in a row still pollute exactly one event.
        let mut debt = WarpDebt::new();
        debt.note_warp();
        debt.note_warp();
        assert!(debt.absorb());
        assert!(!debt.absorb());
    }

    #[test]
    fn a_debt_survives_until_a_motion_event_actually_arrives() {
        // Key and button events pass through without absorbing, since
        // they carry no deltas to be polluted. The debt has to still be
        // there when motion finally comes.
        let mut debt = WarpDebt::new();
        debt.note_warp();
        assert!(debt.is_pending());
        assert!(debt.is_pending(), "checking must not clear the debt");
        assert!(debt.absorb());
        assert!(!debt.is_pending());
    }
}
