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
/// Connection id for the window server. `CGSSetConnectionProperty` and
/// `_CGSDefaultConnection` are private CoreGraphics SPI: undocumented by
/// Apple, but stable for many years and what every tool in this space
/// relies on to hide the cursor from a background process.
type CGSConnectionID = u32;

unsafe extern "C" {
    fn _CGSDefaultConnection() -> CGSConnectionID;
    fn CGSSetConnectionProperty(
        cid: CGSConnectionID,
        target: CGSConnectionID,
        key: core_foundation::string::CFStringRef,
        value: *const std::os::raw::c_void,
    ) -> i32;
}

/// Ask the window server to let this process hide the cursor even though
/// it is not the foreground application.
///
/// Without this, `CGDisplayHideCursor` silently does nothing for hop:
/// macOS honours it only for the frontmost app, and hop is a background
/// process. Verified on macOS 27, where the plain call, and the call
/// after registering as an accessory application, both left the cursor
/// visible, while setting this property made it vanish immediately.
///
/// Safe to call more than once; it just sets a property.
pub fn allow_background_cursor_hiding() {
    use core_foundation::base::TCFType;
    // SAFETY: `_CGSDefaultConnection` takes no arguments and returns a
    // connection id by value. `CGSSetConnectionProperty` borrows the key
    // and value only for the duration of the call, and both outlive it
    // here, so neither pointer can dangle.
    unsafe {
        let cid = _CGSDefaultConnection();
        let key = core_foundation::string::CFString::new("SetsCursorInBackground");
        let yes = core_foundation::boolean::CFBoolean::true_value();
        let err = CGSSetConnectionProperty(
            cid,
            cid,
            key.as_concrete_TypeRef(),
            yes.as_CFTypeRef() as *const std::os::raw::c_void,
        );
        if err != 0 {
            tracing::warn!(error = err, "could not enable background cursor hiding");
        }
    }
}

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

/// Sets how long, in seconds, local hardware input is suppressed after a
/// programmatic cursor warp; see `DEFAULT_LOCAL_EVENTS_SUPPRESSION_INTERVAL`
/// above for why this needs to be overridden while focus is parked on the
/// peer. Best-effort: if Quartz refuses to hand back an event source
/// (undocumented but possible, the same caveat `cursor_position` above
/// already lives with), this silently does nothing rather than panicking
/// on the input path.
/// Permit every event class during a suppression window.
/// `kCGEventFilterMaskPermitAllEvents` is the OR of the local mouse,
/// local keyboard and system-defined permit bits.
const PERMIT_ALL_EVENTS: u32 = 1 | 2 | 4;
/// `kCGEventSupressionStateSupressionInterval`. Apple's own spelling of
/// "suppression" is missing a letter here; kept so the constant is
/// greppable against the system headers.
const SUPPRESSION_STATE_INTERVAL: u32 = 0;

unsafe extern "C" {
    fn CGSetLocalEventsSuppressionInterval(seconds: f64) -> i32;
    fn CGSetLocalEventsFilterDuringSupressionState(filter: u32, state: u32) -> i32;
}

/// Stop macOS ignoring the user's own mouse for a quarter second after a
/// warp.
///
/// This must use the process-wide `CGSetLocalEventsSuppressionInterval`,
/// not `CGEventSourceSetLocalEventsSuppressionInterval`. An earlier
/// version of this function created an event source, set the interval on
/// it, and released it immediately, which sets a property on a throwaway
/// object and does nothing at all to the system. The symptom was a cursor
/// that visibly hesitated for about a second every time focus came back
/// from the peer.
///
/// Both calls are deprecated by Apple and have no supported replacement
/// for this purpose. They still work, and every tool in this space uses
/// them for exactly this.
fn set_local_events_suppression_interval(seconds: f64) {
    // SAFETY: both take plain scalars by value, return a status code, and
    // touch no memory this crate owns. There is nothing to keep alive
    // across the call and nothing to release afterwards.
    unsafe {
        CGSetLocalEventsSuppressionInterval(seconds);
        CGSetLocalEventsFilterDuringSupressionState(PERMIT_ALL_EVENTS, SUPPRESSION_STATE_INTERVAL);
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
    // Re-associate FIRST, then leave the suppression interval at zero.
    //
    // Restoring the interval to its 0.25s default here made the cursor
    // sit still for a moment after focus came back: the warp that returns
    // it to where the user left off would suppress their own hardware
    // movement for that interval. Leaving it at zero costs nothing (it
    // only governs how long local input is ignored after a warp, and hop
    // warps deliberately) and the cursor tracks the hand immediately.
    let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
    set_local_events_suppression_interval(0.0);
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
