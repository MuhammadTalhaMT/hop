#![forbid(unsafe_code)]

//! Control logic for hop: state machine, remapping, transport, supervision.

pub mod remap;
pub use remap::RemapTable;

pub mod held;
pub use held::HeldKeys;

pub mod control;
pub use control::{Action, Control, Focus};

pub mod transport;
pub use transport::{split, TransportError, TransportReader, TransportWriter};

pub mod liveness;
pub use liveness::Liveness;

pub mod backoff;
pub use backoff::Backoff;

pub mod warp;
pub use warp::WarpDebt;

pub mod screen;
pub use screen::{Rect, Screen, Segment, Side};

pub mod device;
pub use device::{
    Capturer, DeviceError, FailingInjector, FakeCapturer, FakeInjector, Injector, InputEvent,
};

pub mod session;
pub use session::{
    coalesce_motion, event_to_message, message_to_event, pump_client, pump_server, send_release_all,
};

pub mod handshake;
pub use handshake::{client_handshake, derive_session, server_handshake, HandshakeError};

pub mod supervisor;
pub use supervisor::{release_everything, ClientSupervisor, ReconnectPolicy};

pub mod clipboard;
pub use clipboard::{Clipboard, ClipboardSync, LocalChange, MAX_CLIPBOARD_BYTES};

pub mod filetransfer;
pub use filetransfer::{send_local_change, FileError, FileReceive, CHUNK_BYTES, MAX_FILE_BYTES};
