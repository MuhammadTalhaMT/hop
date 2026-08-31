//! Reading and writing the Windows clipboard, and noticing when it changes.
//!
//! Change detection uses `GetClipboardSequenceNumber`, which Windows bumps
//! on every clipboard write by any process. Polling that integer is much
//! cheaper than reading the clipboard itself on a timer, and mirrors what
//! the macOS side does with `NSPasteboard`'s `changeCount`.
//!
//! Every function here is best effort. The clipboard is a shared global
//! resource that any other process can hold open, so `OpenClipboard` can
//! and does fail transiently. A failure means one copy does not reach the
//! other machine, which is not worth returning an error over, let alone
//! panicking on.

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

/// Guard that closes the clipboard however the caller leaves the scope.
///
/// Windows requires exactly one `CloseClipboard` per successful
/// `OpenClipboard`, and a missed close locks every other application out
/// of the clipboard until the process exits. An early return on a
/// null handle is easy to write and easy to get wrong, so the pairing is
/// enforced by `Drop` rather than by remembering.
struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Option<Self> {
        // SAFETY: passing a null window handle associates the clipboard
        // with the current task, which is what a process with no window
        // wants. The call either succeeds or returns zero, and only a
        // success produces a guard, so `Drop` can never close a clipboard
        // that was never opened.
        let opened = unsafe { OpenClipboard(std::ptr::null_mut()) };
        if opened == 0 {
            None
        } else {
            Some(Self)
        }
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: only reachable when `open` succeeded, so this closes
        // exactly one successful open.
        unsafe {
            CloseClipboard();
        }
    }
}

/// Windows' clipboard change counter. Zero if it cannot be read, which
/// simply reads as "no change" and disables syncing rather than failing.
pub fn change_count() -> i64 {
    // SAFETY: takes no arguments and returns a value; touches no memory
    // this crate owns.
    unsafe { GetClipboardSequenceNumber() as i64 }
}

/// Current clipboard contents as text, or `None` if it holds something
/// that is not text, is empty, or could not be read.
pub fn get_text() -> Option<String> {
    let _guard = ClipboardGuard::open()?;

    // SAFETY: the handle belongs to the clipboard, not to us, so it must
    // not be freed. It stays valid while the clipboard is open, which the
    // guard guarantees for this whole scope.
    let handle: HANDLE = unsafe { GetClipboardData(CF_UNICODETEXT as u32) };
    if handle.is_null() {
        return None;
    }

    // SAFETY: locking a moveable global handle yields a pointer valid
    // until the matching unlock, which happens on every path below.
    let ptr = unsafe { GlobalLock(handle as HGLOBAL) } as *const u16;
    if ptr.is_null() {
        return None;
    }

    // SAFETY: CF_UNICODETEXT is documented as a null terminated UTF-16
    // string, so scanning for the terminator is bounded by the clipboard's
    // own contents.
    let text = unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        String::from_utf16(slice).ok()
    };

    // SAFETY: pairs with the successful lock above.
    unsafe {
        GlobalUnlock(handle as HGLOBAL);
    }

    text
}

/// Replace the clipboard contents with `text`. Returns whether it worked.
pub fn set_text(text: &str) -> bool {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let bytes = std::mem::size_of_val(utf16.as_slice());

    let _guard = match ClipboardGuard::open() {
        Some(g) => g,
        None => return false,
    };

    // SAFETY: emptying is required before taking ownership of the
    // clipboard, and is valid while it is open.
    unsafe {
        EmptyClipboard();
    }

    // SAFETY: allocating moveable global memory of a size we computed
    // from an owned buffer. A null return means the allocation failed and
    // there is nothing to free.
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if handle.is_null() {
        return false;
    }

    // SAFETY: the lock yields a pointer to at least `bytes` bytes, which
    // is exactly what is copied into it.
    unsafe {
        let dest = GlobalLock(handle) as *mut u16;
        if dest.is_null() {
            GlobalFree(handle);
            return false;
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dest, utf16.len());
        GlobalUnlock(handle);

        // On success the clipboard takes ownership of the handle and it
        // must NOT be freed here; on failure it does not, and it must be.
        let set = SetClipboardData(CF_UNICODETEXT as u32, handle);
        if set.is_null() {
            GlobalFree(handle);
            false
        } else {
            true
        }
    }
}

/// The Windows clipboard, as `hop-core` sees it.
pub struct WindowsClipboard;

impl hop_core::Clipboard for WindowsClipboard {
    fn change_count(&self) -> i64 {
        change_count()
    }
    fn get_text(&self) -> Option<String> {
        get_text()
    }
    fn set_text(&mut self, text: &str) -> bool {
        set_text(text)
    }
}
