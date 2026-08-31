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

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
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

use crate::macos::keymap::virtual_key_to_usage;
use crate::{Capturer, InputEvent};

// `CGEventTapEnable` is declared here rather than used from `core-graphics`
// because the crate only exposes it through `CGEventTap::enable`, which
// requires an owned `CGEventTap`. We need to call it from the tap callback
// and from a watchdog thread, neither of which owns the tap, so we bind the
// same C function directly, exactly as the spike did.
unsafe extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

/// The event tap's mach port, stashed as a plain integer so it can be read
/// from the callback and the watchdog thread without borrowing the
/// `CGEventTap` itself. Raw pointers are not `Send`/`Sync`; a `usize` is,
/// and the value is only ever reinterpreted as the pointer it came from.
static TAP_PORT: OnceLock<usize> = OnceLock::new();

/// How long the watchdog waits without seeing any event before it assumes
/// the tap went deaf without telling anyone and re-arms it anyway.
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

/// Captures keyboard and mouse input system wide via a `CGEventTap`.
///
/// The tap and its run loop live on a dedicated background thread; this
/// struct only holds the receiving end of the channel that thread feeds,
/// plus the flag that tells it whether to suppress what it sees.
pub struct MacCapturer {
    events: Receiver<InputEvent>,
    remote: Arc<AtomicBool>,
}

