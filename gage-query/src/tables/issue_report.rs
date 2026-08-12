//! `issue_report(issue_id)` — return one row with a markdown-formatted
//! report for an issue: scalar fields, the description (with
//! `scanner:/...` refs already resolved), an `## Evidence` section
//! listing related notes, and a `## History` section listing logged
//! events.
//!
//! Replaces the `IssueGet` MCP tool — same rendering, surfaced through
//! Query. `issue_id` is matched as a prefix against `issue.id`; an
//! ambiguous prefix is an error. Unknown ids return zero rows.

use std::any::Any;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use datafusion::arrow::array::StringBuilder;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::{Session, TableFunctionImpl};
use datafusion::common::ScalarValue;
use datafusion::datasource::{MemTable, TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::Expr;
use gage_core::datetime::ms_to_iso8601;
use gage_db::db::open_db;
use gage_db::issue::{self, Issue, LoggedEvent};
use gage_db::note::Note;
use gage_registry::scanner::scanner_home_paths;

use crate::udf::resolve_ref::resolve_one;

pub const ISSUE_REPORT_ARGS: &str = "issue_id text";

pub fn issue_report_schema() -> SchemaRef {
    SCHEMA.clone()
}

static SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("issue_id", DataType::Utf8, false),
        Field::new("report", DataType::Utf8, false),
    ]))
});

#[derive(Debug)]
pub struct IssueReportFn;

impl IssueReportFn {
    pub fn new() -> Self {
        Self
    }
}

impl Default for IssueReportFn {
    fn default() -> Self {
        Self::new()
    }
}

impl TableFunctionImpl for IssueReportFn {
    fn call(&self, args: &[Expr]) -> Result<Arc<dyn TableProvider>> {
        let [arg] = args else {
            return Err(DataFusionError::Plan(
                "issue_report(issue_id) takes exactly one argument".into(),
            ));
        };
        let issue_id = string_literal(arg).ok_or_else(|| {
            DataFusionError::Plan("issue_report issue_id must be a string literal".into())
        })?;
        Ok(Arc::new(IssueReportTable { issue_id }))
    }
}

fn string_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::Literal(ScalarValue::Utf8(Some(s)), _)
        | Expr::Literal(ScalarValue::LargeUtf8(Some(s)), _)
        | Expr::Literal(ScalarValue::Utf8View(Some(s)), _) => Some(s.clone()),
        _ => None,
    }
}

#[derive(Debug)]
struct IssueReportTable {
    issue_id: String,
}

#[async_trait]
impl TableProvider for IssueReportTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        SCHEMA.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let batch = build_row(&self.issue_id)?;
        let mem = MemTable::try_new(SCHEMA.clone(), vec![vec![batch]])?;
        mem.scan(state, projection, &[], None).await
    }
}

fn build_row(issue_id: &str) -> Result<RecordBatch> {
    let conn = open_db().map_err(|e| DataFusionError::Execution(format!("open gage db: {e}")))?;
    let issue = match issue::get(&conn, issue_id) {
        Ok(i) => i,
        Err(issue::IssueError::NotFound(_)) => return Ok(RecordBatch::new_empty(SCHEMA.clone())),
        Err(e @ issue::IssueError::Ambiguous(_, _)) => {
            return Err(DataFusionError::Plan(e.to_string()));
        }
        Err(e) => return Err(DataFusionError::Execution(e.to_string())),
    };
    let related = issue::related_notes(&conn, &issue.id)
        .map_err(|e| DataFusionError::Execution(format!("load related notes: {e}")))?;
    let events = issue::issue_events_for(&conn, &issue.id)
        .map_err(|e| DataFusionError::Execution(format!("load issue events: {e}")))?;

    let report = render(&issue, &related, &events);

    let mut issue_ids = StringBuilder::new();
    let mut reports = StringBuilder::new();
    issue_ids.append_value(&issue.id);
    reports.append_value(&report);

    Ok(RecordBatch::try_new(
        SCHEMA.clone(),
        vec![Arc::new(issue_ids.finish()), Arc::new(reports.finish())],
    )?)
}

fn render(issue: &Issue, related: &[Note], events: &[LoggedEvent]) -> String {
    let homes = scanner_home_paths();
    let scanner_name = issue.author.strip_prefix("scanner:");
    let description = issue
        .description
        .as_deref()
        .map(|d| resolve_one(d, &homes))
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str(&format!("**{}**\n\n", issue.title));
    out.push_str(&format!("- status: {}\n", issue.status.as_str()));
    out.push_str(&format!("- id: {}\n", issue.id));
    out.push_str(&format!("- name: {}\n", issue.name));
    if let Some(r) = issue.status_reason {
        out.push_str(&format!("- closed reason: {}\n", r.as_str()));
    }
    if let Some(s) = scanner_name {
        out.push_str(&format!("- scanner: {s}\n"));
    }
    out.push_str(&format!("- created: {}\n", ms_to_iso8601(issue.created)));
    if let Some(m) = issue.modified {
        out.push_str(&format!("- modified: {}\n", ms_to_iso8601(m)));
    }
    if !description.is_empty() {
        out.push('\n');
        out.push_str(description.trim_end());
        out.push('\n');
    }

    if !related.is_empty() {
        out.push_str("\n## Evidence\n\n");
        for n in related {
            render_note(&mut out, n);
        }
    }

    if !events.is_empty() {
        out.push_str("\n## History\n\n");
        for ev in events {
            render_event(&mut out, ev);
        }
    }

    out
}

fn render_note(out: &mut String, note: &Note) {
    out.push_str(&format!("### {} (note ID {})\n\n", note.name, note.id));
    out.push_str(&format!("- value: {}\n", note.value.to_json()));
    out.push_str(&format!("- target: {}\n", note.target.to_uri()));
    out.push_str(&format!("- author: {}\n", note.author));
    out.push_str(&format!("- created: {}\n", ms_to_iso8601(note.created)));
    if let Some(e) = note.explanation.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("- explanation: {e}\n"));
    }
    if let Some(m) = note.metadata.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("- metadata: {m}\n"));
    }
    out.push('\n');
}

fn render_event(out: &mut String, ev: &LoggedEvent) {
    out.push_str(&format!(
        "- {} · {} · {}\n",
        ev.event.to_label(),
        ev.author,
        ms_to_iso8601(ev.timestamp),
    ));
    if let Some(m) = ev.event.message() {
        for line in m.lines() {
            out.push_str(&format!("  > {line}\n"));
        }
    }
}
