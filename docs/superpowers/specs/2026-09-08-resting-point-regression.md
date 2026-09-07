# Resting point regression: diagnosis and fix design

Date: 2026-09-08
Status: proposed, not implemented
Branch read: `feat/hop-platform` at `e23d818`
Companion: `2026-09-08-edge-crossing-design.md` (the design `f6b791e`
implemented, which this regression sits on top of)

## Summary

`e23d818` added one `CGWarpMouseCursorPosition` call to the Mac's
crossing path: after the cursor crosses the top edge, `CursorPark::park`
warps it from the crossing point to the centre of the primary display so
nothing on the menu bar is left hovered. That warp happens while the
event tap is live and about to forward every motion event to the PC.
macOS folds a warp's displacement into the delta fields of the next
motion event it delivers, and those delta fields are exactly what hop
puts on the wire as `MouseMove { dx, dy }`. The first motion the PC
receives after `Enter` is therefore not the hand's motion but the vector
from the crossing point to the Mac's screen centre, `(735 - x, 478)` on
the reference Mac, and that vector added to the correct landing point is,
to the pixel, the centre of the PC's bottom edge.

Everything the user saw follows from that one forwarded delta, including
the detail that the cursor rises from the taskbar row itself rather than
from 12 pixels above it. Section 2 walks the sequence event by event.

The fix is an invariant, not a tweak: **the machine that is forwarding
deltas must never warp its own cursor while it is forwarding**. The
Mac-side rest warp is removed (the PC-side rest, which is injected on the
receiving machine and never enters the forwarded stream, stays). Section
5 also gives the model that would let the Mac rest its cursor away from
the edge without polluting the stream, if a Mac-side hover latch is ever
actually observed, and section 6 gives the tests that pin the invariant
so a warp cannot be reintroduced on the forwarding path without a test
going red.

## 1. The report, restated as numbers

User's words: "when my cursor goes from mac to windows, it only gets up
from the middle of taskbar".

Reference geometry (the `hop-core` test fixtures, `screen.rs` lines 450
to 463, which match the real desk): Mac one display `[0, 1470) x
[0, 956)`, anchor on the top edge `735`, resting point `(735, 478)`. PC
two `1920 x 1080` monitors side by side, primary on the left, anchor on
the bottom edge `960`, `ENTRY_MARGIN` 12, so a crossing from Mac `x`
should land at `(x - 735 + 960, 1067)` on the primary monitor.

Observed: every crossing ends with the cursor at `x = 960`, on the
taskbar row, whatever `x` the hand left through. `960` is what `landing`
produces for `along = 0`, and `along = 0` is a crossing at the Mac's own
anchor, `x = 735`, which is also the `x` of the new resting point. That
coincidence is real, but it is two steps removed from the cause; the
cause is a forwarded delta, not a corrupted `along`.

## 2. Root cause

### 2.1 The mechanism

`crates/hop-platform/src/macos/capture.rs`, `CursorPark::park`, lines
207 to 219 (added by `e23d818`):

```rust
cursor::hide_cursor();
if let Some((x, y)) = rest {
    cursor::warp_cursor(x, y);        // line 215: NEW in e23d818
}
cursor::enter_parked_state();         // line 217: associate(false)
```

`cursor::warp_cursor` (`cursor.rs` lines 165 to 171) is
`CGWarpMouseCursorPosition`. Its doc comment says it moves the cursor
"without generating a motion event". That is true and is not the whole
story. Quartz does not emit an event for the warp, but the next motion
event it does emit carries the warp's displacement in its
`kCGMouseEventDeltaX/Y` fields, because those deltas are derived from
the change in cursor position since the previous event rather than read
raw from the device. This is well known enough that both mainstream
cross-platform input layers carry code for it:

- SDL, `src/video/cocoa/SDL_cocoamouse.m`, `Cocoa_HandleMouseWarp`:
  "This makes Cocoa_HandleMouseEvent ignore the delta caused by the
  warp, since it gets included in the next movement event." The
  compensation is applied in relative mode, which is the disassociated
  state hop is in, so disassociating first would not have helped.
