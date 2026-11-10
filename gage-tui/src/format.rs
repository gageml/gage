//! The content format this crate shows entries of.
//!
//! An entry's body comes from the session's driver, which reads the
//! entry in its own format. One reader here still depends on the
//! format: the outline decides which rows to show by the entry types
//! and message subtypes of this format. Each document is checked
//! against that format before the outline is built; a second format
//! adds its own row rules here or moves them into the driver.

/// The `content_format` whose entry types this crate's outline reads
pub const SUPPORTED: &str = "claude-jsonl 1";

/// Panics unless `content_format` is [`SUPPORTED`]. A session in
/// another format has no reader here, and rendering it would show
/// nothing or the wrong thing.
pub fn require_supported(session_id: &str, content_format: &str) {
    assert!(
        content_format == SUPPORTED,
        "session {session_id} has content format {content_format:?}; \
         this build reads {SUPPORTED:?} only"
    );
}
