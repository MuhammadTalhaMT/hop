use hop_proto::Usage;
use std::collections::BTreeSet;

/// The set of keys currently held down on the remote machine.
///
/// Every control transition drains this set and synthesizes a release for
/// each key, which is what stops a dropped connection from leaving a
/// modifier stuck down on the receiving side.
#[derive(Debug, Default)]
pub struct HeldKeys {
    down: BTreeSet<Usage>,
}

impl HeldKeys {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, usage: Usage, pressed: bool) {
        if pressed {
            self.down.insert(usage);
        } else {
            self.down.remove(&usage);
        }
    }

    pub fn held(&self) -> Vec<Usage> {
        self.down.iter().copied().collect()
    }

    pub fn drain_release(&mut self) -> Vec<Usage> {
        let all = self.held();
        self.down.clear();
        all
    }

    pub fn is_empty(&self) -> bool {
        self.down.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_press_and_release() {
        let mut keys = HeldKeys::new();
        keys.record(Usage::LEFT_GUI, true);
        assert_eq!(keys.held(), vec![Usage::LEFT_GUI]);
        keys.record(Usage::LEFT_GUI, false);
        assert!(keys.is_empty());
    }

    #[test]
    fn repeated_press_does_not_duplicate() {
        let mut keys = HeldKeys::new();
        keys.record(Usage::A, true);
        keys.record(Usage::A, true); // key repeat
        assert_eq!(keys.held(), vec![Usage::A]);
    }

    #[test]
    fn release_of_unheld_key_is_harmless() {
        let mut keys = HeldKeys::new();
        keys.record(Usage::A, false);
        assert!(keys.is_empty());
    }

    #[test]
    fn drain_returns_everything_and_clears() {
        let mut keys = HeldKeys::new();
        keys.record(Usage::LEFT_GUI, true);
        keys.record(Usage::C, true);
        let drained = keys.drain_release();
        assert_eq!(drained, vec![Usage::C, Usage::LEFT_GUI]);
        assert!(keys.is_empty(), "draining must leave nothing held");
    }

    #[test]
    fn drain_on_empty_is_empty() {
        let mut keys = HeldKeys::new();
        assert!(keys.drain_release().is_empty());
    }
}
