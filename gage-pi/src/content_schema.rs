//! The `content_schema` rows of the Pi driver: what this driver
//! writes into `raw`, `input_raw`, and `result_raw`, as partial JSON
//! Schema documents.
//!
//! A schema lists the common and useful fields and admits others, so
//! a session from a newer Pi passes while a listed field that changes
//! shape fails. The tests hold the schemas to the sessions Pi itself
//! wrote under `fixtures/`.

use gage_session::{ContentField, ContentSchema};

use crate::driver::CONTENT_FORMAT;

const ENTRY: &str = include_str!("../schema/pi-jsonl-1/entry.json");
const TOOL_USE_INPUT: &str = include_str!("../schema/pi-jsonl-1/tool_use_input.json");
const TOOL_USE_RESULT: &str = include_str!("../schema/pi-jsonl-1/tool_use_result.json");

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
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use jsonschema::Validator;
    use serde_json::Value;

    use super::*;
    use crate::testing::{GENERATED, generated, tool_uses_of};

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

    /// Validate every line of a session against the entry schema, and
    /// every tool call's input and result against theirs. Returns the
    /// failures as `line: message`.
    fn check_session(text: &str) -> Vec<String> {
        let entry = validator(ENTRY);
        let input = validator(TOOL_USE_INPUT);
        let result = validator(TOOL_USE_RESULT);
        let mut failures = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let value: Value = serde_json::from_str(line).unwrap();
            for e in entry.iter_errors(&value) {
                failures.push(format!("{}: entry {}: {e}", i + 1, e.instance_path()));
            }
        }
        for row in tool_uses_of(text) {
            let line = row.line;
            let arguments: Value = serde_json::from_str(&row.input_raw).unwrap();
            let by_tool = tool_validator(TOOL_USE_INPUT, &row.name);
            for e in input
                .iter_errors(&arguments)
                .chain(by_tool.iter().flat_map(|v| v.iter_errors(&arguments)))
            {
                failures.push(format!(
                    "{line}: {} input {}: {e}",
                    row.name,
                    e.instance_path()
                ));
            }
            if let Some(raw) = &row.result_raw {
                let recorded: Value = serde_json::from_str(raw).unwrap();
                for e in result.iter_errors(&recorded) {
                    failures.push(format!(
                        "{line}: {} result {}: {e}",
                        row.name,
                        e.instance_path()
                    ));
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
    fn the_sessions_pi_wrote_match_the_schemas() {
        for name in GENERATED {
            let failures = check_session(&generated(name).1);
            assert!(failures.is_empty(), "{name}:\n{}", failures.join("\n"));
        }
    }

    #[test]
    fn schemas_reject_a_listed_field_of_the_wrong_shape() {
        let bad =
            r#"{"type":"message","id":7,"message":{"role":"assistant","usage":{"output":"many"}}}"#;
        let failures = check_session(bad);
        assert_eq!(failures.len(), 2, "{failures:?}");
        let unknown_type = r#"{"type":"future_entry","id":"a1","parentId":null}"#;
        assert!(check_session(unknown_type).is_empty());
    }
}
