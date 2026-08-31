//! macOS system wide input capture.
//!
//! This is the single most important file in the project. It owns a
//! `CGEventTap`, the only mechanism that lets us both see and suppress
//! keyboard and mouse events system wide on macOS. The failure mode this
//! file defends against is "hop silently stops seeing input while looking
//! perfectly healthy", because macOS disables event taps on its own
//! initiative and reports it as an ordinary event rather than an error.
//!
//! A background thread owns the `CFRunLoop` and the tap. The tap callback
//! translates each event and pushes it into a channel; `MacCapturer::poll`
//! drains that channel without blocking, so the trait's non-blocking
//! contract holds even though the tap itself lives on a loop that blocks
//! forever.
//!
//! The same callback also owns edge detection and cursor parking. While
//! focus is local, every mouse-motion event is checked against the
//! configured `Edge` (see `crossed`, the pure part of that decision); on
//! a crossing it emits `InputEvent::EdgeCrossed` and flips `remote`
//! itself rather than waiting for the connection loop to notice, so
//! suppression starts on the very event that crossed. While focus is
//! remote, `CursorPark` pins the real cursor to a fixed point and keeps
//! it hidden on every subsequent motion event, so the user only ever
//! sees the peer's cursor move. It restores position and visibility the
//! moment focus comes back to local, whether that happens through this
//! callback or is only noticed by it, and again on `Drop` so a crash or
//! early exit can never leave the pointer invisible.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use hop_proto::Button;

use crate::macos::cursor;
use crate::macos::keymap::virtual_key_to_usage;
use crate::{Capturer, InputEvent};

// `CGEventTapEnable`/`CGEventTapIsEnabled` are declared here rather than
// used from `core-graphics` because the crate only exposes enabling
// through `CGEventTap::enable`, which requires an owned `CGEventTap`, and
// does not expose a query for the enabled state at all. We need to call
// both from the tap callback and from a watchdog thread, neither of which
// owns the tap, so we bind the same C functions directly, exactly as the
// spike did for `CGEventTapEnable`.
unsafe extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventTapIsEnabled(tap: CFMachPortRef) -> u8;
}

/// How long the watchdog waits without seeing any event before it checks
/// whether the tap went deaf without telling anyone.
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(5);
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("failed to create the macOS event tap; is Accessibility permission granted?")]
    TapUnavailable,
    #[error("failed to create a run loop source for the event tap")]
    RunLoopSourceUnavailable,
    #[error("failed to spawn the capture thread: {0}")]
    ThreadSpawnFailed(std::io::Error),
    #[error("the capture thread exited before the event tap was ready")]
    ThreadExitedEarly,
}

/// One of the four edges of the screen that hands focus to the peer when
/// the cursor reaches it. Which edge is active for a given deployment
/// comes from `[layout]` in the user's config (see `hop::config::Layout`)
/// and is passed into `MacCapturer::start`, never hardcoded here: this
/// project's own reference deployment has the PC's monitors mounted above
/// the Mac, so its config sets `top = "pc"`, but nothing in this type or
/// `crossed` below assumes that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// How far inside the screen, in points, a parked or restored cursor is
/// placed away from the edge it crossed. Without this, restoring the
/// cursor to the exact point it crossed at (which is by definition on the
/// boundary `crossed` treats as a crossing) would trigger another
/// crossing on the very next reported motion, bouncing focus straight
/// back to the peer the instant it returned to local.
const EDGE_MARGIN: f64 = 12.0;

/// Whether cursor position `(x, y)`, in global display coordinates on a
/// screen sized `screen_width` by `screen_height`, has reached `edge`.
/// Pure and side effect free, so it is the part of edge detection that
/// can actually be unit tested without hardware; see the `tests` module
/// below.
///
/// Global display coordinates on macOS put the origin at the top-left
/// with y increasing downward, so the top edge is `y <= 0.0` and the
/// bottom edge is `y >= screen_height - 1.0`; left and right are the same
/// idea on the x axis.
fn crossed(edge: Edge, x: f64, y: f64, screen_width: f64, screen_height: f64) -> bool {
    match edge {
        Edge::Top => y <= 0.0,
        Edge::Bottom => y >= screen_height - 1.0,
        Edge::Left => x <= 0.0,
        Edge::Right => x >= screen_width - 1.0,
    }
}

/// Nudges a point that just crossed `edge` back inside the screen by
/// `EDGE_MARGIN`, clamping so a screen smaller than the margin still
/// yields an in-bounds point rather than a negative coordinate.
fn nudge_inward(edge: Edge, x: f64, y: f64, screen_width: f64, screen_height: f64) -> (f64, f64) {
    match edge {
        Edge::Top => (x, EDGE_MARGIN.min(screen_height)),
        Edge::Bottom => (x, (screen_height - 1.0 - EDGE_MARGIN).max(0.0)),
        Edge::Left => (EDGE_MARGIN.min(screen_width), y),
        Edge::Right => ((screen_width - 1.0 - EDGE_MARGIN).max(0.0), y),
    }
}

/// Owns the state needed to safely hide and pin the real cursor while
/// focus is on the peer, and to always be able to give it back.
///
/// Deliberately dumb: a single `Option<(f64, f64)>` remembering the point
/// to warp back to. `Some` means "currently parked"; taking it back to
/// `None` in `restore` is also the signal that nothing needs undoing,
/// which is what makes `restore` safe to call unconditionally, both from
/// the callback (on a focus-to-local transition it only notices after the
/// fact) and from `MacCapturer`'s `Drop`.
struct CursorPark {
    origin: Mutex<Option<(f64, f64)>>,
}

