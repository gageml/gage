//! The `scan_task` and `scan_task_agent` tables: one row per task of
//! a scan, and one row per agent a task ran. The schemas and row
//! builders are shared with the scan scope, whose rows may describe
//! an active scan: no commit, and tasks still `pending` or `started`.
//!
//! A task's `num` is its position in the scan's plan, the dispatch
//! tie-break order, so a listing reads top-down as the runner
//! dispatched; a scan written without a plan numbers its tasks in
//! tree order. An agent's `result` is the harness's result message
//! verbatim; the driver that ran the agent defines its shape.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;
use serde_json::Value;

use super::batch::{BatchSource, BatchTable, external};
use crate::{Order, ScanStore, ScanTask, Store};

fn timestamp() -> DataType {
    DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
}

pub(crate) fn scan_task_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("scan_id", DataType::Utf8, false),
        // Position in the plan, from 1
        Field::new("num", DataType::Int64, false),
        Field::new("scanner", DataType::Utf8, false),
        Field::new("task", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        // Null until the task starts
        Field::new("started", timestamp(), true),
        // Null until the task stops
        Field::new("stopped", timestamp(), true),
        Field::new("worked_ms", DataType::Int64, true),
        // How the task entered the plan: `explicit`, `group:<name>`,
        // or `required_by:<pattern>`; null without a plan
        Field::new("selected", DataType::Utf8, true),
        // For a skipped task, the `needs` pattern no completed
        // upstream task satisfied
        Field::new("skipped_needs", DataType::Utf8, true),
        // For a skipped task, the upstream tasks that were to write
        // it, as a JSON array of `<scanner>:<task>`
        Field::new("skipped_upstream", DataType::Utf8, true),
        // System
        // Null for an active scan
        Field::new("scan_commit", DataType::Utf8, true),
    ]))
}

pub(crate) fn scan_task_agent_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("scan_id", DataType::Utf8, false),
        Field::new("scanner", DataType::Utf8, false),
        Field::new("task", DataType::Utf8, false),
        // The agent session's object id
        Field::new("session_id", DataType::Utf8, false),
        // The harness process's exit code; -1 when it was signaled
        Field::new("exit_code", DataType::Int64, false),
        // The harness's result message verbatim; null when the agent
        // produced none
        Field::new("result", DataType::Utf8, true),
        // System
        // Null for an active scan
        Field::new("scan_commit", DataType::Utf8, true),
    ]))
}

/// One `scan_task` row from either scope.
pub(crate) struct ScanTaskRow {
    pub scan_id: String,
    pub num: i64,
    pub scanner: String,
    pub task: String,
    pub status: &'static str,
    pub started_ms: Option<i64>,
    pub stopped_ms: Option<i64>,
    pub worked_ms: Option<i64>,
    pub selected: Option<String>,
    pub skipped_needs: Option<String>,
    pub skipped_upstream: Option<String>,
    pub scan_commit: Option<String>,
}

/// One `scan_task_agent` row from either scope.
pub(crate) struct ScanTaskAgentRow {
    pub scan_id: String,
    pub scanner: String,
    pub task: String,
    pub session_id: String,
    pub exit_code: i64,
    pub result: Option<String>,
    pub scan_commit: Option<String>,
}

/// The `scan_task` rows of one scan, in plan order. `plan` is the
/// scan's `plan.json`; without it the tasks keep tree order.
pub(crate) fn task_rows_of(
    scan_id: &str,
    scan_commit: Option<&str>,
    tasks: &[ScanTask],
    plan: Option<&Value>,
) -> Vec<ScanTaskRow> {
    let planned = plan_index(plan);
    let mut rows: Vec<ScanTaskRow> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let label = format!("{}:{}", t.scanner, t.task);
            let (num, selected) = match planned.get(&label) {
                Some((num, selected)) => (*num, selected.clone()),
                None => (planned.len() as i64 + i as i64 + 1, None),
            };
            ScanTaskRow {
                scan_id: scan_id.to_string(),
                num,
                scanner: t.scanner.clone(),
                task: t.task.clone(),
                status: t.attrs.status.as_str(),
                started_ms: t.attrs.started,
                stopped_ms: t.attrs.stopped,
                worked_ms: t.attrs.worked_ms.map(|w| w as i64),
                selected,
                skipped_needs: t.attrs.skipped.as_ref().map(|s| s.needs.clone()),
                skipped_upstream: t
                    .attrs
                    .skipped
                    .as_ref()
                    .map(|s| Value::from(s.upstream.clone()).to_string()),
                scan_commit: scan_commit.map(String::from),
            }
        })
        .collect();
    rows.sort_by_key(|r| r.num);
    rows
}

/// Task label to its plan position, from 1, and its `selected`
/// value. Empty without a plan; a plan entry that is not an object
/// with a `task` string is left out.
fn plan_index(plan: Option<&Value>) -> HashMap<String, (i64, Option<String>)> {
    let mut index = HashMap::new();
    let Some(tasks) = plan.and_then(|p| p.get("tasks")).and_then(Value::as_array) else {
        return index;
    };
    for (i, entry) in tasks.iter().enumerate() {
        if let Some(label) = entry.get("task").and_then(Value::as_str) {
            let selected = entry
                .get("selected")
                .and_then(Value::as_str)
                .map(String::from);
            index.insert(label.to_string(), (i as i64 + 1, selected));
        }
    }
    index
}

