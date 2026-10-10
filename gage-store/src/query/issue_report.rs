//! `issue_report(issue_id)`: one row holding a markdown report of an
//! issue, the view an agent reads before judging it. The report has
//! the issue's fields, its description, an `## Evidence` section with
//! each cited note at the commit the issue cites, and a `## History`
//! section with every change entry. `issue_id` is an id or unique
//! prefix. An unknown id gives zero rows; an ambiguous prefix is an
//! error.

use std::sync::{Arc, Mutex};

use super::batch::{BatchSource, BatchTable, external, string_literal};
use crate::{
    ChangeEvent, IssueChange, IssueFull, IssueStore, NoteFull, NoteStore, NoteValue, StatusReason,
    Store, StoreError,
};
use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::TableFunctionImpl;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::Expr;
use gage_core::datetime::ms_to_iso8601;

/// The store-bound `issue_report` table function.
pub fn issue_report_fn(store: Arc<Mutex<Store>>) -> Arc<dyn TableFunctionImpl> {
    Arc::new(IssueReportFn { store })
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("issue_id", DataType::Utf8, false),
        Field::new("report", DataType::Utf8, false),
    ]))
}

#[derive(Debug)]
struct IssueReportFn {
    store: Arc<Mutex<Store>>,
}

impl TableFunctionImpl for IssueReportFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let [arg] = args else {
            return Err(DataFusionError::Plan(
                "issue_report(issue_id) takes exactly one argument".into(),
            ));
        };
        let issue_id = string_literal(arg).ok_or_else(|| {
            DataFusionError::Plan("issue_report: issue_id must be a string literal".into())
        })?;
        Ok(Arc::new(BatchTable::new(
            Arc::clone(&self.store),
            Arc::new(IssueReportSource { issue_id }),
        )))
    }
}

#[derive(Debug)]
struct IssueReportSource {
    issue_id: String,
}

impl BatchSource for IssueReportSource {
    fn schema(&self) -> SchemaRef {
        schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let issue = match IssueStore::from(store).get(&self.issue_id) {
            Ok(issue) => issue,
            Err(StoreError::ObjectNotFound(_)) => return Ok(RecordBatch::new_empty(schema())),
            Err(e) => return Err(external(e)),
        };
        let notes = NoteStore::from(store);
        let evidence = issue
            .evidence
            .iter()
            .map(|sha| notes.at_commit(sha))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(external)?;
        let mut issue_ids = StringBuilder::new();
        let mut reports = StringBuilder::new();
        issue_ids.append_value(&issue.id);
        reports.append_value(render(&issue, &evidence));
        Ok(RecordBatch::try_new(
            schema(),
            vec![Arc::new(issue_ids.finish()), Arc::new(reports.finish())],
        )?)
    }
}

fn render(issue: &IssueFull, evidence: &[NoteFull]) -> String {
    let mut out = String::new();
    out.push_str(&format!("**{}**\n\n", issue.title));
    out.push_str(&format!("- status: {}\n", issue.status));
    out.push_str(&format!("- id: {}\n", issue.id));
    out.push_str(&format!("- name: {}\n", issue.name));
    if let Some(reason) = issue.status_reason {
        out.push_str(&format!("- closed reason: {reason}\n"));
    }
    out.push_str(&format!("- author: {}\n", issue.author));
    if let Some(scan) = &issue.scan {
        out.push_str(&format!("- scan: {scan}\n"));
    }
    out.push_str(&format!("- created: {}\n", ms_to_iso8601(issue.created_ms)));
    if issue.modified_ms != issue.created_ms {
        out.push_str(&format!(
            "- modified: {}\n",
            ms_to_iso8601(issue.modified_ms)
        ));
    }
    if let Some(description) = issue.description.as_deref().map(str::trim_end)
        && !description.is_empty()
    {
        out.push('\n');
        out.push_str(description);
        out.push('\n');
    }
    if !evidence.is_empty() {
        out.push_str("\n## Evidence\n\n");
        for note in evidence {
            render_note(&mut out, note);
        }
    }
    if !issue.changes.is_empty() {
        out.push_str("\n## History\n\n");
        for change in &issue.changes {
            render_change(&mut out, change);
        }
    }
    out
}

fn render_note(out: &mut String, note: &NoteFull) {
    out.push_str(&format!("### {} (note ID {})\n\n", note.name, note.id));
    let value = match &note.value {
        NoteValue::Text(text) => text.clone(),
        NoteValue::Json(json) => json.to_string(),
    };
    out.push_str(&format!("- value: {value}\n"));
    if let Some(target) = &note.target {
        out.push_str(&format!("- target: {target}\n"));
    }
    out.push_str(&format!("- author: {}\n", note.author));
    out.push_str(&format!("- created: {}\n", ms_to_iso8601(note.created_ms)));
    if let Some(metadata) = &note.metadata {
        out.push_str(&format!("- metadata: {metadata}\n"));
    }
    out.push('\n');
}

/// One history line: `created pending`, `pending → open`,
/// `open → closed (duplicate of <id>)`, `comment`, then the message
/// quoted line by line.
fn render_change(out: &mut String, change: &IssueChange) {
    let mut label = match (change.event, change.from_status, change.to_status) {
        (ChangeEvent::Create, _, Some(to)) => format!("created {to}"),
        (ChangeEvent::Status, Some(from), Some(to)) => format!("{from} → {to}"),
        (event, _, _) => event.as_str().to_string(),
    };
    match (change.reason, &change.of) {
        (Some(StatusReason::Duplicate), Some(of)) => {
            label.push_str(&format!(" (duplicate of {of})"));
        }
        (Some(reason), _) => label.push_str(&format!(" ({reason})")),
        (None, _) => {}
    }
    out.push_str(&format!(
        "- {label} · {} · {}\n",
        change.author,
        ms_to_iso8601(change.timestamp_ms)
    ));
    if let Some(message) = &change.message {
        for line in message.lines() {
            out.push_str(&format!("  > {line}\n"));
        }
    }
}
