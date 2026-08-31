//! Cursor position, warp, and visibility control on macOS.
//!
//! Everything here is a thin, safe wrapper over `core-graphics`'s own
//! wrappers around `CGWarpMouseCursorPosition`, `CGDisplayHideCursor`,
//! `CGDisplayShowCursor`, `CGAssociateMouseAndMouseCursorPosition`, and
//! reading a fresh event's location, plus two small `unsafe extern "C"`
//! declarations for calls `core-graphics` does not expose at all: display
//! enumeration's union (see `display_bounds`) stays entirely on the safe
//! side, but the local-events suppression interval (see
//! `set_local_events_suppression_interval`) is not wrapped by the crate
//! and is not exposed through a call this crate can already reach safely,
//! so it is declared directly, the same way `capture.rs` declares
//! `CGEventTapEnable`. None of this can be exercised by an automated
//! test: it needs a real display and a real cursor. `crossed` and
//! `nudge_inward` in `capture.rs`, the pure decisions this module's
//! output feeds, are what carry this file's unit test coverage instead,
//! along with `union_rects` below, the one piece of arithmetic here that
//! does not need hardware to get right. See Task 13 for the human
//! verification the hardware-touching calls actually get.

use std::ffi::c_void;

use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect};

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

/// A bounding box in global display coordinates: origin at the main
/// display's top-left, y increasing downward, x increasing rightward.
/// `display_bounds` below returns the union of every active display's
/// bounds, which is what makes this the right shape for `crossed` and
/// `nudge_inward` in `capture.rs` to reason about a multi-display Mac as
/// one virtual desktop instead of just the main display.
///
/// macOS anchors the main display's own origin at `(0, 0)` and places
/// every other display relative to it, so a display positioned above or
/// to the left of the main one pushes `min_y` or `min_x` negative. Do
/// not assume either is `0.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

/// Pure fold behind `display_bounds`: unions a list of display rectangles
/// into one bounding box covering all of them. Broken out on its own,
/// separate from `display_bounds`'s call to
/// `CGDisplay::active_displays()`, so the arithmetic that is actually
/// easy to get wrong here, unioning rectangles that may have negative
/// origins, is unit testable without a real display; see the `tests`
/// module below. `None` for an empty slice, which `display_bounds`
/// treats as "fall back to the main display alone".
fn union_rects(rects: &[CGRect]) -> Option<Bounds> {
    rects
        .iter()
        .map(|rect| Bounds {
            min_x: rect.origin.x,
            min_y: rect.origin.y,
            max_x: rect.origin.x + rect.size.width,
            max_y: rect.origin.y + rect.size.height,
        })
        .reduce(|acc, next| Bounds {
            min_x: acc.min_x.min(next.min_x),
            min_y: acc.min_y.min(next.min_y),
            max_x: acc.max_x.max(next.max_x),
            max_y: acc.max_y.max(next.max_y),
        })
}

/// The union of every active display's bounds, in global display
/// coordinates, treated as one virtual desktop for edge detection (see
/// `crossed` in `capture.rs`). This is IMPORTANT 1's fix from the
/// whole-branch review: reading only `CGDisplay::main().bounds()` made
/// every edge decision wrong on a multi-display Mac, most visibly the
/// top edge, which fired the instant the cursor reached the top of any
/// display positioned above the main one, because that display's bounds
/// carry a negative global `y` that a main-display-only bounds check has
/// no way to represent.
///
/// Read once, when the capture thread starts, and never refreshed:
/// `core-graphics` (and Core Graphics itself) has no push notification
/// for a display configuration change that this crate can act on without
/// polling, and polling this on every mouse-move event is a syscall this
/// project is not willing to pay per event. Hot-plugging, unplugging, or
/// rearranging a monitor after `MacCapturer::start` runs is therefore an
/// accepted limitation: hop needs a restart to pick up the new layout.
/// See Task 13.
///
/// Falls back to the main display's own bounds if
/// `CGDisplay::active_displays()` fails or reports nothing, which
/// `core-graphics` documents as possible; a wrong-but-plausible
/// single-display answer is better than a panic on startup.
pub fn display_bounds() -> Bounds {
    let rects: Vec<CGRect> = CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| CGDisplay::new(id).bounds())
        .collect();

    union_rects(&rects).unwrap_or_else(|| {
        let bounds = CGDisplay::main().bounds();
        Bounds {
            min_x: bounds.origin.x,
            min_y: bounds.origin.y,
            max_x: bounds.origin.x + bounds.size.width,
            max_y: bounds.origin.y + bounds.size.height,
        }
    })
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