impl CursorPark {
    fn new() -> Self {
        Self {
            origin: Mutex::new(None),
        }
    }

    /// Records `at` as the point to come back to and hides the cursor, but
    /// only the first time this is called after a crossing: a no-op if
    /// already parked, so it is safe to call on every remote motion event
    /// rather than only the first.
    fn park(&self, at: (f64, f64)) {
        let mut origin = lock_recovering(&self.origin, "cursor_park_origin");
        if origin.is_none() {
            *origin = Some(at);
            cursor::hide_cursor();
        }
    }

    /// Warps the real cursor back to the parked point. A no-op if nothing
    /// is currently parked.
    fn hold(&self) {
        let origin = lock_recovering(&self.origin, "cursor_park_origin");
        if let Some((x, y)) = *origin {
            cursor::warp_cursor(x, y);
        }
    }

    /// Gives the cursor back: warps it to the parked point one last time,
    /// makes it visible again, and clears the parked state. Safe to call
    /// whether or not anything is actually parked, which is what lets
    /// both the callback and `Drop` call it unconditionally rather than
    /// tracking their own "did we already restore this" flag.
    fn restore(&self) {
        let mut origin = lock_recovering(&self.origin, "cursor_park_origin");
        if let Some((x, y)) = origin.take() {
            cursor::warp_cursor(x, y);
            cursor::show_cursor();
        }
    }
}

/// Captures keyboard and mouse input system wide via a `CGEventTap`.
///
/// The tap and its run loop live on a dedicated background thread; this
/// struct only holds the receiving end of the channel that thread feeds,
/// the flag that tells it whether to suppress what it sees, and a handle
/// to the cursor-parking state so `Drop` can always give the cursor back.
pub struct MacCapturer {
    events: Receiver<InputEvent>,
    remote: Arc<AtomicBool>,
    park: Arc<CursorPark>,
}

impl MacCapturer {
    /// Starts the background capture thread and blocks until the tap is
    /// either up and enabled, or has failed to start. `edge` is the
    /// screen edge that hands focus to the peer, taken from the caller's
    /// `[layout]` configuration rather than assumed here.
    pub fn start(edge: Edge) -> Result<Self, CaptureError> {
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let remote = Arc::new(AtomicBool::new(false));
        let remote_for_thread = Arc::clone(&remote);
        let park = Arc::new(CursorPark::new());
        let park_for_thread = Arc::clone(&park);

        thread::Builder::new()
            .name("hop-capture-tap".into())
            .spawn(move || {
                run_capture_thread(event_tx, remote_for_thread, park_for_thread, edge, ready_tx)
            })
            .map_err(CaptureError::ThreadSpawnFailed)?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events: event_rx,
                remote,
                park,
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(CaptureError::ThreadExitedEarly),
        }
    }

    /// A handle the owner flips when focus moves to or from the peer.
    /// While `true`, the tap suppresses everything it captures instead of
    /// letting it also reach the local Mac.
    pub fn remote_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.remote)
    }
}

impl Capturer for MacCapturer {
    fn poll(&mut self) -> Option<InputEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for MacCapturer {
    fn drop(&mut self) {
        // However this capturer is going away, normal shutdown, a
        // connection loop ending, or an early return somewhere above it,
        // the user must never be left with an invisible, pinned cursor:
        // this is the last chance to give it back. `restore` is
        // idempotent, so calling it here even when nothing is parked is
        // harmless.
        self.park.restore();
    }
}

/// Locks `mutex`, recovering its contents instead of propagating the
/// poison if a panic caught elsewhere (the tap callback's `catch_unwind`)
/// left it poisoned. Every mutex in this file guards data with no
/// invariant a mid-panic write could break: a `HashSet` of held keycodes,
/// an `Instant`, or an `Option<usize>` port handle. Taking the
/// possibly-mid-mutation value is safe, and doing so is what keeps a
/// single caught panic from silently disabling this subsystem forever.
/// Also clears the poison flag, so this only warns once per panic rather
/// than on every lock for the rest of the process's life.
fn lock_recovering<'a, T>(mutex: &'a Mutex<T>, name: &'static str) -> MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        tracing::warn!(
            mutex = name,
            "mutex was poisoned by an earlier panic; recovering its contents"
        );
        mutex.clear_poison();
        poisoned.into_inner()
    })
}

/// Everything the tap callback needs beyond the event itself: the channel
/// events are pushed into, shared flags, and the small pieces of mutable
/// state a single capture thread owns. Bundled into one struct, moved
/// whole into the callback closure, so `handle_event` takes a reasonable
/// number of arguments instead of nine separate ones.
struct CaptureContext {
    event_tx: Sender<InputEvent>,
    remote: Arc<AtomicBool>,
    held_modifiers: Mutex<HashSet<i64>>,
    last_seen: Arc<Mutex<Instant>>,
    tap_port: Arc<Mutex<Option<usize>>>,
    /// The screen edge that hands focus to the peer.
    edge: Edge,
    screen_width: f64,
    screen_height: f64,
    park: Arc<CursorPark>,
}

