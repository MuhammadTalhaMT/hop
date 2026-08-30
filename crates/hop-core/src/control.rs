use crate::HeldKeys;
use hop_proto::Usage;

/// Where input is currently going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Input stays on this machine.
    Local,
    /// Input is being sent to the peer.
    Remote,
}

/// What the caller should do as a result of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this key to the peer.
    Forward(Usage, bool),
    /// Tell the peer to release every key it believes is held.
    ReleaseAll,
    None,
}

/// Decides whether input goes to this machine or the peer, and guarantees
/// that focus always comes home on any failure. Every path that leaves
/// `Remote` releases held keys first.
#[derive(Debug)]
pub struct Control {
    focus: Focus,
    held: HeldKeys,
}

impl Control {
    pub fn new() -> Self {
        Self {
            focus: Focus::Local,
            held: HeldKeys::new(),
        }
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn on_edge_crossed(&mut self) -> Action {
        self.focus = Focus::Remote;
        Action::None
    }

    pub fn on_key(&mut self, usage: Usage, pressed: bool) -> Action {
        match self.focus {
            Focus::Local => Action::None,
            Focus::Remote => {
                self.held.record(usage, pressed);
                Action::Forward(usage, pressed)
            }
        }
    }

    pub fn on_release_requested(&mut self) -> Action {
        self.return_focus()
    }

    pub fn on_disconnected(&mut self) -> Action {
        self.return_focus()
    }

    pub fn on_panic_hotkey(&mut self) -> Action {
        self.return_focus()
    }

    fn return_focus(&mut self) -> Action {
        let was_remote = self.focus == Focus::Remote;
        self.focus = Focus::Local;
        let had_keys = !self.held.is_empty();
        self.held.drain_release();
        if was_remote && had_keys {
            Action::ReleaseAll
        } else {
            Action::None
        }
    }
}

impl Default for Control {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_local() {
        assert_eq!(Control::new().focus(), Focus::Local);
    }

    #[test]
    fn crossing_the_edge_takes_focus_remote() {
        let mut c = Control::new();
        assert_eq!(c.on_edge_crossed(), Action::None);
        assert_eq!(c.focus(), Focus::Remote);
    }

    #[test]
    fn keys_are_dropped_while_local() {
        let mut c = Control::new();
        assert_eq!(c.on_key(Usage::C, true), Action::None);
    }

    #[test]
    fn keys_are_forwarded_while_remote() {
        let mut c = Control::new();
        c.on_edge_crossed();
        assert_eq!(c.on_key(Usage::C, true), Action::Forward(Usage::C, true));
    }

    #[test]
    fn release_returns_focus_and_clears_held_keys() {
        let mut c = Control::new();
        c.on_edge_crossed();
        c.on_key(Usage::LEFT_GUI, true);
        assert_eq!(c.on_release_requested(), Action::ReleaseAll);
        assert_eq!(c.focus(), Focus::Local);
    }

    #[test]
    fn disconnect_while_remote_releases_and_returns_focus() {
        // The failure that motivated this project: the link dies mid-use.
        // Focus must come home so the Mac keeps working, and held keys
        // must be released so the PC is not stuck on a modifier.
        let mut c = Control::new();
        c.on_edge_crossed();
        c.on_key(Usage::LEFT_GUI, true);
        assert_eq!(c.on_disconnected(), Action::ReleaseAll);
        assert_eq!(c.focus(), Focus::Local);
    }

    #[test]
    fn disconnect_while_local_is_a_no_op() {
        let mut c = Control::new();
        assert_eq!(c.on_disconnected(), Action::None);
        assert_eq!(c.focus(), Focus::Local);
    }

    #[test]
    fn panic_hotkey_always_returns_focus() {
        let mut c = Control::new();
        c.on_edge_crossed();
        c.on_key(Usage::A, true);
        assert_eq!(c.on_panic_hotkey(), Action::ReleaseAll);
        assert_eq!(c.focus(), Focus::Local);
    }

    #[test]
    fn releasing_a_key_stops_tracking_it() {
        let mut c = Control::new();
        c.on_edge_crossed();
        c.on_key(Usage::A, true);
        c.on_key(Usage::A, false);
        // Nothing is held, so returning focus needs no release sweep.
        assert_eq!(c.on_release_requested(), Action::None);
    }
}
