//! The patch a file change records: per path, the content of a file
//! added or deleted, or the unified diff of a file updated.

use std::path::PathBuf;

use gage_session::PatchHunk;
use serde_json::Value;

/// The hunks of a tool call's result record, in order: those of a
/// `FileChange` item, and none for any other record.
pub(crate) fn hunks(result_raw: &str) -> Vec<PatchHunk> {
    let Ok(item) = serde_json::from_str::<Value>(result_raw) else {
        return Vec::new();
    };
    if item.get("type").and_then(Value::as_str) != Some("FileChange") {
        return Vec::new();
    }
    let Some(changes) = item.get("changes").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (path, change) in changes {
        let path = Some(PathBuf::from(path));
        let text = |key| change.get(key).and_then(Value::as_str);
        match change.get("type").and_then(Value::as_str) {
            Some("add") => out.extend(whole_file(path, '+', text("content"))),
            Some("delete") => out.extend(whole_file(path, '-', text("content"))),
            _ => out.extend(diff_hunks(&path, text("unified_diff").unwrap_or_default())),
        }
    }
    out
}

/// One hunk of every line of `content` under `marker`: a file added
/// or deleted whole.
fn whole_file(path: Option<PathBuf>, marker: char, content: Option<&str>) -> Option<PatchHunk> {
    let lines: Vec<(char, String)> = content?
        .lines()
        .map(|line| (marker, line.to_string()))
        .collect();
    (!lines.is_empty()).then_some(PatchHunk { path, lines })
}

/// The hunks of a unified diff: the lines after each `@@` header,
/// each with its marker. The file headers, and the note that a file
/// lacks a final newline, are not lines of the patch.
fn diff_hunks(path: &Option<PathBuf>, diff: &str) -> Vec<PatchHunk> {
    let mut out: Vec<PatchHunk> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("@@") {
            out.push(PatchHunk {
                path: path.clone(),
                lines: Vec::new(),
            });
            continue;
        }
        let mut chars = line.chars();
        let marker = match chars.next() {
            Some(marker @ ('+' | '-' | ' ')) => marker,
            // A context line emptied of its leading space
            None => ' ',
            Some(_) => continue,
        };
        if let Some(hunk) = out.last_mut() {
            hunk.lines.push((marker, chars.as_str().to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_file_change_yields_its_hunks_by_path() {
        let item = json!({
            "type": "FileChange",
            "id": "exec-1",
            "status": "completed",
            "changes": {
                "/home/u/proj/a.txt": {
                    "type": "update",
                    "unified_diff": "@@ -1,2 +1,2 @@\n goodbye\n-world\n+moon\n@@ -9 +9 @@\n-x\n+y\n",
                    "move_path": null,
                },
                "/home/u/proj/new.md": {"type": "add", "content": "# Title\n\nBody\n"},
            },
        });
        let hunks = hunks(&item.to_string());
        assert_eq!(hunks.len(), 3);
        let update = &hunks[0];
        assert_eq!(update.path, Some(PathBuf::from("/home/u/proj/a.txt")));
        assert_eq!(update.lines_with('+').collect::<Vec<_>>(), ["moon"]);
        assert_eq!(update.lines_with('-').collect::<Vec<_>>(), ["world"]);
        assert_eq!(update.lines_with(' ').collect::<Vec<_>>(), ["goodbye"]);
        assert_eq!(hunks[1].lines_with('+').collect::<Vec<_>>(), ["y"]);
        let added = &hunks[2];
        assert_eq!(added.path, Some(PathBuf::from("/home/u/proj/new.md")));
        assert_eq!(
            added.lines_with('+').collect::<Vec<_>>(),
            ["# Title", "", "Body"]
        );
    }

    #[test]
    fn a_record_that_is_not_a_file_change_has_no_hunks() {
        let command = json!({"type": "CommandExecution", "aggregated_output": "+x"});
        assert!(hunks(&command.to_string()).is_empty());
        assert!(hunks("not json").is_empty());
    }
}
