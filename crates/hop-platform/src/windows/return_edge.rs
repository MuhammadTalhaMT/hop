//! The pure decision behind CRITICAL 2 from the whole-branch review:
//! `Message::Release` was defined, encoded, decoded, tested, and handled
//! by the server, but nothing in the workspace ever sent it, so focus
//! could only ever come home through the panic hotkey (which lives only
//! on the Mac) or a dead link.
//!
//! The Windows client is the only side that can fix this: it is the one
//! that injects motion, so it is the one that can see where the real
//! cursor actually lands. This module holds the part of that decision
//! that has nothing to do with `GetCursorPos` or `GetSystemMetrics`, so
//! it is testable on any host, including the macOS machine this project
//! is developed on. The actual Windows API calls live in
//! `crate::windows::inject`, gated to `cfg(target_os = "windows")`.
//!
//! Not gated on `cfg(target_os = "windows")` at the module level, for the
//! same reason `windows::keymap` is not: everything here is pure data and
//! logic with no `windows-sys` calls, so it is built and tested on every
//! host.

use hop_core::{Screen, Side};

/// The edge of the Windows virtual screen whose crossing hands focus back
/// to the server. This is the mirror image of the server's own
/// configured `[layout]` edge (see `hop_platform::macos::Edge` and
/// `hop::config::Layout`): the reference deployment has the PC's
/// monitors mounted above the Mac, so the server is configured
/// `top = "pc"` and the client is configured `return_edge = "bottom"`.
/// Nothing here assumes bottom specifically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnEdge {
    Top,
    Bottom,
    Left,
    Right,
}

impl From<ReturnEdge> for Side {
    fn from(edge: ReturnEdge) -> Self {
        match edge {
            ReturnEdge::Top => Side::Top,
            ReturnEdge::Bottom => Side::Bottom,
            ReturnEdge::Left => Side::Left,
            ReturnEdge::Right => Side::Right,
        }
    }
}

impl ReturnEdge {
    /// Parses a `[input] return_edge` config value the same way
    /// `hop::config`'s `[layout]` edge names are parsed. `None` for
    /// anything else, so an unrecognized value is a named config error
    /// at load time rather than a silently-ignored typo.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "top" => Some(Self::Top),
            "bottom" => Some(Self::Bottom),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

/// Where the real cursor is, and what monitors this PC has. A trait
/// rather than a pair of free functions so the decision below can be unit
/// tested against a fake without a live Windows desktop;
/// `crate::windows::inject` holds the only real implementation.
pub trait CursorSource {
    /// The cursor's current position, or `None` if the platform call
    /// failed. `None` must never be treated as "at the edge": a query
    /// failure is not evidence the user's hand is on the boundary.
    fn cursor_position(&self) -> Option<(i32, i32)>;
}

/// Whether the cursor `source` reports has reached an outward-facing part
/// of `edge`, and if so how far along it is, in pixels relative to this
/// machine's anchor (see [`Screen::anchor`]).
///
/// `None` whenever the cursor position cannot be read at all, matching
/// `cursor_position`'s doc comment above, and `None` on a seam between
/// two of this PC's own monitors, which is a cursor moving between
/// monitors rather than leaving the machine.
pub fn return_crossing(
    edge: ReturnEdge,
    anchor_override: Option<f32>,
    screen: &Screen,
    position: Option<(i32, i32)>,
) -> Option<f32> {
    let (x, y) = position?;
    let side = Side::from(edge);
    let along = screen.at_outer_edge(side, f64::from(x), f64::from(y))?;
    Some((along - screen.anchor(side, anchor_override)) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hop_core::Rect;

    struct FakeSource {
        position: Option<(i32, i32)>,
        screen: Screen,
    }

    fn one_monitor() -> Screen {
        Screen::new(vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)], 0)
    }

    fn released(edge: ReturnEdge, source: &FakeSource) -> bool {
        return_crossing(edge, None, &source.screen, source.position).is_some()
    }

    #[test]
    fn parses_the_four_edge_names() {
        assert_eq!(ReturnEdge::parse("top"), Some(ReturnEdge::Top));
        assert_eq!(ReturnEdge::parse("bottom"), Some(ReturnEdge::Bottom));
        assert_eq!(ReturnEdge::parse("left"), Some(ReturnEdge::Left));
        assert_eq!(ReturnEdge::parse("right"), Some(ReturnEdge::Right));
    }

