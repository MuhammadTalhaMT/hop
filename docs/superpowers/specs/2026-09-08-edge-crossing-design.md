# Edge crossing: design

Date: 2026-09-08
Status: proposed, not implemented
Branch read: `feat/hop-platform` at `fa01cc8`

## Summary

The cursor should cross between the Mac and the PC the way it already
crosses between two monitors on one machine: at one-to-one scale, onto
the screen that is physically adjacent, at the point it was heading for.
Neither machine should be modelled as one big rectangle. Each machine is
a set of monitor rectangles the OS already knows about; the only thing
the OS does not know, and the only thing hop adds, is which edge of one
machine touches which edge of the other, plus one number saying where
along that edge they line up.

Concretely:

- Crossing is detected per monitor, on the outward-facing part of the
  configured edge, never against the union of all monitors.
- The position along the edge travels on the wire in logical units
  relative to an anchor point, not as a fraction of a union width.
- The receiving machine computes the landing point on its own monitors,
  from its own monitor list, and nudges it inward so it cannot bounce.
- The PC's entry edge is its return edge. It is not the opposite edge.
- The return carries a position too, so the Mac cursor reappears where
  the hand was heading, not where it left minutes earlier.
- One optional config value, `anchor`, on the client, for desks where
  the laptop does not sit under the middle of the primary monitor.

The rest of this document is the evidence, the model, and enough
precision to implement it.

## 1. What is actually wrong

All line numbers are as of `fa01cc8`.

### 1.1 The biggest bug: the PC puts the cursor on the wrong edge

`crates/hop-platform/src/windows/inject.rs`, `entry_point`, lines 106
to 122:

```rust
match return_edge {
    // Leaves through the bottom, so arrives at the top.
    Some(ReturnEdge::Bottom) | None => (along_x, top),
    Some(ReturnEdge::Top) => (along_x, top + (height - 1).max(0)),
    Some(ReturnEdge::Right) => (left, along_y),
    Some(ReturnEdge::Left) => (left + (width - 1).max(0), along_y),
}
```

The comment applies the "opposite edge" relation to the wrong machine.
Leaving the Mac's top arrives at the PC's bottom. Leaving the PC's
bottom arrives at the Mac's top. On any one machine, the edge you enter
through and the edge you leave through are the same edge, because the
peer is on one side of you. The user's client config is
`return_edge = "bottom"` (the only valid pairing with the server's
`top = "pc"`, per `config.rs` lines 49 to 57), so every crossing puts
the cursor on the TOP row of the Windows virtual screen, `top` being
`SM_YVIRTUALSCREEN`. The hand is still moving upward at that moment, so
the cursor pins against the top of the monitors, and to get home the
user must drag it the full height of the desktop. That is "it's really
buggy", and it is "snapping on top" in the most literal sense.

There is no unit test for `entry_point`. The `tests` module in
`inject.rs` covers `mouse_move_absolute_input`, `scale_delta`, keys,
scroll, and buttons, and never calls `entry_point`, which is how an
inverted edge shipped.

### 1.2 The bug you diagnosed: the fraction spans the whole virtual screen

Same function, lines 111 to 114:

```rust
let (left, top, width, height) = screen;
let along_x = left + ((width - 1).max(0) as f64 * f).round() as i32;
```

`screen` is `WindowsCursorSource::virtual_screen()` (lines 240 to 252),
the bounding rectangle of both monitors from `GetSystemMetrics`. A
crossing halfway along the Mac's top edge lands halfway across the
combined width of both PC monitors, which is the seam. Anything to the
right of the Mac's centre lands on the second monitor. That is why the
wrong-edge landing is specifically "on my second monitor". The Mac side
does the mirror of this in `capture.rs` `crossing_fraction`, lines 162
to 172, dividing by the width of the union from
`cursor::display_bounds()`.

Beyond landing on the wrong monitor, this stretches motion: with a
1470 point Mac edge and a 3840 pixel PC edge, horizontal position is
multiplied by 2.6 on the way over and divided by 2.6 on the way back, so
a diagonal motion changes angle at the boundary. The `y` is also wrong
whenever the two PC monitors are not the same height and top-aligned:
`top` is the union's top, which for a point above the shorter monitor is
not on any monitor at all, and Windows silently clamps the injected
position to whatever pixel is nearest. That is the visible "snap".

### 1.3 Return detection against the union can strand focus on the PC

