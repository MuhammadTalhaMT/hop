#![forbid(unsafe_code)]

//! Control logic for hop: state machine, remapping, transport, supervision.

pub mod remap;
pub use remap::RemapTable;

pub mod held;
pub use held::HeldKeys;

pub mod control;
pub use control::{Action, Control, Focus};

pub mod transport;
pub use transport::{Transport, TransportError};

pub mod liveness;
pub use liveness::Liveness;

pub mod backoff;
pub use backoff::Backoff;

pub mod device;
pub use device::{Capturer, DeviceError, FakeCapturer, FakeInjector, Injector, InputEvent};

pub mod session;
pub use session::{event_to_message, message_to_event, pump_client, pump_server, send_release_all};
