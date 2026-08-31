//! Human readable key names, as written in a config file's
//! `[peers.remap]` table or (in a later task) a panic hotkey string,
//! mapped to their USB HID usage codes.
//!
//! Names follow the spelling used in the spec's config examples, for
//! example "LeftGui", "A", "F1", "Up". Lookup never panics: an unknown
//! name simply returns `None`, and callers are expected to turn that into
//! an error that names the offending string rather than dropping it.

use hop_proto::Usage;

/// Look up a key name, returning its HID usage code, or `None` if the
/// name is not recognized.
pub fn lookup(name: &str) -> Option<Usage> {
    modifier(name)
        .or_else(|| letter(name))
        .or_else(|| digit(name))
        .or_else(|| arrow(name))
        .or_else(|| function_key(name))
        .or(if name == "Escape" {
            Some(Usage::ESCAPE)
        } else {
            None
        })
}

fn modifier(name: &str) -> Option<Usage> {
    Some(match name {
        "LeftCtrl" => Usage::LEFT_CTRL,
        "LeftShift" => Usage::LEFT_SHIFT,
        "LeftAlt" => Usage::LEFT_ALT,
        "LeftGui" => Usage::LEFT_GUI,
        "RightCtrl" => Usage::RIGHT_CTRL,
        "RightShift" => Usage::RIGHT_SHIFT,
        "RightAlt" => Usage::RIGHT_ALT,
        "RightGui" => Usage::RIGHT_GUI,
        _ => return None,
    })
}

/// A single uppercase ASCII letter, "A" through "Z".
fn letter(name: &str) -> Option<Usage> {
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if c.is_ascii_uppercase() {
        Some(Usage(0x04 + (c as u16 - 'A' as u16)))
    } else {
        None
    }
}

/// A single digit, "0" through "9". HID orders these 1..=9 then 0.
fn digit(name: &str) -> Option<Usage> {
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match c {
        '1'..='9' => Some(Usage(0x1E + (c as u16 - '1' as u16))),
        '0' => Some(Usage(0x27)),
        _ => None,
    }
}

fn arrow(name: &str) -> Option<Usage> {
    Some(match name {
        "Up" => Usage(0x52),
        "Down" => Usage(0x51),
        "Left" => Usage(0x50),
        "Right" => Usage(0x4F),
        _ => return None,
    })
}

/// "F1" through "F12".
fn function_key(name: &str) -> Option<Usage> {
    let rest = name.strip_prefix('F')?;
    let n: u16 = rest.parse().ok()?;
    if (1..=12).contains(&n) {
        Some(Usage(0x39 + n))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_all_eight_modifiers() {
        assert_eq!(lookup("LeftCtrl"), Some(Usage::LEFT_CTRL));
        assert_eq!(lookup("LeftShift"), Some(Usage::LEFT_SHIFT));
        assert_eq!(lookup("LeftAlt"), Some(Usage::LEFT_ALT));
        assert_eq!(lookup("LeftGui"), Some(Usage::LEFT_GUI));
        assert_eq!(lookup("RightCtrl"), Some(Usage::RIGHT_CTRL));
        assert_eq!(lookup("RightShift"), Some(Usage::RIGHT_SHIFT));
        assert_eq!(lookup("RightAlt"), Some(Usage::RIGHT_ALT));
        assert_eq!(lookup("RightGui"), Some(Usage::RIGHT_GUI));
    }

    #[test]
    fn recognizes_letters() {
        assert_eq!(lookup("A"), Some(Usage::A));
        assert_eq!(lookup("C"), Some(Usage::C));
        assert_eq!(lookup("V"), Some(Usage::V));
        assert_eq!(lookup("Z"), Some(Usage(0x1D)));
        assert_eq!(lookup("a"), None, "names are case sensitive");
    }

    #[test]
    fn recognizes_digits() {
        assert_eq!(lookup("1"), Some(Usage(0x1E)));
        assert_eq!(lookup("9"), Some(Usage(0x26)));
        assert_eq!(lookup("0"), Some(Usage(0x27)));
    }

    #[test]
    fn recognizes_arrow_keys() {
        assert_eq!(lookup("Up"), Some(Usage(0x52)));
        assert_eq!(lookup("Down"), Some(Usage(0x51)));
        assert_eq!(lookup("Left"), Some(Usage(0x50)));
        assert_eq!(lookup("Right"), Some(Usage(0x4F)));
    }

    #[test]
    fn recognizes_function_keys() {
        assert_eq!(lookup("F1"), Some(Usage(0x3A)));
        assert_eq!(lookup("F12"), Some(Usage(0x45)));
        assert_eq!(lookup("F13"), None, "only F1..=F12 are defined");
    }

    #[test]
    fn recognizes_escape() {
        assert_eq!(lookup("Escape"), Some(Usage::ESCAPE));
    }

    #[test]
    fn unknown_names_return_none() {
        assert_eq!(lookup("NotAKey"), None);
        assert_eq!(lookup(""), None);
    }
}