`crates/hop-platform/src/windows/return_edge.rs`, `reached_edge`, lines
73 to 88:

```rust
ReturnEdge::Bottom => y >= top + height - 1,
```

`top + height - 1` is the bottom row of the union. If the PC's two
monitors differ in height, or are vertically offset in Windows Display
Settings, the shorter monitor's bottom row is above the union's bottom
row. The cursor can be pressed against the bottom of that monitor
forever and `should_release` (lines 94 to 100) never answers `true`.
Focus is stuck on the PC and only the panic hotkey brings it back. This
is the exact failure the constraints call the worst one, and the user's
report is consistent with hitting it intermittently depending on which
monitor the cursor is on.

The injector's own clamp compounds this: `inject.rs` lines 384 to 386
clamp the target to the union rectangle, not to a monitor, and then
rely on Windows to fix up a point in the dead zone below the shorter
monitor. Whether Windows leaves it on the bottom row of that monitor is
undocumented behaviour hop should not depend on.

### 1.4 The Mac's own crossing test has the same union problem

`crates/hop-platform/src/macos/capture.rs`, `crossed`, lines 122 to 129,
compares against `cursor::display_bounds()` (`cursor.rs` lines 108 to
124), which is `union_rects` of every active display. Today the user
has a single built-in display (`system_profiler` reports one 2560 x 1664
panel, no externals), so this is latent, but it is the same shape of
bug: with a second Mac display beside the laptop that is shorter or
taller, `y <= bounds.min_y` is only ever true on the taller one. The
top of the shorter display is uncrossable; macOS clamps the cursor
there and nothing happens.

`nudge_inward` (lines 180 to 187) has the matching issue: it parks the
cursor at `(x, min_y + 12)`, which for a multi-display Mac may be a
point on no display.

### 1.5 The fraction is read before it is written

`capture.rs` lines 933 to 935, inside `handle_event`:

```rust
ctx.events.push(InputEvent::EdgeCrossed);
ctx.crossing_fraction
    .store(fraction.to_bits(), Ordering::Relaxed);
```

`EventQueue::push` (lines 344 to 355) calls `notify_one()` before it
returns. The server loop in `run.rs` wakes on that notification and, at
lines 675 and 705, evaluates
`Some(watched.inner.last_crossing_fraction())` before it drains the
queue. The tap callback runs on the capture thread; the loop runs on a
tokio thread. There is a real window in which the loop reads the
previous crossing's fraction (or the initial `0.0`) and sends it as this
crossing's `Enter`. Both accesses are `Relaxed`, so even reordering the
two lines would not make this reliable; the read happens before the
`pop` that would provide the acquire. The fix is structural: the
position belongs inside the `EdgeCrossed` event, not in a side channel.

### 1.6 The return jumps the Mac cursor back to where it left

`CursorPark::restore` (`capture.rs` lines 274 to 285) warps the cursor
to the point recorded at departure. `Message::Release` carries nothing
(`message.rs` line 44), so the Mac has no idea where along the PC's
bottom edge the hand came back through. With two PC monitors this is a
teleport of up to the full width of the Mac on every return, which is
the same discontinuity the `Enter { fraction }` commit fixed in the
other direction and then left unfixed in this one.

### 1.7 Smaller things in the same area

- Once 1.1 is fixed, the PC cursor arrives on the return edge. Without
  an inward nudge on arrival, `should_release` fires on the very next
  motion event and focus ping-pongs. `RELEASE_SUPPRESS_WINDOW` (500 ms,
  `inject.rs` line 272) would mask this most of the time, which is
  worse than failing loudly; the arrival point must be nudged inward
  the way `nudge_inward` already does on the Mac.
- `pump_server` (`session.rs` lines 79 to 126) takes one
  `entry_fraction` per drain. Two crossings in one batch would share
  it. Moot once the position rides in the event.
- `cursor.rs` lines 95 to 102 document that the display list is read
  once and never refreshed, so unplugging a monitor mid-session leaves
  edge detection reasoning about a desktop that no longer exists.
- `mouse_move_absolute_input` normalises against the virtual screen
  from `GetSystemMetrics`. hop declares no DPI awareness, so on a PC
  whose monitors have different scale factors these are virtualised
  coordinates, and it is not documented that `SendInput`'s absolute
  normalisation uses the same virtualised space. If the two disagree
  the cursor lands short of edges. This may be part of what the
  `fa01cc8` "cannot reach the last pixel" fix was actually seeing.
  Section 5.5 recommends declaring per-monitor awareness so every
  coordinate hop touches is a physical pixel.