/// Body of the dedicated capture thread: creates the tap, wires it into a
/// run loop on this thread, starts the watchdog, and then blocks forever
/// pumping that run loop. Reports success or failure back through
/// `ready_tx` once the tap is enabled (or definitely is not going to be).
fn run_capture_thread(
    event_tx: Sender<InputEvent>,
    remote: Arc<AtomicBool>,
    park: Arc<CursorPark>,
    edge: Edge,
    ready_tx: Sender<Result<(), CaptureError>>,
) {
    let last_seen = Arc::new(Mutex::new(Instant::now()));
    let last_seen_for_callback = Arc::clone(&last_seen);

    // The tap's mach port, stashed as a plain integer so it can be read
    // from the callback and the watchdog thread without borrowing the
    // `CGEventTap` itself (raw pointers are not `Send`/`Sync`; a `usize`
    // is, and the value is only ever reinterpreted as the pointer it came
    // from). Owned here, per capture thread, rather than as a process
    // global: a second `MacCapturer::start()` gets its own port instead of
    // silently sharing the first one's, and it is cleared back to `None`
    // in the same lock that drops the `CGEventTap` below, so a stale
    // value can never outlive the port it names.
    let tap_port: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let tap_port_for_callback = Arc::clone(&tap_port);

    // Read once, up front, rather than on every event: the display's size
    // does not change often enough to justify a syscall on every mouse
    // move, and a resolution change mid session is an accepted limitation
    // here (see Task 13).
    let (screen_width, screen_height) = cursor::screen_size();

    let ctx = CaptureContext {
        event_tx,
        remote,
        held_modifiers: Mutex::new(HashSet::new()),
        last_seen: last_seen_for_callback,
        tap_port: tap_port_for_callback,
        edge,
        screen_width,
        screen_height,
        park,
    };

    let events_of_interest = vec![
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::ScrollWheel,
        // `TapDisabledByTimeout` and `TapDisabledByUserInput` are
        // deliberately NOT listed here, and must never be added back.
        // `CGEventTap::new` folds this list into a mask with
        // `1 << (event_type as u64)`, and those two variants'
        // discriminants are `0xFFFFFFFE` and `0xFFFFFFFF`; shifting by
        // either overflows a `u64` and panics in any build with overflow
        // checks on (the dev profile default), inside `core-graphics`,
        // before the tap is even created. No mask bit is needed for them
        // anyway: macOS delivers both to the callback regardless of the
        // mask, which `handle_event` below already handles.
    ];

    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events_of_interest,
        move |_proxy, event_type, event| {
            // The callback crosses the FFI boundary from macOS: unwinding a
            // panic across it is undefined behaviour. Any failure in
            // `handle_event` is caught here, logged, and turned into
            // `Keep`, so a bug in translation degrades to "this one event
            // passes through unsuppressed" rather than corrupting process
            // state or crashing macOS's event dispatch.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_event(event_type, event, &ctx)
            }));
            outcome.unwrap_or_else(|_| {
                tracing::error!("event tap callback panicked; keeping the event unsuppressed");
                CallbackResult::Keep
            })
        },
    );

    let tap = match tap {
        Ok(tap) => tap,
        Err(()) => {
            let _ = ready_tx.send(Err(CaptureError::TapUnavailable));
            return;
        }
    };

    // Stash the port before enabling anything, so the callback and the
    // watchdog can always find it once they might need it.
    *lock_recovering(&tap_port, "tap_port") = Some(tap.mach_port().as_concrete_TypeRef() as usize);

    let loop_source = match tap.mach_port().create_runloop_source(0) {
        Ok(source) => source,
        Err(()) => {
            teardown_tap(&tap_port, tap);
            let _ = ready_tx.send(Err(CaptureError::RunLoopSourceUnavailable));
            return;
        }
    };

    // SAFETY: `kCFRunLoopCommonModes` is a Core Foundation constant string
    // owned by the framework for the life of the process; reading it here
    // only takes a reference to that static, which is exactly how
    // core-graphics's own documented usage of `CGEventTap` reads it.
    CFRunLoop::get_current().add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
    tap.enable();

    spawn_watchdog(last_seen, Arc::clone(&tap_port));

    // The tap is live; `start` can stop waiting.
    let _ = ready_tx.send(Ok(()));

    // Blocks forever, pumping the run loop that drives the tap callback.
    CFRunLoop::run_current();

    // The run loop is not expected to return in normal operation, but if
    // it ever does, `tap` (owned on this stack) is about to go out of
    // scope. Tear it down through the same path every other early return
    // above uses, so the port is never left pointing at a tap that no
    // longer exists.
    teardown_tap(&tap_port, tap);
}

/// Clears the shared port and drops `tap`, both inside the one critical
/// section `reenable_tap` and `tap_is_enabled` also lock for their entire
/// call. That shared section is what makes tearing down the tap here safe
/// with respect to those two functions: either a call to one of them
/// completes entirely before this runs (and so used a still-live port), or
/// it starts entirely after (and so reads the `None` this leaves behind).
/// Neither can ever observe a port whose tap this call is invalidating.
fn teardown_tap(tap_port: &Mutex<Option<usize>>, tap: CGEventTap<'_>) {
    let mut guard = lock_recovering(tap_port, "tap_port");
    *guard = None;
    // `CGEventTap`'s `Drop` calls `CFMachPortInvalidate` and releases the
    // port; dropping it while still holding `guard` is the whole point of
    // this function.
    drop(tap);
}

