use hop_proto::{Button, Usage};

/// A single input event, in this tool's own vocabulary rather than any
/// platform's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    Mouse {
        dx: i32,
        dy: i32,
    },
    Button {
        button: Button,
        pressed: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Key {
        usage: Usage,
        pressed: bool,
    },
    /// The cursor reached the edge that hands control to the peer.
    EdgeCrossed,
}

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("the platform rejected the event: {0}")]
    Rejected(String),
}

/// Source of local input. The macOS implementation lives in Plan B.
pub trait Capturer {
    /// Returns the next captured event, or None if none is pending.
    fn poll(&mut self) -> Option<InputEvent>;
}

/// Sink that replays events as though they came from real hardware.
/// The Windows implementation lives in Plan B.
pub trait Injector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DeviceError>;
}

/// Replays a fixed script of events. Lets the whole input path be tested
/// with no mouse, no keyboard, and no second machine.
pub struct FakeCapturer {
    events: std::collections::VecDeque<InputEvent>,
}

impl FakeCapturer {
    pub fn new(events: Vec<InputEvent>) -> Self {
        Self {
            events: events.into(),
        }
    }
}

impl Capturer for FakeCapturer {
    fn poll(&mut self) -> Option<InputEvent> {
        self.events.pop_front()
    }
}

/// Records everything injected so tests can assert on it.
#[derive(Default)]
pub struct FakeInjector {
    received: Vec<InputEvent>,
}

impl FakeInjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn injected(&self) -> Vec<InputEvent> {
        self.received.clone()
    }
}

impl Injector for FakeInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DeviceError> {
        self.received.push(*event);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_capturer_yields_events_then_stops() {
        let mut c = FakeCapturer::new(vec![
            InputEvent::EdgeCrossed,
            InputEvent::Key {
                usage: Usage::C,
                pressed: true,
            },
        ]);
        assert_eq!(c.poll(), Some(InputEvent::EdgeCrossed));
        assert_eq!(
            c.poll(),
            Some(InputEvent::Key {
                usage: Usage::C,
                pressed: true
            })
        );
        assert_eq!(c.poll(), None);
    }

    #[test]
    fn fake_injector_records_in_order() {
        let mut i = FakeInjector::new();
        i.inject(&InputEvent::Mouse { dx: 1, dy: 2 }).unwrap();
        i.inject(&InputEvent::Key {
            usage: Usage::A,
            pressed: true,
        })
        .unwrap();
        assert_eq!(
            i.injected(),
            vec![
                InputEvent::Mouse { dx: 1, dy: 2 },
                InputEvent::Key {
                    usage: Usage::A,
                    pressed: true
                },
            ]
        );
    }
}