## 2. The mental model

### 2.1 Recommended: one desktop arrangement, two machines

Treat the two machines exactly the way each OS already treats its own
monitors. Every monitor is a rectangle in logical units (macOS points,
Windows pixels). Rectangles on one machine are already arranged by the
OS. The two arrangements are then placed next to each other along the
configured edge (Mac top against PC bottom), at one-to-one scale, with
a single horizontal offset that says where the Mac sits under the PC.

Three consequences fall out of that picture and are the whole design:

1. **A crossing happens on an outward-facing edge segment.** On the Mac,
   the crossing edge is the top edge of every Mac display that has no
   other Mac display directly above it. On the PC, the return edge is
   the bottom edge of every PC monitor that has no other PC monitor
   directly below it. Seams between a machine's own monitors are not
   edges. A cursor pressed against a seam is just a cursor moving
   between monitors, which the OS handles.

2. **Position along the edge is preserved in logical units, relative to
   an anchor.** The Mac's point on its edge and the PC's point on its
   edge are related by `x_pc = x_mac + offset`, nothing else. No
   stretching. A hand moving at 45 degrees arrives moving at 45 degrees.
   The offset is the one thing hop cannot read from either OS, so it is
   defined by an anchor with a sensible default and one optional knob.

3. **Positions off the far edge clamp to its nearest end.** The PC's
   edge is wider than the Mac's. Leaving the PC from the second
   monitor's bottom maps to a point right of the Mac's right corner, so
   the Mac cursor appears at its top-right corner. Coming back always
   works, from every monitor, and the landing is the nearest thing to
   where the motion was heading.

Why this and not something cleverer: this is the model every user has
already learnt from their own OS's display arrangement panel. Nobody
expects a cursor to speed up 2.6x when it crosses between two of their
monitors. And it needs zero configuration to be right for the common
desk (laptop centred under the main monitor) and one number to be right
for every other desk.

### 2.2 Rejected: each machine is one rectangle (the current model)

Take the union of each machine's monitors, treat it as one screen, map
edges by fraction. It is what the code does today and it fails in
three ways this document has already shown: the union's edges are not
real edges (section 1.3 and 1.4, the stuck-focus and uncrossable-top
cases), fractions stretch motion (1.2), and the union's top and bottom
are not on any monitor when heights differ, so landing points are
undefined and resolved by whatever the OS clamps to. It is also not
simpler to implement once you add the special cases needed to make it
not strand focus, because those special cases are exactly the per-monitor
reasoning the recommended model starts from.

### 2.3 Rejected: a per-monitor adjacency map in config

Let the user declare "Mac display 1 top touches PC monitor 2 bottom, PC
monitor 1 bottom touches nothing", Synergy-style. Fully expressive, and
precisely the calibration chore the constraints forbid. The intra-machine
adjacencies it asks for are already known to the OS, so most of what the
user would type is redundant, and the one inter-machine fact it captures
(where the Mac sits) is a single number in the recommended model.

### 2.4 Considered and deferred: physical units

The one-to-one logical mapping assumes a Mac point and a PC pixel are
about the same physical size. For the user's hardware they are within
about 30 percent (a 14 inch MacBook at roughly 49 points per cm against
a 24 or 27 inch monitor at 36 to 43 pixels per cm). Both OSes can report
a display's physical size from EDID (`CGDisplayScreenSize`,
`GetDeviceCaps(HORZSIZE)`), which would let hop convert to millimetres
and get this exact. EDID physical sizes are notoriously wrong on cheap
monitors and TVs, so this would need a plausibility check and a
fallback. It is a refinement of the same model, not a different model,
and it is not needed to fix the reported bug. Leave it out of this
change and note it as a possible later default for a `scale` value that
this design deliberately does not add.

## 3. Computing the arrival position

All of this is pure arithmetic over rectangles and belongs in one module
in `hop-core` (proposed `crates/hop-core/src/screen.rs`), which forbids
`unsafe` and is built and tested on every host. Both platform modules
feed it rectangles read from the OS and act on what it returns.
`hop_platform::macos::Edge` and `hop_platform::windows::ReturnEdge` are
the same enum written twice; they become one `hop_core::Side`.

### 3.1 Types

