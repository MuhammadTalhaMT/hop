#![forbid(unsafe_code)]

//! The `hop` binary crate: configuration loading and, eventually, the CLI
//! that drives the platform loop. This crate wires `hop-core` and
//! `hop-proto` together; it holds no protocol or state-machine logic of
//! its own.

pub mod config;
pub mod keymap;
pub mod settings;
