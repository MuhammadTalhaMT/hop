//! Hiding the PC's cursor while focus is on the Mac.
//!
//! The Mac already hides its own cursor when focus crosses to the PC.
//! Without the same thing here, the PC's arrow sits visibly on the
//! Windows desktop the whole time the user is working on the Mac, which
//! is exactly as odd as it sounds.
//!
//! Windows has no "hide the cursor for every application" call.
//! `ShowCursor` is per input queue and applies to the calling thread's
//! windows, which hop does not have. The only thing that works from
//! outside is replacing the system cursor images themselves, which is
//! what this does, and it is why the restore path here is written far
//! more carefully than the hide path.
//!
//! THE RISK, stated plainly: this changes a system wide setting. If hop
//! dies while the cursors are blank, the PC has no visible cursor until
//! something restores them. Three things make that recoverable rather
//! than a disaster:
//!
//! 1. `restore` is called unconditionally when hop starts, exactly like
//!    `release_all_modifiers`, so simply running hop again repairs it.
//! 2. It is called when focus returns, on Ctrl-C, and from a guard that
//!    runs while unwinding.
//! 3. `SPI_SETCURSORS` restores the user's own cursor scheme from their
//!    settings rather than guessing at defaults, so what comes back is
//!    what they had, including a custom cursor theme.

use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateCursor, SetSystemCursor, SystemParametersInfoW, OCR_APPSTARTING, OCR_CROSS, OCR_HAND,
    OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE,
    OCR_UP, OCR_WAIT, SPI_SETCURSORS,
};

/// Every system cursor a user could plausibly see. Blanking only the
/// arrow would leave the cursor visible the moment it happened to be
/// over a text field or a window edge.
const SYSTEM_CURSORS: &[u32] = &[
    OCR_NORMAL,
    OCR_IBEAM,
    OCR_WAIT,
    OCR_CROSS,
    OCR_UP,
    OCR_SIZENWSE,
    OCR_SIZENESW,
    OCR_SIZEWE,
    OCR_SIZENS,
    OCR_SIZEALL,
    OCR_NO,
    OCR_HAND,
    OCR_APPSTARTING,
];

/// Whether hop currently has the cursors blanked.
///
/// Tracked so `restore` can be called freely from every path that might
/// need it (focus returning, Ctrl-C, a guard unwinding, startup) without
/// each one having to know whether it is the one that hid them.
static HIDDEN: AtomicBool = AtomicBool::new(false);

/// A 32x32 cursor that draws nothing.
///
/// The AND mask is all ones and the XOR mask all zeros, which is the
/// documented encoding for "leave every pixel of the screen exactly as
/// it is": a cursor that is entirely transparent rather than a white
/// square.
fn blank_cursor() -> windows_sys::Win32::UI::WindowsAndMessaging::HCURSOR {
    const SIZE: i32 = 32;
    const BYTES: usize = (SIZE * SIZE / 8) as usize;
    let and_mask = [0xFFu8; BYTES];
    let xor_mask = [0x00u8; BYTES];

    // SAFETY: both masks are exactly the width times height divided by
    // eight bytes the call documents for a 32x32 monochrome cursor, and
    // they live for the duration of the call. A null instance handle
    // means "not from a module", which is correct for masks built here.
    unsafe {
        CreateCursor(
            std::ptr::null_mut(),
            0,
            0,
            SIZE,
            SIZE,
            and_mask.as_ptr() as *const core::ffi::c_void,
            xor_mask.as_ptr() as *const core::ffi::c_void,
        )
    }
}

/// Replaces every system cursor with a blank one.
///
/// Idempotent: calling it while already hidden does nothing, so it is
/// safe on every crossing rather than only the first.
pub fn hide() {
    if HIDDEN.swap(true, Ordering::SeqCst) {
        return;
    }
    for id in SYSTEM_CURSORS {
        let blank = blank_cursor();
        if blank.is_null() {
            continue;
        }
        // SAFETY: `blank` is a cursor this process just created and has
        // not shared. SetSystemCursor takes ownership and destroys it,
        // which is why a fresh one is made for each id rather than one
        // handle being reused: reusing it would be a double free.
        unsafe {
            SetSystemCursor(blank, *id);
        }
    }
    tracing::debug!("hid the PC's cursor while focus is on the peer");
}

/// Puts the user's own cursors back.
///
/// Safe to call at any time, including when hop never hid anything,
/// which is what lets it run unconditionally at startup to repair a
/// previous run that died while hidden.
pub fn restore() {
    // Deliberately does NOT check `HIDDEN` first. The whole point of the
    // startup call is to repair a state left by a process that is gone,
    // whose flag went with it.
    HIDDEN.store(false, Ordering::SeqCst);

    // SAFETY: `SPI_SETCURSORS` takes no parameter, which is why the
    // pointer is null and the counts are zero; this is the documented
    // form for asking Windows to reload the cursor scheme from the
    // user's own settings.
    unsafe {
        SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0);
    }
}

/// Restores the cursors when dropped, including while unwinding from a
/// panic. Held by the client for as long as it is running, so there is
/// no ordinary exit path that can leave the PC without a cursor.
pub struct RestoreOnDrop;

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        restore();
    }
}
