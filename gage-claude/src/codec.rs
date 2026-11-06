//! The Claude codec: a transcript line decoded into an [`EntryValue`]
//! and encoded back as one compact JSON line, with the descriptor of
//! the `claude-jsonl 1` format.

use std::sync::{Arc, LazyLock};

use gage_session::{DriverError, EntryCodec, EntryValue, TextDescriptor};

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
        serde_json::from_slice(raw)
            .map_err(|e| DriverError::Other(format!("entry does not decode: {e}")))
    }

    fn encode(&self, value: &EntryValue) -> Result<Vec<u8>, DriverError> {
        serde_json::to_vec(value)
            .map_err(|e| DriverError::Other(format!("entry does not encode: {e}")))
    }

    fn descriptor(&self) -> &TextDescriptor {
        &self.descriptor
    }
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

    /// A line that is not JSON is a decode error naming the problem,
    /// not a skipped line.
    #[test]
    fn a_line_that_is_not_json_is_an_error() {
        let err = codec().decode(b"{not json").unwrap_err();
        assert!(err.to_string().contains("does not decode"), "{err}");
    }
}
