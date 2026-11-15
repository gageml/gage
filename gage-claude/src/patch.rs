//! The recorded patch of a tool call: the hunks Claude Code writes
//! into an Edit, Write, or MultiEdit result as `structuredPatch`, and
//! into a Bash result as `bashEditDiff` when a command changed files.

use std::path::PathBuf;

use gage_session::PatchHunk;
use serde_json::Value;

/// The hunks of a call's result record, in order; empty when the
/// record carries no patch. An edit result does not name the file it
/// changed; a Bash diff names the file of each hunk.
pub(crate) fn hunks(result_raw: &str) -> Vec<PatchHunk> {
    let Ok(value) = serde_json::from_str::<Value>(result_raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(patch) = value.get("structuredPatch").and_then(Value::as_array) {
        out.extend(patch.iter().filter_map(|h| hunk(h, None)));
    }
    if let Some(files) = value
        .get("bashEditDiff")
        .and_then(|d| d.get("files"))
        .and_then(Value::as_array)
    {
        for file in files {
            let path = file
                .get("filePath")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            if let Some(list) = file.get("hunks").and_then(Value::as_array) {
                out.extend(list.iter().filter_map(|h| hunk(h, path.clone())));
            }
        }
    }
    out
}

fn hunk(value: &Value, path: Option<PathBuf>) -> Option<PatchHunk> {
    let lines = value
        .get("lines")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(|l| {
            let mut chars = l.chars();
            let marker = chars.next().unwrap_or(' ');
            (marker, chars.as_str().to_string())
        })
        .collect();
    Some(PatchHunk { path, lines })
}
