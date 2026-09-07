//! A Windows notification area icon for starting and stopping hop.
//!
//! The counterpart to `hop`'s macOS menu bar item, and it exists for the
//! same reason: hop is a thing you leave running, and leaving a console
//! window open forever to do that is not what a tool people actually use
//! looks like.
//!
//! Written against the Win32 API directly rather than through a tray
//! crate. This crate is already the FFI crate, the whole surface used
//! here is about a dozen calls, and the alternative pulls in a windowing
//! stack to draw one icon.
//!
//! The design deliberately mirrors the Mac's: this module knows how to
//! show an icon and a menu and nothing else. What Start and Stop
//! actually do, including running hop as a separate process so a fault
//! in the engine cannot take the tray down with it, stays in `hop`.

use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    TrackPopupMenu, TranslateMessage, HWND_MESSAGE, IDI_APPLICATION, MF_GRAYED, MF_STRING, MSG,
    TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
};

/// What the user picked from the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Start,
    Stop,
    Quit,
}

/// The message the shell sends this window when the icon is clicked.
/// `WM_APP` and above are reserved for applications, which is exactly
/// what this is for.
const WM_TRAY: u32 = WM_APP + 1;

const ID_START: usize = 1;
const ID_STOP: usize = 2;
const ID_QUIT: usize = 3;

/// Menu picks, waiting to be handed to the caller's handler.
///
/// A queue rather than a callback because the window procedure is a
/// plain C function pointer: it cannot borrow the caller's closure, and
/// giving it a raw pointer to one would mean promising the closure
/// outlives every message the shell will ever send. Pushing to a static
/// queue and draining it in `run` needs no such promise.
static PENDING: Mutex<Vec<TrayEvent>> = Mutex::new(Vec::new());

/// Whether hop is currently running, so the menu can grey out whichever
/// of Start and Stop would do nothing. Read inside the window procedure,
/// which is why it is here and not on `Tray`.
static RUNNING: Mutex<bool> = Mutex::new(false);

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Handles messages for the hidden window that owns the tray icon.
///
/// # Safety
///
/// Called only by Windows, with the arguments it documents for a window
/// procedure. It touches no state but the two statics above, both behind
/// mutexes, and every pointer it passes on is either null or a local it
/// owns for the duration of the call.
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            // The shell packs the mouse message into the low word.
            let event = (lparam as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                unsafe { show_menu(window) };
            }
            0
        }
        WM_COMMAND => {
            let picked = match wparam & 0xFFFF {
                ID_START => Some(TrayEvent::Start),
                ID_STOP => Some(TrayEvent::Stop),
                ID_QUIT => Some(TrayEvent::Quit),
                _ => None,
            };
            if let Some(event) = picked {
                if let Ok(mut pending) = PENDING.lock() {
                    pending.push(event);
                }
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: no arguments, and documented as callable from a
            // window procedure; it only sets the quit flag on this
            // thread's message queue.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: handing the message back untouched is exactly the
        // contract of the default window procedure.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// Builds and shows the right-click menu at the cursor.
///
/// # Safety
///
/// `window` must be the live window this module created.
unsafe fn show_menu(window: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let running = RUNNING.lock().map(|r| *r).unwrap_or(false);
        // Greyed rather than hidden, so the menu never changes shape and
        // the state is legible at a glance.
        let start_flags = if running { MF_GRAYED } else { MF_STRING };
        let stop_flags = if running { MF_STRING } else { MF_GRAYED };
        AppendMenuW(menu, start_flags, ID_START, wide("Start hop").as_ptr());
        AppendMenuW(menu, stop_flags, ID_STOP, wide("Stop hop").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_QUIT, wide("Quit").as_ptr());

        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        // Without this the menu will not dismiss when the user clicks
        // away from it, which is a documented quirk of tray menus.
        SetForegroundWindow(window);
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            window,
            std::ptr::null(),
        );
        DestroyMenu(menu);
    }
}

