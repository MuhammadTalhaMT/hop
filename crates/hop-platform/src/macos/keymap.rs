use hop_proto::Usage;

/// macOS virtual keycode to USB HID usage. Sourced from Carbon's
/// `Events.h` keycodes paired with the HID Keyboard usage page.
///
/// Entries are `(macos_virtual_keycode, hid_usage)`. Keep this sorted by
/// keycode and keep it injective: two macOS keys must never map to one
/// usage, or the reverse lookup would be ambiguous.
const TABLE: &[(i64, u16)] = &[
    (0, 0x04),   // a
    (1, 0x16),   // s
    (2, 0x07),   // d
    (3, 0x09),   // f
    (4, 0x0B),   // h
    (5, 0x0A),   // g
    (6, 0x1D),   // z
    (7, 0x1B),   // x
    (8, 0x06),   // c
    (9, 0x19),   // v
    (11, 0x05),  // b
    (12, 0x14),  // q
    (13, 0x1A),  // w
    (14, 0x08),  // e
    (15, 0x15),  // r
    (16, 0x1C),  // y
    (17, 0x17),  // t
    (18, 0x1E),  // 1
    (19, 0x1F),  // 2
    (20, 0x20),  // 3
    (21, 0x21),  // 4
    (22, 0x23),  // 6
    (23, 0x22),  // 5
    (24, 0x2E),  // =
    (25, 0x26),  // 9
    (26, 0x24),  // 7
    (27, 0x2D),  // -
    (28, 0x25),  // 8
    (29, 0x27),  // 0
    (30, 0x30),  // ]
    (31, 0x12),  // o
    (32, 0x18),  // u
    (33, 0x2F),  // [
    (34, 0x0C),  // i
    (35, 0x13),  // p
    (36, 0x28),  // return
    (37, 0x0F),  // l
    (38, 0x0D),  // j
    (39, 0x34),  // '
    (40, 0x0E),  // k
    (41, 0x33),  // ;
    (42, 0x31),  // backslash
    (43, 0x36),  // ,
    (44, 0x38),  // /
    (45, 0x11),  // n
    (46, 0x10),  // m
    (47, 0x37),  // .
    (48, 0x2B),  // tab
    (49, 0x2C),  // space
    (50, 0x35),  // `
    (51, 0x2A),  // delete
    (53, 0x29),  // escape
    (55, 0xE3),  // command
    (56, 0xE1),  // shift
    (57, 0x39),  // caps lock
    (58, 0xE2),  // option
    (59, 0xE0),  // control
    (60, 0xE5),  // right shift
    (61, 0xE6),  // right option
    (62, 0xE4),  // right control
    (96, 0x3E),  // f5
    (97, 0x3F),  // f6
    (98, 0x40),  // f7
    (99, 0x3C),  // f3
    (100, 0x41), // f8
    (101, 0x42), // f9
    (103, 0x44), // f11
    (109, 0x43), // f10
    (111, 0x45), // f12
    (118, 0x3D), // f4
    (120, 0x3B), // f2
    (122, 0x3A), // f1
    (123, 0x50), // left arrow
    (124, 0x4F), // right arrow
    (125, 0x51), // down arrow
    (126, 0x52), // up arrow
];

pub fn virtual_key_to_usage(code: i64) -> Option<Usage> {
    TABLE
        .iter()
        .find(|(vk, _)| *vk == code)
        .map(|(_, usage)| Usage(*usage))
}

pub fn usage_to_virtual_key(usage: Usage) -> Option<i64> {
    TABLE.iter().find(|(_, u)| *u == usage.0).map(|(vk, _)| *vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_letters_correctly() {
        // macOS virtual keycode 0 is 'a', which is HID usage 0x04.
        assert_eq!(virtual_key_to_usage(0), Some(Usage::A));
        assert_eq!(virtual_key_to_usage(8), Some(Usage::C));
    }

    #[test]
    fn maps_modifiers_correctly() {
        // 55 is Command, which must become the GUI usage so that the
        // remap layer can turn it into Control for Windows.
        assert_eq!(virtual_key_to_usage(55), Some(Usage::LEFT_GUI));
        assert_eq!(virtual_key_to_usage(59), Some(Usage::LEFT_CTRL));
        assert_eq!(virtual_key_to_usage(58), Some(Usage::LEFT_ALT));
        assert_eq!(virtual_key_to_usage(56), Some(Usage::LEFT_SHIFT));
    }

    #[test]
    fn round_trips_every_known_key() {
        for code in 0..=0x7F {
            if let Some(usage) = virtual_key_to_usage(code) {
                assert_eq!(
                    usage_to_virtual_key(usage),
                    Some(code),
                    "keycode {code} did not round trip"
                );
            }
        }
    }

    #[test]
    fn unknown_keycodes_are_none_rather_than_wrong() {
        // Guessing at an unmapped key would type the wrong character on
        // the peer, which is worse than dropping it.
        assert_eq!(virtual_key_to_usage(9999), None);
    }
}
