//! `tool_use` rows from Claude Code session entries.
//!
//! An assistant entry carries `tool_use` content blocks; the user
//! entry that follows carries one `tool_result` block naming the call
//! by `tool_use_id`, with the harness's own record of the result in
//! the entry's `toolUseResult` field. [`ToolUseCollector`] pairs the
//! two by id over one pass of a session's entries.

use std::collections::HashMap;
use std::path::PathBuf;

use gage_session::ToolUse;
use serde_json::Value;

use super::derive::{entry_timestamp_ms, tool_result_text};

#[derive(Default)]
pub struct ToolUseCollector {
    rows: Vec<ToolUse>,
    by_id: HashMap<String, usize>,
}

impl ToolUseCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Absorb one parsed entry: a call per `tool_use` block of an
    /// assistant entry, a result for the `tool_result` block of a
    /// user entry. A result whose call was never seen is dropped.
    pub fn push(&mut self, line: u32, entry: &Value) {
        let Some(blocks) = content_blocks(entry) else {
            return;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for block in blocks.iter().filter(|b| block_type(b) == Some("tool_use")) {
                    self.push_call(line, entry, block);
                }
            }
            Some("user") => {
                let results: Vec<&Value> = blocks
                    .iter()
                    .filter(|b| block_type(b) == Some("tool_result"))
                    .collect();
                // The harness writes one result entry per call, so
                // the entry's `toolUseResult` belongs to that one
                // block. A multi-result entry has no owner for it.
                let sidecar = (results.len() == 1)
                    .then(|| entry.get("toolUseResult"))
                    .flatten();
                for block in results {
                    self.push_result(line, entry, block, sidecar);
                }
            }
            _ => {}
        }
    }

    fn push_call(&mut self, line: u32, entry: &Value, block: &Value) {
        let (Some(id), Some(name)) = (
            block.get("id").and_then(Value::as_str),
            block.get("name").and_then(Value::as_str),
        ) else {
            return;
        };
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        self.by_id.insert(id.to_string(), self.rows.len());
        self.rows.push(ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            line,
            uuid: entry_uuid(entry),
            timestamp_ms: entry_timestamp_ms(entry),
            result_line: None,
            result_uuid: None,
            result_timestamp_ms: None,
            output: None,
            is_error: None,
            input_raw: input.to_string(),
            result_raw: None,
            files_changed: None,
        });
    }

    fn push_result(&mut self, line: u32, entry: &Value, block: &Value, sidecar: Option<&Value>) {
        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
            return;
        };
        let Some(row) = self.by_id.get(id).and_then(|&idx| self.rows.get_mut(idx)) else {
            return;
        };
        row.result_line = Some(line);
        row.result_uuid = entry_uuid(entry);
        row.result_timestamp_ms = entry_timestamp_ms(entry);
        row.output = Some(tool_result_text(block));
        row.is_error = Some(
            block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
        row.result_raw = sidecar.map(Value::to_string);
        row.files_changed = files_changed(&row.name, &row.input_raw, row.is_error, sidecar);
    }

    pub fn finish(self) -> Vec<ToolUse> {
        self.rows
    }
}

/// Tools whose input names the file they write
const FILE_WRITERS: &[(&str, &str)] = &[
    ("Edit", "file_path"),
    ("Write", "file_path"),
    ("MultiEdit", "file_path"),
    ("NotebookEdit", "notebook_path"),
];

/// Tools that never write a file
const NON_WRITERS: &[&str] = &[
    "AskUserQuestion",
    "BashOutput",
    "EnterPlanMode",
    "ExitPlanMode",
    "Glob",
    "Grep",
    "KillShell",
    "LS",
    "ListMcpResourcesTool",
    "NotebookRead",
    "Read",
    "ReadMcpResourceTool",
    "TodoRead",
    "TodoWrite",
    "ToolSearch",
    "WebFetch",
    "WebSearch",
];

