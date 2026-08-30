#![forbid(unsafe_code)]

//! Control logic for hop: state machine, remapping, transport, supervision.

pub mod remap;
pub use remap::RemapTable;
