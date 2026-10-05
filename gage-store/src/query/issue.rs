//! The `issue` and `issue_event` tables: one row per live issue
//! object, and one row per change entry under every live issue's
//! `changes/`. The markers come from the index; everything else comes
//! from the object, read for every row. The row builders are shared
//! with the scan scope, which serves the same tables over one scan's
//! issues.

use std::sync::{Arc, Mutex};

use super::batch::{BatchSource, BatchTable, external, unique_prefix_lens};
use crate::{ISSUE_TYPE, IssueChange, IssueFull, IssueStore, Order, Store};
use datafusion::arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;

fn timestamp() -> DataType {
    DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
}

pub(crate) fn issue_schema() -> SchemaRef {
    // Column order: identity, key, value, timestamps, provenance,
    // system. A new column joins the category it belongs to.
    Arc::new(Schema::new(vec![
        // Identity
        Field::new("id", DataType::Utf8, false),
        // Key
        Field::new("name", DataType::Utf8, false),
        // Value
        Field::new("title", DataType::Utf8, false),
        Field::new("description", DataType::Utf8, true),
        Field::new("status", DataType::Utf8, false),
        // Set when status is `closed`
        Field::new("status_reason", DataType::Utf8, true),
        // Gage URL of the writer
        Field::new("author", DataType::Utf8, false),
        // Notes cited, from `evidence.link`
        Field::new("evidence_count", DataType::Int64, false),
        // Timestamps
        Field::new("created", timestamp(), false),
        Field::new("modified", timestamp(), false),
        // Provenance
        // The scan the issue was written during
        Field::new("scan", DataType::Utf8, true),
        // The writer's identity for the issue; a later write under the
        // same key replaces it
        Field::new("key", DataType::Utf8, true),
        // System
        // Shortest prefix of `id` unique among the issues listed
        Field::new("id_prefix", DataType::Utf8, false),
        // `git:<commit sha>` of the version listed; null for an issue
        // an active scan wrote, which has no commit until apply
        Field::new("locator", DataType::Utf8, true),
        Field::new("commit", DataType::Utf8, true),
    ]))
}

pub(crate) fn issue_event_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("issue_id", DataType::Utf8, false),
        // The change entry's ULID, chronological within an issue
        Field::new("event_id", DataType::Utf8, false),
        // The time encoded in the ULID
        Field::new("timestamp", timestamp(), false),
        Field::new("author", DataType::Utf8, false),
        // `create`, `status`, `comment`, or `edit`
        Field::new("event", DataType::Utf8, false),
        Field::new("from_status", DataType::Utf8, true),
        Field::new("to_status", DataType::Utf8, true),
        // Set when `to_status` is `closed`
        Field::new("reason", DataType::Utf8, true),
        Field::new("message", DataType::Utf8, true),
    ]))
}

/// Every live issue, newest modified first.
fn live_issues(store: &Store) -> Result<Vec<IssueFull>> {
    let issues = IssueStore::from(store);
    issues
        .query()
        .order(Order::ModifiedDesc)
        .tips()
        .map_err(external)?
        .iter()
        .map(|tip| issues.at_commit(&tip.sha).map_err(external))
        .collect()
}

#[derive(Debug)]
struct IssueSource;

impl BatchSource for IssueSource {
    fn schema(&self) -> SchemaRef {
        issue_schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let issues = live_issues(store)?;
        let prefix_len =
            unique_prefix_lens(store.short_prefix_ids(Some(ISSUE_TYPE)).map_err(external)?);
        let rows: Vec<IssueRow> = issues
            .iter()
            .map(|issue| {
                let n = prefix_len.get(&issue.id).copied().unwrap_or(issue.id.len());
                IssueRow::from_full(issue, issue.id.chars().take(n).collect())
            })
            .collect();
        issue_rows(&rows)
    }
}

/// One `issue` row from either scope.
pub(crate) struct IssueRow {
    pub id: String,
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub status_reason: Option<String>,
    pub author: String,
    pub evidence_count: usize,
    pub created_ms: i64,
    pub modified_ms: i64,
    pub scan: Option<String>,
    pub key: Option<String>,
    pub id_prefix: String,
    /// `None` for an issue an active scan wrote
    pub commit: Option<String>,
}

impl IssueRow {
    pub(crate) fn from_full(issue: &IssueFull, id_prefix: String) -> Self {
        IssueRow {
            id: issue.id.clone(),
            name: issue.name.clone(),
            title: issue.title.clone(),
            description: issue.description.clone(),
            status: issue.status.as_str().to_string(),
            status_reason: issue.status_reason.map(|r| r.as_str().to_string()),
            author: issue.author.clone(),
            evidence_count: issue.evidence.len(),
            created_ms: issue.created_ms,
            modified_ms: issue.modified_ms,
            scan: issue.scan.clone(),
            key: issue.key.clone(),
            id_prefix,
            commit: Some(issue.commit_sha.clone()),
        }
    }
}

