//! The `_link` tables: one per link file, one row per SHA the file
//! lists. A row carries the owning object's id and commit and the
//! linked object's id and commit, so a relation between versions is
//! explicit. Views over these tables resolve versions for people.
//!
//! | Table                  | Link file                                  |
//! | ---------------------- | ------------------------------------------ |
//! | `dataset_session_link` | `dataset/sessions.link`                    |
//! | `scan_dataset_link`    | `scan/dataset.link`                        |
//! | `scan_note_link`       | `scan/notes.link`, `scan/notes_carried.link` |
//! | `note_target_link`     | `note/target.link`                         |
//!
//! `dataset_session_link` lists every dataset at its tip and at every
//! commit a scan links, so a scan's members are found by joining on
//! `dataset_commit`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{BooleanBuilder, Int64Builder, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::Result;

use super::batch::{BatchSource, BatchTable, external};
use crate::object::Object;
use crate::{DatasetStore, NoteStore, ScanStore, Store, url};

const SESSIONS_LINK: &str = "sessions.link";
const DATASET_LINK: &str = "dataset.link";
const NOTES_LINK: &str = "notes.link";
const NOTES_CARRIED_LINK: &str = "notes_carried.link";
const TARGET_LINK: &str = "target.link";

/// Which link file a [`LinkTable`] serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    DatasetSession,
    ScanDataset,
    ScanNote,
    NoteTarget,
}

impl LinkKind {
    pub const ALL: [LinkKind; 4] = [
        LinkKind::DatasetSession,
        LinkKind::ScanDataset,
        LinkKind::ScanNote,
        LinkKind::NoteTarget,
    ];

    /// The table name.
    pub fn table_name(self) -> &'static str {
        match self {
            LinkKind::DatasetSession => "dataset_session_link",
            LinkKind::ScanDataset => "scan_dataset_link",
            LinkKind::ScanNote => "scan_note_link",
            LinkKind::NoteTarget => "note_target_link",
        }
    }

    fn schema(self) -> SchemaRef {
        let utf8 = |name: &str, nullable: bool| Field::new(name, DataType::Utf8, nullable);
        Arc::new(Schema::new(match self {
            LinkKind::DatasetSession => vec![
                utf8("dataset_id", false),
                utf8("dataset_commit", false),
                Field::new("session_num", DataType::Int64, false),
                utf8("session_id", false),
                utf8("session_commit", false),
            ],
            LinkKind::ScanDataset => vec![
                utf8("scan_id", false),
                utf8("scan_commit", false),
                utf8("dataset_id", false),
                utf8("dataset_commit", false),
            ],
            LinkKind::ScanNote => vec![
                utf8("scan_id", false),
                utf8("scan_commit", false),
                utf8("note_id", false),
                utf8("note_commit", false),
                Field::new("carried", DataType::Boolean, false),
            ],
            LinkKind::NoteTarget => vec![
                utf8("note_id", false),
                utf8("note_commit", false),
                utf8("target_id", false),
                // The linked object's type name, e.g. `session`
                utf8("target_type", false),
                utf8("target_commit", false),
                // The line selection from the target URL, for sessions
                utf8("lines", true),
            ],
        }))
    }
}

/// The store-bound table for one link file.
pub fn link_table(store: Arc<Mutex<Store>>, kind: LinkKind) -> Arc<dyn TableProvider> {
    Arc::new(BatchTable::new(store, Arc::new(Source { kind })))
}

#[derive(Debug)]
struct Source {
    kind: LinkKind,
}

impl BatchSource for Source {
    fn schema(&self) -> SchemaRef {
        self.kind.schema()
    }

    fn build(&self, store: &Store) -> Result<RecordBatch> {
        match self.kind {
            LinkKind::DatasetSession => dataset_session(store),
            LinkKind::ScanDataset => scan_dataset(store),
            LinkKind::ScanNote => scan_note(store),
            LinkKind::NoteTarget => note_target(store),
        }
    }
}

/// Every live scan object.
fn scan_objects(store: &Store) -> Result<Vec<Object>> {
    ScanStore::from(store)
        .query()
        .tips()
        .map_err(external)?
        .iter()
        .map(|tip| store.read_object(&tip.sha).map_err(external))
        .collect()
}

fn linked(object: &Object, file: &str) -> Vec<String> {
    object.tree.links.get(file).cloned().unwrap_or_default()
}

