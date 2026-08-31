//! Windows input injection via `SendInput`.
//!
//! This whole file is gated to `cfg(target_os = "windows")` at its
//! declaration in `windows.rs`, since it depends on `windows-sys` types
//! that do not exist off Windows. Within the file, construction of the
//! `INPUT` structures is still split into plain functions with no Windows
//! API calls, so the Windows CI runner exercises them as ordinary unit
//! tests without needing a live desktop session; only `send_inputs` reaches
//! across the FFI boundary.

use hop_core::{DeviceError, Injector, InputEvent};
use hop_proto::{Button, Usage};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GetLastError, POINT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, WHEEL_DELTA,
};

use crate::windows::keymap::usage_to_scancode;
use crate::windows::return_edge::{should_release, CursorSource, ReturnEdge};

/// Builds the `INPUT` for a key down or up, or `None` if `usage` has no
/// scancode mapping. `KEYEVENTF_SCANCODE` is always set so the PC's own
/// keyboard layout decides the resulting character rather than us guessing
/// a virtual-key code. Pure: no Windows API calls, so it is unit tested
/// directly without touching the real input stack.
fn key_input(usage: Usage, pressed: bool) -> Option<INPUT> {
    let (scancode, extended) = usage_to_scancode(usage)?;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        // Right control and right alt share a base scancode with their
        // left counterparts and differ only by this flag; dropping it
        // sticks the wrong modifier down on the receiving side.
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    // `INPUT_0` is a 32-byte union but the `ki` arm (`KEYBDINPUT`) is only
    // 24 bytes; `SendInput` reads all 40 bytes of each `INPUT` regardless
    // of which arm is logically in use. Starting from `INPUT::default()`
    // (which `windows-sys` derives as zeroed) initializes every byte, and
    // then writing only the `ki` field of the union (rather than
    // constructing a whole new `INPUT_0 { ki: .. }` value and assigning
    // that over it) touches just those 24 bytes, leaving the union's
    // unused 8-byte tail at the zero `Default::default()` gave it instead
    // of picking up whatever was on the stack. Windows ignores that tail
    // for keyboard input, but it still crosses the FFI boundary in the
    // `SendInput` call below, uninitialized or not.
    let mut input = INPUT {
        r#type: INPUT_KEYBOARD,
        ..Default::default()
    };
    input.Anonymous.ki = KEYBDINPUT {
        wVk: 0,
        wScan: scancode,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    Some(input)
}

/// Builds a `MOUSEINPUT`-flavored `INPUT` with the given relative deltas,
/// wheel amount, and event flags.
fn mouse_input(dx: i32, dy: i32, mouse_data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                // `mouseData` is a DWORD but wheel amounts are signed; the
                // bit pattern from the `i32` cast is exactly what
                // `SendInput` expects to reinterpret as signed.
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Builds the `INPUT` for a relative mouse move, matching the wire
/// protocol's own relative deltas.
fn mouse_move_input(dx: i32, dy: i32) -> INPUT {
    mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)
}

/// Builds the `INPUT` for a mouse button press or release.
fn button_input(button: Button, pressed: bool) -> INPUT {
    let flags = match (button, pressed) {
        (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (Button::Left, false) => MOUSEEVENTF_LEFTUP,
        (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
        (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    };
    mouse_input(0, 0, 0, flags)
}

/// Scales a wire-protocol scroll delta (one unit per notch) into the
/// `WHEEL_DELTA`-multiple units `mouseData` expects for wheel events,
/// saturating instead of wrapping on overflow.
fn wheel_delta(units: i32) -> i32 {
    units.saturating_mul(WHEEL_DELTA as i32)
}

/// Builds the `INPUT`s for a scroll event. Vertical and horizontal wheel
/// motion are separate `MOUSEINPUT` events on Windows; an axis with a zero
/// delta produces no event for that axis rather than a no-op one.
fn scroll_inputs(dx: i32, dy: i32) -> Vec<INPUT> {
    let mut inputs = Vec::with_capacity(2);
    if dy != 0 {
        inputs.push(mouse_input(0, 0, wheel_delta(dy), MOUSEEVENTF_WHEEL));
    }
    if dx != 0 {
        inputs.push(mouse_input(0, 0, wheel_delta(dx), MOUSEEVENTF_HWHEEL));
    }
    inputs
}

/// Calls `SendInput` and turns a short insert count into a
/// `DeviceError::Rejected` carrying `GetLastError`, instead of reporting
/// success for events the platform actually dropped. A rejecting injector
/// that claims success is exactly the silent-stall failure this project
/// exists to avoid.
fn send_inputs(inputs: &[INPUT]) -> Result<(), DeviceError> {
    if inputs.is_empty() {
        return Ok(());
    }
    let cb_size = core::mem::size_of::<INPUT>() as i32;
    // SAFETY: `inputs` is a valid, initialized, live slice of `INPUT` we
    // just constructed above; we pass its exact pointer and length, and
    // `cb_size` is `size_of::<INPUT>()` as `SendInput` requires. The call
    // does not retain the pointer past its return.
    let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), cb_size) };
    if (sent as usize) < inputs.len() {
        // SAFETY: `GetLastError` takes no arguments and reads per-thread
        // state that Windows guarantees is valid to query at any time.
        let code = unsafe { GetLastError() };
        return Err(DeviceError::Rejected(format!(
            "SendInput inserted {sent} of {} events (GetLastError = {code})",
            inputs.len()
        )));
    }
    Ok(())
}

/// Reads the real cursor position and the Windows virtual screen bounds
/// via the Win32 API. The only real `CursorSource`; the trait exists
/// separately (see `crate::windows::return_edge`) so the decision that
/// consumes it is testable without a live Windows desktop.
struct WindowsCursorSource;

impl CursorSource for WindowsCursorSource {
    fn cursor_position(&self) -> Option<(i32, i32)> {
        let mut point = POINT { x: 0, y: 0 };
        // SAFETY: `point` is a valid, live `POINT` on this stack frame;
        // `GetCursorPos` writes into it through the pointer we pass and
        // does not retain that pointer past the call. A zero return means
        // the query failed (documented as rare, for example no desktop is
        // attached to the current session), which is why the result is
        // checked below rather than assumed.
        let ok = unsafe { GetCursorPos(&mut point) };
        if ok == 0 {
            return None;
        }
        Some((point.x, point.y))
    }

    fn virtual_screen(&self) -> (i32, i32, i32, i32) {
        // SAFETY: `GetSystemMetrics` takes a plain integer index and
        // returns a plain integer; it has no pointer arguments and is
        // documented as safe to call at any time.
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        }
    }
}

