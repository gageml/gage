//! Line reader over a transcript: each non-blank line decoded through
//! the Claude codec with its 1-based line number. A line that does not
//! decode is an error carrying its line number; the caller decides
//! whether to skip it or to stop.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use gage_session::{DriverError, EntryCodec, EntryValue};

use crate::codec::codec;

/// Iterator over the decoded lines of a transcript. Blank lines
/// consume a line number and yield nothing.
pub struct SessionReader<R: Read = File> {
    reader: io::Lines<BufReader<R>>,
    line_num: u32,
}

#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    /// A non-blank line the codec did not decode
    Decode {
        line: u32,
        error: DriverError,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Io(e) => write!(f, "io: {e}"),
            ReadError::Decode { line, error } => write!(f, "line {line}: {error}"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<ReadError> for DriverError {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Io(e) => DriverError::Io(e),
            ReadError::Decode { .. } => DriverError::Other(e.to_string()),
        }
    }
}

impl SessionReader<File> {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self::new(File::open(path)?))
    }
}

impl<R: Read> SessionReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader).lines(),
            line_num: 0,
        }
    }
}

impl<R: Read> Iterator for SessionReader<R> {
    type Item = Result<(u32, EntryValue), ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let line = match self.reader.next()? {
                Ok(l) => l,
                Err(e) => return Some(Err(ReadError::Io(e))),
            };
            self.line_num += 1;
            if line.trim().is_empty() {
                continue;
            }
            return Some(match codec().decode(line.as_bytes()) {
                Ok(value) => Ok((self.line_num, value)),
                Err(error) => Err(ReadError::Decode {
                    line: self.line_num,
                    error,
                }),
            });
        }
    }
}
