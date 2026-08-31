//! Keeping two machines' clipboards in step.
//!
//! Neither macOS nor Windows offers a notification when the clipboard
//! changes; both expose a counter that increments on every write. So the
//! only way to notice a copy is to poll that counter, which is cheap
//! precisely because it does not read the contents.
//!
//! The subtle part is not the polling, it is not echoing. Applying a
//! clipboard received from the peer is itself a local clipboard write, so
//! it bumps our own counter. Left alone, each side would forward the
//! other's clipboard straight back, forever. [`ClipboardSync`] exists to
//! make that impossible rather than merely unlikely.

/// A system clipboard. Implemented per platform in `hop-platform`, and by
/// [`FakeClipboard`] for tests.
pub trait Clipboard {
    /// A counter the system bumps on every clipboard write by anyone.
    fn change_count(&self) -> i64;
    /// Current contents as text, or `None` if the clipboard holds
    /// something else (an image, a file) or is empty.
    fn get_text(&self) -> Option<String>;
    /// Replace the contents. Returns whether it worked; a failure means
    /// one copy does not cross, which is not worth an error.
    fn set_text(&mut self, text: &str) -> bool;
}

/// The largest clipboard text hop will send.
///
/// The transport refuses frames above 64 KiB so a peer cannot make us
/// allocate without bound, and clipboard text has to fit inside one. This
/// sits below that with room for the frame's own overhead. Copying a
/// larger selection simply does not sync, with a log line saying so,
/// rather than silently truncating text the user believes crossed intact.
pub const MAX_CLIPBOARD_BYTES: usize = 56 * 1024;

/// Tracks a clipboard well enough to forward local copies without
/// echoing back what the peer sent.
pub struct ClipboardSync {
    last_seen: i64,
    started: bool,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self {
            last_seen: 0,
            started: false,
        }
    }

    /// Text to send to the peer, if the local clipboard has changed since
    /// the last call.
    ///
    /// The first call never reports a change: at startup the clipboard
    /// already holds whatever the user copied before hop ran, and pushing
    /// that at the peer would overwrite their clipboard out of nowhere.
    pub fn poll_local_change<C: Clipboard>(&mut self, clipboard: &C) -> Option<String> {
        let count = clipboard.change_count();
        let first_look = !self.started;
        self.started = true;

        if count == self.last_seen {
            return None;
        }
        self.last_seen = count;
        if first_look {
            return None;
        }

        let text = clipboard.get_text()?;
        if text.len() > MAX_CLIPBOARD_BYTES {
            tracing::debug!(
                bytes = text.len(),
                limit = MAX_CLIPBOARD_BYTES,
                "clipboard too large to sync, leaving the peer's clipboard alone"
            );
            return None;
        }
        Some(text)
    }

    /// Apply text received from the peer, and remember the change it
    /// causes so it is never sent back.
    pub fn apply_remote<C: Clipboard>(&mut self, clipboard: &mut C, text: &str) {
        if !clipboard.set_text(text) {
            tracing::warn!("could not write the peer's clipboard locally");
            return;
        }
        // Record the counter AFTER writing. This is what stops the echo:
        // the next poll sees its own write as already accounted for.
        self.last_seen = clipboard.change_count();
        self.started = true;
    }
}

impl Default for ClipboardSync {
    fn default() -> Self {
        Self::new()
    }
}

/// In-memory clipboard for tests.
#[derive(Default)]
pub struct FakeClipboard {
    pub text: Option<String>,
    pub count: i64,
}

impl FakeClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate the user copying something on this machine.
    pub fn user_copies(&mut self, text: &str) {
        self.text = Some(text.to_string());
        self.count += 1;
    }
}

impl Clipboard for FakeClipboard {
    fn change_count(&self) -> i64 {
        self.count
    }
    fn get_text(&self) -> Option<String> {
        self.text.clone()
    }
    fn set_text(&mut self, text: &str) -> bool {
        self.text = Some(text.to_string());
        self.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_look_never_reports_a_change() {
        // Whatever the user copied before hop started is theirs, not
        // something to push at the peer the moment we connect.
        let mut sync = ClipboardSync::new();
        let mut clip = FakeClipboard::new();
        clip.user_copies("copied before hop ran");
        assert_eq!(sync.poll_local_change(&clip), None);
    }

    #[test]
    fn a_local_copy_is_reported_once() {
        let mut sync = ClipboardSync::new();
        let mut clip = FakeClipboard::new();
        sync.poll_local_change(&clip);

        clip.user_copies("hello");
        assert_eq!(sync.poll_local_change(&clip), Some("hello".to_string()));
        // Polling again with nothing new must stay quiet.
        assert_eq!(sync.poll_local_change(&clip), None);
    }

    #[test]
    fn applying_the_peers_clipboard_is_never_echoed_back() {
        // The whole reason this type exists: writing the peer's text is
        // itself a local change, and forwarding it would loop forever.
        let mut sync = ClipboardSync::new();
        let mut clip = FakeClipboard::new();
        sync.poll_local_change(&clip);

        sync.apply_remote(&mut clip, "from the peer");
        assert_eq!(clip.get_text(), Some("from the peer".to_string()));
        assert_eq!(
            sync.poll_local_change(&clip),
            None,
            "applying the peer's clipboard must not come back as a local change"
        );
    }

    #[test]
    fn a_local_copy_after_a_remote_apply_still_syncs() {
        // Suppressing the echo must not suppress the next real copy.
        let mut sync = ClipboardSync::new();
        let mut clip = FakeClipboard::new();
        sync.poll_local_change(&clip);
        sync.apply_remote(&mut clip, "from the peer");
        assert_eq!(sync.poll_local_change(&clip), None);

        clip.user_copies("something I copied myself");
        assert_eq!(
            sync.poll_local_change(&clip),
            Some("something I copied myself".to_string())
        );
    }

    #[test]
    fn oversized_clipboards_are_skipped_rather_than_truncated() {
        // Truncating would hand the user text they believe crossed whole.
        let mut sync = ClipboardSync::new();
        let mut clip = FakeClipboard::new();
        sync.poll_local_change(&clip);

        clip.user_copies(&"x".repeat(MAX_CLIPBOARD_BYTES + 1));
        assert_eq!(sync.poll_local_change(&clip), None);

        // And the next reasonably sized copy still works.
        clip.user_copies("small again");
        assert_eq!(
            sync.poll_local_change(&clip),
            Some("small again".to_string())
        );
    }

    #[test]
    fn a_non_text_clipboard_is_ignored() {
        // Copying an image bumps the counter but yields no text.
        let mut sync = ClipboardSync::new();
        let mut clip = FakeClipboard::new();
        sync.poll_local_change(&clip);

        clip.text = None;
        clip.count += 1;
        assert_eq!(sync.poll_local_change(&clip), None);
    }
}