/// Pure-ish core of the callback: never touches macOS APIs beyond reading
/// fields off the event it was handed and the cursor calls in `cursor.rs`
/// for edge detection and parking, and never panics. Kept out of the
/// closure so `catch_unwind` has a plain function to wrap.
fn handle_event(event_type: CGEventType, event: &CGEvent, ctx: &CaptureContext) -> CallbackResult {
    *lock_recovering(&ctx.last_seen, "last_seen") = Instant::now();

    if matches!(
        event_type,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        // This is the whole point of the file: macOS delivers this as an
        // ordinary event with no error return anywhere. Ignoring it means
        // capture silently stops while everything still looks up.
        tracing::warn!(
            ?event_type,
            "macOS disabled the event tap; re-enabling immediately"
        );
        reenable_tap(&ctx.tap_port);
        return CallbackResult::Keep;
    }

    let translated = if matches!(event_type, CGEventType::FlagsChanged) {
        let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        let flags = event.get_flags().bits();
        let mut held = lock_recovering(&ctx.held_modifiers, "held_modifiers");
        translate_modifier(keycode, flags, &mut held)
    } else {
        let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        let (dx, dy) = match event_type {
            CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged => (
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as i32,
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as i32,
            ),
            CGEventType::ScrollWheel => {
                let continuous = event
                    .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS)
                    != 0;
                if continuous {
                    // Trackpads and Magic Mice (the primary input device
                    // on an Apple Silicon laptop) report continuous,
                    // pixel-based scrolling. The line-granularity fields
                    // used below read 0 until a full line accumulates,
                    // which on these devices may never happen, so a
                    // continuous event has to read the point-delta fields
                    // instead. Point deltas are a different scale than
                    // line deltas, so the peer's injector may eventually
                    // need its own sensitivity for this axis; unconfirmed
                    // against real trackpad hardware, see Task 13.
                    (
                        event.get_integer_value_field(
                            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
                        ) as i32,
                        event.get_integer_value_field(
                            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
                        ) as i32,
                    )
                } else {
                    (
                        event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)
                            as i32,
                        event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1)
                            as i32,
                    )
                }
            }
            _ => (0, 0),
        };
        let button_number = if matches!(
            event_type,
            CGEventType::OtherMouseDown | CGEventType::OtherMouseUp
        ) {
            event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER)
        } else {
            0
        };
        translate(event_type, keycode, dx, dy, button_number)
    };

    if let Some(input_event) = translated {
        // The receiver only goes away when `MacCapturer` is dropped, at
        // which point there is nothing useful to do with a send failure;
        // dropping the event on the floor is the correct response.
        let _ = ctx.event_tx.send(input_event);
    }

    let is_motion_event = matches!(
        event_type,
        CGEventType::MouseMoved | CGEventType::LeftMouseDragged | CGEventType::RightMouseDragged
    );

    if ctx.remote.load(Ordering::Relaxed) {
        // Focus is on the peer: the real cursor must never be seen moving
        // or land anywhere on this display, so on every motion event it is
        // warped straight back to wherever it was parked at the moment of
        // crossing.
        if is_motion_event {
            ctx.park.hold();
        }
    } else {
        // Focus is local. If a parked point is still on record here, the
        // return to Local happened outside this callback (a disconnect, a
        // panic hotkey, an explicit release, all handled by the
        // connection loop that owns `Control`), and this is simply the
        // first event this callback has seen since. Give the cursor back
        // on this event, of whatever type, rather than waiting for a
        // motion event that also happens to cross the edge again. Cheap
        // and a no-op when nothing is parked, so it is safe to call
        // unconditionally here.
        ctx.park.restore();

        if is_motion_event {
            let location = event.location();
            if crossed(
                ctx.edge,
                location.x,
                location.y,
                ctx.screen_width,
                ctx.screen_height,
            ) {
                let landing = nudge_inward(
                    ctx.edge,
                    location.x,
                    location.y,
                    ctx.screen_width,
                    ctx.screen_height,
                );
                ctx.park.park(landing);
                // Set before the final suppression check below runs, so
                // the very event that crossed the edge is itself already
                // suppressed rather than leaking one more pixel of local
                // motion past the boundary.
                ctx.remote.store(true, Ordering::Relaxed);
                // Not translated by `translate`, and not the receiver's
                // problem if nobody is listening; see the comment above
                // for `input_event`.
                let _ = ctx.event_tx.send(InputEvent::EdgeCrossed);
            }
        }
    }

    if ctx.remote.load(Ordering::Relaxed) {
        CallbackResult::Drop
    } else {
        CallbackResult::Keep
    }
}