- GLFW, `src/cocoa_window.m`, `cursorWarpDeltaX/Y`, subtracted from
  `[event deltaX/Y]` in `mouseMoved:` for the same reason.

hop reads exactly those fields. `handle_event`, lines 753 to 754:

```rust
event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as i32,
event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as i32,
```

and forwards them, gated only on `ctx.remote`, at lines 854 to 856. The
crossing event itself is not forwarded (the gate is read before
`remote` is set at line 907, as the comment at 848 to 853 explains). The
event after it is forwarded, and it is the one carrying the warp.

So for a crossing at Mac `(x, 0)`, the wire carries, in order:

1. `Enter { along: x - 735 }` (correct: `location` is read at line 885,
   before `park` is called at line 902 and issues the warp at line 215,
   and `along` at line 928 is derived from that same `location`).
2. `MouseMove { dx: (735 - x) + hdx, dy: 478 + hdy }` where `(hdx, hdy)`
   is the hand's real motion in that sample.

### 2.2 What the PC does with it

`crates/hop-platform/src/windows/inject.rs`:

- `Enter` arm, lines 510 to 534: `landing(Bottom, along + 960, 12)` gives
  `(x - 735 + 960, 1067)`. Correct.
- `Mouse` arm, lines 469 to 509: reads the cursor back (the landing),
  adds the scaled delta, and `clamp_to_monitors` (line 501 to 503). With
  `mouse_scale = 1.0` (the default, `config.rs` lines 174 and 424, and
  the README's suggested value):

  ```
  x: (x - 735 + 960) + (735 - x) = 960
  y: 1067 + 478 = 1545  -> clamped to monitor 0's last row, 1079
  ```

  The `x` terms cancel exactly. Whatever `x` the hand crossed at, the
  cursor is now at `(960, 1079)`: the middle of the taskbar row, not 12
  pixels above it. With `mouse_scale = s < 1` the cancellation is
  partial, `960 + (1 - s)(x - 735)`, which still reads as "the middle"
  for any realistic `s`, and the `y` still clamps to 1079 for any
  `s > 12 / 478`.

- `return_crossing`, lines 552 to 584, is asked after every injected
  motion (`supervisor.rs` line 198). `at_outer_edge(Bottom, 960, 1079)`
  is `Some(960)`; `along = 960 - 960 = 0`; the 500 ms
  `RELEASE_SUPPRESS_WINDOW` starts (line 561); the PC's own rest move to
  `(960, 540)` is injected (lines 576 to 582); `Release { along: 0 }` is
  sent (`supervisor.rs` line 203).

### 2.3 The bounce that puts the cursor "in the middle of the taskbar"

`crates/hop/src/run.rs` lines 611 to 631: on `Release { 0 }` the Mac
calls `return_focus(Some(0))`, which is `restore_at(landing(Top, 735,
12)) = (735, 12)`: warp, re-associate, show (`capture.rs` 249 to 261).
The Mac cursor reappears at the centre of the Mac's top edge, not under
where the hand left, and focus is local. All of this has taken one
network round trip plus one 15 ms poll tick; the hand is still moving
upward. Within one or two motion samples the cursor is at `(735 + hdx,
0)`, `at_outer_edge(Top)` fires, `along = hdx`, and the whole thing runs
again:

1. `Enter { along: hdx }`: PC lands at `(960 + hdx, 1067)`.
2. `park` warps `(735 + hdx, 0) -> (735, 478)`; the next motion event
   carries `(-hdx, 478)`.
3. PC: `(960 + hdx - hdx, 1067 + 478)` clamps to `(960, 1079)`.
4. `return_crossing`: the cursor is on the edge again, but
   `suppress_release_until` from step 2.2 has not expired, so it returns
   `None` (lines 554 to 559). No release. The cursor is left on the
   taskbar row at `x = 960`.
5. The hand keeps moving up, and the PC cursor rises out of the middle
   of the taskbar.

That is the report, word for word, including "taskbar" (row 1079, not
1067) and "middle" (960 regardless of `x`). The Mac cursor is visible
for a few tens of milliseconds at `(735, 12)` between the two crossings,
which is short enough to read as a flicker or to miss entirely, and the
PC cursor appears at the right `x` for one frame before it is dragged to
the centre.

### 2.4 Why `f6b791e` was immune

`f6b791e`'s `park` was hide + disassociate with no warp. Its only warp
was in `restore_at`, on the way back to local. That warp pollutes the
next motion event's delta exactly the same way, but at that point focus
is local: the tap does not forward deltas (line 854), and even a delta
that slipped through the flag race would be dropped by `pump_server`'s
own `Focus::Remote` gate (`session.rs` line 115). Edge detection uses
`event.location()`, not deltas, and the location is unaffected. So the
design was immune by construction on the return path and had never had
a warp on the departure path until `e23d818` put one there. "Works so
good now" was true, and the same code that made it true is still there;
the new warp is upstream of it.

## 3. Alternatives considered and ruled out

The task listed four candidates. Each is answered from the code.

### 3.1 Does `along` get computed from a warped location?

No, on either crossing. `handle_event` reads `event.location()` at line
885 and derives `crossing`, `along`, and `landing` from it (890 to 901)
before `park` runs at 902. `CGEvent::location` is a field of the event
object already handed to the callback; a later warp cannot change it.
`along` is pushed at 927 to 929 from the same local. On the second
crossing of the bounce (2.3) `along` is again exactly where the cursor
was, which really is near the anchor because the `Release` before it
carried `0`. The location is honest; it is the delta stream that lies.

Across a park/restore/park cycle: `origin` goes `None -> Some(landing)
-> None -> Some(landing)`, `hide`/`show` and `associate(false)`/
`associate(true)` pair up once per cycle. No state leaks between cycles.

### 3.2 Has the resting point warp reintroduced the re-association re-sync?

No. The re-sync bug `hold()`'s doc comment describes (lines 221 to 232)
was caused by warping on every motion event while disassociated and was
fixed by warping once, before `associate(true)`, in `restore_at`. That
path is unchanged in `e23d818`: warp to the landing at line 257, then
`leave_parked_state` at 258. The only new warp is before disassociation,
and after it `restore_at` still sets the cursor position immediately
before re-associating, so the position Quartz re-syncs to is the landing.

There is also direct evidence against a re-sync. If re-association moved
the cursor anywhere other than the landing, one of two things would be
seen on every PC to Mac return: the Mac cursor appearing mid-screen, or,
if the restore warp's displacement were applied to the cursor position
as well as the delta, the cursor jumping to `(2x' - 735, y < 0)`,
clamping to the top edge and immediately crossing back to the PC. The
user reported neither. PC to Mac returns were not mentioned as broken.

`CGWarpMouseCursorPosition` while disassociated does update the position
Quartz re-syncs to on re-association; that is what makes `restore_at`'s
"warp first" ordering work, and it has worked since `fa01cc8`.

### 3.3 Is the Windows rest move arriving late or fighting `RELEASE_SUPPRESS_WINDOW`?

No, and it cannot corrupt an `Enter`. The order inside
`WindowsInjector::return_crossing` is: compute `along` from the real
cursor position (line 560), start the suppress window (561), inject the
rest move synchronously (576 to 582), return `along`. `apply_message`
then sends `Release` (`supervisor.rs` 198 to 203). The rest move is
complete before the `Release` leaves the machine. In-flight `MouseMove`s
that arrive afterwards move the cursor a few pixels around `(960, 540)`
and every one of them is answered `None` by the suppress window. The next
`Enter` is an absolute move to `landing(...)` (lines 526 to 533) that
never reads the current cursor position. The rest move's only effect on
the story is in 2.3 step 4, where the suppress window it started is what
stops the second polluted delta from bouncing focus a second time.

`return_crossing` is only ever asked after a motion event that hop itself
injected (`supervisor.rs` lines 190 to 205), so the PC's own physical
mouse touching the taskbar never triggers the rest move or a stray
`Release`. That was worth checking; it is not a bug.

### 3.4 Is the Windows `Enter` handler correct with `refresh_screen()` on every crossing?

Yes. `refresh_screen` re-reads the monitor list; `anchor` and `landing`
are recomputed from it. Both are unchanged since `f6b791e`, which the
user confirmed working. The failure mode a bad enumeration would produce
is also the wrong shape: `Screen::single(virtual screen)` puts the anchor
at the union centre, `1920`, so a centre crossing would land on the seam
between the two monitors, not the middle of the primary's taskbar.

## 4. The two properties conflict in the current design

Property A: leaving a machine must not leave a hover latched on it.
Property B: the arrival position must track where the hand crossed.

On the receiving machine there is no conflict: the PC's rest move is an
injected absolute move on the PC's own cursor, nothing about the PC's
cursor is ever forwarded, and the `Release` position is read before the
rest move. `e23d818`'s PC half is correct and should stay.

On the sending machine they conflict as long as "rest the cursor" means
"warp the cursor". Every motion event while remote is forwarded as a
delta; the OS puts a warp's displacement into the next delta; therefore
any warp while remote is forwarded as motion. Disassociating first does
not change this (SDL compensates in relative mode precisely because it
does not). Hiding first does not change it (visibility is not position).
Warping to the landing point instead of the centre would forward a
smaller lie, `(0, 12)`, which would still drag the arrival 12 pixels
toward the PC's edge and, with `ENTRY_MARGIN` also 12, straight onto it.

The model that satisfies both is in 5.2. Whether the Mac needs it at all
is a separate question, answered first.

## 5. Fix

### 5.1 Stage 1: remove the Mac-side rest warp, keep the PC-side rest

Change `CursorPark::park` back to hide + `enter_parked_state`, with no
warp, and drop the `rest` parameter. `Screen::resting_point` stays, used
by the Windows injector only. `handle_event` line 902 goes back to
`ctx.park.park(landing)`.

This restores the exact departure-path behaviour of `f6b791e`, which
the user confirmed correct, and keeps the taskbar fix, which is entirely
on the PC side. It is the only change in this document with no
hardware assumption behind it.

Property A on the Mac after this change: the hidden, disassociated cursor
sits at the crossing point on the top edge, `(x, 0)`, which is the menu
bar. The macOS menu bar does not react to hover: menu titles highlight
only while a menu is open, status items do not open on hover, and the
Notch area has no hover behaviour. Hot corners fire on arrival, not
continuously, and only at the two top corners, and a hand crossing at
the exact corner would have fired them under `f6b791e` too, where nobody
reported it. Third-party menu bar utilities with hover popovers are the
one plausible exception; none has been reported. The commit message's
"a menu bar item stays highlighted on the Mac" was inferred by symmetry
with the taskbar, not observed. This is an argument, not a test, and it
is stated as one; if a Mac-side latch is ever seen, 5.2 is the answer.

### 5.2 Stage 2, only if a Mac latch is observed: rest without polluting the stream

The invariant "no warp while forwarding" can be kept while still moving
the cursor by making the forwarded stream know about the warp. The
model, pure and in `hop-core`:

```rust
/// Motion the tap must not forward because the OS is about to report a
/// cursor warp as if the hand had made it.
pub struct WarpDebt { pending: bool }

impl WarpDebt {
    pub fn note_warp(&mut self)            // called when the platform warps while remote
    pub fn absorb(&mut self) -> bool       // true: drop this motion event's deltas, clear the debt
}
```

`park` becomes: hide, `set_local_events_suppression_interval(0.0)`,
`associate(false)`, warp to rest, `debt.note_warp()`. In `handle_event`
the forwarded push at line 855 becomes: if the event is motion and
`debt.absorb()`, do not push it; otherwise push as now. One motion
sample of the hand's real movement is lost per crossing, which is
imperceptible at 125 Hz or more and is the same thing SDL does.

Why swallow rather than subtract the known displacement: subtracting is
exact only if Quartz reports the displacement exactly once, in full, on
exactly the next event. If it does not (it is split, or absent on this
macOS), subtraction injects the negated warp as a fresh lie and the
cursor jumps toward the PC's top instead. Swallowing is correct under
every one of those behaviours: if the displacement is there, it is
dropped; if it is not, one real sample is dropped. The only case it
does not cover is the displacement arriving on the second or later
motion event, which no implementation in the wild has needed to handle.

Ordering matters inside `park` for one more reason: the local events
suppression interval must be zero before the warp, not after (see 7.1).

This stage is specified so it can be built test-first, but it should not
be built until a Mac-side latch has actually been demonstrated. The last
three regressions on this branch came from changing the departure path
on a hypothesis.

### 5.3 What must not change

- `restore_at`'s order (warp, `associate(true)`, show) and the fact that
  it runs from `run.rs` on the `Release` before `remote_flag` is cleared.
- The PC rest move's position in `return_crossing`: after `along` is
  read, before `Release` is sent.
- `RELEASE_SUPPRESS_WINDOW` stays as a backstop. It is what confined the
  regression to one bounce instead of an oscillation.

## 6. Tests

### 6.1 Unit tests that run on this Mac in `cargo test --workspace`

The pollution is an OS behaviour, so no pure test can observe it. What
pure tests can do is pin the invariant that makes it impossible, and
pin the arithmetic that turns the report into numbers so the next person
does not have to rediscover 2.2.

1. **The invariant, in `hop-platform` (macOS).** Make `CursorPark`
   generic over a small trait, `CursorOps { hide, show, warp,
   associate, set_suppression_interval }`, with the real implementation
   in `cursor.rs` and a recording fake in tests. Then:
   - `parking_hides_and_disassociates_and_never_warps`: `park(landing)`
     records exactly `[hide, associate(false)]` and no `warp`. **Fails
     against `e23d818`** (it records a `warp`), passes after 5.1. This is
     the test that would have caught the regression, had the invariant
     been known; it is now known.
   - `restoring_warps_before_reassociating`: `restore_at(Some(p))`
     records `[warp(p), associate(true), show]` in that order. Pins the
     `fa01cc8` fix so the re-sync bug cannot come back either.
   - `restoring_without_a_position_uses_the_departure_point`: pins the
     fallback for disconnect, wake, panic hotkey.
   - `a_second_park_before_restore_is_a_no_op`: no double hide.

2. **The arithmetic, in `hop-core` `screen.rs`.**
   `a_warp_displacement_forwarded_as_motion_lands_on_the_return_edge_centre`:
   for several Mac `x`, compute the PC landing for `u = x - 735`, add
   the delta `(735 - x, 478)`, `clamp_to_monitors`, and assert the result
   is `(960, 1079)` and `at_outer_edge(Bottom, ..)` is `Some(960)`. This
   passes today and always will; it is an executable statement of 2.2,
   and it names the number the user saw. Keep it next to
   `resting_point` so anyone touching that function reads it.

3. **If 5.2 is built, in `hop-core`.** `WarpDebt`: no debt drops
   nothing; one `note_warp` drops exactly the next `absorb` and not the
   one after; two `note_warp`s before any motion still drop exactly one;
   `absorb` on a fresh debt returns `false`. Plus a `pump_server` test
   with a `FakeCapturer` yielding `[EdgeCrossed { along: -435 }, Mouse
   { dx: 435, dy: 478 }, Mouse { dx: 3, dy: -2 }]` where the capturer
   applies the debt: the wire carries `Enter { -435 }` then `MouseMove {
   3, -2 }` and nothing else.

4. **Windows wiring, `#[cfg(all(test, target_os = "windows"))]`, run
   under `cargo check`/`clippy --target x86_64-pc-windows-msvc` here and
   for real in CI.** `return_crossing` currently calls `send_inputs`
   directly, so it cannot be tested without a desktop. Route the rest
   move through the same `CursorSource`-style seam and pin: `along` is
   computed from the position before the rest move, and the rest move is
   sent exactly once per release, not on suppressed calls.

### 6.2 What only Talha can check, on the real desk

Each is one observable thing, in the order that isolates causes.

1. **Confirm the mechanism before changing anything.** Run `e23d818`
   with `RUST_LOG=hop_core=debug` (or add a one-line `tracing::debug!`
   in `pump_server`'s `other` arm for the first `MouseMove` after an
   `Enter`) and cross at the far left of the Mac's top edge. Expect the
   first `MouseMove` after `Enter` to be roughly `(700, 478)`, not a few
   pixels. If it is a few pixels, this document is wrong and the
   pollution is somewhere else; stop and say so.
2. **After 5.1.** Cross at the left third of the Mac's top edge. The PC
   cursor should appear under the left third, 12 pixels above the
   taskbar, and keep rising with the hand. No flicker on the Mac. Repeat
   at the right third and the centre.
3. **The taskbar fix still holds.** Hover a taskbar button on the PC
   until its preview opens, move down through the taskbar to return to
   the Mac. The preview should close. The PC cursor should be at the
   centre of the primary monitor when you look up.
4. **The Mac side has no latch to fix.** While focus is on the PC, look
   at the Mac's menu bar: nothing should be highlighted or open. If
   something is (a third-party menu bar item, for example), that is the
   evidence 5.2 needs; note what it was.
5. **Return still tracks the hand.** From the PC, move down through the
   bottom of the primary monitor at its left third; the Mac cursor
   should appear at the Mac's left third, 12 points below the top.
6. **Panic hotkey** still works from anywhere.

Only 1 and 4 are new; 2, 3, 5 and 6 are the edge-crossing design's
section 7.2 steps 1, 3 and 8 plus the `e23d818` acceptance check, and
should be run every time the departure path changes. Step 1 is the one
that distinguishes this diagnosis from every alternative in section 3;
none of the others predict a large first delta.

## 7. Other bugs the same reasoning exposes

### 7.1 The first crossing of a process warps under a live suppression interval

`enter_parked_state` (`cursor.rs` 275 to 278) sets the local events
suppression interval to zero after `associate(false)`, and `park` warps
before calling it. At process start the interval is Quartz's 0.25 s
default, so the first-ever crossing's warp is followed by up to 250 ms
in which Quartz filters local mouse events. Whether that filtering
happens upstream of an HID-level tap is not documented; if it does, the
PC receives no motion for a quarter second on the first crossing of every
hop run. Every later cycle is fine because `leave_parked_state` leaves
the interval at zero. 5.1 removes the warp and the problem with it; 5.2
orders the interval before the warp. Either way the interval should be
zeroed once in `MacCapturer::start`, next to
`allow_background_cursor_hiding`, so no future warp can meet the default.

### 7.2 `warp_cursor`'s doc comment is misleading

"Moves the cursor to `(x, y)` without generating a motion event, so the
warp itself is never mistaken for user input by this project's own event
tap." The first half is true. The second half is what this regression
disproves: the warp is mistaken for user input, one event later, in the
delta fields. The comment should say so and point at the invariant in
5.1, because it is the comment that made the `e23d818` change look safe.

### 7.3 The Windows release check re-enumerates monitors on every motion event

`return_edge::return_crossing` (line 98) calls `source.screen()`, which
is `read_screen()`: `EnumDisplayMonitors` plus `GetMonitorInfoW` per
monitor, once per injected motion event, while `WindowsInjector.screen`
exists specifically as the cache for this (`inject.rs` 374 to 380).
`resting_point()` at line 576 then reads the cache, so `along` and the
rest move can be computed from two different monitor lists during a
display change. Not the regression, and not urgent (the enumeration is
tens of microseconds), but the cache should be the single source and
`return_crossing` should take a `&Screen`.

### 7.4 `Screen::resting_point` is documented as if it applied to both machines

Its doc comment (`screen.rs` 355 to 370) describes both the taskbar and
the menu bar. After 5.1 it is used by the PC only. The comment should say
that the sending machine cannot use it (section 4 of this document), or
the same change will be made again by someone reading only the comment.

## 8. Why no test could fail, and what changes that

`e23d818` changed the departure path in the one file the project calls
"the single most important file" and touched only hardware-facing calls
(`hide`, `warp`, `associate`), which have no seam for a fake, so it
shipped with tests for `resting_point`'s arithmetic and none for what the
tap does. Three regressions in a row on this path have the same shape:
a platform call was added or reordered, the pure geometry was tested,
the sequence of platform calls was not.

The `CursorOps` seam in 6.1 item 1 is the structural answer. Once
`CursorPark` records its calls against a fake, "what does parking do to
the cursor" is a unit test on this Mac, and the invariant from section 4
is a one-line assertion: no `warp` in `park`. That test fails on
`e23d818` today. It should be written first, watched fail, and then 5.1
applied.
