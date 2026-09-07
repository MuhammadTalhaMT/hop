//! Where the cursor goes when it crosses between the two machines.
//!
//! The model is the one both operating systems already use for their own
//! monitors: every display is a rectangle in the machine's own logical
//! units, arranged by the OS, and the two machines are placed next to
//! each other along one configured edge at one-to-one scale. The only
//! fact neither OS can supply is how far along that edge the machines
//! line up, and that is one number (see [`Screen::anchor`]).
//!
//! Everything here is pure arithmetic over rectangles, which is the
//! point: the Windows-side geometry is unit tested on the macOS machine
//! this project is developed on, which is how an inverted entry edge and
//! a union-rectangle edge test both shipped without a test noticing.
//!
//! What this replaces, and why:
//!
//! - Treating each machine as the union of its monitors. The union's
//!   edges are not real edges. With two PC monitors of different heights
//!   the union's bottom row is below the shorter monitor entirely, so a
//!   cursor pressed against that monitor's bottom never registered as a
//!   return and focus was stranded on the PC.
//! - Mapping position as a fraction of the edge. A fraction stretches
//!   motion by the ratio of the two edge widths, so a diagonal changes
//!   angle at the boundary and a crossing from the middle of the Mac
//!   lands on the seam between two PC monitors.

/// Which side of a machine's desktop faces the peer.
///
/// One enum for both platforms: `hop_platform::macos::Edge` and
/// `hop_platform::windows::ReturnEdge` are the same four values, and the
/// geometry below has to reason about both machines at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

impl Side {
    /// Parses a config edge name. `None` for anything else, so a typo is
    /// a named config error rather than a silently wrong direction.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "top" => Some(Self::Top),
            "bottom" => Some(Self::Bottom),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }

    /// Whether position along this side is measured on the x axis.
    fn horizontal(self) -> bool {
        matches!(self, Self::Top | Self::Bottom)
    }
}

/// One monitor, in its machine's own logical units (macOS points,
/// Windows pixels). `max_x` and `max_y` are exclusive, so a 1920 wide
/// monitor at the origin has `max_x == 1920.0` and its last addressable
/// column is 1919.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Rect {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// From an origin and a size, which is the shape both platform APIs
    /// report.
    pub fn from_origin_size(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self::new(x, y, x + width, y + height)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.min_x && x < self.max_x && y >= self.min_y && y < self.max_y
    }

    /// The nearest addressable point inside this rectangle. Clamped to
    /// `max - 1` rather than `max` because `max` is exclusive and putting
    /// the cursor there would place it on the neighbouring monitor, or on
    /// no monitor at all.
    fn nearest(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x.clamp(self.min_x, (self.max_x - 1.0).max(self.min_x)),
            y.clamp(self.min_y, (self.max_y - 1.0).max(self.min_y)),
        )
    }

    /// This rectangle's extent along `side`'s axis, as `[lo, hi)`.
    fn span(&self, side: Side) -> (f64, f64) {
        if side.horizontal() {
            (self.min_x, self.max_x)
        } else {
            (self.min_y, self.max_y)
        }
    }

    /// The coordinate of this rectangle's `side` edge on the other axis.
    /// The last addressable row or column for a far edge, the first for a
    /// near one, so the value is always a point the cursor can occupy.
    fn edge(&self, side: Side) -> f64 {
        match side {
            Side::Top => self.min_y,
            Side::Bottom => (self.max_y - 1.0).max(self.min_y),
            Side::Left => self.min_x,
            Side::Right => (self.max_x - 1.0).max(self.min_x),
        }
    }
}

/// A stretch of one monitor's edge with no other monitor of the same
/// machine beyond it: the part of the edge that actually faces the peer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    /// Index into [`Screen::monitors`].
    pub monitor: usize,
    /// Range along the edge axis, `hi` exclusive.
    pub lo: f64,
    pub hi: f64,
    /// The edge's coordinate on the other axis.
    pub edge: f64,
}

/// Two rectangles are treated as touching if their edges are within this
/// many units. Both OSes lay monitors out exactly adjacent, but the
/// values arrive as floats through two different APIs, so an exact
/// comparison would be brittle for no benefit.
const TOUCH_TOLERANCE: f64 = 1.0;

/// Everything hop knows about one machine's monitors.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    monitors: Vec<Rect>,
    primary: usize,
}