```rust
pub enum Side { Top, Bottom, Left, Right }

/// One monitor, in the machine's own logical units. `max` is exclusive.
pub struct Rect { pub min_x: f64, pub min_y: f64, pub max_x: f64, pub max_y: f64 }

/// Everything hop knows about one machine's monitors.
pub struct Screen { pub monitors: Vec<Rect>, pub primary: usize }

/// A stretch of a monitor's edge with nothing beyond it.
pub struct Segment {
    pub monitor: usize,
    /// Range along the edge axis (x for Top/Bottom, y for Left/Right). `hi` exclusive.
    pub lo: f64,
    pub hi: f64,
    /// The edge's own coordinate on the other axis: the monitor's min_y for Top,
    /// max_y - 1 for Bottom, min_x for Left, max_x - 1 for Right.
    pub edge: f64,
}
```

### 3.2 Which parts of an edge face outward

```
outward_segments(screen, side) -> Vec<Segment>
  for each monitor m:
    take m's edge on `side` as the range [lo, hi) along the edge axis
    for each other monitor n that touches m across that edge
        (Top: n.max_y == m.min_y; Bottom: n.min_y == m.max_y; and so on,
         compared with a 1 unit tolerance)
      subtract n's range on the edge axis from [lo, hi)
    every remaining piece is a Segment
  sort by lo
```

Equivalently, a point on m's edge faces outward if no monitor contains
the point one unit beyond it. Both OSes guarantee monitors do not
overlap and are arranged adjacent, so subtraction of touching ranges
and the one-unit probe give the same answer; the segment form is what
the anchor and landing functions need.

The **extent** of a side is `[min lo, max hi)` over its segments. If a
side has no segments (impossible for a real desktop, since some monitor
is always outermost) treat the whole machine's bounding box edge as one
segment so nothing degrades to "no edge at all".

### 3.3 Detecting a crossing

Called on every motion event on the Mac (replacing `crossed` and
`crossing_fraction` in `capture.rs`) and after every injected motion on
the PC (replacing `reached_edge`/`should_release` in `return_edge.rs`):

```
at_outer_edge(screen, side, x, y) -> Option<f64>
  m = the monitor containing (x, y); if none, the nearest monitor
  on_edge_row = match side {
      Top    => y <= m.min_y,
      Bottom => y >= m.max_y - 1,
      Left   => x <= m.min_x,
      Right  => x >= m.max_x - 1,
  }
  if !on_edge_row { return None }
  along = x for Top/Bottom, y for Left/Right
  if some segment of outward_segments(screen, side) has monitor == m
     and lo <= along < hi { Some(along) } else { None }
```

The thresholds are the ones the existing tests pin (`y <= min_y` on the
Mac where the location is `f64` and macOS clamps it to the display; the
inclusive last pixel row on Windows). What changes is that `m` is the
monitor the cursor is on, and a cursor at a seam returns `None`.

### 3.4 The anchor and the shared axis

```
anchor(screen, side, override: Option<f32>) -> f64
  segments = outward_segments(screen, side); extent = [lo, hi)
  match override {
    Some(f) => lo + f.clamp(0, 1) * (hi - lo),
    None    => if some segment belongs to screen.primary,
                 the midpoint of the widest such segment
               else the midpoint of the extent
  }
```

The anchor is the point on this machine's edge that hop declares to be
the same physical place as the peer's anchor. Both machines default to
the centre of their primary display's outward segment, which encodes
"the user sits in front of their main monitor and the laptop is in
front of the user". The client's optional `anchor` config (section 6)
overrides the PC side; the Mac side is never configured, because one
translation is one degree of freedom and one knob is enough to set it.

The value that travels on the wire is

```
u = along_local - anchor(local screen, local side, local override)
```

in the sender's logical units, and the receiver computes

```
along_peer = u + anchor(peer screen, peer side, peer override)
```

`u` can be negative and can exceed either extent. That is fine; the
landing function clamps.

### 3.5 Landing

```
landing(screen, side, along, margin) -> (f64, f64)
  segments = outward_segments(screen, side)
  along = clamp(along, extent.lo, extent.hi - 1)
  seg = the segment with lo <= along < hi,
        else the segment whose [lo, hi) is nearest to along,
        and snap along into it
  inward = match side {
      Top    => seg.edge + margin,
      Bottom => seg.edge - margin,
      Left   => seg.edge + margin,
      Right  => seg.edge - margin,
  }
  clamp inward into seg.monitor's rect so a monitor thinner than the margin
  still yields an in-bounds point
  Top/Bottom => (along, inward); Left/Right => (inward, along)
```

