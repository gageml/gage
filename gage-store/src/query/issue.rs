//! The `issue` and `issue_event` tables: one row per live issue
//! object, and one row per change entry under every live issue's
//! `changes/`. The markers come from the index; everything else comes
//! from the object, read for every row.

use std::sync::{Arc, Mutex};

use super::batch::{BatchSource, BatchTable, external, unique_prefix_lens};
use crate::{ISSUE_TYPE, IssueFull, IssueStore, Order, Store};
use datafusion::arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;

fn timestamp() -> DataType {
    DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into()))
}

fn issue_schema() -> SchemaRef {
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
        // System
        // Shortest prefix of `id` unique among every live issue
        Field::new("id_prefix", DataType::Utf8, false),
        // `git:<commit sha>` of the version listed
        Field::new("locator", DataType::Utf8, false),
        Field::new("commit", DataType::Utf8, false),
    ]))
}

fn issue_event_schema() -> SchemaRef {
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
        let len = issues.len();
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
        let mut id_prefixes = StringBuilder::with_capacity(len, len * 4);
        let mut locators = StringBuilder::with_capacity(len, len * 44);
        let mut commits = StringBuilder::with_capacity(len, len * 40);
        for issue in &issues {
            ids.append_value(&issue.id);
            names.append_value(&issue.name);
            titles.append_value(&issue.title);
            descriptions.append_option(issue.description.as_deref());
            statuses.append_value(issue.status.as_str());
            reasons.append_option(issue.status_reason.map(|r| r.as_str()));
            authors.append_value(&issue.author);
            evidence_counts.append_value(issue.evidence.len() as i64);
            createds.append_value(issue.created_ms);
            modifieds.append_value(issue.modified_ms);
            scans.append_option(issue.scan.as_deref());
            let n = prefix_len.get(&issue.id).copied().unwrap_or(issue.id.len());
            id_prefixes.append_value(issue.id.chars().take(n).collect::<String>());
            locators.append_value(format!("git:{}", issue.commit_sha));
            commits.append_value(&issue.commit_sha);
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
                Arc::new(id_prefixes.finish()),
                Arc::new(locators.finish()),
                Arc::new(commits.finish()),
            ],
        )?)
    }
}

#[derive(Debug)]
struct IssueEventSource;

impl BatchSource for IssueEventSource {
    fn schema(&self) -> SchemaRef {
        issue_event_schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        let mut issue_ids = StringBuilder::new();
        let mut event_ids = StringBuilder::new();
        let mut timestamps = TimestampMillisecondBuilder::new();
        let mut authors = StringBuilder::new();
        let mut events = StringBuilder::new();
        let mut from_statuses = StringBuilder::new();
        let mut to_statuses = StringBuilder::new();
        let mut reasons = StringBuilder::new();
        let mut messages = StringBuilder::new();
        for issue in live_issues(store)? {
            for change in &issue.changes {
                issue_ids.append_value(&issue.id);
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
}

/// The store-bound `issue` table.
pub fn issue_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(IssueSource)))
}

/// The store-bound `issue_event` table.
pub fn issue_event_table(store: Arc<Mutex<Store>>) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(IssueEventSource)))
}
