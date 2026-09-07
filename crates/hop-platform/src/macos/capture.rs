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
//! translates each event and pushes it into a capped queue (`EventQueue`)
//! only while focus is actually remote, since local input is never
//! forwarded; `MacCapturer::poll` drains that queue without blocking, so
//! the trait's non-blocking contract holds even though the tap itself
//! lives on a loop that blocks forever. See IMPORTANT 4 from the
//! whole-branch review for why the queue is gated and capped rather than
//! an unconditional, unbounded channel.
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

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use hop_proto::{Button, Usage};

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

/// Whether cursor position `(x, y)`, in global display coordinates, has
/// reached `edge` of `bounds`, the union of every active display (see
/// `cursor::display_bounds`). Pure and side effect free, so it is the
/// part of edge detection that can actually be unit tested without
/// hardware; see the `tests` module below.
///
/// This is IMPORTANT 1's fix from the whole-branch review: comparing
/// against `bounds`, rather than assuming the screen starts at `(0, 0)`,
/// is what makes this correct on a multi-display Mac. A display
/// positioned above or to the left of the main one gives `bounds` a
/// negative `min_y` or `min_x`; comparing against that instead of a
/// hardcoded `0.0` is the whole fix. Global display coordinates on macOS
/// put the origin at the main display's top-left with y increasing
/// downward, so the top edge is `y <= bounds.min_y` and the bottom edge
/// is `y >= bounds.max_y - 1.0`; left and right are the same idea on the
/// x axis.
fn crossed(edge: Edge, x: f64, y: f64, bounds: cursor::Bounds) -> bool {
    match edge {
        Edge::Top => y <= bounds.min_y,
        Edge::Bottom => y >= bounds.max_y - 1.0,
        Edge::Left => x <= bounds.min_x,
        Edge::Right => x >= bounds.max_x - 1.0,
    }
}

/// Whether a motion event that has crossed `edge`, while focus is local,
/// should actually start a crossing into the peer. Pure: the whole
/// decision is "is a peer actually connected", so it is directly
/// testable without hardware. This is CRITICAL 1 from the whole-branch
/// review: before this gate existed, `MacCapturer::start` began edge
/// detection the instant it returned, long before `run_server`'s
/// listener even binds and far before any client connects, so crossing
/// the edge with the PC off, asleep, or rebooting suppressed the Mac's
/// own keyboard and mouse with nothing to hand them to and no way to get
/// them back short of SSH from another machine or a forced power-off.
fn should_begin_crossing(peer_connected: bool, edge_crossed: bool) -> bool {
    peer_connected && edge_crossed
}

/// Whether the currently held keys satisfy the panic hotkey `combo`. An
/// empty combo (no panic hotkey configured) never matches, mirroring
/// `HotkeyWatcher` in hop's run.rs, which makes the equivalent check from
/// the connection loop. Pure, so this exact decision is directly
/// testable without a tap; see the callback's use of it for why the same
/// check also has to live here rather than only there.
fn hotkey_matched(combo: &HashSet<Usage>, held: &HashSet<Usage>) -> bool {
    !combo.is_empty() && combo.is_subset(held)
}

/// How far along `edge` a crossing at `(x, y)` happened, from 0.0 at the
/// left or top end to 1.0 at the right or bottom.
///
/// A fraction rather than a pixel offset because the two machines have
/// different resolutions and the point is to enter the peer at the same
/// RELATIVE place the cursor left from, so the movement reads as
/// continuous.
fn crossing_fraction(edge: Edge, x: f64, y: f64, bounds: cursor::Bounds) -> f32 {
    let width = (bounds.max_x - bounds.min_x).max(1.0);
    let height = (bounds.max_y - bounds.min_y).max(1.0);
    let fraction = match edge {
        // Crossing the top or bottom edge varies along x.
        Edge::Top | Edge::Bottom => (x - bounds.min_x) / width,
        // Crossing the left or right edge varies along y.
        Edge::Left | Edge::Right => (y - bounds.min_y) / height,
    };
    fraction.clamp(0.0, 1.0) as f32
}

/// Nudges a point that just crossed `edge` back inside `bounds` by
/// `EDGE_MARGIN`, clamping so a display smaller than the margin still
/// yields an in-bounds point rather than one that overshoots past the
/// opposite edge. Bounds-relative for the same reason `crossed` is: on a
/// multi-display Mac the screen this point is nudged back into does not
/// start at `(0, 0)`.
fn nudge_inward(edge: Edge, x: f64, y: f64, bounds: cursor::Bounds) -> (f64, f64) {
    match edge {
        Edge::Top => (x, (bounds.min_y + EDGE_MARGIN).min(bounds.max_y)),
        Edge::Bottom => (x, (bounds.max_y - 1.0 - EDGE_MARGIN).max(bounds.min_y)),
        Edge::Left => ((bounds.min_x + EDGE_MARGIN).min(bounds.max_x), y),
        Edge::Right => ((bounds.max_x - 1.0 - EDGE_MARGIN).max(bounds.min_x), y),
    }
}