/// Translates a non-modifier tap event into this tool's own vocabulary.
/// Pure function: no macOS calls, no I/O, so it is the part of this file
/// that can actually be unit tested without hardware.
///
/// `keycode` is read for key events, `dx`/`dy` for mouse move, drag and
/// scroll events, `button_number` for the third-and-up mouse button
/// events; irrelevant fields are ignored by the arms that do not need
/// them. An unmapped keycode or button number yields `None` rather than a
/// guess, matching `virtual_key_to_usage`.
fn translate(
    event_type: CGEventType,
    keycode: i64,
    dx: i32,
    dy: i32,
    button_number: i64,
) -> Option<InputEvent> {
    match event_type {
        CGEventType::KeyDown => virtual_key_to_usage(keycode).map(|usage| InputEvent::Key {
            usage,
            pressed: true,
        }),
        CGEventType::KeyUp => virtual_key_to_usage(keycode).map(|usage| InputEvent::Key {
            usage,
            pressed: false,
        }),
        // `LeftMouseDragged`/`RightMouseDragged` are what macOS sends for
        // motion while a button is held; `MouseMoved` only fires while no
        // button is down. Treating them as anything other than motion
        // loses every drag: drag-select, drag-and-drop, window dragging.
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged => Some(InputEvent::Mouse { dx, dy }),
        CGEventType::LeftMouseDown => Some(InputEvent::Button {
            button: Button::Left,
            pressed: true,
        }),
        CGEventType::LeftMouseUp => Some(InputEvent::Button {
            button: Button::Left,
            pressed: false,
        }),
        CGEventType::RightMouseDown => Some(InputEvent::Button {
            button: Button::Right,
            pressed: true,
        }),
        CGEventType::RightMouseUp => Some(InputEvent::Button {
            button: Button::Right,
            pressed: false,
        }),
        CGEventType::OtherMouseDown => {
            other_mouse_button(button_number).map(|button| InputEvent::Button {
                button,
                pressed: true,
            })
        }
        CGEventType::OtherMouseUp => {
            other_mouse_button(button_number).map(|button| InputEvent::Button {
                button,
                pressed: false,
            })
        }
        CGEventType::ScrollWheel => Some(InputEvent::Scroll { dx, dy }),
        _ => None,
    }
}

/// Maps a `MOUSE_EVENT_BUTTON_NUMBER` value from an `OtherMouseDown` or
/// `OtherMouseUp` event to a protocol button. Only button 2 (0-indexed;
/// the middle button) has a home in the wire protocol; anything past it
/// (a mouse's 4th or 5th button) is ignored rather than guessed at.
fn other_mouse_button(button_number: i64) -> Option<Button> {
    match button_number {
        2 => Some(Button::Middle),
        _ => None,
    }
}

/// Device-dependent flag bits from IOLLEvent.h (`NX_DEVICE*KEYMASK`) that
/// distinguish left and right modifier keys, which otherwise share one
/// device-independent bit in `CGEventFlags`. This crate's `CGEventFlags`
/// does not name them, but `CGEventGetFlags` still returns them in the raw
/// `u64` it hands back, so they are read directly by bit value here.
mod device_flag {
    pub const LEFT_CONTROL: u64 = 0x0001;
    pub const LEFT_SHIFT: u64 = 0x0002;
    pub const RIGHT_SHIFT: u64 = 0x0004;
    pub const LEFT_COMMAND: u64 = 0x0008;
    pub const RIGHT_COMMAND: u64 = 0x0010;
    pub const LEFT_OPTION: u64 = 0x0020;
    pub const RIGHT_OPTION: u64 = 0x0040;
    pub const RIGHT_CONTROL: u64 = 0x2000;
}

/// Maps a macOS virtual keycode for a modifier key to the device-dependent
/// flag bit that reports whether that specific key, as opposed to its
/// same-side sibling, is currently held. `None` for modifiers with no
/// left/right distinction to make, which today is only caps lock.
fn device_bit_for_keycode(keycode: i64) -> Option<u64> {
    match keycode {
        59 => Some(device_flag::LEFT_CONTROL),
        62 => Some(device_flag::RIGHT_CONTROL),
        56 => Some(device_flag::LEFT_SHIFT),
        60 => Some(device_flag::RIGHT_SHIFT),
        55 => Some(device_flag::LEFT_COMMAND),
        54 => Some(device_flag::RIGHT_COMMAND),
        58 => Some(device_flag::LEFT_OPTION),
        61 => Some(device_flag::RIGHT_OPTION),
        _ => None,
    }
}

/// Translates a `FlagsChanged` event (produced for modifier keys such as
/// shift, control, option, command and caps lock) into a key press or
/// release.
///
/// `flags` is the raw bit pattern from the event's `CGEventGetFlags()` at
/// the moment it fired. For modifiers with a left/right distinction, that
/// value's device-dependent bits (see `device_flag`) report directly
/// whether THIS key is down right now: absolute state, read fresh from
/// every event. That makes it self-correcting after a dropped event,
/// unlike inferring press/release by toggling a "currently held" set,
/// which desyncs forever the first time a `FlagsChanged` is missed (for
/// example while the tap is disabled and re-armed): the next press for
/// that key would read as a release, leaving the peer holding a modifier
/// that was never actually pressed there. An earlier version of this
/// function used that toggle for every modifier and reasoned the
/// device-independent mask alone could not recover direction; that is
/// true only of the device-independent bits, not of `CGEventFlags` as a
/// whole.
///
/// Caps lock has no left/right distinction and so no device bit to read
/// this way; it keeps the toggle in `held`, which is fine for it in
/// practice since caps lock is not usually held through a tap gap.
fn translate_modifier(keycode: i64, flags: u64, held: &mut HashSet<i64>) -> Option<InputEvent> {
    let usage = virtual_key_to_usage(keycode)?;
    let pressed = match device_bit_for_keycode(keycode) {
        Some(bit) => flags & bit != 0,
        None => {
            let inserted = held.insert(keycode);
            if !inserted {
                held.remove(&keycode);
            }
            inserted
        }
    };
    Some(InputEvent::Key { usage, pressed })
}

