//! Derived indexes over session corpora, kept under the Gage cache
//! root and rebuildable at any time.
//!
//! A corpus is one session source or one store. Each index category
//! keeps one directory per corpus at `<cache root>/<category>/<slug>`,
//! where the slug is the corpus's canonical path with every
//! non-alphanumeric character replaced by `-` ([`corpus_slug`]). The
//! [`fts`] module holds the full-text index over message text.

pub mod fts;

use std::path::Path;

pub use fts::{
    DEFAULT_SNIPPET_CHARS, Error, LockMode, MessageRow, Result, TextIndex, TextIndexStatus,
    TextWriter,
};

/// The directory name of one corpus under an index category: the
/// canonical form of `path` with every non-alphanumeric character
/// replaced by `-`. A path that cannot be canonicalized, typically
/// one that does not exist yet, is keyed as given.
pub fn corpus_slug(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_replaces_every_non_alphanumeric_character() {
        assert_eq!(
            corpus_slug(Path::new("/no/such/dir.git")),
            "-no-such-dir-git"
        );
    }
}