/// How many continuous-scroll points (the unit trackpads and Magic Mice
/// report; see `handle_event`'s `ScrollWheel` arm) are treated as
/// equivalent to one line-delta unit, the unit a real wheel mouse's
/// `ScrollWheel` events already report roughly one-per-notch, and what
/// `wheel_delta` in `windows/inject.rs` multiplies by `WHEEL_DELTA` (120)
/// to get a single notch of Windows wheel input.
///
/// 10.0 was chosen by reasoning from the default per-line scroll height
/// AppKit documents for `NSScrollView` (about 10 points), not measured
/// against real hardware, and needs confirming on an actual trackpad and
/// Magic Mouse before it can be trusted; see Task 13. This is IMPORTANT 3
/// from the whole-branch review: without this scaling, a single 30 point
/// trackpad flick became 30 full notches (3600 raw wheel units) in one
/// event.
const CONTINUOUS_SCROLL_POINTS_PER_UNIT: f64 = 10.0;

/// Converts one axis of a continuous (trackpad/Magic Mouse) scroll
/// event's point delta into whole line-delta units, carrying forward
/// whatever does not divide evenly so a string of slow, sub-threshold
/// flicks still adds up to a scroll instead of being silently discarded
/// on every event. Pure: takes this event's raw point delta and the
/// remainder left over from the previous event on this axis, returns the
/// whole-unit delta to send plus the new remainder to carry forward; see
/// the `tests` module below.
fn scale_continuous_scroll(points: i32, remainder: f64) -> (i32, f64) {
    let total = points as f64 / CONTINUOUS_SCROLL_POINTS_PER_UNIT + remainder;
    let whole = total.trunc();
    (whole as i32, total - whole)
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

    /// Records `at` as the point to come back to, hides the cursor, and
    /// disconnects hardware mouse movement from the cursor (see
    /// `cursor::enter_parked_state`, IMPORTANT 2's fix), but only the
    /// first time this is called after a crossing: a no-op if already
    /// parked, so it is safe to call on every remote motion event rather
    /// than only the first.
    fn park(&self, at: (f64, f64)) {
        let mut origin = lock_recovering(&self.origin, "cursor_park_origin");
        if origin.is_none() {
            *origin = Some(at);
            cursor::hide_cursor();
            cursor::enter_parked_state();
        }
    }

    /// Warps the real cursor back to the parked point. A no-op if nothing
    /// is currently parked.
    /// Deliberately does nothing while parked.
    ///
    /// `enter_parked_state` has already disassociated the mouse, so the
    /// cursor is frozen where `park` left it and hardware movement cannot
    /// drag it anywhere. Warping it back on every motion event, which this
    /// used to do, was therefore redundant, and the warps fought macOS's
    /// own cursor position bookkeeping: on re-association it re-synced the
    /// cursor to where it believed the hardware was, landing it in the
    /// middle of the screen instead of at the edge the user left through.
    fn hold(&self) {}

    /// Gives the cursor back: warps it to the parked point one last time,
    /// makes it visible again, reassociates hardware mouse movement with
    /// the cursor and restores the default suppression interval (undoing
    /// `enter_parked_state`), and clears the parked state. Safe to call
    /// whether or not anything is actually parked, which is what lets
    /// both the callback and `Drop` call it unconditionally rather than
    /// tracking their own "did we already restore this" flag.
    fn restore(&self) {
        let mut origin = lock_recovering(&self.origin, "cursor_park_origin");
        if let Some((x, y)) = origin.take() {
            // Put the cursor back at the edge it left through BEFORE
            // re-associating. Warping afterwards loses the race with
            // macOS's own re-sync, which drops the cursor wherever it
            // thinks the hardware is, typically mid-screen.
            cursor::warp_cursor(x, y);
            cursor::leave_parked_state();
            cursor::show_cursor();
        }
    }
}

/// Hard cap on how many events `EventQueue` holds for `MacCapturer::poll`
/// to drain. This is IMPORTANT 4's second line of defence from the
/// whole-branch review: the tap callback only pushes onto this queue
/// while focus is remote (see `handle_event`), which is the actual fix
/// for the unbounded growth an always-connected consumer (`pump_server`)
/// never has a problem draining; this cap exists for the case where focus
/// really is remote but the consumer has stalled or fallen behind anyway.
/// 4096 events, at a couple hundred bytes each worst case, is a low
/// single-digit number of megabytes: several seconds of even fast mouse
/// motion, comfortably more slack than a healthy connection ever needs.
const MAX_QUEUED_EVENTS: usize = 4096;

/// How often the "queue is full, dropping the oldest event" warning is
/// allowed to fire, so a consumer that stays stalled for a long time logs
/// about it once in a while instead of once per dropped event.
const QUEUE_OVERFLOW_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// A capped FIFO queue shared between the tap callback (the only
/// producer) and `MacCapturer::poll` (the only consumer). Deliberately
/// not `std::sync::mpsc`: dropping the oldest entry once
/// `MAX_QUEUED_EVENTS` is reached needs access to the front of the queue
/// from the producer side, and an mpsc `Sender` can only ever push. A
/// `Mutex<VecDeque<_>>` gives both sides that access. The lock is held
/// only for a `push_back`/`pop_front`, cheap and non-blocking enough for
/// the tap callback's "must stay cheap, must never block" requirement
/// (see the module doc comment), and it is never contended for long since
/// the only other holder is `poll`'s own quick pop.
struct EventQueue {
    events: Mutex<VecDeque<InputEvent>>,
    last_overflow_log: Mutex<Option<Instant>>,
    /// Signalled every time `push` adds an event, so `MacCapturer`'s owner
    /// can `await` this instead of polling `poll()` on a fixed tick.
    /// `Notify::notify_one` is documented as safe to call from any thread,
    /// with or without a tokio runtime on it, and never blocks, which is
    /// exactly what the tap callback's "must stay cheap, must never
    /// block" requirement (see the module doc comment) needs. A single
    /// stored permit is enough even if several events are pushed between
    /// two calls to `notified().await`, since the awaiting side always
    /// drains the whole queue on each wakeup rather than assuming one
    /// wakeup means one event.
    notify: Arc<Notify>,
}

