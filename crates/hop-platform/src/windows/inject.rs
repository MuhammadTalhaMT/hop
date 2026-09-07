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
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
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

/// Where the cursor should appear when focus arrives, given how far along
/// the peer's edge it left.
///
/// The entry edge is the OPPOSITE of this machine's return edge: if the
/// cursor leaves here through the bottom, it must arrive at the top.
/// `fraction` runs 0.0 to 1.0 along that edge, so the same relative point
/// is used on machines of different resolutions.
fn entry_point(
    return_edge: Option<ReturnEdge>,
    fraction: f32,
    screen: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (left, top, width, height) = screen;
    let f = fraction.clamp(0.0, 1.0) as f64;
    let along_x = left + ((width - 1).max(0) as f64 * f).round() as i32;
    let along_y = top + ((height - 1).max(0) as f64 * f).round() as i32;
    match return_edge {
        // Leaves through the bottom, so arrives at the top.
        Some(ReturnEdge::Bottom) | None => (along_x, top),
        Some(ReturnEdge::Top) => (along_x, top + (height - 1).max(0)),
        Some(ReturnEdge::Right) => (left, along_y),
        Some(ReturnEdge::Left) => (left + (width - 1).max(0), along_y),
    }
}

/// Builds the `INPUT` for moving the cursor to an absolute position.
///
/// Absolute movement is what makes pointer speed match the Mac. A
/// relative `MOUSEEVENTF_MOVE` is put through Windows pointer
/// acceleration, on top of the acceleration macOS already applied before
/// sending the delta, so the same hand movement travelled further here.
/// Absolute coordinates bypass that curve entirely, so the cursor lands
/// exactly where the Mac's motion says it should.
///
/// Coordinates are normalised to 0..=65535 across the whole virtual
/// desktop, which is what `MOUSEEVENTF_VIRTUALDESK` selects.
fn mouse_move_absolute_input(x: i32, y: i32, screen: (i32, i32, i32, i32)) -> INPUT {
    let (left, top, width, height) = screen;
    // Guard against a zero sized desktop rather than dividing by it.
    let width = width.max(1) as f64;
    let height = height.max(1) as f64;
    let nx = ((x - left) as f64 * 65535.0 / width).round() as i32;
    let ny = ((y - top) as f64 * 65535.0 / height).round() as i32;
    mouse_input(
        nx.clamp(0, 65535),
        ny.clamp(0, 65535),
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
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
    /// Multiplier applied to incoming mouse deltas.
    ///
    /// macOS has already applied its own pointer acceleration to the
    /// deltas the tap reports, and Windows applies its acceleration again
    /// when they are injected, so the same hand movement travels further
    /// here than it did on the Mac. Scaling below 1.0 cancels the second
    /// helping.
    mouse_scale: f64,
    /// Sub-pixel remainder carried between events.
    ///
    /// Without it, scaling truncates every delta towards zero, so slow
    /// deliberate movement (a stream of one pixel events) would scale to
    /// zero and the cursor would simply not move.
    scale_remainder: (f64, f64),
}

/// Apply `scale` to a delta, carrying the sub-pixel remainder so slow
/// movement is not rounded away.
pub(crate) fn scale_delta(dx: i32, dy: i32, scale: f64, remainder: &mut (f64, f64)) -> (i32, i32) {
    let wanted_x = dx as f64 * scale + remainder.0;
    let wanted_y = dy as f64 * scale + remainder.1;
    let out_x = wanted_x.trunc();
    let out_y = wanted_y.trunc();
    remainder.0 = wanted_x - out_x;
    remainder.1 = wanted_y - out_y;
    (out_x as i32, out_y as i32)
}

impl WindowsInjector {
    pub fn new(return_edge: Option<ReturnEdge>, mouse_scale: f64) -> Self {
        let injector = Self {
            return_edge,
            suppress_release_until: None,
            mouse_scale,
            scale_remainder: (0.0, 0.0),
        };
        injector.release_all_modifiers();
        injector
    }

    /// Send a key-up for every modifier, unconditionally.
    ///
    /// This exists because hop cannot always clean up after itself. If the
    /// client process dies while a modifier is held down (a crash, a kill,
    /// or the user restarting it to pick up a new build), the key-down has
    /// already been delivered to Windows and no key-up ever follows, so
    /// the modifier stays down system wide. Nothing in the running process
    /// can fix that after the fact, because the process is gone.
    ///
    /// Doing this on startup makes the next run repair it: whatever was
    /// left stuck is released before any input is injected. Releasing a
    /// modifier that is not held is a harmless no-op, so this costs
    /// nothing in the normal case.
    ///
    /// A stuck modifier on the far machine is the worst outcome this tool
    /// can produce (see `CLAUDE.md`), so it is worth being unconditional
    /// about.
    pub fn release_all_modifiers(&self) {
        for usage in [
            Usage::LEFT_CTRL,
            Usage::LEFT_SHIFT,
            Usage::LEFT_ALT,
            Usage::LEFT_GUI,
            Usage::RIGHT_CTRL,
            Usage::RIGHT_SHIFT,
            Usage::RIGHT_ALT,
            Usage::RIGHT_GUI,
        ] {
            if let Some(input) = key_input(usage, false) {
                // SAFETY: same contract as every other SendInput call in
                // this file; a one element array of a fully initialised
                // INPUT, with cbSize matching the type.
                unsafe {
                    SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
                }
            }
        }
    }
}

impl Injector for WindowsInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), DeviceError> {
        match *event {
            // Capture-side signal that a peer's edge was crossed; never
            // itself replayed as input.
            InputEvent::EdgeCrossed => Ok(()),
            InputEvent::Mouse { dx, dy } => {
                let (dx, dy) = scale_delta(dx, dy, self.mouse_scale, &mut self.scale_remainder);
                if dx == 0 && dy == 0 {
                    // Scaled away to nothing this time; the remainder
                    // carries it into a later event rather than losing it.
                    return Ok(());
                }
                // Track the position ourselves and move absolutely, so
                // Windows pointer acceleration never applies a second
                // curve on top of the one macOS already applied. That is
                // what makes pointer speed match between the machines.
                let screen = WindowsCursorSource.virtual_screen();
                let Some((cx, cy)) = WindowsCursorSource.cursor_position() else {
                    return send_inputs(&[mouse_move_input(dx, dy)]);
                };
                let (left, top, width, height) = screen;
                let x = (cx + dx).clamp(left, left + width - 1);
                let y = (cy + dy).clamp(top, top + height - 1);
                send_inputs(&[mouse_move_absolute_input(x, y, screen)])
            }
            InputEvent::Enter { fraction } => {
                // Place the cursor at the same relative point on this
                // machine's entry edge that it left the peer's edge from,
                // so the movement reads as one continuous motion instead
                // of the pointer jumping to wherever it was left.
                let screen = WindowsCursorSource.virtual_screen();
                let (x, y) = entry_point(self.return_edge, fraction, screen);
                send_inputs(&[mouse_move_absolute_input(x, y, screen)])
            }
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
    #[test]
    fn absolute_coordinates_span_the_whole_virtual_desktop() {
        // 0 and 65535 are the ends of the normalised range, whatever the
        // real pixel size. Getting this wrong compresses all movement
        // into a corner of the screen.
        let screen = (0, 0, 1920, 1080);
        let top_left = mouse_move_absolute_input(0, 0, screen);
        let bottom_right = mouse_move_absolute_input(1919, 1079, screen);
        unsafe {
            assert_eq!(top_left.Anonymous.mi.dx, 0);
            assert_eq!(top_left.Anonymous.mi.dy, 0);
            assert!(bottom_right.Anonymous.mi.dx > 65000);
            assert!(bottom_right.Anonymous.mi.dy > 65000);
        }
    }

    #[test]
    fn absolute_coordinates_handle_a_negative_origin() {
        // A monitor left of or above the primary pushes the virtual
        // desktop origin negative; the normalisation must be relative to
        // that origin, not to zero.
        let screen = (-1920, -1080, 3840, 2160);
        let at_origin = mouse_move_absolute_input(-1920, -1080, screen);
        unsafe {
            assert_eq!(at_origin.Anonymous.mi.dx, 0);
            assert_eq!(at_origin.Anonymous.mi.dy, 0);
        }
    }

    #[test]
    fn a_zero_sized_desktop_does_not_divide_by_zero() {
        let input = mouse_move_absolute_input(0, 0, (0, 0, 0, 0));
        unsafe {
            assert_eq!(input.Anonymous.mi.dx, 0);
        }
    }

    #[test]
    fn scaling_reduces_a_delta() {
        let mut rem = (0.0, 0.0);
        assert_eq!(scale_delta(10, 10, 0.5, &mut rem), (5, 5));
    }

    #[test]
    fn a_scale_of_one_changes_nothing() {
        let mut rem = (0.0, 0.0);
        assert_eq!(scale_delta(7, -3, 1.0, &mut rem), (7, -3));
    }

    #[test]
    fn slow_movement_is_not_rounded_away_to_nothing() {
        // A stream of one pixel events at half sensitivity truncates to
        // zero every time without a carried remainder, and the cursor
        // would simply never move.
        let mut rem = (0.0, 0.0);
        let mut moved = 0;
        for _ in 0..10 {
            let (dx, _) = scale_delta(1, 0, 0.5, &mut rem);
            moved += dx;
        }
        assert_eq!(moved, 5, "ten one pixel steps at 0.5 should travel five");
    }

    #[test]
    fn the_remainder_does_not_accumulate_error_over_time() {
        // Whatever the scale, total distance should track the input.
        let mut rem = (0.0, 0.0);
        let mut moved = 0;
        for _ in 0..100 {
            let (dx, _) = scale_delta(3, 0, 0.7, &mut rem);
            moved += dx;
        }
        let expected = (100.0 * 3.0 * 0.7) as i32;
        assert!(
            (moved - expected).abs() <= 1,
            "drifted: moved {moved}, expected about {expected}"
        );
    }

    #[test]
    fn negative_deltas_scale_symmetrically() {
        let mut a = (0.0, 0.0);
        let mut b = (0.0, 0.0);
        let (px, _) = scale_delta(10, 0, 0.6, &mut a);
        let (nx, _) = scale_delta(-10, 0, 0.6, &mut b);
        assert_eq!(px, -nx);
    }

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
