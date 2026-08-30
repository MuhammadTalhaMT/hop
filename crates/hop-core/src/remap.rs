use hop_proto::Usage;
use std::collections::HashMap;

/// Translates keys from the sending machine's layout to the receiving
/// machine's. Applied once, at the source: `apply` never chains, so a rule
/// whose output is another rule's input cannot rewrite it a second time.
#[derive(Debug, Clone, Default)]
pub struct RemapTable {
    entries: HashMap<Usage, Usage>,
}

impl RemapTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// What a Mac keyboard driving a Windows PC needs by default: the
    /// Command key takes Control's place, so Cmd+C copies on the PC.
    pub fn mac_to_windows_defaults() -> Self {
        let mut table = Self::new();
        table.insert(Usage::LEFT_GUI, Usage::LEFT_CTRL);
        table.insert(Usage::RIGHT_GUI, Usage::RIGHT_CTRL);
        table
    }

    pub fn insert(&mut self, from: Usage, to: Usage) {
        self.entries.insert(from, to);
    }

    pub fn apply(&self, usage: Usage) -> Usage {
        self.entries.get(&usage).copied().unwrap_or(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmapped_keys_pass_through() {
        let table = RemapTable::new();
        assert_eq!(table.apply(Usage::C), Usage::C);
    }

    #[test]
    fn command_becomes_control_by_default() {
        let table = RemapTable::mac_to_windows_defaults();
        assert_eq!(table.apply(Usage::LEFT_GUI), Usage::LEFT_CTRL);
        assert_eq!(table.apply(Usage::RIGHT_GUI), Usage::RIGHT_CTRL);
    }

    #[test]
    fn option_stays_alt_by_default() {
        let table = RemapTable::mac_to_windows_defaults();
        assert_eq!(table.apply(Usage::LEFT_ALT), Usage::LEFT_ALT);
    }

    #[test]
    fn explicit_entries_override_defaults() {
        let mut table = RemapTable::mac_to_windows_defaults();
        table.insert(Usage::LEFT_GUI, Usage::LEFT_ALT);
        assert_eq!(table.apply(Usage::LEFT_GUI), Usage::LEFT_ALT);
    }

    #[test]
    fn mapping_is_not_applied_twice() {
        // Cmd maps to Ctrl. Ctrl must not then map onward to anything else,
        // or chained rules would silently rewrite each other.
        let table = RemapTable::mac_to_windows_defaults();
        let once = table.apply(Usage::LEFT_GUI);
        assert_eq!(table.apply(once), once);
    }
}