impl EventQueue {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            last_overflow_log: Mutex::new(None),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Pushes `event` onto the back of the queue, dropping the oldest
    /// queued event first if this would exceed `MAX_QUEUED_EVENTS`. Never
    /// blocks: the lock it takes is only ever held briefly by this or by
    /// `pop`, and `Notify::notify_one` below is itself non-blocking.
    fn push(&self, event: InputEvent) {
        let mut events = lock_recovering(&self.events, "event_queue");
        if events.len() >= MAX_QUEUED_EVENTS {
            events.pop_front();
            // `last_overflow_log` is a separate mutex from `events`, so
            // logging here while still holding this lock cannot deadlock
            // against `pop`, which only ever takes `events`.
            self.log_overflow_rate_limited();
        }
        events.push_back(event);
        drop(events);
        self.notify.notify_one();
    }

    /// Pops the oldest queued event, or `None` if the queue is empty.
    fn pop(&self) -> Option<InputEvent> {
        lock_recovering(&self.events, "event_queue").pop_front()
    }

    fn log_overflow_rate_limited(&self) {
        let mut last = lock_recovering(&self.last_overflow_log, "event_queue_overflow_log");
        let now = Instant::now();
        let should_log = last
            .map(|previous| now.duration_since(previous) >= QUEUE_OVERFLOW_LOG_INTERVAL)
            .unwrap_or(true);
        if should_log {
            *last = Some(now);
            tracing::warn!(
                cap = MAX_QUEUED_EVENTS,
                "input event queue is full; dropping the oldest queued events"
            );
        }
    }
}

/// Captures keyboard and mouse input system wide via a `CGEventTap`.
///
/// The tap and its run loop live on a dedicated background thread; this
/// struct only holds a handle to the capped queue that thread feeds, the
/// flag that tells it whether to suppress what it sees, and a handle to
/// the cursor-parking state so `Drop` can always give the cursor back.
pub struct MacCapturer {
    events: Arc<EventQueue>,
    remote: Arc<AtomicBool>,
    park: Arc<CursorPark>,
    /// Where along the edge the last crossing happened, as an f32
    /// fraction stored in its bit pattern (an `AtomicU32` because the
    /// callback cannot take a lock cheaply and there is no atomic float).
    /// 0.0 is the left or top end of the edge, 1.0 the right or bottom.
    crossing_fraction: Arc<AtomicU32>,
    peer_connected: Arc<AtomicBool>,
}

impl MacCapturer {
    /// Starts the background capture thread and blocks until the tap is
    /// either up and enabled, or has failed to start. `edge` is the
    /// screen edge that hands focus to the peer, taken from the caller's
    /// `[layout]` configuration rather than assumed here. `panic_combo` is
    /// the panic hotkey's set of usages, or empty if none is configured;
    /// it is checked on every event, entirely inside the tap callback, so
    /// the escape hatch it provides (see `hotkey_matched`) works even when
    /// nothing is driving the connection loop that owns `poll`.
    pub fn start(edge: Edge, panic_combo: HashSet<Usage>) -> Result<Self, CaptureError> {
        let events = Arc::new(EventQueue::new());
        let events_for_thread = Arc::clone(&events);
        let (ready_tx, ready_rx) = mpsc::channel();
        let remote = Arc::new(AtomicBool::new(false));
        let remote_for_thread = Arc::clone(&remote);
        // Let the window server hide the cursor even though hop is not the
        // foreground app. Without this the hide silently does nothing.
        cursor::allow_background_cursor_hiding();
        let park = Arc::new(CursorPark::new());
        let park_for_thread = Arc::clone(&park);
        let crossing_fraction = Arc::new(AtomicU32::new(0));
        let crossing_fraction_for_thread = Arc::clone(&crossing_fraction);
        let peer_connected = Arc::new(AtomicBool::new(false));
        let peer_connected_for_thread = Arc::clone(&peer_connected);

        thread::Builder::new()
            .name("hop-capture-tap".into())
            .spawn(move || {
                run_capture_thread(
                    events_for_thread,
                    remote_for_thread,
                    park_for_thread,
                    crossing_fraction_for_thread,
                    peer_connected_for_thread,
                    edge,
                    panic_combo,
                    ready_tx,
                )
            })
            .map_err(CaptureError::ThreadSpawnFailed)?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events,
                remote,
                park,
                crossing_fraction,
                peer_connected,
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

    /// A handle the owner sets while a peer connection actually exists,
    /// and clears the moment it ends, by any path including an error.
    /// Edge detection in the tap callback only starts a crossing while
    /// this is `true` (see `should_begin_crossing`): before this flag
    /// existed, `MacCapturer::start` began watching for the edge the
    /// instant it returned, long before `run_server`'s listener even
    /// binds, so crossing the edge with no client connected suppressed
    /// this machine's own keyboard and mouse with nothing to hand them to
    /// and no way to get them back short of SSH or a forced power-off.
    /// This is CRITICAL 1 from the whole-branch review.
    /// Where along the edge the last crossing happened, 0.0 to 1.0. The
    /// server sends this to the peer so its cursor enters at the same
    /// relative point rather than resuming wherever it was left.
    pub fn last_crossing_fraction(&self) -> f32 {
        f32::from_bits(self.crossing_fraction.load(Ordering::Relaxed))
    }

    pub fn peer_connected_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.peer_connected)
    }

    /// A handle the owner can `await` (`notified().await`) to wake up the
    /// instant the tap callback queues a new event, instead of polling
    /// `poll()` on a fixed tick. See `EventQueue::notify`'s doc comment
    /// for why a single missed or coalesced wakeup is harmless: the
    /// caller is expected to drain the queue fully on every wakeup, not
    /// assume one wakeup means exactly one event.
    pub fn event_ready(&self) -> Arc<Notify> {
        Arc::clone(&self.events.notify)
    }
}

