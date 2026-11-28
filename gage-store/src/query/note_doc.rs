//! The `note_doc` table: one row per note name a task of a scan
//! declared it writes, with the doc the declaration carried. The rows
//! come from each scan's plan, which records every task's
//! `note_writes`, so a doc is the one in force when the scan ran. A
//! note joins to its doc on the scan, the note name, and the task its
//! `author` names.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use serde_json::Value;

use super::batch::{BatchSource, BatchTable, external, readable};
use crate::{Order, ScanStore, Store};

pub fn note_doc_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("scan_id", DataType::Utf8, false),
        Field::new("note_name", DataType::Utf8, false),
        Field::new("doc", DataType::Utf8, false),
        // The declaring task's scanner, as `scan_task` names it
        Field::new("scanner", DataType::Utf8, false),
        Field::new("task", DataType::Utf8, false),
    ]))
}

/// One `note_doc` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDocRow {
    pub scan_id: String,
    pub note_name: String,
    pub doc: String,
    pub scanner: String,
    pub task: String,
}

/// The `note_doc` batch for `rows`.
pub fn note_doc_rows(rows: &[NoteDocRow]) -> Result<RecordBatch> {
    let mut scan_ids = StringBuilder::new();
    let mut note_names = StringBuilder::new();
    let mut docs = StringBuilder::new();
    let mut scanners = StringBuilder::new();
    let mut tasks = StringBuilder::new();
    for row in rows {
        scan_ids.append_value(&row.scan_id);
        note_names.append_value(&row.note_name);
        docs.append_value(&row.doc);
        scanners.append_value(&row.scanner);
        tasks.append_value(&row.task);
    }
    Ok(RecordBatch::try_new(
        note_doc_schema(),
        vec![
            Arc::new(scan_ids.finish()),
            Arc::new(note_names.finish()),
            Arc::new(docs.finish()),
            Arc::new(scanners.finish()),
            Arc::new(tasks.finish()),
        ],
    )?)
}

/// The rows the plan of the scan `scan_id` declares: every
/// `note_writes` entry of every task, in plan order. Plans written
/// before the field was renamed record it as `writes`.
pub fn note_doc_rows_from_plan(scan_id: &str, plan: &Value) -> Vec<NoteDocRow> {
    let mut rows = Vec::new();
    let Some(tasks) = plan.get("tasks").and_then(|t| t.as_array()) else {
        return rows;
    };
    for entry in tasks {
        // The label is `<scanner>:<task>`, and neither part holds a `:`
        let label = entry.get("task").and_then(|t| t.as_str());
        let Some((scanner, task)) = label.and_then(|label| label.rsplit_once(':')) else {
            continue;
        };
        let writes = entry
            .get("note_writes")
            .or_else(|| entry.get("writes"))
            .and_then(|w| w.as_object());
        let Some(writes) = writes else {
            continue;
        };
        for (name, doc) in writes {
            rows.push(NoteDocRow {
                scan_id: scan_id.to_string(),
                note_name: name.clone(),
                doc: doc.as_str().unwrap_or_default().to_string(),
                scanner: scanner.to_string(),
                task: task.to_string(),
            });
        }
    }
    rows
}

/// Rows over every live scan, newest first, read from each scan
/// object's `plan.json`. Nothing else of the scan is read.
#[derive(Debug)]
struct Source;

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        note_doc_schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let scans = ScanStore::from(store);
        let tips = scans
            .query()
            .order(Order::ModifiedDesc)
            .tips()
            .map_err(external)?;
        let mut rows = Vec::new();
        for tip in &tips {
            let Some(plan) = readable(scans.plan_at_commit(&tip.sha))? else {
                continue;
            };
            if let Some(plan) = plan {
                rows.extend(note_doc_rows_from_plan(&tip.id, &plan));
            }
        }
        note_doc_rows(&rows)
    }
}

/// The store-bound `note_doc` table.
pub fn note_doc_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_rows_read_note_writes_and_the_legacy_writes_key() {
        let plan = serde_json::json!({
            "tasks": [
                { "task": "gage/a:new", "note_writes": { "n": "the n note" } },
                { "task": "b:old", "writes": { "o": "the o note" } },
                { "task": "c:none" },
                { "task": "no-task-part", "note_writes": { "x": "" } }
            ]
        });
        let rows = note_doc_rows_from_plan("SCAN", &plan);
        let seen: Vec<[&str; 5]> = rows
            .iter()
            .map(|r| {
                [
                    r.scan_id.as_str(),
                    r.note_name.as_str(),
                    r.doc.as_str(),
                    r.scanner.as_str(),
                    r.task.as_str(),
                ]
            })
            .collect();
        assert_eq!(
            seen,
            [
                ["SCAN", "n", "the n note", "gage/a", "new"],
                ["SCAN", "o", "the o note", "b", "old"]
            ]
        );
    }
}
