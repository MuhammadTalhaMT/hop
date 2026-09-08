//! Windows implementation of hop's capture and injection traits.

pub mod keymap;

// Pure decision logic with no `windows-sys` calls, so, like `keymap`, it
// is built and tested on every host rather than gated to Windows.
pub mod return_edge;
pub use return_edge::ReturnEdge;

// `inject` calls into `windows-sys` (`SendInput`, `GetLastError`), which is
// only present as a dependency on Windows (see `hop-platform`'s
// `Cargo.toml`). Gating the declaration here, rather than gating items
// inside the file, keeps the whole module off the build on any other host.
#[cfg(target_os = "windows")]
pub mod clipboard;

#[cfg(target_os = "windows")]
pub mod inject;

#[cfg(target_os = "windows")]
pub use inject::WindowsInjector;

// The one network call hop makes that is not its own protocol: the
// self-updater fetching a release from GitHub. It lives here because it
// is FFI, and this is the only crate allowed any.
#[cfg(target_os = "windows")]
pub mod http;

// The notification area icon, the Windows counterpart to hop's macOS menu
// bar item.
#[cfg(target_os = "windows")]
pub mod tray;

#[cfg(target_os = "windows")]
pub mod console;

// Hiding the PC's own cursor while focus is on the Mac, the counterpart
// to what macos::cursor already does in the other direction.
#[cfg(target_os = "windows")]
pub mod cursor;

// Starting hop when Windows starts.
#[cfg(target_os = "windows")]
pub mod autostart;