/// Calls `CGEventTapEnable(port, true)` on whatever port `tap_port`
/// currently holds; a no-op if it has been cleared, which happens once the
/// tap has been torn down (see `teardown_tap`).
///
/// SAFETY: this function holds `tap_port`'s lock for the entire call to
/// `CGEventTapEnable` below, and `teardown_tap` holds the same lock for
/// its entire clear-and-drop. Because of that, this can never read a
/// port whose tap is concurrently being invalidated: either
/// `teardown_tap` finishes first (and this then reads `None`), or this
/// finishes first (and used a port that was still valid for the whole
/// call). `CGEventTapEnable` itself is documented as safe to call at any
/// time, including from the tap's own callback and from another thread.
fn reenable_tap(tap_port: &Mutex<Option<usize>>) {
    let guard = lock_recovering(tap_port, "tap_port");
    if let Some(port) = *guard {
        unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
    }
}

/// Reads whether the tap `tap_port` refers to is currently enabled, or
/// `None` if the tap has already been torn down. Uses the same lock, held
/// for the same reason, as `reenable_tap`; see its SAFETY comment.
fn tap_is_enabled(tap_port: &Mutex<Option<usize>>) -> Option<bool> {
    let guard = lock_recovering(tap_port, "tap_port");
    let port = (*guard)?;
    Some(unsafe { CGEventTapIsEnabled(port as CFMachPortRef) } != 0)
}