/// The `scan_task_agent` rows of one scan. `read_result` returns the
/// bytes of an agent's `result` file given the scanner, task, and
/// agent session id, or `None` when the record lacks one.
pub(crate) fn agent_rows_of(
    scan_id: &str,
    scan_commit: Option<&str>,
    tasks: &[ScanTask],
    read_result: impl Fn(&str, &str, &str) -> Result<Option<Vec<u8>>>,
) -> Result<Vec<ScanTaskAgentRow>> {
    let mut rows = Vec::new();
    for t in tasks {
        for a in &t.agents {
            let result = read_result(&t.scanner, &t.task, &a.id)?
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            rows.push(ScanTaskAgentRow {
                scan_id: scan_id.to_string(),
                scanner: t.scanner.clone(),
                task: t.task.clone(),
                session_id: a.id.clone(),
                exit_code: a.attrs.exit_code,
                result,
                scan_commit: scan_commit.map(String::from),
            });
        }
    }
    Ok(rows)
}

/// The `scan_task` batch for `rows`.
pub(crate) fn scan_task_rows(rows: &[ScanTaskRow]) -> Result<RecordBatch> {
    let len = rows.len();
    let mut scan_ids = StringBuilder::with_capacity(len, len * 26);
    let mut nums = Int64Builder::with_capacity(len);
    let mut scanners = StringBuilder::new();
    let mut tasks = StringBuilder::new();
    let mut statuses = StringBuilder::new();
    let mut starteds = TimestampMillisecondBuilder::with_capacity(len);
    let mut stoppeds = TimestampMillisecondBuilder::with_capacity(len);
    let mut workeds = Int64Builder::with_capacity(len);
    let mut selecteds = StringBuilder::new();
    let mut needs = StringBuilder::new();
    let mut upstreams = StringBuilder::new();
    let mut commits = StringBuilder::new();
    for row in rows {
        scan_ids.append_value(&row.scan_id);
        nums.append_value(row.num);
        scanners.append_value(&row.scanner);
        tasks.append_value(&row.task);
        statuses.append_value(row.status);
        starteds.append_option(row.started_ms);
        stoppeds.append_option(row.stopped_ms);
        workeds.append_option(row.worked_ms);
        selecteds.append_option(row.selected.as_deref());
        needs.append_option(row.skipped_needs.as_deref());
        upstreams.append_option(row.skipped_upstream.as_deref());
        commits.append_option(row.scan_commit.as_deref());
    }
    Ok(RecordBatch::try_new(
        scan_task_schema(),
        vec![
            Arc::new(scan_ids.finish()),
            Arc::new(nums.finish()),
            Arc::new(scanners.finish()),
            Arc::new(tasks.finish()),
            Arc::new(statuses.finish()),
            Arc::new(starteds.finish().with_timezone("UTC")),
            Arc::new(stoppeds.finish().with_timezone("UTC")),
            Arc::new(workeds.finish()),
            Arc::new(selecteds.finish()),
            Arc::new(needs.finish()),
            Arc::new(upstreams.finish()),
            Arc::new(commits.finish()),
        ],
    )?)
}

/// The `scan_task_agent` batch for `rows`.
pub(crate) fn scan_task_agent_rows(rows: &[ScanTaskAgentRow]) -> Result<RecordBatch> {
    let len = rows.len();
    let mut scan_ids = StringBuilder::with_capacity(len, len * 26);
    let mut scanners = StringBuilder::new();
    let mut tasks = StringBuilder::new();
    let mut sessions = StringBuilder::with_capacity(len, len * 26);
    let mut exit_codes = Int64Builder::with_capacity(len);
    let mut results = StringBuilder::new();
    let mut commits = StringBuilder::new();
    for row in rows {
        scan_ids.append_value(&row.scan_id);
        scanners.append_value(&row.scanner);
        tasks.append_value(&row.task);
        sessions.append_value(&row.session_id);
        exit_codes.append_value(row.exit_code);
        results.append_option(row.result.as_deref());
        commits.append_option(row.scan_commit.as_deref());
    }
    Ok(RecordBatch::try_new(
        scan_task_agent_schema(),
        vec![
            Arc::new(scan_ids.finish()),
            Arc::new(scanners.finish()),
            Arc::new(tasks.finish()),
            Arc::new(sessions.finish()),
            Arc::new(exit_codes.finish()),
            Arc::new(results.finish()),
            Arc::new(commits.finish()),
        ],
    )?)
}

/// Which of the two tables a store-scope source builds
#[derive(Debug, Clone, Copy)]
enum Kind {
    Task,
    Agent,
}

/// Rows over every live scan, newest first, read from each scan
/// object.
#[derive(Debug)]
struct Source(Kind);

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        match self.0 {
            Kind::Task => scan_task_schema(),
            Kind::Agent => scan_task_agent_schema(),
        }
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let scans = ScanStore::from(store);
        let tips = scans
            .query()
            .order(Order::ModifiedDesc)
            .tips()
            .map_err(external)?;
        let mut task_rows = Vec::new();
        let mut agent_rows = Vec::new();
        for tip in &tips {
            let record = scans.at_commit(&tip.sha).map_err(external)?;
            let content = &record.content;
            match self.0 {
                Kind::Task => task_rows.extend(task_rows_of(
                    &record.id,
                    Some(&record.commit_sha),
                    &content.tasks,
                    content.plan.as_ref(),
                )),
                Kind::Agent => agent_rows.extend(agent_rows_of(
                    &record.id,
                    Some(&record.commit_sha),
                    &content.tasks,
                    |scanner, task, id| {
                        scans
                            .agent_file(&record.commit_sha, scanner, task, id, "result")
                            .map_err(external)
                    },
                )?),
            }
        }
        match self.0 {
            Kind::Task => scan_task_rows(&task_rows),
            Kind::Agent => scan_task_agent_rows(&agent_rows),
        }
    }
}

/// The store-bound `scan_task` table.
pub fn scan_task_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source(Kind::Task))))
}

/// The store-bound `scan_task_agent` table.
pub fn scan_task_agent_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source(Kind::Agent))))
}