/// The `issue` batch for `rows`.
pub(crate) fn issue_rows(rows: &[IssueRow]) -> Result<RecordBatch> {
    let len = rows.len();
    let mut ids = StringBuilder::with_capacity(len, len * 26);
    let mut names = StringBuilder::new();
    let mut titles = StringBuilder::new();
    let mut descriptions = StringBuilder::new();
    let mut statuses = StringBuilder::new();
    let mut reasons = StringBuilder::new();
    let mut authors = StringBuilder::new();
    let mut evidence_counts = Int64Builder::with_capacity(len);
    let mut createds = TimestampMillisecondBuilder::with_capacity(len);
    let mut modifieds = TimestampMillisecondBuilder::with_capacity(len);
    let mut scans = StringBuilder::new();
    let mut keys = StringBuilder::new();
    let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
    let mut locators = StringBuilder::with_capacity(len, len * 44);
    let mut commits = StringBuilder::with_capacity(len, len * 40);
    for row in rows {
        ids.append_value(&row.id);
        names.append_value(&row.name);
        titles.append_value(&row.title);
        descriptions.append_option(row.description.as_deref());
        statuses.append_value(&row.status);
        reasons.append_option(row.status_reason.as_deref());
        authors.append_value(&row.author);
        evidence_counts.append_value(row.evidence_count as i64);
        createds.append_value(row.created_ms);
        modifieds.append_value(row.modified_ms);
        scans.append_option(row.scan.as_deref());
        keys.append_option(row.key.as_deref());
        id_prefixes.append_value(&row.id_prefix);
        locators.append_option(row.commit.as_ref().map(|c| format!("git:{c}")));
        commits.append_option(row.commit.as_deref());
    }
    Ok(RecordBatch::try_new(
        issue_schema(),
        vec![
            Arc::new(ids.finish()),
            Arc::new(names.finish()),
            Arc::new(titles.finish()),
            Arc::new(descriptions.finish()),
            Arc::new(statuses.finish()),
            Arc::new(reasons.finish()),
            Arc::new(authors.finish()),
            Arc::new(evidence_counts.finish()),
            Arc::new(createds.finish().with_timezone("UTC")),
            Arc::new(modifieds.finish().with_timezone("UTC")),
            Arc::new(scans.finish()),
            Arc::new(keys.finish()),
            Arc::new(id_prefixes.finish()),
            Arc::new(locators.finish()),
            Arc::new(commits.finish()),
        ],
    )?)
}

/// The `issue_event` batch for the change entries of each issue.
pub(crate) fn issue_event_rows<'a>(
    issues: impl IntoIterator<Item = (&'a str, &'a [IssueChange])>,
) -> Result<RecordBatch> {
    let mut issue_ids = StringBuilder::new();
    let mut event_ids = StringBuilder::new();
    let mut timestamps = TimestampMillisecondBuilder::new();
    let mut authors = StringBuilder::new();
    let mut events = StringBuilder::new();
    let mut from_statuses = StringBuilder::new();
    let mut to_statuses = StringBuilder::new();
    let mut reasons = StringBuilder::new();
    let mut messages = StringBuilder::new();
    for (issue_id, changes) in issues {
        for change in changes {
            issue_ids.append_value(issue_id);
            event_ids.append_value(&change.id);
            timestamps.append_value(change.timestamp_ms);
            authors.append_value(&change.author);
            events.append_value(change.event.as_str());
            from_statuses.append_option(change.from_status.map(|s| s.as_str()));
            to_statuses.append_option(change.to_status.map(|s| s.as_str()));
            reasons.append_option(change.reason.map(|r| r.as_str()));
            messages.append_option(change.message.as_deref());
        }
    }
    Ok(RecordBatch::try_new(
        issue_event_schema(),
        vec![
            Arc::new(issue_ids.finish()),
            Arc::new(event_ids.finish()),
            Arc::new(timestamps.finish().with_timezone("UTC")),
            Arc::new(authors.finish()),
            Arc::new(events.finish()),
            Arc::new(from_statuses.finish()),
            Arc::new(to_statuses.finish()),
            Arc::new(reasons.finish()),
            Arc::new(messages.finish()),
        ],
    )?)
}

#[derive(Debug)]
struct IssueEventSource;

impl BatchSource for IssueEventSource {
    fn schema(&self) -> SchemaRef {
        issue_event_schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let issues = live_issues(store)?;
        issue_event_rows(
            issues
                .iter()
                .map(|issue| (issue.id.as_str(), issue.changes.as_slice())),
        )
    }
}

/// The store-bound `issue` table.
pub fn issue_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(IssueSource)))
}

/// The store-bound `issue_event` table.
pub fn issue_event_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(IssueEventSource)))
}