`margin` is `EDGE_MARGIN` (12 points) on the Mac, which already exists
for this purpose, and a new `ENTRY_MARGIN` of the same order (8 to 12
pixels) on the PC. The invariant every landing must satisfy, and the
test that pins it: `at_outer_edge(screen, side, landing(...)) == None`.
A landing that is itself a crossing is a ping-pong.

The monitor the cursor lands on is `seg.monitor`: the monitor whose
outward-facing bottom segment contains the mapped `x`. With two PC
monitors side by side and the default anchor, the whole Mac edge maps
inside the primary monitor's segment, so every direct crossing lands on
the primary. Reaching the second monitor means moving across the seam
on the PC, which is what the user does with a directly attached mouse.

### 3.6 On the PC, in `inject.rs`

- `WindowsInjector` holds a `Screen` read via `EnumDisplayMonitors` and
  `GetMonitorInfoW` (see section 5.4 for when it is refreshed). These
  live under `windows-sys`'s `Win32_Graphics_Gdi` feature, which is a
  feature flag on a dependency the crate already has, not a new
  dependency. `MONITORINFOF_PRIMARY` identifies `primary`.
- `InputEvent::Enter { along }`: `x, y = landing(screen, return_edge,
  along + anchor(...), ENTRY_MARGIN)`, then one
  `mouse_move_absolute_input(x, y, virtual_screen)`. The normalisation
  to 0..65535 over the virtual screen rectangle stays exactly as it is;
  that is how `MOUSEEVENTF_VIRTUALDESK` is specified, and it is the
  right use of the union: as the API's coordinate space, not as the
  model of where monitors are.
- `InputEvent::Mouse { dx, dy }`: target = cursor + scaled delta, then
  `clamp_to_monitors(screen, target)`, a pure nearest-point clamp onto
  the set of monitor rectangles (replacing the union clamp at lines
  384 to 386). Moving into the neighbouring monitor still works because
  the target is inside that monitor; moving into a dead zone slides
  along the nearest edge instead of relying on Windows to guess.
  Absolute positioning is kept for the reason the code already states.
- `reached_return_edge` becomes
  `fn return_crossing(&mut self) -> Option<f32>`: `GetCursorPos`, then
  `at_outer_edge(screen, return_edge, x, y)`, then
  `Some(along - anchor)` as `u`. `RELEASE_SUPPRESS_WINDOW` stays as a
  backstop against in-flight motion after a release; it is no longer
  what prevents ping-pong.

### 3.7 On the Mac, in `capture.rs`

- `CaptureContext.bounds: Bounds` becomes `screen: Screen`, built from
  `CGDisplay::active_displays()` rectangles with `CGDisplay::main()` as
  `primary`. `union_rects` and `display_bounds` go away.
- In `handle_event`, `crossed` + `crossing_fraction` + `nudge_inward`
  become: `if let Some(along) = at_outer_edge(screen, edge, x, y)`,
  park at `landing(screen, edge, along, EDGE_MARGIN)`, compute
  `u = along - anchor(screen, edge, None)`, and push
  `InputEvent::EdgeCrossed { along: u }`. The `AtomicU32`,
  `last_crossing_fraction`, and the `entry_fraction` parameter of
  `pump_server` are deleted; the race in 1.5 cannot exist when the
  payload is in the event.
- `pump_server` sends `Message::Enter { along }` from the event's own
  field.

## 4. The return

### 4.1 Sequence

1. The PC injects a motion event, reads the real cursor, and
   `at_outer_edge` says the cursor is on an outward-facing bottom row at
   `along_pc`. It sends `Message::Release { along: along_pc - anchor_pc }`.
2. The server loop in `run.rs` (the `Message::Release` arm at lines 610
   to 618) calls `control.on_release_requested()` as now, and
   additionally computes `landing(mac_screen, Edge::Top, along +
   anchor_mac, EDGE_MARGIN)` and hands it to the capturer as the point
   to restore to.
3. `CursorPark::restore` warps to that point rather than the departure
   point, then re-associates the mouse and shows the cursor, in the
   order the existing comments already justify. The departure point
   remains the fallback for returns that carry no position: disconnect,
   panic hotkey, wake, and the client's `Release` from a build that
   predates this change (which the handshake will refuse anyway, see
   section 6.3).

