//! Derived indexes over session corpora, kept under the Gage cache
//! root and rebuildable at any time.
//!
//! A corpus is one session source or one store, and its directory
//! under the cache root is laid out by `gage_core::cache`. The
//! [`fts`] module holds the full-text index over message text, the
//! `text/` category of a corpus directory.

pub mod fts;

pub use fts::{
    DEFAULT_SNIPPET_CHARS, Error, LockMode, MessageRow, Result, TextIndex, TextIndexStatus,
    TextWriter,
};