fn dataset_session(store: &Store) -> Result<RecordBatch> {
    // The dataset commits with members to list: every tip, and every
    // commit a scan links
    let mut commits: Vec<String> = DatasetStore::from(store)
        .query()
        .tips()
        .map_err(external)?
        .into_iter()
        .map(|tip| tip.sha)
        .collect();
    let mut seen: BTreeSet<String> = commits.iter().cloned().collect();
    for scan in scan_objects(store)? {
        for sha in linked(&scan, DATASET_LINK) {
            if seen.insert(sha.clone()) {
                commits.push(sha);
            }
        }
    }
    let mut dataset_ids = StringBuilder::new();
    let mut dataset_commits = StringBuilder::new();
    let mut session_nums = Int64Builder::new();
    let mut session_ids = StringBuilder::new();
    let mut session_commits = StringBuilder::new();
    for commit in &commits {
        let dataset = store.read_object(commit).map_err(external)?;
        for (idx, sha) in linked(&dataset, SESSIONS_LINK).iter().enumerate() {
            let session = store.read_header(sha).map_err(external)?;
            dataset_ids.append_value(&dataset.header.id);
            dataset_commits.append_value(commit);
            session_nums.append_value(idx as i64 + 1);
            session_ids.append_value(session.id);
            session_commits.append_value(sha);
        }
    }
    Ok(RecordBatch::try_new(
        LinkKind::DatasetSession.schema(),
        vec![
            Arc::new(dataset_ids.finish()),
            Arc::new(dataset_commits.finish()),
            Arc::new(session_nums.finish()),
            Arc::new(session_ids.finish()),
            Arc::new(session_commits.finish()),
        ],
    )?)
}

fn scan_dataset(store: &Store) -> Result<RecordBatch> {
    let mut scan_ids = StringBuilder::new();
    let mut scan_commits = StringBuilder::new();
    let mut dataset_ids = StringBuilder::new();
    let mut dataset_commits = StringBuilder::new();
    for scan in scan_objects(store)? {
        for sha in linked(&scan, DATASET_LINK) {
            let dataset = store.read_header(&sha).map_err(external)?;
            scan_ids.append_value(&scan.header.id);
            scan_commits.append_value(&scan.commit_sha);
            dataset_ids.append_value(dataset.id);
            dataset_commits.append_value(&sha);
        }
    }
    Ok(RecordBatch::try_new(
        LinkKind::ScanDataset.schema(),
        vec![
            Arc::new(scan_ids.finish()),
            Arc::new(scan_commits.finish()),
            Arc::new(dataset_ids.finish()),
            Arc::new(dataset_commits.finish()),
        ],
    )?)
}

fn scan_note(store: &Store) -> Result<RecordBatch> {
    let mut scan_ids = StringBuilder::new();
    let mut scan_commits = StringBuilder::new();
    let mut note_ids = StringBuilder::new();
    let mut note_commits = StringBuilder::new();
    let mut carrieds = BooleanBuilder::new();
    for scan in scan_objects(store)? {
        for (file, carried) in [(NOTES_LINK, false), (NOTES_CARRIED_LINK, true)] {
            for sha in linked(&scan, file) {
                let note = store.read_header(&sha).map_err(external)?;
                scan_ids.append_value(&scan.header.id);
                scan_commits.append_value(&scan.commit_sha);
                note_ids.append_value(note.id);
                note_commits.append_value(&sha);
                carrieds.append_value(carried);
            }
        }
    }
    Ok(RecordBatch::try_new(
        LinkKind::ScanNote.schema(),
        vec![
            Arc::new(scan_ids.finish()),
            Arc::new(scan_commits.finish()),
            Arc::new(note_ids.finish()),
            Arc::new(note_commits.finish()),
            Arc::new(carrieds.finish()),
        ],
    )?)
}

fn note_target(store: &Store) -> Result<RecordBatch> {
    let mut note_ids = StringBuilder::new();
    let mut note_commits = StringBuilder::new();
    let mut target_ids = StringBuilder::new();
    let mut target_types = StringBuilder::new();
    let mut target_commits = StringBuilder::new();
    let mut lines = StringBuilder::new();
    let notes = NoteStore::from(store);
    for tip in notes.query().tips().map_err(external)? {
        let note = store.read_object(&tip.sha).map_err(external)?;
        let fragment = note
            .tree
            .attrs
            .as_ref()
            .and_then(|a| a.get("target"))
            .and_then(|t| t.as_str())
            .and_then(|t| url::parse(t).ok())
            .and_then(|u| u.fragment.map(String::from));
        for sha in linked(&note, TARGET_LINK) {
            let target = store.read_header(&sha).map_err(external)?;
            note_ids.append_value(&note.header.id);
            note_commits.append_value(&note.commit_sha);
            target_ids.append_value(target.id);
            target_types.append_value(
                target
                    .object_type
                    .strip_prefix("gage::")
                    .unwrap_or(&target.object_type),
            );
            target_commits.append_value(&sha);
            lines.append_option(fragment.as_deref());
        }
    }
    Ok(RecordBatch::try_new(
        LinkKind::NoteTarget.schema(),
        vec![
            Arc::new(note_ids.finish()),
            Arc::new(note_commits.finish()),
            Arc::new(target_ids.finish()),
            Arc::new(target_types.finish()),
            Arc::new(target_commits.finish()),
            Arc::new(lines.finish()),
        ],
    )?)
}
