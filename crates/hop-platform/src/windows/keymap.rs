use hop_proto::Usage;

/// USB HID usage to PS/2 set 1 scancode, with the extended-key flag.
///
/// Entries are `(hid_usage, scancode, extended)`. Extended keys share a
/// base scancode with a non-extended key and are distinguished by a 0xE0
/// prefix, which `SendInput` expresses with `KEYEVENTF_EXTENDEDKEY`.
pub(crate) const TABLE: &[(u16, u16, bool)] = &[
    (0x04, 0x1E, false), // a
    (0x05, 0x30, false), // b
    (0x06, 0x2E, false), // c
    (0x07, 0x20, false), // d
    (0x08, 0x12, false), // e
    (0x09, 0x21, false), // f
    (0x0A, 0x22, false), // g
    (0x0B, 0x23, false), // h
    (0x0C, 0x17, false), // i
    (0x0D, 0x24, false), // j
    (0x0E, 0x25, false), // k
    (0x0F, 0x26, false), // l
    (0x10, 0x32, false), // m
    (0x11, 0x31, false), // n
    (0x12, 0x18, false), // o
    (0x13, 0x19, false), // p
    (0x14, 0x10, false), // q
    (0x15, 0x13, false), // r
    (0x16, 0x1F, false), // s
    (0x17, 0x14, false), // t
    (0x18, 0x16, false), // u
    (0x19, 0x2F, false), // v
    (0x1A, 0x11, false), // w
    (0x1B, 0x2D, false), // x
    (0x1C, 0x15, false), // y
    (0x1D, 0x2C, false), // z
    (0x1E, 0x02, false), // 1
    (0x1F, 0x03, false), // 2
    (0x20, 0x04, false), // 3
    (0x21, 0x05, false), // 4
    (0x22, 0x06, false), // 5
    (0x23, 0x07, false), // 6
    (0x24, 0x08, false), // 7
    (0x25, 0x09, false), // 8
    (0x26, 0x0A, false), // 9
    (0x27, 0x0B, false), // 0
    (0x28, 0x1C, false), // return
    (0x29, 0x01, false), // escape
    (0x2A, 0x0E, false), // backspace
    (0x2B, 0x0F, false), // tab
    (0x2C, 0x39, false), // space
    (0x2D, 0x0C, false), // -
    (0x2E, 0x0D, false), // =
    (0x2F, 0x1A, false), // [
    (0x30, 0x1B, false), // ]
    (0x31, 0x2B, false), // backslash
    (0x33, 0x27, false), // ;
    (0x34, 0x28, false), // '
    (0x35, 0x29, false), // `
    (0x36, 0x33, false), // ,
    (0x37, 0x34, false), // .
    (0x38, 0x35, false), // /
    (0x39, 0x3A, false), // caps lock
    (0x3A, 0x3B, false), // f1
    (0x3B, 0x3C, false), // f2
    (0x3C, 0x3D, false), // f3
    (0x3D, 0x3E, false), // f4
    (0x3E, 0x3F, false), // f5
    (0x3F, 0x40, false), // f6
    (0x40, 0x41, false), // f7
    (0x41, 0x42, false), // f8
    (0x42, 0x43, false), // f9
    (0x43, 0x44, false), // f10
    (0x44, 0x57, false), // f11
    (0x45, 0x58, false), // f12
    (0x4F, 0x4D, true),  // right arrow
    (0x50, 0x4B, true),  // left arrow
    (0x51, 0x50, true),  // down arrow
    (0x52, 0x48, true),  // up arrow
    (0xE0, 0x1D, false), // left control
    (0xE1, 0x2A, false), // left shift
    (0xE2, 0x38, false), // left alt
    (0xE3, 0x5B, true),  // left gui (windows key)
    (0xE4, 0x1D, true),  // right control
    (0xE5, 0x36, false), // right shift
    (0xE6, 0x38, true),  // right alt
    (0xE7, 0x5C, true),  // right gui
];

pub fn usage_to_scancode(usage: Usage) -> Option<(u16, bool)> {
    TABLE
        .iter()
        .find(|(u, _, _)| *u == usage.0)
        .map(|(_, code, extended)| (*code, *extended))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_letters_to_set_one_scancodes() {
        assert_eq!(usage_to_scancode(Usage::A), Some((0x1E, false)));
        assert_eq!(usage_to_scancode(Usage::C), Some((0x2E, false)));
    }

    #[test]
    fn maps_modifiers_including_extended_flag() {
        assert_eq!(usage_to_scancode(Usage::LEFT_CTRL), Some((0x1D, false)));
        // Right control shares the base code and is distinguished only by
        // the extended flag. Getting this wrong sticks the wrong modifier.
        assert_eq!(usage_to_scancode(Usage::RIGHT_CTRL), Some((0x1D, true)));
        assert_eq!(usage_to_scancode(Usage::LEFT_ALT), Some((0x38, false)));
        assert_eq!(usage_to_scancode(Usage::RIGHT_ALT), Some((0x38, true)));
    }

    #[test]
    fn unmapped_usages_are_none() {
        assert_eq!(usage_to_scancode(Usage(0xFFFF)), None);
    }

    #[test]
    fn every_entry_is_reachable_and_distinct() {
        // A duplicated (code, extended) pair would silently type the wrong
        // key for one of the two usages.
        let mut seen = std::collections::HashSet::new();
        for (_, code, extended) in TABLE {
            assert!(
                seen.insert((*code, *extended)),
                "duplicate scancode {code:#x}"
            );
        }
    }
}