/// macOS's own documented default: any local hardware mouse or keyboard
/// event arriving within this many seconds of a programmatic warp
/// (`CGWarpMouseCursorPosition`) is suppressed rather than allowed to move
/// the cursor, so a real mouse nudge right after a warp cannot fight it.
/// Reasonable for a single, one-off warp; this is exactly what breaks
/// IMPORTANT 2 from the whole-branch review, because `CursorPark::hold`
/// (in `capture.rs`) warps on every remote motion event, dozens of times a
/// second while focus is remote, so this window never has a chance to
/// expire before the next warp restarts it: the local cursor moves once
/// and then appears to freeze.
const DEFAULT_LOCAL_EVENTS_SUPPRESSION_INTERVAL: f64 = 0.25;

/// Opaque pointer type matching `CGEventSourceRef` from
/// `<CoreGraphics/CGEventSource.h>`. `core-graphics`'s own
/// `event_source::CGEventSource` wraps the same underlying pointer, but
/// only exposes it through the `foreign_types` crate's `ForeignType`
/// trait, which is not a dependency of this crate; adding it solely to
/// reach one pointer accessor would violate this project's "no new
/// dependencies" rule. This type is only ever passed straight from
/// `CGEventSourceCreate`'s return value into
/// `CGEventSourceSetLocalEventsSuppressionInterval` and then released; it
/// is never dereferenced, so an opaque `c_void` pointer is exactly as
/// much type as this file needs for it.
type CGEventSourceRef = *const c_void;

// `CGEventSourceCreate` and `CGEventSourceSetLocalEventsSuppressionInterval`
// are declared here rather than used from `core-graphics` because the
// crate exposes event source creation only through its safe
// `CGEventSource::new`, which (see `CGEventSourceRef` above) offers no way
// to get the raw pointer back out, and does not expose
// `CGEventSourceSetLocalEventsSuppressionInterval` at all. Both are part
// of the same public `<CoreGraphics/CGEventSource.h>` API `CGEventSource`
// itself already binds against, so declaring their C signatures directly
// is the same kind of bridging `capture.rs` already does for
// `CGEventTapEnable`/`CGEventTapIsEnabled`.
unsafe extern "C" {
    fn CGEventSourceCreate(state_id: CGEventSourceStateID) -> CGEventSourceRef;
    fn CGEventSourceSetLocalEventsSuppressionInterval(source: CGEventSourceRef, seconds: f64);
}

/// Sets how long, in seconds, local hardware input is suppressed after a
/// programmatic cursor warp; see `DEFAULT_LOCAL_EVENTS_SUPPRESSION_INTERVAL`
/// above for why this needs to be overridden while focus is parked on the
/// peer. Best-effort: if Quartz refuses to hand back an event source
/// (undocumented but possible, the same caveat `cursor_position` above
/// already lives with), this silently does nothing rather than panicking
/// on the input path.
fn set_local_events_suppression_interval(seconds: f64) {
    // SAFETY: `CGEventSourceCreate` follows Core Foundation's "create
    // rule": a non-null return is a new, owned reference this call alone
    // is responsible for releasing, and a null return means creation
    // failed and there is nothing to release. The null check below
    // handles the second case; `CFRelease` at the end handles the first
    // on every path out of this function, so no reference ever leaks.
    // `source`, between creation and release, is a valid pointer for the
    // one call to `CGEventSourceSetLocalEventsSuppressionInterval`, which
    // Apple documents as safe to call from any thread and which does
    // nothing but write a property on the source object it is given, no
    // different in kind from `CGEventTapEnable` elsewhere in this crate.
    unsafe {
        let source = CGEventSourceCreate(CGEventSourceStateID::HIDSystemState);
        if source.is_null() {
            return;
        }
        CGEventSourceSetLocalEventsSuppressionInterval(source, seconds);
        core_foundation::base::CFRelease(source);
    }
}

