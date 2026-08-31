//! Windows implementation of hop's capture and injection traits.

pub mod keymap;

// `inject` calls into `windows-sys` (`SendInput`, `GetLastError`), which is
// only present as a dependency on Windows (see `hop-platform`'s
// `Cargo.toml`). Gating the declaration here, rather than gating items
// inside the file, keeps the whole module off the build on any other host.
#[cfg(target_os = "windows")]
pub mod inject;

#[cfg(target_os = "windows")]
pub use inject::WindowsInjector;
