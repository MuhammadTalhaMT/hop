use serde::{Deserialize, Serialize};

/// A key identified by its USB HID usage code (Keyboard page, 0x07).
///
/// Using HID codes keeps the wire format platform neutral: each platform
/// translates its native codes to and from this type at the edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Usage(pub u16);

impl Usage {
    pub const A: Usage = Usage(0x04);
    pub const C: Usage = Usage(0x06);
    pub const V: Usage = Usage(0x19);
    pub const ESCAPE: Usage = Usage(0x29);

    pub const LEFT_CTRL: Usage = Usage(0xE0);
    pub const LEFT_SHIFT: Usage = Usage(0xE1);
    pub const LEFT_ALT: Usage = Usage(0xE2);
    pub const LEFT_GUI: Usage = Usage(0xE3);
    pub const RIGHT_CTRL: Usage = Usage(0xE4);
    pub const RIGHT_SHIFT: Usage = Usage(0xE5);
    pub const RIGHT_ALT: Usage = Usage(0xE6);
    pub const RIGHT_GUI: Usage = Usage(0xE7);

    /// True for the eight modifier keys, which occupy 0xE0..=0xE7 in the
    /// HID table. Modifiers matter separately because a modifier stuck
    /// down on the receiving side is the worst failure mode this tool has.
    pub fn is_modifier(&self) -> bool {
        (0xE0..=0xE7).contains(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_are_recognized() {
        assert!(Usage::LEFT_GUI.is_modifier());
        assert!(Usage::RIGHT_CTRL.is_modifier());
        assert!(!Usage::A.is_modifier());
    }

    #[test]
    fn usage_values_match_hid_spec() {
        assert_eq!(Usage::A.0, 0x04);
        assert_eq!(Usage::LEFT_CTRL.0, 0xE0);
        assert_eq!(Usage::LEFT_GUI.0, 0xE3);
        assert_eq!(Usage::RIGHT_GUI.0, 0xE7);
    }
}
