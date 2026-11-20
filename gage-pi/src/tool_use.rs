//! `tool_use` rows from a session file.
//!
//! An assistant message holds a `toolCall` block for each tool the
//! model called, and a later `toolResult` message answers each call
//! by its id. A row pairs the two: `input_raw` is the call's
//! `arguments`, `result_raw` the result's message, and `output` the
//! text of the result's content. A call no result answers has a row
//! with no result side.
//!
//! The files of a call are typed by what Pi's own tools do. `read`,
//! `ls`, `find`, and `grep` do not write. `write` and `edit` write the
//! path they name. `bash` writes what its command redirects output to
//! or edits in place, and cites every other path the command names.
//! Any other tool is an extension's, and the paths its arguments name
//! are cited. A call that failed, or that has no result, proves
//! nothing about a path, so its paths are cited.

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use gage_corpus::shell_paths::{command_writes, referenced_paths};
use gage_session::{FileType, ToolUse, ToolUseFile};
use serde_json::Value;

use crate::derive::{blocks, content_text, line_type, role, str_at, timestamp_ms};
use crate::session_file::Header;

/// The tool whose `command` argument is a shell command
const SHELL_TOOL: &str = "bash";

/// Pi's tools that read and never write
const READ_TOOLS: &[&str] = &["read", "ls", "find", "grep"];

/// Pi's tools that write the path they name
const WRITE_TOOLS: &[&str] = &["write", "edit"];

#[derive(Default)]
pub(crate) struct ToolUseCollector {
    rows: Vec<ToolUse>,
    /// The call of each row, for typing its files once its result is
    /// read
    calls: Vec<Call>,
    /// The row of each call not yet answered, by the call's id
    open: HashMap<String, usize>,
    /// The session's working directory, which relative paths resolve
    /// against
    cwd: Option<PathBuf>,
    /// The home directory `~` resolves to, read once
    home: Option<PathBuf>,
}

struct Call {
    name: String,
    arguments: Value,
}

impl ToolUseCollector {
    pub(crate) fn new() -> Self {
        Self {
            home: env::var_os("HOME").map(PathBuf::from),
            ..Self::default()
        }
    }

    /// Absorb one parsed session line. The header names the working
    /// directory the relative paths of later calls resolve against.
    pub(crate) fn push(&mut self, line_num: u32, line: &Value) {
        if self.cwd.is_none()
            && let Some(header) = Header::from_line(line)
        {
            self.cwd = header.cwd;
        }
        if line_type(line) != Some("message") {
            return;
        }
        let Some(message) = line.get("message") else {
            return;
        };
        match role(line) {
            Some("assistant") => {
                for block in blocks(message.get("content")) {
                    if str_at(block, "type") == Some("toolCall") {
                        self.push_call(line_num, line, block);
                    }
                }
            }
            Some("toolResult") => self.answer(line_num, line, message),
            _ => {}
        }
    }

    fn push_call(&mut self, line_num: u32, line: &Value, block: &Value) {
        let Some(id) = str_at(block, "id") else {
            return;
        };
        let call = Call {
            name: str_at(block, "name").unwrap_or_default().to_string(),
            arguments: block.get("arguments").cloned().unwrap_or(Value::Null),
        };
        self.open.insert(id.to_string(), self.rows.len());
        self.rows.push(ToolUse {
            id: id.to_string(),
            name: call.name.clone(),
            line: line_num,
            uuid: str_at(line, "id").map(String::from),
            timestamp_ms: timestamp_ms(line),
            result_line: None,
            result_uuid: None,
            result_timestamp_ms: None,
            output: None,
            is_error: None,
            input_raw: call.arguments.to_string(),
            result_raw: None,
            files: self.files(&call, None),
        });
        self.calls.push(call);
    }

    /// Record the result `message` on the row of the call it answers.
    /// A result that answers no open call has no row.
    fn answer(&mut self, line_num: u32, line: &Value, message: &Value) {
        let Some(index) = str_at(message, "toolCallId").and_then(|id| self.open.remove(id)) else {
            return;
        };
        let is_error = message.get("isError").and_then(Value::as_bool) == Some(true);
        let Some(files) = self
            .calls
            .get(index)
            .map(|call| self.files(call, Some(is_error)))
        else {
            return;
        };
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.result_line = Some(line_num);
        row.result_uuid = str_at(line, "id").map(String::from);
        row.result_timestamp_ms = timestamp_ms(line);
        row.output = Some(content_text(message.get("content")));
        row.is_error = Some(is_error);
        row.result_raw = Some(message.to_string());
        row.files = files;
    }