impl Screen {
    /// `primary` is clamped into range rather than rejected: a platform
    /// that reports no primary monitor should degrade to "the first one",
    /// not fail to hand over the cursor at all.
    pub fn new(monitors: Vec<Rect>, primary: usize) -> Self {
        let primary = if primary < monitors.len() { primary } else { 0 };
        Self { monitors, primary }
    }

    /// The fallback for a platform whose monitor enumeration failed: one
    /// monitor covering the whole virtual desktop. This is exactly the
    /// old union model, so a failed enumeration degrades to the previous
    /// behaviour rather than to "no edge at all".
    pub fn single(bounds: Rect) -> Self {
        Self {
            monitors: vec![bounds],
            primary: 0,
        }
    }

    pub fn monitors(&self) -> &[Rect] {
        &self.monitors
    }

    pub fn primary(&self) -> usize {
        self.primary
    }

    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    /// The parts of `side` that face the peer rather than another of this
    /// machine's own monitors.
    ///
    /// A seam between two of a machine's monitors is not an edge: a
    /// cursor pressed against it is just a cursor moving between
    /// monitors, which the OS handles. Excluding seams is what stops
    /// focus returning halfway across the PC's desktop.
    pub fn outward_segments(&self, side: Side) -> Vec<Segment> {
        let mut out = Vec::new();
        for (i, m) in self.monitors.iter().enumerate() {
            let (lo, hi) = m.span(side);
            let mut pieces = vec![(lo, hi)];
            for (j, n) in self.monitors.iter().enumerate() {
                if i == j || !beyond(*m, *n, side) {
                    continue;
                }
                let (n_lo, n_hi) = n.span(side);
                pieces = subtract(&pieces, n_lo, n_hi);
            }
            let edge = m.edge(side);
            out.extend(pieces.into_iter().map(|(lo, hi)| Segment {
                monitor: i,
                lo,
                hi,
                edge,
            }));
        }
        out.sort_by(|a, b| a.lo.partial_cmp(&b.lo).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    /// The full range covered by `side`'s outward segments, as
    /// `[lo, hi)`. `None` only for a screen with no monitors.
    pub fn extent(&self, side: Side) -> Option<(f64, f64)> {
        let segments = self.outward_segments(side);
        let lo = segments.iter().map(|s| s.lo).fold(f64::INFINITY, f64::min);
        let hi = segments
            .iter()
            .map(|s| s.hi)
            .fold(f64::NEG_INFINITY, f64::max);
        if segments.is_empty() {
            None
        } else {
            Some((lo, hi))
        }
    }

    /// Whether `(x, y)` is on an outward-facing part of `side`, and if so
    /// how far along that side it is.
    ///
    /// Per monitor, not against the union of them all. A cursor on the
    /// bottom row of the shorter of two PC monitors is at the bottom of
    /// its own monitor, and that is what decides the crossing.
    pub fn at_outer_edge(&self, side: Side, x: f64, y: f64) -> Option<f64> {
        let index = self.monitor_at(x, y)?;
        let m = self.monitors[index];
        let on_edge_row = match side {
            Side::Top => y <= m.min_y,
            Side::Bottom => y >= m.max_y - 1.0,
            Side::Left => x <= m.min_x,
            Side::Right => x >= m.max_x - 1.0,
        };
        if !on_edge_row {
            return None;
        }
        let along = if side.horizontal() { x } else { y };
        self.outward_segments(side)
            .into_iter()
            .find(|s| s.monitor == index && along >= s.lo && along < s.hi)
            .map(|_| along)
    }

    /// The point on `side` that hop declares to be the same physical
    /// place as the peer's anchor. Positions travel on the wire relative
    /// to it, which is what makes the mapping a translation rather than a
    /// stretch.
    ///
    /// The default is the centre of the primary monitor's outward
    /// segment: "the user sits in front of their main monitor and the
    /// laptop is in front of the user". `override_fraction` moves it
    /// along the side for desks where that is wrong, as a fraction of the
    /// side's extent.
    pub fn anchor(&self, side: Side, override_fraction: Option<f32>) -> f64 {
        let segments = self.outward_segments(side);
        let Some((lo, hi)) = self.extent(side) else {
            return 0.0;
        };
        if let Some(f) = override_fraction {
            return lo + f64::from(f.clamp(0.0, 1.0)) * (hi - lo);
        }
        let widest_primary = segments
            .iter()
            .filter(|s| s.monitor == self.primary)
            .max_by(|a, b| {
                (a.hi - a.lo)
                    .partial_cmp(&(b.hi - b.lo))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        match widest_primary {
            Some(s) => (s.lo + s.hi) / 2.0,
            None => (lo + hi) / 2.0,
        }
    }

    /// Where the cursor should be placed when focus arrives at `along` on
    /// `side`, nudged `margin` units inward.
    ///
    /// The nudge is not cosmetic. A cursor placed exactly on the edge row
    /// it arrived at is by definition at a crossing point, so the very
    /// next motion event would hand focus straight back and the pointer
    /// would ping-pong between the machines.
    ///
    /// `along` may be anywhere, including off the ends: the PC's edge is
    /// wider than the Mac's, so a return from the far monitor maps past
    /// the Mac's corner. Clamping to the nearest end is the honest
    /// answer, since the corner is the closest point to where the hand
    /// was heading.
    pub fn landing(&self, side: Side, along: f64, margin: f64) -> Option<(f64, f64)> {
        let segments = self.outward_segments(side);
        let (lo, hi) = self.extent(side)?;
        let along = along.clamp(lo, (hi - 1.0).max(lo));
        let seg = segments
            .iter()
            .find(|s| along >= s.lo && along < s.hi)
            .copied()
            .or_else(|| {
                segments
                    .iter()
                    .min_by(|a, b| {
                        distance_to(a, along)
                            .partial_cmp(&distance_to(b, along))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .copied()
            })?;
        let along = along.clamp(seg.lo, (seg.hi - 1.0).max(seg.lo));
        let inward = match side {
            Side::Top | Side::Left => seg.edge + margin,
            Side::Bottom | Side::Right => seg.edge - margin,
        };
        let m = self.monitors[seg.monitor];
        let point = if side.horizontal() {
            (along, inward)
        } else {
            (inward, along)
        };
        // A monitor thinner than the margin would otherwise land the
        // cursor past its far edge, which is off that monitor entirely.
        Some(m.nearest(point.0, point.1))
    }

    /// The nearest point on this machine's monitors to `(x, y)`.
    ///
    /// Replaces clamping to the union rectangle, which could leave the
    /// cursor in a dead zone that belongs to no monitor (below the
    /// shorter of two side-by-side monitors, say) and rely on the OS to
    /// silently fix it up. Sliding along the nearest monitor edge is what
    /// the OS does for a directly attached mouse.
    pub fn clamp_to_monitors(&self, x: f64, y: f64) -> (f64, f64) {
        if self.monitors.iter().any(|m| m.contains(x, y)) {
            return (x, y);
        }
        self.monitors
            .iter()
            .map(|m| m.nearest(x, y))
            .min_by(|a, b| {
                squared_distance(*a, (x, y))
                    .partial_cmp(&squared_distance(*b, (x, y)))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or((x, y))
    }

    /// Where to leave the real cursor while focus is on the peer.
    ///
    /// The cursor has to be left SOMEWHERE, and wherever it is left, the
    /// OS goes on treating it as hovering whatever is under it. No mouse
    /// leave event ever follows, because the hand is on the other
    /// machine, so the hover latches: a taskbar thumbnail preview stays
    /// open on the PC, a menu bar item stays highlighted on the Mac.
    ///
    /// Leaving it at the edge it crossed through is the worst possible
    /// choice, and is what hop used to do. On both machines the crossing
    /// edge is the OS's most hover-sensitive strip: the PC's bottom edge
    /// is the taskbar, the Mac's top edge is the menu bar, and a corner
    /// is a macOS hot corner. The centre of the primary monitor is away
    /// from all of them. It can still be over an ordinary window, but a
    /// button highlight nobody can see is a world away from a preview
    /// popup sitting open on an unattended screen.
    pub fn resting_point(&self) -> Option<(f64, f64)> {
        let m = self
            .monitors
            .get(self.primary)
            .or_else(|| self.monitors.first())?;
        Some(((m.min_x + m.max_x) / 2.0, (m.min_y + m.max_y) / 2.0))
    }

    /// The monitor containing `(x, y)`, or the nearest one. Nearest
    /// rather than `None` because the OS can report a cursor position in
    /// a dead zone between monitors, and a crossing decision still has to
    /// be made about it.
    fn monitor_at(&self, x: f64, y: f64) -> Option<usize> {
        if let Some(i) = self.monitors.iter().position(|m| m.contains(x, y)) {
            return Some(i);
        }
        self.monitors
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                squared_distance(a.nearest(x, y), (x, y))
                    .partial_cmp(&squared_distance(b.nearest(x, y), (x, y)))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
    }
}

/// Whether `n` lies immediately beyond `m`'s `side` edge, so that the
/// part of that edge `n` covers is a seam rather than an outward face.
fn beyond(m: Rect, n: Rect, side: Side) -> bool {
    match side {
        Side::Top => (n.max_y - m.min_y).abs() <= TOUCH_TOLERANCE,
        Side::Bottom => (n.min_y - m.max_y).abs() <= TOUCH_TOLERANCE,
        Side::Left => (n.max_x - m.min_x).abs() <= TOUCH_TOLERANCE,
        Side::Right => (n.min_x - m.max_x).abs() <= TOUCH_TOLERANCE,
    }
}

/// Removes `[lo, hi)` from a set of disjoint ascending ranges.
fn subtract(pieces: &[(f64, f64)], lo: f64, hi: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for &(a, b) in pieces {
        if hi <= a || lo >= b {
            out.push((a, b));
            continue;
        }
        if lo > a {
            out.push((a, lo));
        }
        if hi < b {
            out.push((hi, b));
        }
    }
    out
}

fn distance_to(segment: &Segment, along: f64) -> f64 {
    if along < segment.lo {
        segment.lo - along
    } else if along >= segment.hi {
        along - segment.hi
    } else {
        0.0
    }
}

fn squared_distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference PC: two 1920 x 1080 monitors side by side, primary
    /// on the left, exactly the desk this was reported broken on.
    fn pc() -> Screen {
        Screen::new(
            vec![
                Rect::new(0.0, 0.0, 1920.0, 1080.0),
                Rect::new(1920.0, 0.0, 3840.0, 1080.0),
            ],
            0,
        )
    }

    /// The reference Mac: one built-in display, 1470 x 956 points.
    fn mac() -> Screen {
        Screen::new(vec![Rect::new(0.0, 0.0, 1470.0, 956.0)], 0)
    }

    // The reported bug, as a regression test. A crossing from the middle
    // of the Mac's top edge must land near the BOTTOM of the PC's primary
    // monitor: bottom because entry and return happen on the same edge,
    // and primary because nothing is stretched across both monitors.
    //
    // The previous code failed this twice over: it placed the cursor on
    // the top row of the virtual screen, and it mapped the fraction
    // across the union of both monitors so the middle of the Mac landed
    // on the seam.
    #[test]
    fn a_crossing_from_the_middle_of_the_mac_lands_at_the_bottom_of_the_primary_monitor() {
        let (mac, pc) = (mac(), pc());
        let u = mac.anchor(Side::Top, None) - mac.anchor(Side::Top, None);
        let (x, y) = pc
            .landing(Side::Bottom, u + pc.anchor(Side::Bottom, None), 12.0)
            .expect("the reference PC has a bottom edge");
        assert_eq!((x, y), (960.0, 1067.0));
        assert!(pc.monitors()[0].contains(x, y), "landed off the primary");
        assert!(y > 1000.0, "landed at the top instead of the bottom");
    }

    // No landing may itself be a crossing point, on any side. This is
    // what stops focus ping-ponging the instant it arrives, and it has to
    // hold on the entry edge specifically, which is the same edge as the
    // return edge.
    #[test]
    fn a_landing_is_never_itself_at_the_edge() {
        for side in [Side::Top, Side::Bottom, Side::Left, Side::Right] {
            for screen in [pc(), mac()] {
                let along = screen.anchor(side, None);
                let (x, y) = screen.landing(side, along, 12.0).expect("has an edge");
                assert_eq!(
                    screen.at_outer_edge(side, x, y),
                    None,
                    "{side:?} landing at ({x}, {y}) would immediately cross back"
                );
            }
        }
    }

    #[test]
    fn side_by_side_monitors_both_face_outward_along_the_bottom() {
        let segments = pc().outward_segments(Side::Bottom);
        assert_eq!(segments.len(), 2);
        assert_eq!((segments[0].lo, segments[0].hi), (0.0, 1920.0));
        assert_eq!((segments[1].lo, segments[1].hi), (1920.0, 3840.0));
        assert_eq!(segments[0].edge, 1079.0);
        assert_eq!(segments[1].edge, 1079.0);
    }

    #[test]
    fn stacked_monitors_expose_only_the_lower_ones_bottom() {
        let screen = Screen::new(
            vec![
                Rect::new(0.0, 0.0, 1920.0, 1080.0),
                Rect::new(0.0, 1080.0, 1920.0, 2160.0),
            ],
            0,
        );
        let segments = screen.outward_segments(Side::Bottom);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].monitor, 1);
        assert_eq!(segments[0].edge, 2159.0);
    }

    #[test]
    fn a_display_above_the_laptop_covers_only_the_part_it_overlaps() {
        // A 1920 wide display centred above a 2560 wide laptop leaves a
        // 320 point strip of laptop top edge facing outward on each side.
        let screen = Screen::new(
            vec![
                Rect::new(0.0, 0.0, 2560.0, 1440.0),
                Rect::new(320.0, -1080.0, 2240.0, 0.0),
            ],
            0,
        );
        let segments = screen.outward_segments(Side::Top);
        let laptop: Vec<_> = segments.iter().filter(|s| s.monitor == 0).collect();
        assert_eq!(laptop.len(), 2);
        assert_eq!((laptop[0].lo, laptop[0].hi), (0.0, 320.0));
        assert_eq!((laptop[1].lo, laptop[1].hi), (2240.0, 2560.0));
    }

    // The stranded-focus case. Two monitors top-aligned but different
    // heights: the shorter one's bottom row is well above the union's
    // bottom row, so union-based detection never fired there and the only
    // way back was the panic hotkey.
    #[test]
    fn the_bottom_of_a_shorter_monitor_is_still_an_edge() {
        let screen = Screen::new(
            vec![
                Rect::new(0.0, 0.0, 2560.0, 1440.0),
                Rect::new(2560.0, 0.0, 4480.0, 1080.0),
            ],
            0,
        );
        assert_eq!(
            screen.at_outer_edge(Side::Bottom, 3000.0, 1079.0),
            Some(3000.0)
        );
        assert_eq!(
            screen.at_outer_edge(Side::Bottom, 1000.0, 1439.0),
            Some(1000.0)
        );
        // Still inside the taller monitor at the shorter one's bottom row.
        assert_eq!(screen.at_outer_edge(Side::Bottom, 1000.0, 1079.0), None);
    }

    #[test]
    fn a_seam_between_two_monitors_is_not_an_edge() {
        let screen = Screen::new(
            vec![
                Rect::new(0.0, 0.0, 1920.0, 1080.0),
                Rect::new(0.0, 1080.0, 1920.0, 2160.0),
            ],
            0,
        );
        assert_eq!(screen.at_outer_edge(Side::Bottom, 960.0, 1079.0), None);
    }

    // Returning from the far PC monitor maps past the Mac's right corner.
    // The cursor should appear at that corner rather than nowhere.
    #[test]
    fn a_return_from_the_far_monitor_clamps_to_the_mac_corner() {
        let (mac, pc) = (mac(), pc());
        let u = 3000.0 - pc.anchor(Side::Bottom, None);
        assert_eq!(u, 2040.0);
        let along = u + mac.anchor(Side::Top, None);
        assert_eq!(along, 2775.0);
        let (x, y) = mac.landing(Side::Top, along, 12.0).expect("has a top edge");
        assert_eq!((x, y), (1469.0, 12.0));
    }

    #[test]
    fn the_default_anchor_is_the_centre_of_the_primary_monitors_segment() {
        assert_eq!(pc().anchor(Side::Bottom, None), 960.0);
    }

    #[test]
    fn an_anchor_override_is_a_fraction_of_the_whole_extent() {
        assert_eq!(pc().anchor(Side::Bottom, Some(0.5)), 1920.0);
        assert_eq!(pc().anchor(Side::Bottom, Some(0.75)), 2880.0);
        // Out of range values clamp rather than escaping the desktop;
        // config rejects them before they ever get here.
        assert_eq!(pc().anchor(Side::Bottom, Some(2.0)), 3840.0);
        assert_eq!(pc().anchor(Side::Bottom, Some(-1.0)), 0.0);
    }

    #[test]
    fn a_primary_monitor_that_does_not_face_the_edge_falls_back_to_the_extent_centre() {
        // Primary stacked above the secondary: only the lower monitor's
        // bottom faces outward, so there is no primary segment to centre on.
        let screen = Screen::new(
            vec![
                Rect::new(0.0, 0.0, 1920.0, 1080.0),
                Rect::new(0.0, 1080.0, 1920.0, 2160.0),
            ],
            0,
        );
        assert_eq!(screen.anchor(Side::Bottom, None), 960.0);
    }

    #[test]
    fn a_position_survives_a_round_trip_through_the_anchor() {
        let (mac, pc) = (mac(), pc());
        for x in [0.0, 300.0, 735.0, 1200.0, 1469.0] {
            let u = x - mac.anchor(Side::Top, None);
            let (px, _) = pc
                .landing(Side::Bottom, u + pc.anchor(Side::Bottom, None), 12.0)
                .unwrap();
            let back = (px - pc.anchor(Side::Bottom, None)) + mac.anchor(Side::Top, None);
            assert!((back - x).abs() < 1.0, "{x} came back as {back}");
        }
    }

    #[test]
    fn clamping_leaves_a_point_inside_a_monitor_alone() {
        assert_eq!(pc().clamp_to_monitors(2500.0, 500.0), (2500.0, 500.0));
    }

    #[test]
    fn clamping_slides_a_dead_zone_point_onto_the_nearest_monitor() {
        // Below the shorter of two monitors is a point on no monitor at
        // all. The union rectangle contained it and left the cursor there.
        let screen = Screen::new(
            vec![
                Rect::new(0.0, 0.0, 2560.0, 1440.0),
                Rect::new(2560.0, 0.0, 4480.0, 1080.0),
            ],
            0,
        );
        assert_eq!(screen.clamp_to_monitors(3000.0, 1300.0), (3000.0, 1079.0));
    }

    #[test]
    fn a_crossing_at_the_mac_corner_lands_at_the_near_end_of_the_pc_edge() {
        let (mac, pc) = (mac(), pc());
        let along = mac
            .at_outer_edge(Side::Top, 0.0, 0.0)
            .expect("the top-left corner is on the top edge");
        assert_eq!(along, 0.0);
        let u = along - mac.anchor(Side::Top, None);
        let (x, y) = pc
            .landing(Side::Bottom, u + pc.anchor(Side::Bottom, None), 12.0)
            .unwrap();
        assert_eq!((x, y), (225.0, 1067.0));
    }

    #[test]
    fn only_the_configured_side_registers_a_crossing() {
        let mac = mac();
        assert!(mac.at_outer_edge(Side::Top, 700.0, 0.0).is_some());
        assert!(mac.at_outer_edge(Side::Bottom, 700.0, 0.0).is_none());
        assert!(mac.at_outer_edge(Side::Left, 700.0, 0.0).is_none());
    }

    #[test]
    fn a_single_monitor_fallback_behaves_like_the_old_union_model() {
        let screen = Screen::single(Rect::from_origin_size(-1920.0, 0.0, 3840.0, 1080.0));
        assert_eq!(
            screen.at_outer_edge(Side::Left, -1920.0, 500.0),
            Some(500.0)
        );
        assert_eq!(screen.at_outer_edge(Side::Bottom, 0.0, 1079.0), Some(0.0));
        assert_eq!(screen.at_outer_edge(Side::Bottom, 0.0, 500.0), None);
    }

    #[test]
    fn a_monitor_thinner_than_the_margin_still_lands_on_it() {
        let screen = Screen::new(vec![Rect::new(0.0, 0.0, 100.0, 8.0)], 0);
        let (x, y) = screen.landing(Side::Bottom, 50.0, 12.0).unwrap();
        assert_eq!((x, y), (50.0, 0.0));
    }

    #[test]
    fn the_resting_point_is_the_centre_of_the_primary_monitor() {
        assert_eq!(pc().resting_point(), Some((960.0, 540.0)));
        assert_eq!(mac().resting_point(), Some((735.0, 478.0)));
    }

    // The whole point of the resting point: it must not be on the edge
    // the cursor crossed through, because that edge is the taskbar on the
    // PC and the menu bar on the Mac, and a cursor left there leaves a
    // hover latched open with nobody watching.
    #[test]
    fn the_resting_point_is_clear_of_every_edge() {
        for screen in [pc(), mac()] {
            let (x, y) = screen.resting_point().expect("has a monitor");
            for side in [Side::Top, Side::Bottom, Side::Left, Side::Right] {
                assert_eq!(
                    screen.at_outer_edge(side, x, y),
                    None,
                    "resting point sits on the {side:?} edge"
                );
            }
        }
    }

    #[test]
    fn a_screen_with_no_monitors_has_no_resting_point() {
        assert_eq!(Screen::new(Vec::new(), 0).resting_point(), None);
    }

    #[test]
    fn parses_the_four_side_names() {
        assert_eq!(Side::parse("top"), Some(Side::Top));
        assert_eq!(Side::parse("bottom"), Some(Side::Bottom));
        assert_eq!(Side::parse("left"), Some(Side::Left));
        assert_eq!(Side::parse("right"), Some(Side::Right));
        assert_eq!(Side::parse("diagonal"), None);
    }
}