    #[test]
    fn an_unknown_edge_name_is_none_rather_than_a_guess() {
        assert_eq!(ReturnEdge::parse("diagonal"), None);
        assert_eq!(ReturnEdge::parse(""), None);
    }

    // "cursor at the return edge produces a Release; cursor elsewhere
    // does not" (see CRITICAL 2 in the whole-branch review), at the
    // level this module can actually test: the pure should_release
    // decision. The wiring that turns `true` into an actual
    // Message::Release send is covered in hop-core's supervisor tests.
    #[test]
    fn cursor_at_the_return_edge_says_release() {
        let source = FakeSource {
            position: Some((960, 1079)),
            screen: one_monitor(),
        };
        // The anchor of a single monitor is its centre, so a release from
        // the centre travels as zero.
        assert_eq!(
            return_crossing(ReturnEdge::Bottom, None, &source.screen, source.position),
            Some(0.0)
        );
    }

    #[test]
    fn the_release_carries_where_along_the_edge_the_cursor_left() {
        let source = FakeSource {
            position: Some((1400, 1079)),
            screen: one_monitor(),
        };
        assert_eq!(
            return_crossing(ReturnEdge::Bottom, None, &source.screen, source.position),
            Some(440.0)
        );
    }

    #[test]
    fn cursor_elsewhere_says_no_release() {
        let source = FakeSource {
            position: Some((960, 500)),
            screen: one_monitor(),
        };
        assert!(!released(ReturnEdge::Bottom, &source));
    }

    // The stranded-focus case: two monitors top-aligned but different
    // heights. The shorter one's bottom row is 360 pixels above the
    // union's bottom row, so union-based detection never fired there.
    #[test]
    fn the_bottom_of_a_shorter_monitor_still_releases() {
        let source = FakeSource {
            position: Some((3000, 1079)),
            screen: Screen::new(
                vec![
                    Rect::new(0.0, 0.0, 2560.0, 1440.0),
                    Rect::new(2560.0, 0.0, 4480.0, 1080.0),
                ],
                0,
            ),
        };
        assert!(released(ReturnEdge::Bottom, &source));
    }

    // Moving between this PC's own monitors must never hand focus back.
    #[test]
    fn a_seam_between_two_monitors_never_releases() {
        let source = FakeSource {
            position: Some((960, 1079)),
            screen: Screen::new(
                vec![
                    Rect::new(0.0, 0.0, 1920.0, 1080.0),
                    Rect::new(0.0, 1080.0, 1920.0, 2160.0),
                ],
                0,
            ),
        };
        assert!(!released(ReturnEdge::Bottom, &source));
    }

    #[test]
    fn only_the_configured_edge_triggers_a_release() {
        // At the bottom-right corner, only Bottom and Right should read
        // as reached; a client configured for Top or Left must not fire
        // early just because the cursor happens to be in a corner.
        let source = FakeSource {
            position: Some((1919, 1079)),
            screen: one_monitor(),
        };
        assert!(released(ReturnEdge::Bottom, &source));
        assert!(released(ReturnEdge::Right, &source));
        assert!(!released(ReturnEdge::Top, &source));
        assert!(!released(ReturnEdge::Left, &source));
    }

    #[test]
    fn a_failed_position_read_never_triggers_a_release() {
        let source = FakeSource {
            position: None,
            screen: one_monitor(),
        };
        assert!(!released(ReturnEdge::Bottom, &source));
    }

    #[test]
    fn a_secondary_monitor_to_the_left_puts_the_desktop_at_a_negative_origin() {
        // A monitor extending left of or above the primary gives negative
        // coordinates. A cursor at that monitor's left edge must still
        // read as reached.
        let source = FakeSource {
            position: Some((-1920, 500)),
            screen: Screen::new(
                vec![
                    Rect::new(-1920.0, 0.0, 0.0, 1080.0),
                    Rect::new(0.0, 0.0, 1920.0, 1080.0),
                ],
                1,
            ),
        };
        assert!(released(ReturnEdge::Left, &source));
        assert!(!released(ReturnEdge::Right, &source));
    }
}