/// How long `WindowsInjector` ignores its own return-edge check after it
/// has just answered yes and sent a `Message::Release`.
///
/// The server (see hop's run.rs) returns focus to Local within one poll
/// tick (15 ms) of receiving that message and stops sending new motion
/// almost immediately after, but a handful of `MouseMove` messages that
/// were already in flight can still land here in the meantime; without
/// this window each of them would independently see the cursor still
/// sitting at the edge and ask again. The window is short and clears
/// itself on its own rather than waiting for an explicit "focus is back"
/// signal from the server, because there isn't always one: a crossing
/// that held no keys down returns `Action::None`, not `ReleaseAll` (see
/// `hop_core::Control::return_focus`), so nothing would ever arrive to
/// clear a flag that depended on it. Self-clearing is what guarantees
/// this can never get permanently stuck: even in the worst case it has
/// long since cleared itself before a human crosses back and needs it to
/// ask again.
const RELEASE_SUPPRESS_WINDOW: Duration = Duration::from_millis(500);

/// Replays events on the local PC via `SendInput`.
#[derive(Debug, Default)]
pub struct WindowsInjector {
    /// The edge of this PC's virtual screen whose crossing hands focus
    /// back to the server; `None` leaves the automatic return path
    /// disabled, matching this project's behavior before CRITICAL 2 was
    /// fixed (only the panic hotkey or a dead link bring focus home).
    return_edge: Option<ReturnEdge>,
    suppress_release_until: Option<Instant>,
}

impl WindowsInjector {
    pub fn new(return_edge: Option<ReturnEdge>) -> Self {
        Self {
            return_edge,
            suppress_release_until: None,
        }
    }
}

