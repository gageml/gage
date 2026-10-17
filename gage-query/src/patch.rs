//! The recorded patch of a tool call --- the hunks Claude Code writes
//! into an Edit, Write, or MultiEdit result as `structuredPatch`, and
//! into a Bash result as `bashEditDiff` when a command changed files
//! --- and `patch_lines(result_raw, op)`, the lines of that patch with
//! `op` of `add`, `remove`, or `context`.

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use datafusion::arrow::array::{Array, ListBuilder, StringArray, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{
    ColumnarValue, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};
use datafusion::scalar::ScalarValue;
use serde_json::Value;

/// One hunk of a recorded patch
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The file the hunk changed, when the record names it: a Bash
    /// diff does, an edit result does not
    pub path: Option<PathBuf>,
    /// Each line with its marker: `+`, `-`, or a space
    pub lines: Vec<(char, String)>,
}

impl Hunk {
    /// The lines with marker `op`
    pub fn lines_with(&self, op: char) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .filter(move |(m, _)| *m == op)
            .map(|(_, l)| l.as_str())
    }
}

/// The hunks of a call's result record, in order; empty when the
/// record carries no patch.
pub fn hunks(result_raw: &str) -> Vec<Hunk> {
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

fn hunk(value: &Value, path: Option<PathBuf>) -> Option<Hunk> {
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
    Some(Hunk { path, lines })
}

/// The marker `op` names, or `None` for an unknown op
fn marker(op: &str) -> Option<char> {
    match op {
        "add" => Some('+'),
        "remove" => Some('-'),
        "context" => Some(' '),
        _ => None,
    }
}

pub fn patch_lines_udf() -> ScalarUDF {
    ScalarUDF::from(PatchLines::new())
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct PatchLines {
    signature: Signature,
}

impl PatchLines {
    fn new() -> Self {
        Self {
            signature: Signature::exact(
                vec![DataType::Utf8, DataType::Utf8],
                Volatility::Immutable,
            ),
        }
    }
}

impl ScalarUDFImpl for PatchLines {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "patch_lines"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::List(Arc::new(Field::new(
            "item",
            DataType::Utf8,
            true,
        ))))
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let [result_raw, op] = <[ColumnarValue; 2]>::try_from(args.args).map_err(|args| {
            DataFusionError::Internal(format!(
                "patch_lines takes two arguments, got {}",
                args.len()
            ))
        })?;
        let ColumnarValue::Scalar(ScalarValue::Utf8(Some(op))) = op else {
            return Err(DataFusionError::Plan(
                "patch_lines: op must be a string literal".into(),
            ));
        };
        let marker = marker(&op).ok_or_else(|| {
            DataFusionError::Plan(format!(
                "patch_lines: op {op:?} is not add, remove, or context"
            ))
        })?;
        let rows = result_raw.into_array(1)?;
        let rows = rows
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| DataFusionError::Internal("patch_lines expects text".into()))?;
        let mut out = ListBuilder::new(StringBuilder::new());
        for i in 0..rows.len() {
            if rows.is_null(i) {
                out.append_null();
                continue;
            }
            for h in hunks(rows.value(i)) {
                for line in h.lines_with(marker) {
                    out.values().append_value(line);
                }
            }
            out.append(true);
        }
        Ok(ColumnarValue::Array(Arc::new(out.finish())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDIT: &str = r#"{"filePath":"/w/a.rs","oldString":"x","newString":"y",
        "structuredPatch":[{"oldStart":1,"oldLines":2,"newStart":1,"newLines":2,
        "lines":[" fn a() {","-    x","+    y","+    z"]}],"userModified":false}"#;
    const SHELL: &str = r#"{"stdout":"","bashEditDiff":{"files":[{"filePath":"/w/b.rs",
        "hunks":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":2,"lines":[" a","+b"]}]},
        {"filePath":"/w/c.rs","hunks":[{"lines":["-gone"]}]}],"changedFiles":["/w/b.rs","/w/c.rs"]}}"#;

    #[test]
    fn hunks_come_from_an_edit_result_or_a_shell_diff() {
        let edit = hunks(EDIT);
        assert_eq!(edit.len(), 1);
        assert_eq!(edit[0].path, None);
        assert_eq!(
            edit[0].lines_with('+').collect::<Vec<_>>(),
            ["    y", "    z"]
        );
        assert_eq!(edit[0].lines_with('-').collect::<Vec<_>>(), ["    x"]);
        assert_eq!(edit[0].lines_with(' ').collect::<Vec<_>>(), ["fn a() {"]);

        let shell = hunks(SHELL);
        assert_eq!(
            shell.iter().map(|h| h.path.clone()).collect::<Vec<_>>(),
            [
                Some(PathBuf::from("/w/b.rs")),
                Some(PathBuf::from("/w/c.rs"))
            ]
        );
        assert_eq!(shell[1].lines_with('-').collect::<Vec<_>>(), ["gone"]);
        assert!(hunks(r#"{"stdout":"no diff"}"#).is_empty());
        assert!(hunks("not json").is_empty());
    }

    #[tokio::test]
    async fn patch_lines_selects_lines_by_op() {
        let ctx = crate::new_context(false, false);
        let sql = format!(
            "SELECT array_to_string(patch_lines('{}', 'add'), '|') AS a, \
             array_to_string(patch_lines('{}', 'remove'), '|') AS r, \
             patch_lines(NULL, 'context') AS n",
            EDIT.replace('\'', "''"),
            SHELL.replace('\'', "''"),
        );
        let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        let text = datafusion::arrow::util::pretty::pretty_format_batches(&batches)
            .unwrap()
            .to_string();
        assert!(text.contains("    y|    z"), "{text}");
        assert!(text.contains("gone"), "{text}");
        let err = match ctx.sql("SELECT patch_lines('{}', 'added')").await {
            Ok(df) => df.collect().await.err(),
            Err(e) => Some(e),
        }
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(err.contains("not add, remove, or context"), "{err}");
    }
}
