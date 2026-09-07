//! Reattaching a console to a GUI process.
//!
//! `hop.exe` is built as a Windows GUI application, so that double
//! clicking it, or starting it from the tray, never flashes up a console
//! window. That is the whole point of the tray existing.
//!
//! The cost is that a GUI process starts with no console at all, so
//! running `hop keygen` from a command prompt would print into nowhere.
//! `attach_parent` gives it the console it was launched from, when there
//! is one, so the command line half of hop keeps working.

use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};

/// Attaches this process to the console of whatever launched it.
///
/// A no-op when there is no such console, which is the case when hop is
/// double clicked or started by the tray, and is not an error: it is
/// exactly the situation this is here to tolerate.
pub fn attach_parent() {
    // SAFETY: takes a single documented constant and no pointers. It
    // fails harmlessly when the parent has no console, which is why the
    // result is deliberately ignored.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
