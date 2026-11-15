//! `tool_use` rows from a rollout.
//!
//! A Codex model calls its tools by writing a script to one `exec`
//! tool, so the model's own record of a call says little about what
//! ran. The harness records what ran: in the paginated history mode,
//! an `event_msg` of type `item_completed` carries one item per
//! command executed, MCP tool called, and patch applied, with its
//! input and its result together. The rows are built from those
//! items:
//!
//! | Item | `name` | `input_raw` | `output` |
//! |---|---|---|---|
//! | `CommandExecution` | `exec_command` | `command`, `cwd` | `aggregated_output` |
//! | `McpToolCall` | `mcp__<server>__<tool>` | `arguments` | the result's text, or the error |
//! | `FileChange` | `apply_patch` | each path with its kind | `stdout`, `stderr` |
//!
//! `result_raw` is the item as recorded. Any other item is not a tool
//! call the driver reads. A rollout in the legacy history mode
//! carries no items, so it has no rows.
//!
//! A row's `line` is the line of the model's call that the item ran
//! under, and its `result_line` the line of that call's output, so a
//! row pairs with the `tool_use` and `tool_result` messages as it does
//! for any driver. The rollout does not name the call an item ran
//! under; an item belongs to the call most recently made and not yet
//! answered, which is the order the harness writes them in. An item
//! with no call open stands on its own line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gage_corpus::shell_paths::{command_paths, command_writes, referenced_paths, resolve};
use gage_session::{FileType, ToolUse, ToolUseFile};
use serde_json::{Map, Value, json};

use crate::derive::{content_text, line_type, payload_type, timestamp_ms};

/// The `name` of a command execution: the tool the model runs a
/// shell command with
pub(crate) const EXEC_COMMAND: &str = "exec_command";

/// The `name` of a file change: the tool the model edits files with
pub(crate) const APPLY_PATCH: &str = "apply_patch";

/// The response items that are a model's call to a tool
const CALLS: &[&str] = &["custom_tool_call", "function_call", "local_shell_call"];

/// The response items that are a call's output
const OUTPUTS: &[&str] = &["custom_tool_call_output", "function_call_output"];

#[derive(Default)]
pub(crate) struct ToolUseCollector {
    rows: Vec<ToolUse>,
    /// The model's calls not yet answered, oldest first
    open: Vec<OpenCall>,
    /// The rows that ran under each call, by the call's id
    under: HashMap<String, Vec<usize>>,
    /// The home directory `~` resolves to, read once
    home: Option<PathBuf>,
}

struct OpenCall {
    call_id: String,
    line: u32,
    timestamp_ms: Option<i64>,
}

