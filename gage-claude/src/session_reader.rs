//! Line reader over a transcript: each non-blank line decoded through
//! the Claude codec with its 1-based line number and its `raw` text,
//! the codec's JSON text of the entry. A line that does not decode is
//! an error carrying its line number; the caller decides whether to
//! skip it or to stop.

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use gage_session::{DriverError, EntryCodec, EntryValue};

use crate::codec::codec;

/// Iterator over the decoded lines of a transcript as `(line, raw,
/// value)`. Blank lines consume a line number and yield nothing.
///
/// `raw` is the codec's JSON text of the entry. A line the codec
/// decoded as it stands is that text already and is yielded as read;
/// a line the codec stood in for yields the stand-in entry's text, as
/// `encode` would not give the line back.
pub struct SessionReader<R: Read = File> {
    reader: io::Lines<BufReader<R>>,
    line_num: u32,
}

#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    /// A non-blank line the codec did not decode. The codec stands in
    /// for the corruption it recognizes, so this is a line in a shape
    /// the build knows nothing about, and a reader fails the session.
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

impl From<ReadError> for io::Error {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Io(e) => e,
            ReadError::Decode { .. } => io::Error::new(io::ErrorKind::InvalidData, e.to_string()),
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
    type Item = Result<(u32, String, EntryValue), ReadError>;

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
                Ok(value) => {
                    let raw = match value.invalid_entry_raw() {
                        Some(_) => value.to_string(),
                        None => line,
                    };
                    Ok((self.line_num, raw, value))
                }
                Err(error) => Err(ReadError::Decode {
                    line: self.line_num,
                    error,
                }),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decodable line is its own `raw`, byte for byte, whatever its
    /// key order or spacing; a torn line's `raw` is the stand-in entry
    /// the codec yields for it.
    #[test]
    fn raw_is_the_line_as_read_unless_the_codec_stood_in() {
        let spaced = r#"{ "z" : 1.50, "a" : [ "s" ] , "type":"user" }"#;
        let torn = r#"{"type":"user","a":"cu{"type":"c"}"#;
        let text = format!("{spaced}\n\n{torn}\n");
        let items: Vec<(u32, String, EntryValue)> = SessionReader::new(text.as_bytes())
            .map(Result::unwrap)
            .collect();
        assert_eq!(items[0].0, 1);
        assert_eq!(items[0].1, spaced);
        assert_eq!(items[1].0, 3);
        assert_eq!(items[1].1, items[1].2.to_string());
        assert_eq!(items[1].2.invalid_entry_raw(), Some(torn));
    }
}