`restore` should be called directly from the server loop when the
`Release` arrives, not only lazily from the tap callback on the next
event as today (`capture.rs` line 908). `CGWarpMouseCursorPosition` is
callable from any thread, `restore` is already idempotent and already
called from `Drop` off the capture thread, and the user's hand is moving
so the lazy path would fire almost immediately anyway; making it
immediate just removes a "cursor reappears one event late" variable
from the user's testing.

### 4.2 Returning from the far monitor

The PC's bottom edge extent is, say, `[0, 3840)`; the default anchor is
the centre of the primary monitor at `960`; the Mac's top extent is
`[0, 1470)` with anchor `735`. A return from the second monitor at
`x_pc = 3000` gives `u = 2040` and `along_mac = 2775`, which `landing`
clamps to `1469`: the Mac cursor appears at its top-right corner, 12
points down. The hand was moving down and to the right of the Mac; the
top-right corner is the honest answer. No configuration is needed for
this to work, and it works from every pixel of every bottom-facing PC
segment because `at_outer_edge` is per monitor.

### 4.3 Returning when the PC monitors differ in height

Side by side, top-aligned, one 1440 tall and one 1080 tall: both bottoms
face outward, at `y = 1439` and `y = 1079`. `outward_segments` yields two
segments with different `edge` values. The cursor on the shorter
monitor reaches `y = 1079`, `at_outer_edge` finds it on the shorter
monitor's edge row inside that monitor's segment, and the release fires.
Under the current code this is the stuck case from 1.3.

### 4.4 What must never regress

- A failed `GetCursorPos` still never releases.
- If `EnumDisplayMonitors` fails or returns nothing, fall back to a
  `Screen` with one monitor equal to the virtual screen rectangle. That
  is the current model, so the return degrades to today's behaviour
  rather than to "no return edge".
- The panic hotkey path is untouched.

## 5. Awkward edges

### 5.1 Near a corner

Only the configured side is ever a crossing. A cursor at the Mac's
top-left corner moving up-left crosses because it is on the top edge
row; its `along` is the Mac's `min_x`, `u` is negative, and the PC
landing clamps to the left end of the PC's extent (with the default
anchor, `x = 225` on the primary monitor, which is where a Mac corner
sits when the Mac is centred under it). Continued leftward motion on
the PC moves the cursor left as normal. A cursor at the Mac's right
edge does nothing, since Right is not configured. The existing
`only_the_top_edge_triggers_at_the_top_boundary` tests carry straight
over to `at_outer_edge`.

### 5.2 Moving between the PC's own two monitors

Not hop's concern beyond not breaking it. `at_outer_edge` returns
`None` on a seam, so a cursor that passes through a seam pixel row on
its way to the other monitor never releases. `clamp_to_monitors` lets
a target that lands inside the neighbour through unchanged and slides a
target that falls in a dead zone (below the shorter monitor while moving
sideways) along the nearest monitor edge, which is what the OS would do
for a directly attached mouse. Stacked PC monitors work for free: only
the lower monitor's bottom faces outward; from the upper monitor the
user moves down through the seam and then off the bottom.

### 5.3 Mismatched aspect ratios and sizes

Handled by construction: nothing is ever stretched, and anything that
maps off the far edge clamps to the nearest end. The one genuinely
lossy case is the reverse of 4.2: a Mac wider than the PC's edge (an
ultrawide on the Mac, a single 1080p on the PC). Crossings from the
outer parts of the Mac's edge all land on the PC's corners. That is
correct for the physical picture (those parts of the Mac are not under
the PC) and the user can move the anchor if their desk disagrees.

### 5.4 A monitor is unplugged or rearranged mid-session

Windows: refresh the `Screen` on every `Enter` (once per crossing, a
few microseconds), whenever `GetCursorPos` returns a point inside no
cached monitor (Windows has moved the cursor because its monitor went
away), and on the supervisor's existing one-second heartbeat tick. The
worst case is one second of edge detection against a stale list, after
which the return works again; the panic hotkey covers that second. This
avoids needing a window to receive `WM_DISPLAYCHANGE`, which hop does
not have.

macOS: register `CGDisplayRegisterReconfigurationCallback` from the
capture thread (a public Core Graphics function; declare it `extern
"C"` the way `capture.rs` already declares `CGEventTapEnable`) and have
it set an `AtomicBool`. `handle_event` re-reads `active_displays()` the
next time it sees the flag set. This replaces the "restart hop to pick
up a new layout" limitation documented at `cursor.rs` lines 95 to 102.
As belt and braces, `restore` clamps its landing point into the current
`Screen` before warping, so a parked point on a display that has since
vanished cannot put the cursor off every screen.