impl MacCapturer {
    /// Starts the background capture thread and blocks until the tap is
    /// either up and enabled, or has failed to start.
    pub fn start() -> Result<Self, CaptureError> {
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let remote = Arc::new(AtomicBool::new(false));
        let remote_for_thread = Arc::clone(&remote);

        thread::Builder::new()
            .name("hop-capture-tap".into())
            .spawn(move || run_capture_thread(event_tx, remote_for_thread, ready_tx))
            .map_err(CaptureError::ThreadSpawnFailed)?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events: event_rx,
                remote,
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

/// Body of the dedicated capture thread: creates the tap, wires it into a
/// run loop on this thread, starts the watchdog, and then blocks forever
/// pumping that run loop. Reports success or failure back through
/// `ready_tx` once the tap is enabled (or definitely is not going to be).
fn run_capture_thread(
    event_tx: Sender<InputEvent>,
    remote: Arc<AtomicBool>,
    ready_tx: Sender<Result<(), CaptureError>>,
) {
    let held_modifiers: Mutex<HashSet<i64>> = Mutex::new(HashSet::new());
    let last_seen = Arc::new(Mutex::new(Instant::now()));
    let last_seen_for_callback = Arc::clone(&last_seen);

    let events_of_interest = vec![
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::ScrollWheel,
        CGEventType::TapDisabledByTimeout,
        CGEventType::TapDisabledByUserInput,
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
                handle_event(
                    event_type,
                    event,
                    &event_tx,
                    &remote,
                    &held_modifiers,
                    &last_seen_for_callback,
                )
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
    let _ = TAP_PORT.set(tap.mach_port().as_concrete_TypeRef() as usize);

    let loop_source = match tap.mach_port().create_runloop_source(0) {
        Ok(source) => source,
        Err(()) => {
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

    spawn_watchdog(last_seen);

    // The tap is live; `start` can stop waiting.
    let _ = ready_tx.send(Ok(()));

    // Blocks forever, pumping the run loop that drives the tap callback.
    CFRunLoop::run_current();
}

/// Pure-ish core of the callback: never touches macOS APIs beyond reading
/// fields off the event it was handed, and never panics. Kept out of the
/// closure so `catch_unwind` has a plain function to wrap.
fn handle_event(
    event_type: CGEventType,
    event: &CGEvent,
    event_tx: &Sender<InputEvent>,
    remote: &AtomicBool,
    held_modifiers: &Mutex<HashSet<i64>>,
    last_seen: &Mutex<Instant>,
) -> CallbackResult {
    if let Ok(mut guard) = last_seen.lock() {
        *guard = Instant::now();
    }

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
        reenable_tap();
        return CallbackResult::Keep;
    }

    let translated = if matches!(event_type, CGEventType::FlagsChanged) {
        let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        match held_modifiers.lock() {
            Ok(mut held) => translate_modifier(keycode, &mut held),
            Err(_) => None,
        }
    } else {
        let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        let (dx, dy) = match event_type {
            CGEventType::MouseMoved => (
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as i32,
                event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as i32,
            ),
            CGEventType::ScrollWheel => (
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as i32,
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as i32,
            ),
            _ => (0, 0),
        };
        translate(event_type, keycode, dx, dy)
    };

    if let Some(input_event) = translated {
        // The receiver only goes away when `MacCapturer` is dropped, at
        // which point there is nothing useful to do with a send failure;
        // dropping the event on the floor is the correct response.
        let _ = event_tx.send(input_event);
    }

    if remote.load(Ordering::Relaxed) {
        CallbackResult::Drop
    } else {
        CallbackResult::Keep
    }
}

/// Translates a non-modifier tap event into this tool's own vocabulary.
/// Pure function: no macOS calls, no I/O, so it is the part of this file
/// that can actually be unit tested without hardware.
///
/// `keycode` is read for key events, `dx`/`dy` for mouse move and scroll
/// events; irrelevant fields are ignored by the arms that do not need
/// them. An unmapped keycode yields `None` rather than a guess, matching
/// `virtual_key_to_usage`.
fn translate(event_type: CGEventType, keycode: i64, dx: i32, dy: i32) -> Option<InputEvent> {
    match event_type {
        CGEventType::KeyDown => virtual_key_to_usage(keycode).map(|usage| InputEvent::Key {
            usage,
            pressed: true,
        }),
        CGEventType::KeyUp => virtual_key_to_usage(keycode).map(|usage| InputEvent::Key {
            usage,
            pressed: false,
        }),
        CGEventType::MouseMoved => Some(InputEvent::Mouse { dx, dy }),
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
        CGEventType::ScrollWheel => Some(InputEvent::Scroll { dx, dy }),
        _ => None,
    }
}

/// Translates a `FlagsChanged` event (produced for modifier keys such as
/// shift, control, option, command and caps lock) into a key press or
/// release.
///
/// `FlagsChanged` does not say which direction the change was; macOS only
/// hands back the keycode that changed and the resulting flag bitmask,
/// and left/right variants of the same modifier share a bit, so the
/// bitmask cannot be used to recover direction reliably. Tracking which
/// modifier keycodes are currently considered held and toggling on each
/// event is unambiguous instead, since a physical key produces exactly
/// one `FlagsChanged` event per press and one per release.
fn translate_modifier(keycode: i64, held: &mut HashSet<i64>) -> Option<InputEvent> {
    let usage = virtual_key_to_usage(keycode)?;
    let pressed = held.insert(keycode);
    if !pressed {
        held.remove(&keycode);
    }
    Some(InputEvent::Key { usage, pressed })
}

/// Calls `CGEventTapEnable(port, true)` on whatever port the callback (or
/// the watchdog) most recently learned about. A no-op if the tap has not
/// finished being created yet.
fn reenable_tap() {
    if let Some(&port) = TAP_PORT.get() {
        // SAFETY: `port` was captured from a live `CFMachPortRef` right
        // after `CGEventTapCreate` succeeded and is never invalidated
        // before the process exits (`MacCapturer` never drops the tap).
        // `CGEventTapEnable` is documented as safe to call at any time,
        // including from the tap's own callback and from another thread,
        // and the spike proved re-enabling this way recovers capture.
        unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
    }
}

/// Belt-and-braces recovery for disable causes macOS does not report as a
/// `TapDisabledBy*` event. The spike found that locking the screen alone
/// did not produce one on macOS 27, so silence for `WATCHDOG_TIMEOUT` is
/// itself treated as evidence the tap needs re-arming.
fn spawn_watchdog(last_seen: Arc<Mutex<Instant>>) {
    let spawned = thread::Builder::new()
        .name("hop-capture-watchdog".into())
        .spawn(move || loop {
            thread::sleep(WATCHDOG_POLL_INTERVAL);
            let elapsed = match last_seen.lock() {
                Ok(guard) => guard.elapsed(),
                Err(_) => continue,
            };
            if elapsed >= WATCHDOG_TIMEOUT {
                tracing::warn!(
                    elapsed_secs = elapsed.as_secs(),
                    "no event tap activity recently; re-enabling as a precaution"
                );
                reenable_tap();
                if let Ok(mut guard) = last_seen.lock() {
                    *guard = Instant::now();
                }
            }
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
            translate(CGEventType::KeyDown, 0, 0, 0),
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
            translate(CGEventType::KeyUp, 8, 0, 0),
            Some(InputEvent::Key {
                usage: Usage::C,
                pressed: false
            })
        );
    }

    #[test]
    fn translates_mouse_move_from_deltas_not_position() {
        assert_eq!(
            translate(CGEventType::MouseMoved, 0, 12, -7),
            Some(InputEvent::Mouse { dx: 12, dy: -7 })
        );
    }

    #[test]
    fn translates_scroll() {
        assert_eq!(
            translate(CGEventType::ScrollWheel, 0, 1, -3),
            Some(InputEvent::Scroll { dx: 1, dy: -3 })
        );
    }

    #[test]
    fn unmapped_keycode_yields_none_rather_than_a_guess() {
        assert_eq!(translate(CGEventType::KeyDown, 9999, 0, 0), None);
        assert_eq!(translate(CGEventType::KeyUp, 9999, 0, 0), None);
    }

    #[test]
    fn translates_mouse_buttons() {
        assert_eq!(
            translate(CGEventType::LeftMouseDown, 0, 0, 0),
            Some(InputEvent::Button {
                button: Button::Left,
                pressed: true
            })
        );
        assert_eq!(
            translate(CGEventType::RightMouseUp, 0, 0, 0),
            Some(InputEvent::Button {
                button: Button::Right,
                pressed: false
            })
        );
    }

    #[test]
    fn irrelevant_event_types_yield_none() {
        assert_eq!(translate(CGEventType::FlagsChanged, 56, 0, 0), None);
        assert_eq!(translate(CGEventType::TapDisabledByTimeout, 0, 0, 0), None);
    }

    #[test]
    fn modifier_toggles_press_then_release() {
        let mut held = HashSet::new();
        // Keycode 56 is left shift.
        assert_eq!(
            translate_modifier(56, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: true
            })
        );
        assert!(held.contains(&56));
        assert_eq!(
            translate_modifier(56, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: false
            })
        );
        assert!(!held.contains(&56));
    }

    #[test]
    fn unmapped_modifier_yields_none_and_does_not_get_tracked() {
        let mut held = HashSet::new();
        assert_eq!(translate_modifier(9999, &mut held), None);
        assert!(held.is_empty());
    }

    #[test]
    fn two_modifiers_held_independently() {
        // Left shift (56) and left control (59) pressed together, then
        // released in the opposite order; each must toggle on its own
        // keycode regardless of the other's state.
        let mut held = HashSet::new();
        assert_eq!(
            translate_modifier(56, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: true
            })
        );
        assert_eq!(
            translate_modifier(59, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_CTRL,
                pressed: true
            })
        );
        assert_eq!(
            translate_modifier(56, &mut held),
            Some(InputEvent::Key {
                usage: Usage::LEFT_SHIFT,
                pressed: false
            })
        );
        assert!(held.contains(&59));
        assert!(!held.contains(&56));
    }
}
