//! The content formats this crate shows entries of.
//!
//! An entry's body comes from the session's driver, which reads the
//! entry in its own format, and the outline decides which rows to
//! show by who produced each message and by its subtype, which every
//! driver names alike. One reader here still depends on the format:
//! turns are numbered from a field of the Claude format's entries, and
//! a session of another format has none. Each document is checked
//! against the formats below before the outline is built.

/// The `content_format` of a Claude Code session
pub const CLAUDE: &str = "claude-jsonl 1";

/// The `content_format` values whose sessions this crate shows
pub const SUPPORTED: &[&str] = &[CLAUDE, "codex-rollout 1", "pi-jsonl 1"];

/// Panics unless `content_format` is one of [`SUPPORTED`]. A session
/// in another format has no reader here, and rendering it would show
/// nothing or the wrong thing.
pub fn require_supported(session_id: &str, content_format: &str) {
    assert!(
        SUPPORTED.contains(&content_format),
        "session {session_id} has content format {content_format:?}; \
         this build reads {SUPPORTED:?} only"
    );
}