impl ToolUseCollector {
    pub(crate) fn new() -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            ..Self::default()
        }
    }

    /// Absorb one parsed rollout line.
    pub(crate) fn push(&mut self, line_num: u32, line: &Value) {
        let Some(payload) = line.get("payload") else {
            return;
        };
        match (line_type(line), payload_type(line)) {
            (Some("response_item"), Some(kind)) if CALLS.contains(&kind) => {
                if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                    self.open.push(OpenCall {
                        call_id: call_id.to_string(),
                        line: line_num,
                        timestamp_ms: timestamp_ms(line),
                    });
                }
            }
            (Some("response_item"), Some(kind)) if OUTPUTS.contains(&kind) => {
                if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                    self.answer(call_id, line_num);
                }
            }
            (Some("event_msg"), Some("item_completed")) => {
                if let Some(item) = payload.get("item") {
                    self.push_item(line_num, timestamp_ms(line), item);
                }
            }
            _ => {}
        }
    }

    /// Record the output line of the call `call_id` on each row that
    /// ran under it.
    fn answer(&mut self, call_id: &str, line_num: u32) {
        self.open.retain(|call| call.call_id != call_id);
        for index in self.under.remove(call_id).unwrap_or_default() {
            if let Some(row) = self.rows.get_mut(index) {
                row.result_line = Some(line_num);
            }
        }
    }

    fn push_item(&mut self, line_num: u32, completed_ms: Option<i64>, item: &Value) {
        let Some(call) = self.tool_call(item) else {
            return;
        };
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let owner = self
            .open
            .last()
            .map(|call| (call.call_id.clone(), call.line, call.timestamp_ms));
        let row = ToolUse {
            id: id.to_string(),
            name: call.name,
            line: owner.as_ref().map_or(line_num, |(_, line, _)| *line),
            uuid: None,
            timestamp_ms: owner
                .as_ref()
                .map_or(completed_ms, |(_, _, timestamp_ms)| *timestamp_ms),
            // An item with no call open is its own result.
            result_line: owner.is_none().then_some(line_num),
            result_uuid: None,
            result_timestamp_ms: completed_ms,
            output: Some(call.output),
            is_error: Some(call.is_error),
            input_raw: call.input.to_string(),
            result_raw: Some(item.to_string()),
            files: call.files,
        };
        if let Some((call_id, _, _)) = owner {
            self.under.entry(call_id).or_default().push(self.rows.len());
        }
        self.rows.push(row);
    }

    /// The tool call an item records; `None` for an item that is not
    /// one the driver reads.
    fn tool_call(&self, item: &Value) -> Option<Call> {
        match item.get("type").and_then(Value::as_str)? {
            "CommandExecution" => Some(self.command_execution(item)),
            "McpToolCall" => Some(self.mcp_tool_call(item)),
            "FileChange" => Some(self.file_change(item)),
            _ => None,
        }
    }

    /// A command the harness ran. A path an output redirect or an
    /// in-place edit names is written when the command succeeded;
    /// every other path the command names is cited, since the record
    /// does not say what else the command wrote.
    fn command_execution(&self, item: &Value) -> Call {
        let cwd = item
            .get("cwd")
            .and_then(Value::as_str)
            .map(|cwd| PathBuf::from(cwd.strip_prefix("file://").unwrap_or(cwd)));
        let script = shell_script(item.get("command"));
        let is_error = match item.get("exit_code").and_then(Value::as_i64) {
            Some(code) => code != 0,
            None => item.get("status").and_then(Value::as_str) != Some("completed"),
        };
        let home = self.home.as_deref();
        let written = match is_error {
            false => command_writes(&script, cwd.as_deref(), home),
            true => Vec::new(),
        };
        let named = command_paths(&script, cwd.as_deref(), home);
        let mut input = Map::new();
        for key in ["command", "cwd"] {
            if let Some(value) = item.get(key) {
                input.insert(key.to_string(), value.clone());
            }
        }
        Call {
            name: EXEC_COMMAND.to_string(),
            input: Value::Object(input),
            output: text_at(item, "aggregated_output"),
            is_error,
            files: files(written, named),
        }
    }

    /// An MCP tool the harness called. The record does not say what a
    /// tool wrote, so the paths its arguments name are cited.
    fn mcp_tool_call(&self, item: &Value) -> Call {
        let field = |key| item.get(key).and_then(Value::as_str).unwrap_or_default();
        let name = format!("mcp__{}__{}", field("server"), field("tool"));
        let input = item.get("arguments").cloned().unwrap_or(Value::Null);
        let error = item
            .get("error")
            .filter(|error| !error.is_null())
            .map(|error| match error.get("message").and_then(Value::as_str) {
                Some(message) => message.to_string(),
                None => error.to_string(),
            });
        let failed = field("status") == "failed"
            || item.pointer("/result/isError").and_then(Value::as_bool) == Some(true);
        let named = referenced_paths(EXEC_COMMAND, &name, &input, None, self.home.as_deref());
        Call {
            name,
            is_error: failed || error.is_some(),
            output: error.unwrap_or_else(|| content_text(item.pointer("/result/content"))),
            input,
            files: files(Vec::new(), named),
        }
    }

    /// A patch the harness applied. Each path it changed is written,
    /// with the path a file moved to, unless the patch failed.
    fn file_change(&self, item: &Value) -> Call {
        let changes = item.get("changes").and_then(Value::as_object);
        let is_error = item.get("status").and_then(Value::as_str) != Some("completed");
        let mut paths = Vec::new();
        let mut kinds = Map::new();
        for (path, change) in changes.into_iter().flatten() {
            let mut kind = Map::new();
            for key in ["type", "move_path"] {
                if let Some(value) = change.get(key).filter(|value| !value.is_null()) {
                    kind.insert(key.to_string(), value.clone());
                }
            }
            kinds.insert(path.clone(), Value::Object(kind));
            let moved = change.get("move_path").and_then(Value::as_str);
            for path in std::iter::once(path.as_str()).chain(moved) {
                paths.extend(resolve(path, None, self.home.as_deref()));
            }
        }
        let output = [text_at(item, "stdout"), text_at(item, "stderr")]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let (written, named) = match is_error {
            false => (paths, Vec::new()),
            true => (Vec::new(), paths),
        };
        Call {
            name: APPLY_PATCH.to_string(),
            input: json!({ "changes": kinds }),
            output,
            is_error,
            files: files(written, named),
        }
    }

    /// The rows, in the order their items were recorded.
    pub(crate) fn finish(self) -> Vec<ToolUse> {
        self.rows
    }
}