### 5.5 DPI on Windows

Declare `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` at client startup
(`SetProcessDpiAwarenessContext`, `Win32_UI_HiDpi` feature of
`windows-sys`, again a feature flag not a dependency). With it,
`GetCursorPos`, `EnumDisplayMonitors`, `GetSystemMetrics`, and
`SendInput` all speak physical pixels and the model's rectangles are the
real ones. Without it, a PC with monitors at different scale factors
gives hop virtualised rectangles whose relationship to `SendInput`'s
normalised space is not documented. This is the one item in this
document whose necessity cannot be established from the code; it is a
recommendation with a hardware test behind it (section 7.2).

## 6. Configuration

### 6.1 What exists and stays

- Server `[layout] top = "pc"`: which of the Mac's edges hands focus
  over. Unchanged.
- Client `[input] return_edge = "bottom"`: which of the PC's edges hands
  it back, and, after this change, also where an `Enter` arrives.
  Unchanged in form; the documentation should stop calling the entry
  edge the opposite edge. The mirrored-pair requirement between the two
  files stays, and stays unverifiable at load time for the reason the
  config docs already give.
- Client `[input] mouse_scale`. Unchanged; it is about delta scaling,
  not geometry.

### 6.2 What is added: `anchor`, client side, optional

```toml
[input]
return_edge = "bottom"
# Where along return_edge the Mac sits, as a fraction from 0.0 (left or
# top end) to 1.0 (right or bottom end). Leave unset for "centred under
# the primary monitor", which is right for most desks. Set it to about
# 0.5 if the laptop sits under the seam between two monitors, or 0.75
# if it sits under the right-hand one.
# anchor = 0.25
```

Why this one and nothing else: the model has exactly one quantity
neither OS can tell hop, which is the horizontal offset between the two
machines. The default guess (centre of the primary monitor) is right
for the common desk, and when it is wrong the user can tell by looking
at where the cursor lands and fix it with one unitless number that
does not change when they change resolution. It lives on the client
because that is the machine whose monitors it describes, and because
the geometry is computed on the receiving side of each crossing so the
PC needs it at runtime and the Mac never does. Rejected: putting a
second `anchor` on the server (a second knob for one degree of
freedom), and a `scale` (section 2.4).

Validation: a number outside `0.0..=1.0` is a named `ConfigError`, not a
clamp, consistent with how `mouse_scale` refuses nonsense values.

### 6.3 Wire changes (proposed, explicit)

Nothing is released, so these are free, and they are the point of the
design rather than incidental:

- `Message::Enter { fraction: f32 }` becomes `Message::Enter { along:
  f32 }`: logical units relative to the sender's anchor, signed,
  unbounded. Same tag, same postcard `f32` body, different meaning.
- `Message::Release` gains a body: `Message::Release { along: f32 }`,
  same meaning in the other direction.
- `PROTOCOL_VERSION` (`message.rs` line 6) goes from 1 to 2. The body
  shape of `Enter` is unchanged, so a mismatched pair would decode
  successfully and place the cursor somewhere confusing; a version bump
  makes the mismatch a handshake failure with a log line instead.
- `InputEvent::EdgeCrossed` becomes `InputEvent::EdgeCrossed { along:
  f32 }`, `InputEvent::Enter { fraction }` becomes `InputEvent::Enter {
  along }`, and `Injector::reached_return_edge() -> bool` becomes
  `Injector::return_crossing(&mut self) -> Option<f32>`.

## 7. Testing

### 7.1 Unit tests, no hardware, run on the Mac in `cargo test --workspace`

All of section 3 is pure and lives in `hop-core`, so the Windows-side
geometry is tested on the macOS development machine for the first time.
The tests that matter most, in the order they should be written:

1. **The reported bug, as a regression test.** Screen = two 1920 x 1080
   monitors side by side, primary at the origin. `Side::Bottom`,
   default anchor. A Mac `u = 0` (centre of a Mac edge) lands at
   `(960, 1079 - margin)` on monitor 0. It does not land on monitor 1
   and its `y` is not `0`. This single test fails against the current
   `entry_point` for both reasons in 1.1 and 1.2.
2. **Entry edge equals return edge.** For every `Side`, the landing for
   that side satisfies `at_outer_edge(screen, side, landing) == None`
   (no ping-pong) and lies within `margin + 1` of that side's edge,
   not the opposite one.