/// Belt-and-braces recovery for disable causes macOS does not report as a
/// `TapDisabledBy*` event. The spike found that locking the screen alone
/// did not produce one on macOS 27, so silence for `WATCHDOG_TIMEOUT` is
/// treated as reason to check on the tap.
///
/// Checking is not the same as re-arming: an unattended machine is
/// silent for exactly the same reason (nothing has happened), so this
/// only calls `CGEventTapEnable`, and only warns, when
/// `CGEventTapIsEnabled` actually reports the tap disabled. Idle silence
/// with the tap still enabled logs at debug, so an unattended Mac does
/// not train its operator to ignore this file's one log line that
/// matters.
fn spawn_watchdog(last_seen: Arc<Mutex<Instant>>, tap_port: Arc<Mutex<Option<usize>>>) {
    let spawned = thread::Builder::new()
        .name("hop-capture-watchdog".into())
        .spawn(move || loop {
            thread::sleep(WATCHDOG_POLL_INTERVAL);
            let elapsed = lock_recovering(&last_seen, "last_seen").elapsed();
            if elapsed < WATCHDOG_TIMEOUT {
                continue;
            }
            match tap_is_enabled(&tap_port) {
                Some(false) => {
                    tracing::warn!(
                        elapsed_secs = elapsed.as_secs(),
                        "event tap was disabled without a TapDisabledBy* event; re-enabling"
                    );
                    reenable_tap(&tap_port);
                }
                Some(true) => {
                    tracing::debug!(
                        elapsed_secs = elapsed.as_secs(),
                        "no event tap activity recently; tap is still enabled, assuming idle"
                    );
                }
                None => {
                    // The tap has been torn down; nothing left to watch.
                    return;
                }
            }
            *lock_recovering(&last_seen, "last_seen") = Instant::now();
        });

    if let Err(err) = spawned {
        // The tap still works without the watchdog; it just loses the
        // extra safety net for disable causes the callback itself never
        // sees. Worth knowing about, not worth failing startup over.
        tracing::error!(%err, "failed to spawn the event tap watchdog thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hop_proto::Usage;

    #[test]
    fn translates_key_down() {
        // macOS virtual keycode 0 is 'a', HID usage 0x04.
        assert_eq!(
            translate(CGEventType::KeyDown, 0, 0, 0, 0),
            Some(InputEvent::Key {
                usage: Usage::A,
                pressed: true
            })
        );
    }

    #[test]
    fn translates_key_up() {
        // Keycode 8 is 'c'.
        assert_eq!(
            translate(CGEventType::KeyUp, 8, 0, 0, 0),
            Some(InputEvent::Key {
                usage: Usage::C,
                pressed: false
            })
        );
    }

    #[test]
    fn translates_mouse_move_from_deltas_not_position() {
        assert_eq!(
            translate(CGEventType::MouseMoved, 0, 12, -7, 0),
            Some(InputEvent::Mouse { dx: 12, dy: -7 })
        );
    }

    #[test]
    fn translates_dragged_events_as_motion_like_mouse_moved() {
        // A held button turns MouseMoved into a Dragged variant; both
        // must produce the same motion event or every drag is lost.
        assert_eq!(
            translate(CGEventType::LeftMouseDragged, 0, 5, -2, 0),
            Some(InputEvent::Mouse { dx: 5, dy: -2 })
        );
        assert_eq!(
            translate(CGEventType::RightMouseDragged, 0, -3, 9, 0),
            Some(InputEvent::Mouse { dx: -3, dy: 9 })
        );
    }

    #[test]
    fn translates_scroll() {
        assert_eq!(
            translate(CGEventType::ScrollWheel, 0, 1, -3, 0),
            Some(InputEvent::Scroll { dx: 1, dy: -3 })
        );
    }

    #[test]
    fn unmapped_keycode_yields_none_rather_than_a_guess() {
        assert_eq!(translate(CGEventType::KeyDown, 9999, 0, 0, 0), None);
        assert_eq!(translate(CGEventType::KeyUp, 9999, 0, 0, 0), None);
    }

    #[test]
    fn translates_mouse_buttons() {
        assert_eq!(
            translate(CGEventType::LeftMouseDown, 0, 0, 0, 0),
            Some(InputEvent::Button {
                button: Button::Left,
                pressed: true
            })
        );
        assert_eq!(
            translate(CGEventType::RightMouseUp, 0, 0, 0, 0),
            Some(InputEvent::Button {
                button: Button::Right,
                pressed: false
            })
        );
    }

    #[test]
    fn translates_middle_button_from_other_mouse_events() {
        assert_eq!(
            translate(CGEventType::OtherMouseDown, 0, 0, 0, 2),
            Some(InputEvent::Button {
                button: Button::Middle,
                pressed: true
            })
        );
        assert_eq!(
            translate(CGEventType::OtherMouseUp, 0, 0, 0, 2),
            Some(InputEvent::Button {
                button: Button::Middle,
                pressed: false
            })
        );
    }

    #[test]
    fn other_mouse_buttons_past_middle_are_ignored_not_guessed() {
        assert_eq!(translate(CGEventType::OtherMouseDown, 0, 0, 0, 3), None);
        assert_eq!(translate(CGEventType::OtherMouseUp, 0, 0, 0, 4), None);
    }

    #[test]
    fn irrelevant_event_types_yield_none() {
        assert_eq!(translate(CGEventType::FlagsChanged, 56, 0, 0, 0), None);
        assert_eq!(
            translate(CGEventType::TapDisabledByTimeout, 0, 0, 0, 0),
            None
        );
    }

    #[test]
    fn left_shift_reads_from_its_own_device_bit() {
        let mut held = HashSet::new();
        // CGEventFlagShift (0x00020000) | NX_DEVICELSHIFTKEYMASK (0x2).
        let down = 0x0002_0000 | 0x2;
        assert_eq!(
            translate_modifier(56, down, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: true
            })
        );
        assert_eq!(
            translate_modifier(56, 0, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: false
            })
        );
    }

    #[test]
    fn right_shift_is_distinguished_from_left_by_its_own_device_bit() {
        let mut held = HashSet::new();
        // CGEventFlagShift | NX_DEVICERSHIFTKEYMASK (0x4): only the right
        // key's own bit is set.
        let right_down = 0x0002_0000 | 0x4;
        assert_eq!(
            translate_modifier(60, right_down, &mut held),
            Some(InputEvent::Key {
                usage: Usage::RIGHT_SHIFT,
                pressed: true
            })
        );
        // Left shift's own bit is not part of that mask, so it must read
        // as not pressed even though the device-independent Shift bit is
        // set alongside the right key's bit.
        assert_eq!(
            translate_modifier(56, right_down, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: false
            })
        );
    }

    #[test]
    fn shift_state_survives_a_dropped_flags_changed_event() {
        // Regression test for IMPORTANT 3: a toggle-based implementation
        // desyncs the first time a `FlagsChanged` is missed (for example
        // while the tap is disabled and re-armed). Reading the device bit
        // fresh from every event means a missed release cannot desync
        // anything: the very next down for the same key still reads as a
        // press.
        let mut held = HashSet::new();
        let down = 0x0002_0000 | 0x2; // left shift down
        assert_eq!(
            translate_modifier(56, down, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: true
            })
        );
        // The matching release never reaches here (lost while the tap was
        // disabled), so no call happens for it. Shift goes down again
        // with the same device bit set, and must still be reported as a
        // press rather than a stray release.
        assert_eq!(
            translate_modifier(56, down, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: true
            })
        );
    }

    #[test]
    fn caps_lock_still_toggles_via_the_held_set() {
        // Caps lock has no left/right distinction and so no device bit;
        // it keeps the previous toggle behavior.
        let mut held = HashSet::new();
        assert_eq!(
            translate_modifier(57, 0, &mut held),
            Some(InputEvent::Key {
                usage: Usage(0x39),
                pressed: true
            })
        );
        assert!(held.contains(&57));
        assert_eq!(
            translate_modifier(57, 0, &mut held),
            Some(InputEvent::Key {
                usage: Usage(0x39),
                pressed: false
            })
        );
        assert!(!held.contains(&57));
    }

    #[test]
    fn unmapped_modifier_yields_none_and_does_not_get_tracked() {
        let mut held = HashSet::new();
        assert_eq!(translate_modifier(9999, 0, &mut held), None);
        assert!(held.is_empty());
    }

    // `crossed` is the pure decision behind edge detection: given where
    // the cursor is and how big the screen is, has it reached the
    // configured edge. Everything else this task adds (reading the real
    // cursor, warping it, hiding it) needs hardware and is out of reach
    // for an automated test; this is the part that actually is one.
    const SCREEN_W: f64 = 1920.0;
    const SCREEN_H: f64 = 1080.0;

    #[test]
    fn top_edge_triggers_exactly_at_y_zero() {
        assert!(crossed(Edge::Top, 960.0, 0.0, SCREEN_W, SCREEN_H));
    }

    #[test]
    fn top_edge_does_not_trigger_just_inside() {
        assert!(!crossed(Edge::Top, 960.0, 5.0, SCREEN_W, SCREEN_H));
    }

    #[test]
    fn bottom_edge_triggers_at_the_screen_height_boundary() {
        assert!(crossed(
            Edge::Bottom,
            960.0,
            SCREEN_H - 1.0,
            SCREEN_W,
            SCREEN_H
        ));
    }

    #[test]
    fn bottom_edge_does_not_trigger_just_inside() {
        assert!(!crossed(
            Edge::Bottom,
            960.0,
            SCREEN_H - 6.0,
            SCREEN_W,
            SCREEN_H
        ));
    }

    #[test]
    fn left_edge_triggers_exactly_at_x_zero() {
        assert!(crossed(Edge::Left, 0.0, 540.0, SCREEN_W, SCREEN_H));
    }

    #[test]
    fn left_edge_does_not_trigger_just_inside() {
        assert!(!crossed(Edge::Left, 5.0, 540.0, SCREEN_W, SCREEN_H));
    }

    #[test]
    fn right_edge_triggers_at_the_screen_width_boundary() {
        assert!(crossed(
            Edge::Right,
            SCREEN_W - 1.0,
            540.0,
            SCREEN_W,
            SCREEN_H
        ));
    }

    #[test]
    fn right_edge_does_not_trigger_just_inside() {
        assert!(!crossed(
            Edge::Right,
            SCREEN_W - 6.0,
            540.0,
            SCREEN_W,
            SCREEN_H
        ));
    }

    #[test]
    fn only_the_top_edge_triggers_at_the_top_boundary() {
        // The deployment this project ships for: the PC's monitors sit
        // above the Mac, so `top` is the edge that actually matters, and
        // it must not be possible for a point on that boundary to also
        // read as having crossed any other edge.
        let (x, y) = (960.0, 0.0);
        assert!(crossed(Edge::Top, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Bottom, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Left, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Right, x, y, SCREEN_W, SCREEN_H));
    }

    #[test]
    fn only_the_left_edge_triggers_at_the_left_boundary() {
        let (x, y) = (0.0, 540.0);
        assert!(crossed(Edge::Left, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Top, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Bottom, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Right, x, y, SCREEN_W, SCREEN_H));
    }

    #[test]
    fn only_the_right_edge_triggers_at_the_right_boundary() {
        let (x, y) = (SCREEN_W - 1.0, 540.0);
        assert!(crossed(Edge::Right, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Top, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Bottom, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Left, x, y, SCREEN_W, SCREEN_H));
    }

    #[test]
    fn only_the_bottom_edge_triggers_at_the_bottom_boundary() {
        let (x, y) = (960.0, SCREEN_H - 1.0);
        assert!(crossed(Edge::Bottom, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Top, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Left, x, y, SCREEN_W, SCREEN_H));
        assert!(!crossed(Edge::Right, x, y, SCREEN_W, SCREEN_H));
    }

    // `nudge_inward` is what keeps a restored cursor from sitting exactly
    // on the boundary `crossed` treats as a crossing, which would bounce
    // focus straight back to the peer on the next reported motion. Pure,
    // so it gets the same direct coverage as `crossed`.
    #[test]
    fn nudge_inward_moves_away_from_each_edge_past_its_own_boundary() {
        let (_, y) = nudge_inward(Edge::Top, 960.0, 0.0, SCREEN_W, SCREEN_H);
        assert!(!crossed(Edge::Top, 960.0, y, SCREEN_W, SCREEN_H));

        let (_, y) = nudge_inward(Edge::Bottom, 960.0, SCREEN_H - 1.0, SCREEN_W, SCREEN_H);
        assert!(!crossed(Edge::Bottom, 960.0, y, SCREEN_W, SCREEN_H));

        let (x, _) = nudge_inward(Edge::Left, 0.0, 540.0, SCREEN_W, SCREEN_H);
        assert!(!crossed(Edge::Left, x, 540.0, SCREEN_W, SCREEN_H));

        let (x, _) = nudge_inward(Edge::Right, SCREEN_W - 1.0, 540.0, SCREEN_W, SCREEN_H);
        assert!(!crossed(Edge::Right, x, 540.0, SCREEN_W, SCREEN_H));
    }

    #[test]
    fn nudge_inward_clamps_on_a_screen_smaller_than_the_margin() {
        // A screen thinner than `EDGE_MARGIN` must still yield an
        // in-bounds, non-negative point rather than going negative.
        let (x, _) = nudge_inward(Edge::Left, 0.0, 5.0, 3.0, 3.0);
        assert!((0.0..=3.0).contains(&x));

        let (_, y) = nudge_inward(Edge::Top, 5.0, 0.0, 3.0, 3.0);
        assert!((0.0..=3.0).contains(&y));
    }

    // `CursorPark::park`/`hold`/`restore` are deliberately not exercised
    // here: every path through them ends in a real `cursor::hide_cursor`,
    // `warp_cursor`, or `show_cursor` call, and this workspace's tests run
    // on real macOS hosts, so calling them from a unit test would actually
    // hide and warp the developer's cursor as a side effect of `cargo
    // test`. That is exactly the kind of hardware-dependent behavior this
    // task's brief calls out as only verifiable by a human, in Task 13;
    // `crossed` and `nudge_inward` above are the parts of this file that
    // are actually pure.
}
