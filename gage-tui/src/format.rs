//! The content format this crate reads entries in directly.
//!
//! An entry's `raw` is the harness's own row, opaque by contract.
//! Two readers here open it anyway: the message pane renders its
//! content blocks, and the outline labels entries by what the
//! driver derived from it. Each document is checked against the one
//! format those readers understand before any of them runs. Reading
//! `raw` this way is a decision to keep, not an accident; a second
//! format adds its own readers here or moves the work into the
//! driver.

/// The `content_format` whose `raw` entries this crate reads
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
