//! Reading and deriving a stored session whose content is one JSON
//! Lines file, for the drivers that store a session file as it is.

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use gage_session::{
    ContentSink, ContentSource, DecodedEntry, Derivation, DriverError, EntryCodec, EntryValue,
};

/// Each entry of the stored file `path` decoded through `codec`, with
/// its line. A blank line and a line without a `type` consume a line
/// number and yield nothing; a line that does not decode is an error
/// naming the line.
pub fn decoded_entries(
    source: &dyn ContentSource,
    path: &str,
    codec: Arc<dyn EntryCodec>,
) -> Box<dyn Iterator<Item = Result<DecodedEntry, DriverError>> + 'static> {
    let reader = match source.open(path) {
        Ok(r) => io::BufReader::new(r),
        Err(e) => return Box::new(std::iter::once(Err(DriverError::Io(e)))),
    };
    Box::new(reader.lines().enumerate().filter_map(move |(i, item)| {
        let line = i as u32 + 1;
        match item {
            Ok(text) if text.trim().is_empty() => None,
            Ok(text) => match codec.decode(text.as_bytes()) {
                Ok(value) => is_entry(&value).then_some(Ok(DecodedEntry { line, value })),
                Err(e) => Some(Err(DriverError::Other(format!("line {line}: {e}")))),
            },
            Err(e) => Some(Err(DriverError::Io(e))),
        }
    }))
}

/// Whether a decoded line is an entry: a value with a `type`.
fn is_entry(value: &EntryValue) -> bool {
    value.get("type").and_then(EntryValue::as_str).is_some()
}

/// Write the stored content of `source` into `sink` with `derivation`
/// applied to the session file `path`, line by line, the appended
/// entries after its last line, and every other path copied as
/// stored. Returns the line of each appended entry in the derived
/// content, in order.
pub fn write_derived(
    source: &dyn ContentSource,
    path: &str,
    codec: &dyn EntryCodec,
    derivation: &Derivation,
    sink: &mut dyn ContentSink,
) -> Result<Vec<u32>, DriverError> {
    let mut appended = Vec::new();
    for stored in source.paths()? {
        let mut reader = source.open(&stored)?;
        let mut writer = sink.create(&stored)?;
        if stored != path {
            io::copy(&mut reader, &mut writer)?;
            continue;
        }
        let mut written: u32 = 0;
        for (i, line) in io::BufReader::new(reader).lines().enumerate() {
            let line = line?;
            let number = i as u32 + 1;
            if derivation.dropped.contains(&number) {
                continue;
            }
            match derivation.replaced.get(&number) {
                Some(value) => writer.write_all(&codec.encode(value)?)?,
                None => writer.write_all(line.as_bytes())?,
            }
            writer.write_all(b"\n")?;
            written += 1;
        }
        for value in &derivation.appended {
            writer.write_all(&codec.encode(value)?)?;
            writer.write_all(b"\n")?;
            written += 1;
            appended.push(written);
        }
    }
    Ok(appended)
}
