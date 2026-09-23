use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use serde_json::Value;

/// Iterator over parsed NDJSON session lines.
///
/// Yields `(line_number, parsed_value)` pairs where `line_number` is
/// the 1-based position in the input (including empty and malformed
/// lines in the count). Empty lines and lines that fail JSON parsing
/// are skipped — they consume a line number but produce no item.
pub struct SessionReader<R: Read = File> {
    reader: io::Lines<BufReader<R>>,
    line_num: u32,
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
    type Item = io::Result<(u32, Value)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let line = match self.reader.next()? {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            self.line_num += 1;

            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<Value>(&line) {
                Ok(v) => return Some(Ok((self.line_num, v))),
                Err(_) => continue,
            }
        }
    }
}
