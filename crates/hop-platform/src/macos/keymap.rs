use hop_proto::Usage;

/// macOS virtual keycode to USB HID usage. Sourced from Carbon's
/// `Events.h` keycodes (the `kVK_*` constants) paired with the HID
/// Keyboard usage page.
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
    (51, 0x2A),  // delete (backspace)
    (53, 0x29),  // escape
    (54, 0xE7),  // right command (kVK_RightCommand)
    (55, 0xE3),  // command
    (56, 0xE1),  // shift
    (57, 0x39),  // caps lock
    (58, 0xE2),  // option
    (59, 0xE0),  // control
    (60, 0xE5),  // right shift
    (61, 0xE6),  // right option
    (62, 0xE4),  // right control
    (64, 0x6C),  // f17 (kVK_F17)
    (65, 0x63),  // keypad . (kVK_ANSI_KeypadDecimal)
    (67, 0x55),  // keypad * (kVK_ANSI_KeypadMultiply)
    (69, 0x57),  // keypad + (kVK_ANSI_KeypadPlus)
    (71, 0x53),  // keypad num lock / clear (kVK_ANSI_KeypadClear)
    (75, 0x54),  // keypad / (kVK_ANSI_KeypadDivide)
    (76, 0x58),  // keypad enter (kVK_ANSI_KeypadEnter)
    (78, 0x56),  // keypad - (kVK_ANSI_KeypadMinus)
    (79, 0x6D),  // f18 (kVK_F18)
    (80, 0x6E),  // f19 (kVK_F19)
    (81, 0x67),  // keypad = (kVK_ANSI_KeypadEquals)
    (82, 0x62),  // keypad 0
    (83, 0x59),  // keypad 1
    (84, 0x5A),  // keypad 2
    (85, 0x5B),  // keypad 3
    (86, 0x5C),  // keypad 4
    (87, 0x5D),  // keypad 5
    (88, 0x5E),  // keypad 6
    (89, 0x5F),  // keypad 7
    (90, 0x6F),  // f20 (kVK_F20)
    (91, 0x60),  // keypad 8
    (92, 0x61),  // keypad 9
    (96, 0x3E),  // f5
    (97, 0x3F),  // f6
    (98, 0x40),  // f7
    (99, 0x3C),  // f3
    (100, 0x41), // f8
    (101, 0x42), // f9
    (103, 0x44), // f11
    (105, 0x68), // f13 (kVK_F13)
    (106, 0x6B), // f16 (kVK_F16)
    (107, 0x69), // f14 (kVK_F14)
    (109, 0x43), // f10
    (111, 0x45), // f12
    (113, 0x6A), // f15 (kVK_F15)
    (115, 0x4A), // home (kVK_Home)
    (116, 0x4B), // page up (kVK_PageUp)
    (117, 0x4C), // forward delete (kVK_ForwardDelete)
    (118, 0x3D), // f4
    (119, 0x4D), // end (kVK_End)
    (120, 0x3B), // f2
    (121, 0x4E), // page down (kVK_PageDown)
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

    #[test]
    fn right_command_maps_to_right_gui() {
        // FINDING 3: keycode 54 is Right Command. It was missing
        // entirely, so a key that appears in the default remap table and
        // in the spec's example config could never actually be
        // captured. `device_bit_for_keycode` in capture.rs already
        // handles 54; this table must too.
        assert_eq!(virtual_key_to_usage(54), Some(Usage::RIGHT_GUI));
        // And left command (55) must remain distinct, or the reverse
        // lookup in `usage_to_virtual_key` would be ambiguous between
        // the two.
        assert_eq!(virtual_key_to_usage(55), Some(Usage::LEFT_GUI));
        assert_eq!(usage_to_virtual_key(Usage::RIGHT_GUI), Some(54));
        assert_eq!(usage_to_virtual_key(Usage::LEFT_GUI), Some(55));
    }

    #[test]
    fn table_stays_injective() {
        // Two macOS keys mapping to the same HID usage would make
        // `usage_to_virtual_key`'s reverse lookup ambiguous, silently
        // picking whichever entry comes first.
        let mut seen = std::collections::HashSet::new();
        for (_, usage) in TABLE {
            assert!(seen.insert(*usage), "duplicate HID usage {usage:#x}");
        }
    }

    #[test]
    fn navigation_and_forward_delete_round_trip() {
        // Home, End, Page Up, Page Down and forward delete: previously
        // silently dropped (FINDING 4). Verify both directions and the
        // exact HID usage codes standard keyboards expect.
        assert_eq!(virtual_key_to_usage(115), Some(Usage(0x4A))); // home
        assert_eq!(virtual_key_to_usage(116), Some(Usage(0x4B))); // page up
        assert_eq!(virtual_key_to_usage(117), Some(Usage(0x4C))); // forward delete
        assert_eq!(virtual_key_to_usage(119), Some(Usage(0x4D))); // end
        assert_eq!(virtual_key_to_usage(121), Some(Usage(0x4E))); // page down

        for code in [115_i64, 116, 117, 119, 121] {
            let usage = virtual_key_to_usage(code).unwrap();
            assert_eq!(usage_to_virtual_key(usage), Some(code));
        }
    }

    #[test]
    fn numeric_keypad_and_f13_and_above_are_mapped() {
        // The numeric keypad and F13+ were entirely absent (FINDING 4).
        assert_eq!(virtual_key_to_usage(82), Some(Usage(0x62))); // keypad 0
        assert_eq!(virtual_key_to_usage(83), Some(Usage(0x59))); // keypad 1
        assert_eq!(virtual_key_to_usage(76), Some(Usage(0x58))); // keypad enter
        assert_eq!(virtual_key_to_usage(105), Some(Usage(0x68))); // f13
        assert_eq!(virtual_key_to_usage(90), Some(Usage(0x6F))); // f20

        for code in [
            65_i64, 67, 69, 71, 75, 76, 78, 81, 82, 83, 84, 85, 86, 87, 88, 89, 91, 92,
        ] {
            let usage = virtual_key_to_usage(code).unwrap();
            assert_eq!(usage_to_virtual_key(usage), Some(code), "keypad key {code}");
        }
        for code in [64_i64, 79, 80, 90, 105, 106, 107, 113] {
            let usage = virtual_key_to_usage(code).unwrap();
            assert_eq!(
                usage_to_virtual_key(usage),
                Some(code),
                "function key {code}"
            );
        }
    }
}
