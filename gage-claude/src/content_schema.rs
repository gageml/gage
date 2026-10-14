//! The `content_schema` rows of the Claude driver: what this driver
//! writes into `raw`, `input_raw`, and `result_raw` for each content
//! format it reads, as partial JSON Schema documents.
//!
//! A schema lists the common and useful fields and admits others, so
//! a transcript from a newer Claude Code passes while a listed field
//! that changes shape fails. `schema/claude-jsonl-1/sample.jsonl` is a
//! transcript in the described shape; the tests hold the schemas and
//! the sample to each other.

use gage_session::{ContentField, ContentSchema};

use crate::driver::CONTENT_FORMAT;

const ENTRY: &str = include_str!("../schema/claude-jsonl-1/entry.json");
const TOOL_USE_INPUT: &str = include_str!("../schema/claude-jsonl-1/tool_use_input.json");
const TOOL_USE_RESULT: &str = include_str!("../schema/claude-jsonl-1/tool_use_result.json");

pub(crate) fn content_schemas() -> Vec<ContentSchema> {
    [
        (ContentField::Entry, ENTRY),
        (ContentField::ToolUseInput, TOOL_USE_INPUT),
        (ContentField::ToolUseResult, TOOL_USE_RESULT),
    ]
    .into_iter()
    .map(|(field, schema)| ContentSchema {
        content_format: CONTENT_FORMAT.to_string(),
        field,
        schema: schema.to_string(),
        doc: None,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::env;
    use std::fs;
    use std::path::Path;

    use jsonschema::Validator;
    use serde_json::Value;

    use super::*;

    const SAMPLE: &str = include_str!("../schema/claude-jsonl-1/sample.jsonl");

    fn validator(schema: &str) -> Validator {
        let schema: Value = serde_json::from_str(schema).unwrap();
        jsonschema::validator_for(&schema).unwrap()
    }

    /// A validator for the `$defs` entry named `tool`, when the schema
    /// describes that tool
    fn tool_validator(schema: &str, tool: &str) -> Option<Validator> {
        let schema: Value = serde_json::from_str(schema).unwrap();
        let def = schema.get("$defs")?.get(tool)?;
        Some(jsonschema::validator_for(def).unwrap())
    }

    /// Validate every line of a transcript against the entry schema,
    /// every tool call's input against its tool's input schema, and
    /// every recorded result against its tool's result schema. Returns
    /// the failures as `line: message`.
    fn check_transcript(text: &str) -> Vec<String> {
        let entry = validator(ENTRY);
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut failures = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let n = i + 1;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(format!("{n}: not JSON: {e}"));
                    continue;
                }
            };
            for e in entry.iter_errors(&value) {
                failures.push(format!("{n}: entry {}: {e}", e.instance_path()));
            }
            let blocks = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            for block in &blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && let (Some(id), Some(name), Some(input)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                        block.get("input"),
                    )
                {
                    tool_names.insert(id.to_string(), name.to_string());
                    if let Some(v) = tool_validator(TOOL_USE_INPUT, name) {
                        for e in v.iter_errors(input) {
                            failures.push(format!("{n}: {name} input {}: {e}", e.instance_path()));
                        }
                    }
                }
                if block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && let Some(result) = value.get("toolUseResult")
                    && let Some(name) = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .and_then(|id| tool_names.get(id))
                    && result.is_object()
                    && let Some(v) = tool_validator(TOOL_USE_RESULT, name)
                {
                    for e in v.iter_errors(result) {
                        failures.push(format!("{n}: {name} result {}: {e}", e.instance_path()));
                    }
                }
            }
        }
        failures
    }

    #[test]
    fn schemas_are_valid_json_schema_documents() {
        for row in content_schemas() {
            let schema: Value = serde_json::from_str(&row.schema).unwrap();
            jsonschema::meta::validate(&schema).unwrap();
            assert_eq!(row.content_format, CONTENT_FORMAT);
        }
    }

    #[test]
    fn sample_transcript_matches_the_schemas() {
        let failures = check_transcript(SAMPLE);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn schemas_reject_a_listed_field_of_the_wrong_shape() {
        let bad = r#"{"type":"assistant","message":{"id":7,"usage":{"output_tokens":"many"}}}"#;
        let failures = check_transcript(bad);
        assert_eq!(failures.len(), 2, "{failures:?}");
        let unknown_type = r#"{"type":"future-record","sessionId":"s"}"#;
        assert_eq!(check_transcript(unknown_type).len(), 1);
    }

    /// Validates a real transcript named by `GAGE_CLAUDE_SCHEMA_TRANSCRIPT`,
    /// for checking the schemas against a Claude Code release on a
    /// developer's machine. Passes without one.
    #[test]
    fn real_transcript_matches_the_schemas_when_named() {
        let Some(path) = env::var_os("GAGE_CLAUDE_SCHEMA_TRANSCRIPT") else {
            return;
        };
        let text = fs::read_to_string(Path::new(&path)).unwrap();
        let failures = check_transcript(&text);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