3. **Outward segments.** Side by side equal heights (two bottom
   segments, same `edge`); side by side top-aligned different heights
   (two segments, different `edge`); stacked (one bottom segment, from
   the lower monitor only); a Mac laptop with a display above it
   (laptop's top has no segment where the display overlaps it, and a
   segment where it does not). The existing `ABOVE_MAIN`/`BELOW_MAIN`
   cases in `capture.rs` translate directly.
4. **Stuck-focus regression.** Top-aligned monitors of different
   heights, cursor on the bottom row of the shorter one:
   `at_outer_edge(Bottom)` is `Some`. Against `reached_edge` on the
   union this is `false`, which is 1.3.
5. **Seam is not an edge.** Stacked monitors, cursor on the upper
   monitor's bottom row: `None`.
6. **Far-monitor return clamps to the corner.** The 4.2 numbers, ending
   at `x = 1469`.
7. **Anchor defaults and override.** Primary not on the outward edge
   falls back to extent centre; `anchor = 0.5` on a 3840 extent gives
   1920; override clamps.
8. **Round trip.** For random screens and positions, `landing` of
   `u + anchor` on the same screen and side returns the original
   `along` (within the clamp) and a point not on the edge row.
9. **`clamp_to_monitors`.** Inside a monitor: unchanged. In a dead zone
   below the shorter monitor: nearest edge point. In the neighbour: unchanged.
10. **Corner.** Mac top-left corner crossing gives `along = min_x`;
    landing on the PC is the left end of its extent, on monitor 0.
11. **Fallback screen.** An empty monitor list produces a single
    monitor equal to a supplied bounding rectangle, and `at_outer_edge`
    on it behaves like today's `reached_edge`.
12. **The race is gone.** `pump_server` with a `FakeCapturer` yielding
    `EdgeCrossed { along: 123.0 }` sends `Enter { along: 123.0 }` with
    no other input; there is no `entry_fraction` parameter to get wrong.
13. **`Release { along }` reaches the capturer.** A `hop-core` test of
    the server loop's release handling asserting the restore point is
    `landing(mac_screen, Top, along + anchor, EDGE_MARGIN)`, using the
    fake capturer.
14. **Config.** `anchor` parses, defaults to `None`, and rejects `1.5`
    and `-0.1` with the named error.

Windows-only `#[cfg(test)]` in `inject.rs` shrinks to what it already
covers (`mouse_move_absolute_input` normalisation, `scale_delta`) plus
one test that `Enter` produces an absolute move to the pure `landing`
result, so the wiring is checked even though the geometry is tested in
`hop-core`.

### 7.2 What only Talha can check, on the real desk

A short list, each a single observable thing, in the order that
isolates causes:

1. Cross the top of the Mac at its centre. The cursor should appear
   near the bottom of the PC monitor that is physically above the Mac,
   roughly under where it left, a few pixels up from the bottom, not
   at the top of anything. If it appears on the other monitor, set
   `anchor` (0.5 for under the seam, 0.75 for under the right monitor)
   and try again; that is the only tuning this design expects.
2. Keep moving up after crossing. The cursor should keep moving up on
   the PC at the same apparent speed, with no pause and no jump.
3. Move down to the bottom of the PC monitor you are on. Focus should
   return and the Mac cursor should appear just below the Mac's top
   edge, under where you left the PC. Try it from both PC monitors;
   from the far one, expect the Mac cursor at its nearest top corner.
4. If the PC monitors are different heights or not aligned in Windows
   Display Settings: return from the shorter one specifically. This is
   the case that used to strand focus.
5. Move the cursor between the two PC monitors, sideways and diagonally,
   including along the bottom row. Focus must not return while crossing
   the seam.
6. Unplug one PC monitor while focus is on the PC, then return via the
   bottom of the remaining one. Then plug it back in and cross again.
7. If the two PC monitors have different scale factors in Windows: after
   5.5 is applied, repeat 1 to 3. This is the one item where the design
   is a recommendation rather than a derivation from the code.
8. Panic hotkey still works from anywhere; it is the safety net for all
   of the above, not a step that should ever be needed.

The macOS `y <= min_y` clamp behaviour, the exact `edge` value Windows
reports for a bottom row after `SendInput`, and whether
`CGDisplayRegisterReconfigurationCallback` fires on the capture thread's
run loop are the three platform facts the unit tests take on trust and
steps 1, 3, and 6 confirm.