/// The paths a completed call wrote. A file-writing tool wrote its
/// input path unless the result is an error. Bash wrote the paths of
/// its `bashEditDiff`, which the harness records only when a command
/// changed files; a Bash result without one is unjudged, since older
/// records never carry the diff. Any other tool is unjudged.
fn files_changed(
    name: &str,
    input_raw: &str,
    is_error: Option<bool>,
    sidecar: Option<&Value>,
) -> Option<Vec<PathBuf>> {
    if let Some((_, key)) = FILE_WRITERS.iter().find(|(n, _)| *n == name) {
        if is_error == Some(true) {
            return Some(vec![]);
        }
        let input: Value = serde_json::from_str(input_raw).ok()?;
        let path = input.get(key).and_then(Value::as_str)?;
        return Some(vec![PathBuf::from(path)]);
    }
    if NON_WRITERS.contains(&name) {
        return Some(vec![]);
    }
    if name == "Bash" {
        let diff = sidecar?.get("bashEditDiff")?;
        let paths: Vec<&str> = match diff.get("changedFiles").and_then(Value::as_array) {
            Some(list) => list.iter().filter_map(Value::as_str).collect(),
            None => diff
                .get("files")
                .and_then(Value::as_array)
                .map(|files| {
                    files
                        .iter()
                        .filter_map(|f| f.get("filePath").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default(),
        };
        return Some(paths.into_iter().map(PathBuf::from).collect());
    }
    None
}

fn content_blocks(entry: &Value) -> Option<&Vec<Value>> {
    entry.get("message")?.get("content")?.as_array()
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn entry_uuid(entry: &Value) -> Option<String> {
    entry.get("uuid").and_then(Value::as_str).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(uuid: &str, ts: &str, id: &str, name: &str, input: Value) -> Value {
        json!({
            "type": "assistant", "uuid": uuid, "timestamp": ts,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": name, "input": input}
            ]}
        })
    }

    fn result(uuid: &str, ts: &str, id: &str, content: Value, sidecar: Option<Value>) -> Value {
        let mut e = json!({
            "type": "user", "uuid": uuid, "timestamp": ts,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": content}
            ]}
        });
        if let Some(s) = sidecar {
            e["toolUseResult"] = s;
        }
        e
    }

    fn collect(entries: &[Value]) -> Vec<ToolUse> {
        let mut c = ToolUseCollector::new();
        for (i, e) in entries.iter().enumerate() {
            c.push(i as u32 + 1, e);
        }
        c.finish()
    }

    #[test]
    fn pairs_a_call_with_its_result_by_id() {
        let rows = collect(&[
            call(
                "a1",
                "2025-01-01T00:00:01Z",
                "toolu_1",
                "Edit",
                json!({"file_path": "/w/a.rs", "old_string": "x", "new_string": "y"}),
            ),
            result(
                "u1",
                "2025-01-01T00:00:02Z",
                "toolu_1",
                json!("The file /w/a.rs has been updated."),
                Some(json!({"filePath": "/w/a.rs", "oldString": "x", "newString": "y"})),
            ),
        ]);
        assert_eq!(rows.len(), 1);
        let t = &rows[0];
        assert_eq!(t.id, "toolu_1");
        assert_eq!(t.name, "Edit");
        assert_eq!((t.line, t.result_line), (1, Some(2)));
        assert_eq!(t.uuid.as_deref(), Some("a1"));
        assert_eq!(t.result_uuid.as_deref(), Some("u1"));
        assert_eq!(t.timestamp_ms, Some(1_735_689_601_000));
        assert_eq!(t.result_timestamp_ms, Some(1_735_689_602_000));
        assert_eq!(
            t.output.as_deref(),
            Some("The file /w/a.rs has been updated.")
        );
        assert_eq!(t.is_error, Some(false));
        assert_eq!(
            t.input_raw,
            r#"{"file_path":"/w/a.rs","new_string":"y","old_string":"x"}"#
        );
        assert_eq!(
            t.result_raw.as_deref(),
            Some(r#"{"filePath":"/w/a.rs","newString":"y","oldString":"x"}"#)
        );
        assert_eq!(t.files_changed, Some(vec![PathBuf::from("/w/a.rs")]));
    }

    #[test]
    fn an_unanswered_call_keeps_null_result_columns() {
        let rows = collect(&[call(
            "a1",
            "2025-01-01T00:00:01Z",
            "toolu_1",
            "Read",
            json!({"file_path": "/w/a.rs"}),
        )]);
        let t = &rows[0];
        assert_eq!(t.result_line, None);
        assert_eq!(t.output, None);
        assert_eq!(t.is_error, None);
        assert_eq!(t.result_raw, None);
        assert_eq!(t.files_changed, None);
    }

    #[test]
    fn several_calls_in_one_entry_are_several_rows() {
        let entry = json!({
            "type": "assistant", "uuid": "a1", "timestamp": "2025-01-01T00:00:01Z",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "two reads"},
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/a"}},
                {"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "/b"}}
            ]}
        });
        let rows = collect(&[
            entry,
            result("u1", "2025-01-01T00:00:02Z", "t2", json!("b"), None),
            result("u2", "2025-01-01T00:00:03Z", "t1", json!("a"), None),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].id.as_str(), rows[0].result_line), ("t1", Some(3)));
        assert_eq!((rows[1].id.as_str(), rows[1].result_line), ("t2", Some(2)));
        assert_eq!(rows[0].files_changed, Some(vec![]));
    }

    #[test]
    fn an_errored_write_changed_nothing() {
        let rows = collect(&[
            call(
                "a1",
                "2025-01-01T00:00:01Z",
                "t1",
                "Write",
                json!({"file_path": "/w/a.rs", "content": "x"}),
            ),
            json!({
                "type": "user", "uuid": "u1", "timestamp": "2025-01-01T00:00:02Z",
                "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
                     "content": "File has not been read yet."}
                ]},
                "toolUseResult": "Error: File has not been read yet."
            }),
        ]);
        let t = &rows[0];
        assert_eq!(t.is_error, Some(true));
        assert_eq!(t.files_changed, Some(vec![]));
        assert_eq!(
            t.result_raw.as_deref(),
            Some(r#""Error: File has not been read yet.""#)
        );
    }

    #[test]
    fn bash_changes_come_from_the_edit_diff() {
        let rows = collect(&[
            call(
                "a1",
                "2025-01-01T00:00:01Z",
                "t1",
                "Bash",
                json!({"command": "sed -i s/a/b/ x.rs"}),
            ),
            result(
                "u1",
                "2025-01-01T00:00:02Z",
                "t1",
                json!(""),
                Some(json!({
                    "stdout": "", "stderr": "", "interrupted": false,
                    "bashEditDiff": {
                        "changedFiles": ["/w/x.rs", "/w/y.rs"],
                        "files": [{"filePath": "/w/x.rs", "hunks": []}],
                        "moreFiles": 1
                    }
                })),
            ),
            call(
                "a2",
                "2025-01-01T00:00:03Z",
                "t2",
                "Bash",
                json!({"command": "cargo test"}),
            ),
            result(
                "u2",
                "2025-01-01T00:00:04Z",
                "t2",
                json!("ok"),
                Some(json!({"stdout": "ok", "stderr": "", "interrupted": false})),
            ),
        ]);
        assert_eq!(
            rows[0].files_changed,
            Some(vec![PathBuf::from("/w/x.rs"), PathBuf::from("/w/y.rs")])
        );
        assert_eq!(rows[1].files_changed, None);
    }

    #[test]
    fn result_output_joins_text_items() {
        let rows = collect(&[
            call(
                "a1",
                "2025-01-01T00:00:01Z",
                "t1",
                "Grep",
                json!({"pattern": "x"}),
            ),
            result(
                "u1",
                "2025-01-01T00:00:02Z",
                "t1",
                json!([{"type": "text", "text": "a.rs:1"}, {"type": "text", "text": "b.rs:2"}]),
                None,
            ),
        ]);
        assert_eq!(rows[0].output.as_deref(), Some("a.rs:1\nb.rs:2"));
    }

    #[test]
    fn a_result_without_a_call_is_dropped() {
        let rows = collect(&[result(
            "u1",
            "2025-01-01T00:00:02Z",
            "t9",
            json!("orphan"),
            None,
        )]);
        assert!(rows.is_empty());
    }
}
