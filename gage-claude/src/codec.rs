//! The Claude codec: a transcript line decoded into an [`EntryValue`]
//! and encoded back as one compact JSON line, with the descriptor of
//! the `claude-jsonl 1` format.
//!
//! The one corruption the codec stands in for is a torn write: the
//! harness stops writing an entry part way, with no newline, and the
//! next entry's bytes follow on the same line. Such a line starts an
//! object and fails to parse. It decodes to an invalid entry whose
//! `raw` is the line, and encodes back to the line.

use std::str;
use std::sync::{Arc, LazyLock};

use gage_session::{DriverError, EntryCodec, EntryValue, TextDescriptor};
use serde_json::error::Category;

use crate::text_descriptor::text_descriptor;

pub struct ClaudeCodec {
    descriptor: TextDescriptor,
}

static CODEC: LazyLock<Arc<ClaudeCodec>> = LazyLock::new(|| {
    Arc::new(ClaudeCodec {
        descriptor: text_descriptor(),
    })
});

/// The one codec of the format this build writes. The descriptor is
/// compiled once per process.
pub fn codec() -> Arc<ClaudeCodec> {
    Arc::clone(&CODEC)
}

impl EntryCodec for ClaudeCodec {
    fn decode(&self, raw: &[u8]) -> Result<EntryValue, DriverError> {
        let text = str::from_utf8(raw)
            .map_err(|e| DriverError::Other(format!("entry is not UTF-8: {e}")))?;
        match serde_json::from_str(text) {
            Ok(value) => Ok(value),
            Err(e) if is_torn_object(text, &e) => {
                Ok(EntryValue::invalid_entry(&e.to_string(), text))
            }
            Err(e) => Err(DriverError::Other(format!("entry does not decode: {e}"))),
        }
    }

    fn encode(&self, value: &EntryValue) -> Result<Vec<u8>, DriverError> {
        if value.get("type").and_then(EntryValue::as_str) == Some(gage_session::INVALID_ENTRY_TYPE)
        {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const SAMPLE: &str = include_str!("../schema/claude-jsonl-1/sample.jsonl");

    /// The lines of `text` that do not come back byte for byte from a
    /// decode and an encode, each with the first differing position
    /// and the bytes around it on both sides.
    fn round_trip_failures(text: &str) -> Vec<String> {
        let codec = codec();
        let mut failures = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value = match codec.decode(line.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(format!("line {}: {e}", i + 1));
                    continue;
                }
            };
            let encoded = codec.encode(&value).unwrap();
            if encoded != line.as_bytes() {
                let at = encoded
                    .iter()
                    .zip(line.bytes())
                    .position(|(a, b)| *a != b)
                    .unwrap_or(encoded.len().min(line.len()));
                let window = |s: &[u8]| {
                    String::from_utf8_lossy(&s[at.saturating_sub(30)..(at + 30).min(s.len())])
                        .into_owned()
                };
                failures.push(format!(
                    "line {}: differs at byte {at}: original {:?}, encoded {:?}",
                    i + 1,
                    window(line.as_bytes()),
                    window(&encoded)
                ));
            }
        }
        failures
    }

    /// Every line of the sample transcript decodes and encodes back to
    /// the bytes it was written as.
    #[test]
    fn the_sample_transcript_round_trips_byte_for_byte() {
        let failures = round_trip_failures(SAMPLE);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// The transcript `GAGE_CLAUDE_SCHEMA_TRANSCRIPT` names round-trips
    /// byte for byte; the failure lists each line that does not. The
    /// test passes when no transcript is named.
    #[test]
    fn a_real_transcript_round_trips_byte_for_byte_when_named() {
        let Some(path) = std::env::var_os("GAGE_CLAUDE_SCHEMA_TRANSCRIPT") else {
            return;
        };
        let text = fs::read_to_string(&path).unwrap();
        let failures = round_trip_failures(&text);
        assert!(
            failures.is_empty(),
            "{} of {} lines differ:\n{}",
            failures.len(),
            text.lines().count(),
            failures.join("\n")
        );
    }

    /// A torn line, an object cut short with the next entry's bytes
    /// after it or with nothing after it, decodes to an invalid entry
    /// naming the error and holding the line, and encodes back to the
    /// line.
    #[test]
    fn a_torn_line_round_trips_as_an_invalid_entry() {
        let codec = codec();
        for line in [
            r#"{"type":"user","message":{"content":"Exit co{"type":"last-prompt","lastPrompt":"x"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","signature":"Eq"#,
            "{",
        ] {
            let value = codec.decode(line.as_bytes()).unwrap();
            assert_eq!(
                value.get("type").and_then(EntryValue::as_str),
                Some("$gage-invalid-entry"),
                "{line}"
            );
            assert!(
                value
                    .get("error")
                    .and_then(EntryValue::as_str)
                    .is_some_and(|e| !e.is_empty()),
                "{line}"
            );
            assert_eq!(value.invalid_entry_raw(), Some(line));
            assert_eq!(codec.encode(&value).unwrap(), line.as_bytes(), "{line}");
        }
    }

    /// Bytes that do not start an object are not a torn write: prose,
    /// a bare token, and non-UTF-8 bytes are errors naming the problem.
    /// Valid JSON that is not an object decodes as it is.
    #[test]
    fn a_line_that_is_not_a_torn_object_is_an_error() {
        let codec = codec();
        for line in ["not json", "[1, 2", "\"open string", "nul", ""] {
            let err = codec.decode(line.as_bytes()).unwrap_err();
            assert!(
                err.to_string().contains("does not decode"),
                "{line:?}: {err}"
            );
        }
        let err = codec.decode(b"{\"a\":\"\xff\"}").unwrap_err();
        assert!(err.to_string().contains("not UTF-8"), "{err}");
        assert_eq!(codec.decode(b"[1,2]").unwrap().to_string(), "[1,2]");
    }

    /// An invalid entry without its raw text cannot be encoded.
    #[test]
    fn an_invalid_entry_without_raw_text_does_not_encode() {
        let value = codec()
            .decode(br#"{"type":"$gage-invalid-entry"}"#)
            .unwrap();
        let err = codec().encode(&value).unwrap_err();
        assert!(err.to_string().contains("no raw text"), "{err}");
    }
}
