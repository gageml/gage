//! The line reader over a Claude Code transcript: each non-blank line
//! decoded through the Claude codec.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub use gage_corpus::{ReadError, SessionReader};

use crate::codec::codec;

/// A reader over the transcript at `path`.
pub fn open(path: &Path) -> io::Result<SessionReader<File>> {
    SessionReader::open(path, codec())
}

/// A reader over the transcript `reader` yields.
pub fn reader<R: Read>(reader: R) -> SessionReader<R> {
    SessionReader::new(reader, codec())
}