impl Injector for WindowsInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DeviceError> {
        match *event {
            // Capture-side signal that a peer's edge was crossed; never
            // itself replayed as input.
            InputEvent::EdgeCrossed => Ok(()),
            InputEvent::Mouse { dx, dy } => send_inputs(&[mouse_move_input(dx, dy)]),
            InputEvent::Button { button, pressed } => send_inputs(&[button_input(button, pressed)]),
            InputEvent::Scroll { dx, dy } => send_inputs(&scroll_inputs(dx, dy)),
            InputEvent::Key { usage, pressed } => match key_input(usage, pressed) {
                Some(input) => send_inputs(&[input]),
                None => {
                    // An unmapped usage is skipped rather than guessed: a
                    // wrong keystroke is worse than a dropped one.
                    tracing::warn!(
                        usage = usage.0,
                        "no scancode mapping for HID usage; skipping key event"
                    );
                    Ok(())
                }
            },
        }
    }

    fn reached_return_edge(&mut self) -> bool {
        let Some(edge) = self.return_edge else {
            return false;
        };
        if let Some(until) = self.suppress_release_until {
            if Instant::now() < until {
                return false;
            }
            self.suppress_release_until = None;
        }
        if should_release(edge, &WindowsCursorSource) {
            self.suppress_release_until = Some(Instant::now() + RELEASE_SUPPRESS_WINDOW);
            true
        } else {
            false
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    /// SAFETY: test-only read of the union arm the function under test just
    /// wrote; both fields are plain-old-data, so any bit pattern is valid.
    fn keybd(input: INPUT) -> KEYBDINPUT {
        unsafe { input.Anonymous.ki }
    }

    #[test]
    fn key_down_sets_no_keyup_flag() {
        let input = key_input(Usage::A, true).expect("A is mapped");
        assert_eq!(input.r#type, INPUT_KEYBOARD);
        let ki = keybd(input);
        assert_eq!(ki.dwFlags & KEYEVENTF_KEYUP, 0);
        assert_ne!(ki.dwFlags & KEYEVENTF_SCANCODE, 0);
    }

    #[test]
    fn key_up_sets_keyup_flag() {
        let input = key_input(Usage::A, false).expect("A is mapped");
        let ki = keybd(input);
        assert_ne!(ki.dwFlags & KEYEVENTF_KEYUP, 0);
    }

    #[test]
    fn right_hand_modifier_sets_extended_flag() {
        let right = keybd(key_input(Usage::RIGHT_CTRL, true).expect("right ctrl is mapped"));
        assert_ne!(right.dwFlags & KEYEVENTF_EXTENDEDKEY, 0);

        // Left control shares the same scancode and must not carry the
        // flag, or the wrong-hand modifier sticks down on the far side.
        let left = keybd(key_input(Usage::LEFT_CTRL, true).expect("left ctrl is mapped"));
        assert_eq!(left.wScan, right.wScan);
        assert_eq!(left.dwFlags & KEYEVENTF_EXTENDEDKEY, 0);
    }

    #[test]
    fn unmapped_usage_returns_none() {
        assert!(key_input(Usage(0xFFFF), true).is_none());
    }

    #[test]
    fn scroll_skips_zero_axes_but_keeps_nonzero_ones() {
        assert!(scroll_inputs(0, 0).is_empty());
        assert_eq!(scroll_inputs(0, 2).len(), 1);
        assert_eq!(scroll_inputs(3, 0).len(), 1);
        assert_eq!(scroll_inputs(3, 2).len(), 2);
    }

    #[test]
    fn button_press_and_release_use_distinct_flags() {
        let down = mouse_input_flags(button_input(Button::Left, true));
        let up = mouse_input_flags(button_input(Button::Left, false));
        assert_eq!(down, MOUSEEVENTF_LEFTDOWN);
        assert_eq!(up, MOUSEEVENTF_LEFTUP);
    }

    /// SAFETY: test-only read of the union arm the function under test just
    /// wrote; both fields are plain-old-data, so any bit pattern is valid.
    fn mouse_input_flags(input: INPUT) -> u32 {
        unsafe { input.Anonymous.mi.dwFlags }
    }
}