/// Stops hardware mouse movement from driving the local cursor, and tells
/// Quartz not to suppress local hardware input after a warp. This is
/// IMPORTANT 2's fix from the whole-branch review; see
/// `DEFAULT_LOCAL_EVENTS_SUPPRESSION_INTERVAL`'s doc comment for the
/// mechanism, and `CursorPark::park` in `capture.rs`, the only caller,
/// for when this runs. Pairs with `leave_parked_state`, which must always
/// be called to undo this exactly once for every call here; `CursorPark`
/// enforces that the same way it already does for `hide_cursor`/
/// `show_cursor`.
pub fn enter_parked_state() {
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
    set_local_events_suppression_interval(0.0);
}

/// Undoes `enter_parked_state`. Safe to call even when entering never
/// actually happened (see `CursorPark::restore` in `capture.rs`, the only
/// caller, for why it calls this unconditionally): reassociating an
/// already-associated mouse, or resetting an already-default suppression
/// interval, is a harmless no-op.
pub fn leave_parked_state() {
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    set_local_events_suppression_interval(DEFAULT_LOCAL_EVENTS_SUPPRESSION_INTERVAL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(
            &CGPoint::new(x, y),
            &core_graphics::geometry::CGSize::new(width, height),
        )
    }

    #[test]
    fn union_of_no_rects_is_none() {
        assert_eq!(union_rects(&[]), None);
    }

    #[test]
    fn union_of_one_rect_is_its_own_bounds() {
        let bounds = union_rects(&[rect(0.0, 0.0, 1920.0, 1080.0)]).unwrap();
        assert_eq!(
            bounds,
            Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 1920.0,
                max_y: 1080.0,
            }
        );
    }

    #[test]
    fn union_extends_upward_for_a_display_above_main() {
        // The main display sits at (0, 0); a second, taller display is
        // centered above it, so its origin has a negative y. The union
        // must reach up to that negative min_y rather than staying
        // pinned at the main display's own top edge.
        let main = rect(0.0, 0.0, 1920.0, 1080.0);
        let above = rect(-320.0, -1440.0, 2560.0, 1440.0);
        let bounds = union_rects(&[main, above]).unwrap();
        assert_eq!(
            bounds,
            Bounds {
                min_x: -320.0,
                min_y: -1440.0,
                max_x: 2240.0,
                max_y: 1080.0,
            }
        );
    }

    #[test]
    fn union_extends_leftward_for_a_display_to_the_left() {
        let main = rect(0.0, 0.0, 1920.0, 1080.0);
        let left = rect(-1920.0, 0.0, 1920.0, 1080.0);
        let bounds = union_rects(&[main, left]).unwrap();
        assert_eq!(bounds.min_x, -1920.0);
        assert_eq!(bounds.max_x, 1920.0);
    }

    #[test]
    fn union_is_order_independent() {
        let main = rect(0.0, 0.0, 1920.0, 1080.0);
        let above = rect(-320.0, -1440.0, 2560.0, 1440.0);
        let forward = union_rects(&[main, above]).unwrap();
        let backward = union_rects(&[above, main]).unwrap();
        assert_eq!(forward, backward);
    }
}
