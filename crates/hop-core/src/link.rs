//! What the link is doing, in the few words a person actually wants.
//!
//! The engine already logs every transition, but a log line is a poor
//! thing to build a status light out of: a reader has to parse prose that
//! was written for a human and that changes whenever someone rewords it.
//! This is the same information as a value, so that anything watching hop
//! (today, its own window) reads a state rather than scraping text.

/// Where the connection is right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// Trying to reach the peer, including every retry after a failure.
    Connecting,
    /// Connected, authenticated, and pumping input.
    Connected { peer: String },
    /// Not connected. `reason` is the short human sentence explaining
    /// why, kept because "disconnected" on its own tells the user
    /// nothing they can act on.
    Disconnected { reason: String },
}

impl LinkState {
    /// The one line a status display shows.
    pub fn summary(&self) -> String {
        match self {
            Self::Connecting => "Connecting".to_string(),
            Self::Connected { peer } => format!("Connected to {peer}"),
            Self::Disconnected { reason } => format!("Disconnected: {reason}"),
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

/// Somewhere to send link transitions.
///
/// A trait rather than a channel so the supervisor stays free of any
/// opinion about where status goes, and so tests can assert on the exact
/// sequence of transitions rather than on log output.
pub trait LinkObserver: Send {
    fn link_changed(&mut self, state: LinkState);
}

/// Discards everything. What `hop run` uses when nothing is watching,
/// which is the case whenever hop is started from a terminal.
pub struct IgnoreLink;

impl LinkObserver for IgnoreLink {
    fn link_changed(&mut self, _state: LinkState) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disconnect_says_why_rather_than_just_that() {
        // "Disconnected" alone tells the user nothing they can act on;
        // the reason is the whole value of showing it.
        let state = LinkState::Disconnected {
            reason: "connect failed".into(),
        };
        assert_eq!(state.summary(), "Disconnected: connect failed");
        assert!(!state.is_connected());
    }

    #[test]
    fn a_connection_names_the_peer() {
        let state = LinkState::Connected {
            peer: "192.168.1.42:24810".into(),
        };
        assert_eq!(state.summary(), "Connected to 192.168.1.42:24810");
        assert!(state.is_connected());
    }

    #[test]
    fn connecting_is_not_connected() {
        // The distinction the status light lives on: a reconnect loop
        // that never succeeds must not read as healthy.
        assert!(!LinkState::Connecting.is_connected());
        assert_eq!(LinkState::Connecting.summary(), "Connecting");
    }
}
