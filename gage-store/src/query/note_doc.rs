//! The `note_doc` table: one row per note name a task declares it
//! writes, with the doc the declaration carries. The scan scope reads
//! the rows from the scan's plan, which records each task's
//! `note_writes`; the store scope builds them from the scanner registry.

use std::sync::Arc;

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::Result;

pub fn note_doc_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("note_name", DataType::Utf8, false),
        Field::new("doc", DataType::Utf8, false),
        // `<scanner>:<task>`
        Field::new("written_by", DataType::Utf8, false),
    ]))
}

/// One `note_doc` row.
pub struct NoteDocRow {
    pub note_name: String,
    pub doc: String,
    pub written_by: String,
}

/// The `note_doc` batch for `rows`.
pub fn note_doc_rows(rows: &[NoteDocRow]) -> Result<RecordBatch> {
    let mut note_names = StringBuilder::new();
    let mut docs = StringBuilder::new();
    let mut written_bys = StringBuilder::new();
    for row in rows {
        note_names.append_value(&row.note_name);
        docs.append_value(&row.doc);
        written_bys.append_value(&row.written_by);
    }
    Ok(RecordBatch::try_new(
        note_doc_schema(),
        vec![
            Arc::new(note_names.finish()),
            Arc::new(docs.finish()),
            Arc::new(written_bys.finish()),
        ],
    )?)
}

/// The rows a scan's plan declares: every `note_writes` entry of
/// every task, in plan order. Plans written before the field was
/// renamed record it as `writes`.
pub fn note_doc_rows_from_plan(plan: &serde_json::Value) -> Vec<NoteDocRow> {
    let mut rows = Vec::new();
    let Some(tasks) = plan.get("tasks").and_then(|t| t.as_array()) else {
        return rows;
    };
    for task in tasks {
        let Some(label) = task.get("task").and_then(|t| t.as_str()) else {
            continue;
        };
        let writes = task
            .get("note_writes")
            .or_else(|| task.get("writes"))
            .and_then(|w| w.as_object());
        let Some(writes) = writes else {
            continue;
        };
        for (name, doc) in writes {
            rows.push(NoteDocRow {
                note_name: name.clone(),
                doc: doc.as_str().unwrap_or_default().to_string(),
                written_by: label.to_string(),
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_rows_read_note_writes_and_the_legacy_writes_key() {
        let plan = serde_json::json!({
            "tasks": [
                { "task": "a:new", "note_writes": { "n": "the n note" } },
                { "task": "b:old", "writes": { "o": "the o note" } },
                { "task": "c:none" }
            ]
        });
        let rows = note_doc_rows_from_plan(&plan);
        let seen: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|r| (r.note_name.as_str(), r.doc.as_str(), r.written_by.as_str()))
            .collect();
        assert_eq!(
            seen,
            [("n", "the n note", "a:new"), ("o", "the o note", "b:old")]
        );
    }
}