/// A tray icon and the hidden window that owns it.
pub struct Tray {
    window: HWND,
    icon: NOTIFYICONDATAW,
}

impl Tray {
    /// Creates the hidden window and adds the icon to the tray.
    pub fn new(tooltip: &str) -> Result<Self, String> {
        let class_name = wide("hop-tray");

        // SAFETY: `GetModuleHandleW(null)` is documented as returning
        // this process's own module handle and cannot fail for that
        // argument. The class and window creation are passed pointers to
        // buffers that outlive the calls; the class name in particular is
        // held in `class_name` for the whole function, and Windows copies
        // what it needs out of `WNDCLASSW`.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());

            let mut class: WNDCLASSW = std::mem::zeroed();
            class.lpfnWndProc = Some(window_proc);
            class.hInstance = instance;
            class.lpszClassName = class_name.as_ptr();
            // A second Tray in one process would fail to register the
            // class again, which is not an error worth failing on: the
            // class it wanted is already there.
            RegisterClassW(&class);

            let window = CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                // Message only: no taskbar button, no visible window, no
                // painting. All this window does is receive the shell's
                // click notifications.
                HWND_MESSAGE,
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if window.is_null() {
                return Err("could not create the tray's window".into());
            }

            let mut icon: NOTIFYICONDATAW = std::mem::zeroed();
            icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            icon.hWnd = window;
            icon.uID = 1;
            icon.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            icon.uCallbackMessage = WM_TRAY;
            icon.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
            write_tip(&mut icon.szTip, tooltip);

            if Shell_NotifyIconW(NIM_ADD, &icon) == 0 {
                return Err("the shell refused to add hop's tray icon".into());
            }
            Ok(Self { window, icon })
        }
    }

    /// Updates what the menu offers and what the tooltip says.
    pub fn set_running(&mut self, running: bool, tooltip: &str) {
        if let Ok(mut flag) = RUNNING.lock() {
            *flag = running;
        }
        write_tip(&mut self.icon.szTip, tooltip);
        // SAFETY: `self.icon` is the same fully initialised structure
        // that NIM_ADD accepted, still naming a live window.
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &self.icon);
        }
    }

    /// Pumps the message loop, calling `handler` for every menu pick.
    ///
    /// The handler is given the tray back, so it can call `set_running`
    /// in response to what the user just picked without having to hold a
    /// second reference to something this loop already borrows.
    ///
    /// Returns when the user picks Quit, or when Windows ends the loop.
    /// This blocks the calling thread for the life of the tray, which is
    /// what it is for.
    pub fn run<F: FnMut(&mut Self, TrayEvent)>(&mut self, mut handler: F) {
        // SAFETY: a standard Win32 message loop. `message` is a local
        // this loop owns, and `GetMessageW` fills it before either other
        // call reads it. A zero return means WM_QUIT, and a negative one
        // means the queue is broken; both end the loop rather than
        // looping on a message that was never written.
        unsafe {
            let mut message: MSG = std::mem::zeroed();
            loop {
                let got = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
                if got <= 0 {
                    return;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);

                let drained: Vec<TrayEvent> = match PENDING.lock() {
                    Ok(mut pending) => pending.drain(..).collect(),
                    Err(_) => Vec::new(),
                };
                for event in drained {
                    handler(self, event);
                    if event == TrayEvent::Quit {
                        return;
                    }
                }
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // Take the icon out of the tray rather than leaving a dead one
        // behind that only disappears when the user hovers it.
        //
        // SAFETY: `self.icon` still names this Tray's own window and id,
        // and this runs once, since `Tray` is not `Clone`.
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &self.icon);
        }
        let _ = self.window;
    }
}

/// Copies `text` into a fixed tooltip buffer, truncated to fit and always
/// null terminated. The shell reads until the null, so an unterminated
/// buffer would leak whatever followed it into the tooltip.
fn write_tip(buffer: &mut [u16; 128], text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().take(buffer.len() - 1).collect();
    buffer.fill(0);
    buffer[..encoded.len()].copy_from_slice(&encoded);
}