impl Capturer for MacCapturer {
    fn poll(&mut self) -> Option<InputEvent> {
        self.events.pop()
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

/// Everything the tap callback needs beyond the event itself: the queue
/// events are pushed into, shared flags, and the small pieces of mutable
/// state a single capture thread owns. Bundled into one struct, moved
/// whole into the callback closure, so `handle_event` takes a reasonable
/// number of arguments instead of nine separate ones.
struct CaptureContext {
    events: Arc<EventQueue>,
    remote: Arc<AtomicBool>,
    held_modifiers: Mutex<HashSet<i64>>,
    last_seen: Arc<Mutex<Instant>>,
    tap_port: Arc<Mutex<Option<usize>>>,
    /// The screen edge that hands focus to the peer.
    edge: Edge,
    /// The union of every active display's bounds; see
    /// `cursor::display_bounds`. IMPORTANT 1's fix from the whole-branch
    /// review: `crossed` and `nudge_inward` compare against this instead
    /// of the main display's bounds alone.
    bounds: cursor::Bounds,
    park: Arc<CursorPark>,
    /// Where along the edge the last crossing happened, as an f32
    /// fraction stored in its bit pattern (an `AtomicU32` because the
    /// callback cannot take a lock cheaply and there is no atomic float).
    /// 0.0 is the left or top end of the edge, 1.0 the right or bottom.
    crossing_fraction: Arc<AtomicU32>,
    /// Set only while a peer is actually connected; see
    /// `MacCapturer::peer_connected_flag` and `should_begin_crossing`.
    peer_connected: Arc<AtomicBool>,
    /// The panic hotkey's usages, or empty if none is configured. Checked
    /// against `held_usages` on every key event so the escape hatch this
    /// file provides (see `hotkey_matched`) never depends on `poll` or on
    /// anything outside this callback.
    panic_combo: HashSet<Usage>,
    /// Keys currently held, tracked purely for the panic-hotkey check
    /// above. Deliberately separate from `held_modifiers`, which tracks
    /// raw macOS device keycodes for the left/right modifier toggle, not
    /// canonical `Usage`s.
    held_usages: Mutex<HashSet<Usage>>,
    /// Fractional remainder carried across continuous (trackpad/Magic
    /// Mouse) scroll events, `(x, y)`. See `scale_continuous_scroll`,
    /// IMPORTANT 3's fix from the whole-branch review, for why this needs
    /// to persist between events rather than being recomputed from
    /// scratch each time.
    scroll_remainder: Mutex<(f64, f64)>,
}

/// Body of the dedicated capture thread: creates the tap, wires it into a
/// run loop on this thread, starts the watchdog, and then blocks forever
/// pumping that run loop. Reports success or failure back through
/// `ready_tx` once the tap is enabled (or definitely is not going to be).
#[allow(clippy::too_many_arguments)]
fn run_capture_thread(
    events: Arc<EventQueue>,
    remote: Arc<AtomicBool>,
    park: Arc<CursorPark>,
    crossing_fraction: Arc<AtomicU32>,
    peer_connected: Arc<AtomicBool>,
    edge: Edge,
    panic_combo: HashSet<Usage>,
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

    // Read once, up front, rather than on every event: display bounds do
    // not change often enough to justify recomputing them (a
    // `CGDisplay::active_displays()` call plus one `bounds()` per
    // display) on every mouse move, and a display being hot-plugged,
    // unplugged, or rearranged mid session is an accepted limitation here
    // (see Task 13 and `cursor::display_bounds`'s doc comment).
    let bounds = cursor::display_bounds();

    let ctx = CaptureContext {
        crossing_fraction,
        events,
        remote,
        held_modifiers: Mutex::new(HashSet::new()),
        last_seen: last_seen_for_callback,
        tap_port: tap_port_for_callback,
        edge,
        bounds,
        park,
        peer_connected,
        panic_combo,
        held_usages: Mutex::new(HashSet::new()),
        scroll_remainder: Mutex::new((0.0, 0.0)),
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
        // Middle button drags. Without this, holding the middle button
        // and moving while focus is on the peer leaks straight through to
        // this machine, because the tap only ever sees event types in
        // this mask.
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        // Not translated into input, but included so they are SUPPRESSED
        // while focus is remote. Anything left out of this mask reaches
        // the Mac regardless of focus.
        CGEventType::TabletPointer,
        CGEventType::TabletProximity,
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
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged => (
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
                    // instead. Point deltas are tens of units per event,
                    // a different scale entirely from the roughly
                    // one-per-notch line deltas the non-continuous branch
                    // below reports, so they are scaled down and
                    // accumulated across events by `scale_continuous_scroll`
                    // (IMPORTANT 3's fix from the whole-branch review)
                    // rather than passed straight through, which would
                    // send about thirty scroll notches for a single
                    // trackpad flick once the peer's injector multiplies
                    // by `WHEEL_DELTA`.
                    let raw_dx = event
                        .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2)
                        as i32;
                    let raw_dy = event
                        .get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1)
                        as i32;
                    let mut remainder = lock_recovering(&ctx.scroll_remainder, "scroll_remainder");
                    let (dx, remainder_x) = scale_continuous_scroll(raw_dx, remainder.0);
                    let (dy, remainder_y) = scale_continuous_scroll(raw_dy, remainder.1);
                    *remainder = (remainder_x, remainder_y);
                    (dx, dy)
                } else {
                    // A real wheel mouse already reports one unit per
                    // notch here; no scaling needed, matching the
                    // behavior before IMPORTANT 3's fix.
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

    if let Some(InputEvent::Key { usage, pressed }) = translated {
        // The unconditional escape hatch for CRITICAL 1: checked on every
        // key event, entirely inside this callback, before any
        // suppression decision below. This does not replace
        // `HotkeyWatcher` in hop's run.rs, which still runs the sanctioned
        // path (releasing the peer's held keys through `Control`) once
        // the connection loop's next poll tick notices the same key
        // event via the queue push just below; this is what makes the
        // local keyboard and mouse come back even when that loop, or
        // `poll`, is not currently running at all, for example because no
        // client has ever connected.
        let mut held = lock_recovering(&ctx.held_usages, "held_usages");
        if pressed {
            held.insert(usage);
        } else {
            held.remove(&usage);
        }
        if hotkey_matched(&ctx.panic_combo, &held) {
            ctx.remote.store(false, Ordering::Relaxed);
            ctx.park.restore();
        }
    }

    if let Some(input_event) = translated {
        // Only queued while focus is actually remote: this is IMPORTANT
        // 4's fix from the whole-branch review. `pump_server` in
        // hop-core only forwards these while `control.focus() ==
        // Focus::Remote` (and drops Key events on the floor via
        // `Control::on_key` while `Focus::Local`), so an event queued
        // while local would only ever be drained and discarded, never
        // acted on. Before this gate, every keystroke and every mouse
        // motion queued unconditionally, for as long as nobody drains
        // the queue at all, which is exactly what happens whenever no
        // client is connected: with the PC off overnight, that grows
        // without bound and holds the user's entire keystroke history in
        // process memory. Reading `ctx.remote` here, before the crossing
        // check below can flip it, also means the one motion event that
        // itself crosses the edge is not queued (its own local dx/dy
        // means nothing to the peer); only the `EdgeCrossed` signal
        // below is, which is what `pump_server` actually acts on to
        // start forwarding.
        if ctx.remote.load(Ordering::Relaxed) {
            ctx.events.push(input_event);
        }
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
            let edge_crossed = crossed(ctx.edge, location.x, location.y, ctx.bounds);
            if should_begin_crossing(ctx.peer_connected.load(Ordering::Relaxed), edge_crossed) {
                let landing = nudge_inward(ctx.edge, location.x, location.y, ctx.bounds);
                ctx.park.park(landing);
                // Set before the final suppression check below runs, so
                // the very event that crossed the edge is itself already
                // suppressed rather than leaking one more pixel of local
                // motion past the boundary.
                ctx.remote.store(true, Ordering::Relaxed);
                // Always queued, unlike the gated push above: this is the
                // one signal `pump_server` needs regardless of focus to
                // start forwarding at all (see `Control::on_edge_crossed`
                // in hop-core), and it only ever fires while a peer is
                // actually connected (`should_begin_crossing` requires
                // `peer_connected`), so it can never be the source of
                // unbounded growth IMPORTANT 4 was about.
                // Where along the edge the crossing happened, as a
                // fraction of that edge's length. The peer enters at the
                // same relative point so the motion looks continuous
                // rather than resuming wherever its pointer was left.
                let fraction = crossing_fraction(ctx.edge, location.x, location.y, ctx.bounds);
                ctx.events.push(InputEvent::EdgeCrossed);
                ctx.crossing_fraction
                    .store(fraction.to_bits(), Ordering::Relaxed);
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
    // the cursor is and the bounds of the virtual desktop (the union of
    // every active display; see `cursor::display_bounds`), has it
    // reached the configured edge. Everything else this task adds
    // (reading the real cursor, warping it, hiding it) needs hardware
    // and is out of reach for an automated test; this is the part that
    // actually is one.
    const SCREEN_W: f64 = 1920.0;
    const SCREEN_H: f64 = 1080.0;
    const SINGLE_DISPLAY: cursor::Bounds = cursor::Bounds {
        min_x: 0.0,
        min_y: 0.0,
        max_x: SCREEN_W,
        max_y: SCREEN_H,
    };

    #[test]
    fn top_edge_triggers_exactly_at_y_zero() {
        assert!(crossed(Edge::Top, 960.0, 0.0, SINGLE_DISPLAY));
    }

    #[test]
    fn top_edge_does_not_trigger_just_inside() {
        assert!(!crossed(Edge::Top, 960.0, 5.0, SINGLE_DISPLAY));
    }

    #[test]
    fn bottom_edge_triggers_at_the_screen_height_boundary() {
        assert!(crossed(Edge::Bottom, 960.0, SCREEN_H - 1.0, SINGLE_DISPLAY));
    }

    #[test]
    fn bottom_edge_does_not_trigger_just_inside() {
        assert!(!crossed(
            Edge::Bottom,
            960.0,
            SCREEN_H - 6.0,
            SINGLE_DISPLAY
        ));
    }

    #[test]
    fn left_edge_triggers_exactly_at_x_zero() {
        assert!(crossed(Edge::Left, 0.0, 540.0, SINGLE_DISPLAY));
    }

    #[test]
    fn left_edge_does_not_trigger_just_inside() {
        assert!(!crossed(Edge::Left, 5.0, 540.0, SINGLE_DISPLAY));
    }

    #[test]
    fn right_edge_triggers_at_the_screen_width_boundary() {
        assert!(crossed(Edge::Right, SCREEN_W - 1.0, 540.0, SINGLE_DISPLAY));
    }

    #[test]
    fn right_edge_does_not_trigger_just_inside() {
        assert!(!crossed(Edge::Right, SCREEN_W - 6.0, 540.0, SINGLE_DISPLAY));
    }

    #[test]
    fn only_the_top_edge_triggers_at_the_top_boundary() {
        // The deployment this project ships for: the PC's monitors sit
        // above the Mac, so `top` is the edge that actually matters, and
        // it must not be possible for a point on that boundary to also
        // read as having crossed any other edge.
        let (x, y) = (960.0, 0.0);
        assert!(crossed(Edge::Top, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Bottom, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Left, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Right, x, y, SINGLE_DISPLAY));
    }

    #[test]
    fn only_the_left_edge_triggers_at_the_left_boundary() {
        let (x, y) = (0.0, 540.0);
        assert!(crossed(Edge::Left, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Top, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Bottom, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Right, x, y, SINGLE_DISPLAY));
    }

    #[test]
    fn only_the_right_edge_triggers_at_the_right_boundary() {
        let (x, y) = (SCREEN_W - 1.0, 540.0);
        assert!(crossed(Edge::Right, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Top, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Bottom, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Left, x, y, SINGLE_DISPLAY));
    }

    #[test]
    fn only_the_bottom_edge_triggers_at_the_bottom_boundary() {
        let (x, y) = (960.0, SCREEN_H - 1.0);
        assert!(crossed(Edge::Bottom, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Top, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Left, x, y, SINGLE_DISPLAY));
        assert!(!crossed(Edge::Right, x, y, SINGLE_DISPLAY));
    }

    // IMPORTANT 1 from the whole-branch review: on a multi-display Mac,
    // `bounds` is the union of every active display (see
    // `cursor::display_bounds`), not just the main display's own bounds,
    // and a display positioned above or to the left of the main one
    // pushes `min_y`/`min_x` negative. These tests pin the fix: a
    // multi-display union whose main display still sits at `(0, 0)` in
    // the middle of the virtual desktop.
    const ABOVE_MAIN: cursor::Bounds = cursor::Bounds {
        // Main display 1920x1080 at (0, 0); a second, wider and taller
        // display centered above it at (-320, -1440).
        min_x: -320.0,
        min_y: -1440.0,
        max_x: 2240.0,
        max_y: 1080.0,
    };
    const BELOW_MAIN: cursor::Bounds = cursor::Bounds {
        // Main display 1920x1080 at (0, 0); a second 1920x1080 display
        // below and to the right, at (200, 1080).
        min_x: 0.0,
        min_y: 0.0,
        max_x: 2120.0,
        max_y: 2160.0,
    };

    #[test]
    fn top_edge_does_not_trigger_at_the_main_displays_own_top_when_a_display_sits_above_it() {
        // This is the exact failure IMPORTANT 1 describes: y = 0 is the
        // main display's own top edge, but with a second display above
        // it that point is mid-desktop, not the top of the virtual
        // desktop, and must not read as a crossing.
        assert!(!crossed(Edge::Top, 500.0, 0.0, ABOVE_MAIN));
    }

    #[test]
    fn top_edge_triggers_at_the_true_top_of_a_display_above_main() {
        assert!(crossed(Edge::Top, 500.0, ABOVE_MAIN.min_y, ABOVE_MAIN));
        assert!(!crossed(
            Edge::Top,
            500.0,
            ABOVE_MAIN.min_y + 5.0,
            ABOVE_MAIN
        ));
    }

    #[test]
    fn bottom_edge_does_not_trigger_at_the_main_displays_own_bottom_when_a_display_sits_below_it() {
        // Mirror of the top-edge case: 1079 is the main display's own
        // bottom edge, but with a display below it that point is well
        // inside the virtual desktop.
        assert!(!crossed(Edge::Bottom, 500.0, SCREEN_H - 1.0, BELOW_MAIN));
    }

    #[test]
    fn bottom_edge_triggers_at_the_true_bottom_of_a_display_below_main() {
        assert!(crossed(
            Edge::Bottom,
            500.0,
            BELOW_MAIN.max_y - 1.0,
            BELOW_MAIN
        ));
        assert!(!crossed(
            Edge::Bottom,
            500.0,
            BELOW_MAIN.max_y - 6.0,
            BELOW_MAIN
        ));
    }

    #[test]
    fn each_edge_triggers_at_and_only_at_its_own_boundary_with_negative_origins() {
        // A virtual desktop that extends into negative territory on both
        // axes at once, so this cannot pass by accident from an
        // implementation that only special-cases one negative origin.
        let bounds = cursor::Bounds {
            min_x: -500.0,
            min_y: -300.0,
            max_x: 1420.0,
            max_y: 780.0,
        };

        assert!(crossed(Edge::Top, 0.0, bounds.min_y, bounds));
        assert!(!crossed(Edge::Top, 0.0, bounds.min_y + 5.0, bounds));

        assert!(crossed(Edge::Bottom, 0.0, bounds.max_y - 1.0, bounds));
        assert!(!crossed(Edge::Bottom, 0.0, bounds.max_y - 6.0, bounds));

        assert!(crossed(Edge::Left, bounds.min_x, 0.0, bounds));
        assert!(!crossed(Edge::Left, bounds.min_x + 5.0, 0.0, bounds));

        assert!(crossed(Edge::Right, bounds.max_x - 1.0, 0.0, bounds));
        assert!(!crossed(Edge::Right, bounds.max_x - 6.0, 0.0, bounds));
    }

    // `nudge_inward` is what keeps a restored cursor from sitting exactly
    // on the boundary `crossed` treats as a crossing, which would bounce
    // focus straight back to the peer on the next reported motion. Pure,
    // so it gets the same direct coverage as `crossed`.
    #[test]
    fn nudge_inward_moves_away_from_each_edge_past_its_own_boundary() {
        let (_, y) = nudge_inward(Edge::Top, 960.0, 0.0, SINGLE_DISPLAY);
        assert!(!crossed(Edge::Top, 960.0, y, SINGLE_DISPLAY));

        let (_, y) = nudge_inward(Edge::Bottom, 960.0, SCREEN_H - 1.0, SINGLE_DISPLAY);
        assert!(!crossed(Edge::Bottom, 960.0, y, SINGLE_DISPLAY));

        let (x, _) = nudge_inward(Edge::Left, 0.0, 540.0, SINGLE_DISPLAY);
        assert!(!crossed(Edge::Left, x, 540.0, SINGLE_DISPLAY));

        let (x, _) = nudge_inward(Edge::Right, SCREEN_W - 1.0, 540.0, SINGLE_DISPLAY);
        assert!(!crossed(Edge::Right, x, 540.0, SINGLE_DISPLAY));
    }

    #[test]
    fn nudge_inward_moves_away_from_the_true_edge_on_a_multi_display_union() {
        // Same property as above, but against a bounds whose top edge is
        // not at y = 0, so this would fail if `nudge_inward` were still
        // implicitly assuming a zero origin.
        let (_, y) = nudge_inward(Edge::Top, 500.0, ABOVE_MAIN.min_y, ABOVE_MAIN);
        assert!(!crossed(Edge::Top, 500.0, y, ABOVE_MAIN));
    }

    #[test]
    fn nudge_inward_clamps_on_a_screen_smaller_than_the_margin() {
        // A screen thinner than `EDGE_MARGIN` must still yield an
        // in-bounds point rather than overshooting past the opposite
        // edge.
        let tiny = cursor::Bounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 3.0,
            max_y: 3.0,
        };
        let (x, _) = nudge_inward(Edge::Left, 0.0, 5.0, tiny);
        assert!((tiny.min_x..=tiny.max_x).contains(&x));

        let (_, y) = nudge_inward(Edge::Top, 5.0, 0.0, tiny);
        assert!((tiny.min_y..=tiny.max_y).contains(&y));
    }

    // `should_begin_crossing` is the pure decision behind CRITICAL 1: an
    // edge crossing with nobody connected must never suppress this
    // machine's own input.
    #[test]
    fn crossing_with_no_peer_connected_is_a_no_op() {
        assert!(!should_begin_crossing(false, true));
    }

    #[test]
    fn crossing_with_a_peer_connected_starts_a_crossing() {
        assert!(should_begin_crossing(true, true));
    }

    #[test]
    fn no_edge_crossing_never_starts_one_regardless_of_peer_state() {
        assert!(!should_begin_crossing(true, false));
        assert!(!should_begin_crossing(false, false));
    }

    // `hotkey_matched` is the pure decision behind CRITICAL 1's escape
    // hatch: it must clear `remote` the moment the configured combo is
    // fully held, and must never fire when no hotkey is configured.
    #[test]
    fn hotkey_matches_once_every_key_in_the_combo_is_held() {
        let combo: HashSet<Usage> = [Usage::LEFT_CTRL, Usage::LEFT_ALT, Usage::ESCAPE]
            .into_iter()
            .collect();
        let mut held = HashSet::new();
        held.insert(Usage::LEFT_CTRL);
        held.insert(Usage::LEFT_ALT);
        assert!(!hotkey_matched(&combo, &held), "combo not fully held yet");
        held.insert(Usage::ESCAPE);
        assert!(hotkey_matched(&combo, &held), "combo now fully held");
    }

    #[test]
    fn an_empty_combo_never_matches() {
        let combo: HashSet<Usage> = HashSet::new();
        let mut held = HashSet::new();
        held.insert(Usage::A);
        held.insert(Usage::LEFT_GUI);
        assert!(!hotkey_matched(&combo, &held));
    }

    #[test]
    fn extra_held_keys_beyond_the_combo_still_match() {
        // The combo only has to be a subset of what is held, not exactly
        // equal to it: pressing an extra key alongside the combo must not
        // block the escape hatch.
        let combo: HashSet<Usage> = [Usage::LEFT_CTRL, Usage::LEFT_ALT].into_iter().collect();
        let mut held = HashSet::new();
        held.insert(Usage::LEFT_CTRL);
        held.insert(Usage::LEFT_ALT);
        held.insert(Usage::A);
        assert!(hotkey_matched(&combo, &held));
    }

    // `scale_continuous_scroll` is the pure decision behind IMPORTANT 3's
    // fix: given a continuous scroll event's raw point delta and the
    // remainder left over from the previous event, how many whole
    // line-delta units should be sent, and what remainder carries
    // forward.
    #[test]
    fn a_single_flick_is_scaled_down_to_a_few_notches_not_thousands_of_units() {
        // The exact scenario IMPORTANT 3 describes: a 30 point flick.
        // Before this fix, that became `30 * WHEEL_DELTA` (3600) wheel
        // units, thirty notches, in one event; scaled down it becomes a
        // small, plausible number of line-delta units instead.
        let (units, remainder) = scale_continuous_scroll(30, 0.0);
        assert_eq!(units, 3);
        assert_eq!(remainder, 0.0);
    }

    #[test]
    fn slow_scrolling_accumulates_across_events_instead_of_being_discarded() {
        // Individual sub-threshold deltas would truncate to zero every
        // time under naive integer division; carrying the remainder
        // forward means they still add up to a whole unit eventually.
        // 5 points per event divides `CONTINUOUS_SCROLL_POINTS_PER_UNIT`
        // exactly (0.5), so the running total is exactly representable
        // in binary floating point at every step and this is not at the
        // mercy of rounding, unlike a value such as 3 points (0.3 per
        // event) would be.
        let mut remainder = 0.0;
        let mut total_units = 0;
        for _ in 0..6 {
            let (units, new_remainder) = scale_continuous_scroll(5, remainder);
            total_units += units;
            remainder = new_remainder;
        }
        // 6 events of 5 points each is 30 points, the same total as the
        // single-flick case above, and must add up to the same 3 units
        // rather than losing everything to per-event truncation (which
        // would yield 0, since 5 / 10.0 truncates to 0 every time).
        assert_eq!(total_units, 3);
        assert_eq!(remainder, 0.0);
    }

    #[test]
    fn negative_deltas_scale_and_accumulate_the_same_way() {
        let (units, remainder) = scale_continuous_scroll(-30, 0.0);
        assert_eq!(units, -3);
        assert_eq!(remainder, 0.0);

        let mut remainder = 0.0;
        let mut total_units = 0;
        for _ in 0..6 {
            let (units, new_remainder) = scale_continuous_scroll(-5, remainder);
            total_units += units;
            remainder = new_remainder;
        }
        assert_eq!(total_units, -3);
    }

    // A real wheel mouse's non-continuous `ScrollWheel` branch in
    // `handle_event` never calls `scale_continuous_scroll` at all;
    // `translate` passes its line-delta dx/dy straight through unscaled,
    // which `translates_scroll` above already covers, so that behavior
    // has no separate test here.

    // `EventQueue` is IMPORTANT 4's second line of defence: a cap on how
    // many events can pile up, with the oldest dropped first once it is
    // reached. Pure in-memory state, no macOS calls, so unlike
    // `CursorPark` below it is directly testable.
    #[test]
    fn event_queue_pops_in_fifo_order() {
        let queue = EventQueue::new();
        queue.push(InputEvent::Mouse { dx: 1, dy: 0 });
        queue.push(InputEvent::Mouse { dx: 2, dy: 0 });
        queue.push(InputEvent::Mouse { dx: 3, dy: 0 });
        assert_eq!(queue.pop(), Some(InputEvent::Mouse { dx: 1, dy: 0 }));
        assert_eq!(queue.pop(), Some(InputEvent::Mouse { dx: 2, dy: 0 }));
        assert_eq!(queue.pop(), Some(InputEvent::Mouse { dx: 3, dy: 0 }));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn event_queue_drops_the_oldest_event_once_the_cap_is_reached() {
        let queue = EventQueue::new();
        for i in 0..MAX_QUEUED_EVENTS {
            queue.push(InputEvent::Mouse {
                dx: i as i32,
                dy: 0,
            });
        }
        // One more push past the cap must evict the oldest (dx: 0), not
        // grow the queue past `MAX_QUEUED_EVENTS`, and not silently drop
        // the newest instead.
        queue.push(InputEvent::Mouse {
            dx: MAX_QUEUED_EVENTS as i32,
            dy: 0,
        });
        assert_eq!(queue.pop(), Some(InputEvent::Mouse { dx: 1, dy: 0 }));
        let mut remaining = 1;
        while queue.pop().is_some() {
            remaining += 1;
        }
        assert_eq!(remaining, MAX_QUEUED_EVENTS);
    }

    // `CursorPark::park`/`hold`/`restore` are deliberately not exercised
    // here: every path through them ends in a real `cursor::hide_cursor`,
    // `warp_cursor`, `show_cursor`, `cursor::enter_parked_state`, or
    // `cursor::leave_parked_state` call, and this workspace's tests run
    // on real macOS hosts, so calling them from a unit test would
    // actually hide, warp, and disassociate the developer's cursor as a
    // side effect of `cargo test`. That is exactly the kind of
    // hardware-dependent behavior this task's brief calls out as only
    // verifiable by a human, in Task 13; `crossed`, `nudge_inward`,
    // `scale_continuous_scroll`, and `EventQueue` above are the parts of
    // this file that are actually pure.
}
