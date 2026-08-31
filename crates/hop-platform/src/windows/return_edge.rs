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

/// Where the real cursor is, and how big the Windows virtual screen is,
/// both in the same coordinate space `GetCursorPos` and
/// `GetSystemMetrics(SM_XVIRTUALSCREEN, ...)` report. A trait rather than
/// a pair of free functions so the decision below can be unit tested
/// against a fake without a live Windows desktop; `crate::windows::inject`
/// holds the only real implementation.
pub trait CursorSource {
    /// The cursor's current position, or `None` if the platform call
    /// failed. `None` must never be treated as "at the edge": a query
    /// failure is not evidence the user's hand is on the boundary.
    fn cursor_position(&self) -> Option<(i32, i32)>;

    /// The virtual screen's bounds as `(left, top, width, height)`,
    /// covering every monitor rather than just the primary one, so a
    /// multi-monitor PC is handled correctly.
    fn virtual_screen(&self) -> (i32, i32, i32, i32);
}

/// Whether cursor position `(x, y)` has reached `edge` of a virtual
/// screen with origin `(left, top)` and size `(width, height)`. Pure and
/// side effect free: the same shape as `hop_platform::macos::capture`'s
/// `crossed`, just on the returning side of the handoff.
fn reached_edge(
    edge: ReturnEdge,
    x: i32,
    y: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
) -> bool {
    match edge {
        ReturnEdge::Top => y <= top,
        ReturnEdge::Bottom => y >= top + height - 1,
        ReturnEdge::Left => x <= left,
        ReturnEdge::Right => x >= left + width - 1,
    }
}

/// Whether the cursor `source` reports has reached `edge`, and therefore
/// whether `Message::Release` should be sent. `false` whenever the
/// cursor position cannot be read at all, matching `cursor_position`'s
/// doc comment above.
pub fn should_release<C: CursorSource>(edge: ReturnEdge, source: &C) -> bool {
    let Some((x, y)) = source.cursor_position() else {
        return false;
    };
    let (left, top, width, height) = source.virtual_screen();
    reached_edge(edge, x, y, left, top, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSource {
        position: Option<(i32, i32)>,
        virtual_screen: (i32, i32, i32, i32),
    }

    impl CursorSource for FakeSource {
        fn cursor_position(&self) -> Option<(i32, i32)> {
            self.position
        }

        fn virtual_screen(&self) -> (i32, i32, i32, i32) {
            self.virtual_screen
        }
    }

    const SCREEN: (i32, i32, i32, i32) = (0, 0, 1920, 1080);

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
            virtual_screen: SCREEN,
        };
        assert!(should_release(ReturnEdge::Bottom, &source));
    }

    #[test]
    fn cursor_elsewhere_says_no_release() {
        let source = FakeSource {
            position: Some((960, 500)),
            virtual_screen: SCREEN,
        };
        assert!(!should_release(ReturnEdge::Bottom, &source));
    }

    #[test]
    fn only_the_configured_edge_triggers_a_release() {
        // At the bottom-right corner, only Bottom and Right should read
        // as reached; a client configured for Top or Left must not fire
        // early just because the cursor happens to be in a corner.
        let source = FakeSource {
            position: Some((1919, 1079)),
            virtual_screen: SCREEN,
        };
        assert!(should_release(ReturnEdge::Bottom, &source));
        assert!(should_release(ReturnEdge::Right, &source));
        assert!(!should_release(ReturnEdge::Top, &source));
        assert!(!should_release(ReturnEdge::Left, &source));
    }

    #[test]
    fn a_failed_position_read_never_triggers_a_release() {
        let source = FakeSource {
            position: None,
            virtual_screen: SCREEN,
        };
        assert!(!should_release(ReturnEdge::Bottom, &source));
    }

    #[test]
    fn a_secondary_monitor_to_the_left_is_handled_via_virtual_screen_origin() {
        // SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN can be negative when a
        // monitor extends left or above the primary display, so the
        // origin is not always (0, 0). A cursor at that origin's left
        // edge must still read as reached.
        let source = FakeSource {
            position: Some((-1920, 500)),
            virtual_screen: (-1920, 0, 3840, 1080),
        };
        assert!(should_release(ReturnEdge::Left, &source));
        assert!(!should_release(ReturnEdge::Right, &source));
    }
}
