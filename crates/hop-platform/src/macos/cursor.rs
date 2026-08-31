//! Cursor position, warp, and visibility control on macOS.
//!
//! Everything here is a thin, safe wrapper over `core-graphics`'s own
//! wrappers around `CGWarpMouseCursorPosition`, `CGDisplayHideCursor`,
//! `CGDisplayShowCursor`, and reading a fresh event's location. None of it
//! can be exercised by an automated test: it needs a real display and a
//! real cursor. `crossed` in `capture.rs`, the pure decision of whether a
//! position counts as an edge crossing, is what carries this file's unit
//! test coverage instead. See Task 13 for the human verification these
//! calls actually get.
//!
//! No unsafe code lives directly in this file: every call below goes
//! through `core-graphics`'s own safe wrappers, which do the `unsafe`
//! FFI internally.

use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

/// Reads the current cursor position in global display coordinates
/// (origin at the top-left, y increasing downward).
///
/// Falls back to `(0.0, 0.0)` if the system refuses to hand back an
/// event source or an event, which `core-graphics` documents as
/// possible; a wrong-but-defined answer here is far better than a panic
/// on the input path.
pub fn cursor_position() -> (f64, f64) {
    let point = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .and_then(CGEvent::new)
        .map(|event| event.location())
        .unwrap_or_default();
    (point.x, point.y)
}

/// The main display's size in points, used to know where its edges are.
pub fn screen_size() -> (f64, f64) {
    let bounds = CGDisplay::main().bounds();
    (bounds.size.width, bounds.size.height)
}

/// Moves the cursor to `(x, y)` without generating a motion event, so the
/// warp itself is never mistaken for user input by this project's own
/// event tap.
pub fn warp_cursor(x: f64, y: f64) {
    // `warp_mouse_cursor_position` only fails if Quartz itself rejects the
    // point (a `CGError` from an invalid display state); there is nothing
    // more useful to do here than accept the cursor did not move this one
    // time. The next call, a moment later on the next event, tries again.
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
}

/// Hides the cursor. Pairs with `show_cursor`; see its doc comment for why
/// callers must never leave a call to this one unmatched.
pub fn hide_cursor() {
    let _ = CGDisplay::main().hide_cursor();
}

/// Shows the cursor. `CGDisplayHideCursor`/`CGDisplayShowCursor` are
/// documented as incrementing and decrementing a single hide count rather
/// than being a plain on/off switch, so every `hide_cursor` call this
/// project makes needs exactly one matching `show_cursor`. `CursorPark` in
/// `capture.rs` is the only caller of either, and its `park`/`restore`
/// pair enforces that one-to-one relationship.
pub fn show_cursor() {
    let _ = CGDisplay::main().show_cursor();
}
