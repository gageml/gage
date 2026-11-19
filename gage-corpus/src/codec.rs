//! The codec of a JSON Lines session file: a line decoded into an
//! [`EntryValue`] and encoded back as one compact JSON line, with the
//! descriptor of the driver's format.
//!
//! The one corruption the codec stands in for is a torn write: the
//! harness stops writing an entry part way, with no newline, and the
//! next entry's bytes follow on the same line. Such a line starts an
//! object and fails to parse. It decodes to an invalid entry whose
//! `raw` is the line, and encodes back to the line.

use std::str;

use gage_session::{DriverError, EntryCodec, EntryValue, INVALID_ENTRY_TYPE, TextDescriptor};
use serde_json::error::Category;

pub struct JsonlCodec {
    descriptor: TextDescriptor,
}

impl JsonlCodec {
    /// The codec of the format `descriptor` describes.
    pub fn new(descriptor: TextDescriptor) -> Self {
        Self { descriptor }
    }
}

impl EntryCodec for JsonlCodec {
    fn decode(&self, raw: &[u8]) -> Result<EntryValue, DriverError> {
        let text = str::from_utf8(raw)
            .map_err(|e| DriverError::Other(format!("entry is not UTF-8: {e}")))?;
        match text.parse() {
            Ok(value) => Ok(value),
            Err(e) if is_torn_object(text, &e) => {
                Ok(EntryValue::invalid_entry(&e.to_string(), text))
            }
            Err(e) => Err(DriverError::Other(format!("entry does not decode: {e}"))),
        }
    }

    fn encode(&self, value: &EntryValue) -> Result<Vec<u8>, DriverError> {
        if value.get("type").and_then(EntryValue::as_str) == Some(INVALID_ENTRY_TYPE) {
            return value
                .invalid_entry_raw()
                .map(|raw| raw.as_bytes().to_vec())
                .ok_or_else(|| DriverError::Other("invalid entry has no raw text".into()));
        }
        serde_json::to_vec(value)
            .map_err(|e| DriverError::Other(format!("entry does not encode: {e}")))
    }

    fn descriptor(&self) -> &TextDescriptor {
        &self.descriptor
    }
}

/// Whether `text` starts an object and `error` is a failure to finish
/// it: a syntax error inside, or the end of the text before the close.
/// Any other failure is not a torn write.
fn is_torn_object(text: &str, error: &serde_json::Error) -> bool {
    text.starts_with('{') && matches!(error.classify(), Category::Syntax | Category::Eof)
}