    /// The paths `call` names, each typed by what its result proves.
    /// `is_error` is the result's, and `None` for a call with no
    /// result.
    fn files(&self, call: &Call, is_error: Option<bool>) -> Vec<ToolUseFile> {
        let (cwd, home) = (self.cwd.as_deref(), self.home.as_deref());
        let named = referenced_paths(SHELL_TOOL, &call.name, &call.arguments, cwd, home);
        let succeeded = is_error == Some(false);
        let name = call.name.as_str();
        let written = match call.arguments.get("command").and_then(Value::as_str) {
            Some(command) if succeeded && name == SHELL_TOOL => command_writes(command, cwd, home),
            _ => Vec::new(),
        };
        let type_of = |path: &PathBuf| match () {
            () if !succeeded => FileType::Cite,
            () if written.contains(path) || WRITE_TOOLS.contains(&name) => FileType::Write,
            () if READ_TOOLS.contains(&name) => FileType::Read,
            () => FileType::Cite,
        };
        let mut files: Vec<ToolUseFile> = named
            .iter()
            .chain(&written)
            .map(|path| ToolUseFile {
                path: path.clone(),
                type_: type_of(path),
            })
            .collect();
        files.sort();
        files.dedup();
        files
    }

    /// The rows, in the order the calls were made.
    pub(crate) fn finish(self) -> Vec<ToolUse> {
        self.rows
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(id: &str, name: &str, arguments: Value) -> Value {
        json!({
            "type": "message", "id": "a1", "parentId": null,
            "timestamp": "2026-01-02T03:04:05.000Z",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "On it."},
                {"type": "toolCall", "id": id, "name": name, "arguments": arguments},
            ]},
        })
    }

    fn result(id: &str, text: &str, is_error: bool) -> Value {
        json!({
            "type": "message", "id": "r1", "parentId": "a1",
            "timestamp": "2026-01-02T03:04:06.000Z",
            "message": {"role": "toolResult", "toolCallId": id, "toolName": "t",
                "content": [{"type": "text", "text": text}], "isError": is_error},
        })
    }

    fn rows(lines: &[Value]) -> Vec<ToolUse> {
        let header = json!({"type": "session", "version": 3, "id": "s", "cwd": "/w/proj"});
        let mut collector = ToolUseCollector::new();
        for (i, line) in std::iter::once(&header).chain(lines).enumerate() {
            collector.push(i as u32 + 1, line);
        }
        collector.finish()
    }

    fn files(row: &ToolUse) -> Vec<(&str, FileType)> {
        row.files
            .iter()
            .map(|f| (f.path.to_str().unwrap(), f.type_))
            .collect()
    }

    #[test]
    fn a_row_pairs_a_call_with_the_result_that_answers_it() {
        let rows = rows(&[
            call("c1", "read", json!({"path": "notes.txt"})),
            result("c1", "alpha", false),
        ]);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!((row.id.as_str(), row.name.as_str()), ("c1", "read"));
        assert_eq!((row.line, row.result_line), (2, Some(3)));
        assert_eq!(row.uuid.as_deref(), Some("a1"));
        assert_eq!(row.result_uuid.as_deref(), Some("r1"));
        assert_eq!(row.timestamp_ms, Some(1_767_323_045_000));
        assert_eq!(row.result_timestamp_ms, Some(1_767_323_046_000));
        assert_eq!(row.output.as_deref(), Some("alpha"));
        assert_eq!(row.is_error, Some(false));
        assert_eq!(row.input_raw, r#"{"path":"notes.txt"}"#);
        let result_raw: Value = serde_json::from_str(row.result_raw.as_deref().unwrap()).unwrap();
        assert_eq!(result_raw["role"], "toolResult");
        assert_eq!(files(row), [("/w/proj/notes.txt", FileType::Read)]);
    }

    #[test]
    fn a_call_with_no_result_has_no_result_side_and_cites_its_paths() {
        let rows = rows(&[
            call(
                "c1",
                "write",
                json!({"path": "/w/proj/out.txt", "content": "x"}),
            ),
            result("other", "stray", false),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result_line, None);
        assert_eq!(rows[0].is_error, None);
        assert_eq!(files(&rows[0]), [("/w/proj/out.txt", FileType::Cite)]);
    }

    #[test]
    fn files_are_typed_by_the_tool_and_the_outcome() {
        let typed = |name: &str, arguments: Value, is_error: bool| {
            let rows = rows(&[call("c1", name, arguments), result("c1", "", is_error)]);
            rows[0]
                .files
                .iter()
                .map(|f| (f.path.to_str().unwrap().to_string(), f.type_))
                .collect::<Vec<_>>()
        };
        let path = json!({"path": "a.txt"});
        let at = |type_| vec![("/w/proj/a.txt".to_string(), type_)];
        assert_eq!(typed("edit", path.clone(), false), at(FileType::Write));
        assert_eq!(typed("edit", path.clone(), true), at(FileType::Cite));
        assert_eq!(typed("grep", path.clone(), false), at(FileType::Read));
        assert_eq!(typed("deploy", path, false), at(FileType::Cite));
        assert_eq!(
            typed(
                "bash",
                json!({"command": "sort in/a.txt > out/b.txt"}),
                false
            ),
            [
                ("/w/proj/in/a.txt".to_string(), FileType::Cite),
                ("/w/proj/out/b.txt".to_string(), FileType::Write),
            ]
        );
    }
}