/// What one item says of the tool call it records
struct Call {
    name: String,
    input: Value,
    output: String,
    is_error: bool,
    files: Vec<ToolUseFile>,
}

/// The shell text of a recorded command. Codex records a command as
/// the argument list it ran, which for a shell command is the shell,
/// `-lc` or `-c`, and the command text; any other list is its words.
fn shell_script(command: Option<&Value>) -> String {
    let words: Vec<&str> = command
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    match words.as_slice() {
        [shell, flag, script] if is_shell(shell) && matches!(*flag, "-lc" | "-c") => {
            script.to_string()
        }
        _ => words.join(" "),
    }
}

fn is_shell(program: &str) -> bool {
    matches!(
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str()),
        Some("bash" | "sh" | "zsh")
    )
}

fn text_at(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The files of a call: each path it wrote as `write`, and each other
/// path it names as `cite`, each once, sorted.
fn files(written: Vec<PathBuf>, named: Vec<PathBuf>) -> Vec<ToolUseFile> {
    let file = |path: PathBuf, type_: FileType| ToolUseFile { path, type_ };
    let cited: Vec<ToolUseFile> = named
        .into_iter()
        .filter(|path| !written.contains(path))
        .map(|path| file(path, FileType::Cite))
        .collect();
    let mut out: Vec<ToolUseFile> = cited
        .into_iter()
        .chain(written.into_iter().map(|path| file(path, FileType::Write)))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::lines;

    fn rows_of(lines: &[Value]) -> Vec<ToolUse> {
        let mut collector = ToolUseCollector::new();
        for (i, line) in lines.iter().enumerate() {
            collector.push(i as u32 + 1, line);
        }
        collector.finish()
    }

    fn file(path: &str, type_: FileType) -> ToolUseFile {
        ToolUseFile {
            path: PathBuf::from(path),
            type_,
        }
    }

    #[test]
    fn each_item_is_a_row_on_the_lines_of_the_call_it_ran_under() {
        let rows = rows_of(&lines());
        let summary: Vec<String> = rows
            .iter()
            .map(|r| {
                format!(
                    "{} {} {} {:?} {:?}",
                    r.id, r.name, r.line, r.result_line, r.is_error
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                "exec-1 exec_command 12 Some(15) Some(false)",
                "exec-2 apply_patch 16 Some(19) Some(false)",
                "exec-3 mcp__gage__query 16 Some(19) Some(true)",
            ],
            "the patch and the MCP call ran under one script"
        );
    }

    #[test]
    fn a_row_carries_the_items_input_output_and_record() {
        let rows = rows_of(&lines());
        let command = &rows[0];
        assert_eq!(
            serde_json::from_str::<Value>(&command.input_raw).unwrap(),
            json!({
                "command": ["/bin/bash", "-lc", "printf 'hello\\n' > hello.txt && cat notes/todo.md"],
                "cwd": "file:///home/u/proj",
            })
        );
        assert_eq!(command.output.as_deref(), Some("buy milk\n"));
        assert_eq!(
            command.timestamp_ms,
            Some(1_767_323_052_000),
            "the call's time"
        );
        assert_eq!(
            command.result_timestamp_ms,
            Some(1_767_323_054_000),
            "the time the item completed"
        );
        let recorded: Value = serde_json::from_str(command.result_raw.as_deref().unwrap()).unwrap();
        assert_eq!(recorded["type"], "CommandExecution");
        assert_eq!(recorded["exit_code"], 0);

        let patch = &rows[1];
        assert_eq!(
            serde_json::from_str::<Value>(&patch.input_raw).unwrap(),
            json!({"changes": {
                "/home/u/proj/hello.txt": {"type": "update"},
                "/home/u/proj/new.md": {"type": "add"},
            }})
        );
        assert_eq!(
            patch.output.as_deref(),
            Some("Success. Updated the following files:\nM hello.txt\nA new.md\n")
        );

        let mcp = &rows[2];
        assert_eq!(
            serde_json::from_str::<Value>(&mcp.input_raw).unwrap(),
            json!({"sql": "select 1", "path": "/home/u/proj/q.sql"})
        );
        assert_eq!(
            mcp.output.as_deref(),
            Some("MCP tool call requires approval, but approval policy is never")
        );
    }

    #[test]
    fn files_are_written_where_the_record_proves_it_and_cited_otherwise() {
        let rows = rows_of(&lines());
        assert_eq!(
            rows[0].files,
            [
                file("/home/u/proj/hello.txt", FileType::Write),
                file("/home/u/proj/notes/todo.md", FileType::Cite),
            ],
            "a redirect target is written; a path the command reads is cited"
        );
        assert_eq!(
            rows[1].files,
            [
                file("/home/u/proj/hello.txt", FileType::Write),
                file("/home/u/proj/new.md", FileType::Write),
            ]
        );
        assert_eq!(rows[2].files, [file("/home/u/proj/q.sql", FileType::Cite)]);
    }

    #[test]
    fn a_failed_command_and_a_failed_patch_write_nothing() {
        let item = |item: Value| json!({"type": "event_msg", "payload": {"type": "item_completed", "item": item}});
        let rows = rows_of(&[
            item(json!({
                "type": "CommandExecution", "id": "c1", "exit_code": 1,
                "command": ["/bin/bash", "-lc", "echo x > out.txt"], "cwd": "file:///w",
            })),
            item(json!({
                "type": "FileChange", "id": "f1", "status": "failed",
                "changes": {"/w/a.rs": {"type": "update", "unified_diff": ""}},
            })),
        ]);
        assert_eq!(rows[0].is_error, Some(true));
        assert_eq!(rows[0].files, [file("/w/out.txt", FileType::Cite)]);
        assert_eq!(rows[1].is_error, Some(true));
        assert_eq!(rows[1].files, [file("/w/a.rs", FileType::Cite)]);
    }

    #[test]
    fn an_item_with_no_call_open_stands_on_its_own_line() {
        let rows = rows_of(&[
            json!({"type": "turn_context", "payload": {}}),
            json!({
                "timestamp": "2026-01-02T03:04:05.000Z",
                "type": "event_msg",
                "payload": {"type": "item_completed", "item": {
                    "type": "McpToolCall", "id": "m1", "server": "s", "tool": "t",
                    "arguments": {}, "status": "completed",
                    "result": {"content": [{"type": "text", "text": "ok"}], "isError": false},
                }},
            }),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].line, rows[0].result_line), (2, Some(2)));
        assert_eq!(rows[0].name, "mcp__s__t");
        assert_eq!(rows[0].output.as_deref(), Some("ok"));
        assert_eq!(rows[0].is_error, Some(false));
    }

    #[test]
    fn a_rollout_without_items_has_no_rows() {
        let legacy: Vec<Value> = lines()
            .into_iter()
            .filter(|line| payload_type(line) != Some("item_completed"))
            .collect();
        assert!(rows_of(&legacy).is_empty());
    }
}
