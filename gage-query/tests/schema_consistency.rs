//! Drift guard: scan every registered table and TVF end-to-end and
//! confirm the streamed batches match the declared schema. DataFusion's
//! scan plumbing raises "Mismatch between schema and batches" when a
//! provider returns a batch whose schema disagrees with what it
//! advertised via `TableProvider::schema`. A bare `SELECT * FROM <t>`
//! drives the full scan and surfaces that drift without needing a
//! per-table assertion.
//!
//! Whenever a new table or TVF is added, register it below.

#![allow(clippy::indexing_slicing)]

#[allow(dead_code)]
mod common;

use gage_db::note::{self, Note, NoteValue};
use gage_db::target::{NoteTarget, SessionTarget};

const SESSION_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

/// Every plain table registered on the gage context. `note` is empty
/// in the fresh GAGE_HOME but the scan still validates the declared
/// schema against an empty stream.
const TABLES: &[&str] = &[
    "config",
    "entry",
    "issue",
    "issue_evidence",
    "message",
    "note",
    "note_doc",
    "session",
    "session_issue",
    "session_note",
];

/// Every TVF registered on the gage context, with an arg list known to
/// resolve against the seeded fixtures.
fn tvf_calls(note_id: &str) -> Vec<String> {
    vec![
        "message_text('main.rs', 200)".to_string(),
        format!("note_message_context('{note_id}', 1, 1)"),
        // Unknown id returns zero rows, so the smoke test doesn't need a
        // seeded issue.
        "issue_report('no-such-issue')".to_string(),
    ]
}

#[tokio::test]
#[serial_test::serial(gage_home)]
async fn every_table_and_tvf_scans_without_schema_drift() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this test binary is the sole reader of GAGE_HOME, set
    // before any access.
    unsafe {
        std::env::set_var("GAGE_HOME", tmp.path());
    }

    // Seed a line-anchored note so `note_message_context` resolves.
    let conn = gage_db::db::open_db().unwrap();
    let note = Note::new(
        NoteTarget::Session(SessionTarget::new(SESSION_B).with_line(1)),
        "drift.anchor",
        NoteValue::from(serde_json::json!({})),
        "user:test",
    );
    let note_id = note.id.clone();
    note::insert(&conn, &note).unwrap();
    drop(conn);

    let ctx = gage_query::create_context(&common::testdata(), &tmp.path().join("cache")).await;

    for table in TABLES {
        let sql = format!("SELECT * FROM {table}");
        ctx.sql(&sql)
            .await
            .unwrap_or_else(|e| panic!("plan {table}: {e}"))
            .collect()
            .await
            .unwrap_or_else(|e| panic!("scan {table}: {e}"));
    }

    for call in tvf_calls(&note_id) {
        let sql = format!("SELECT * FROM {call}");
        ctx.sql(&sql)
            .await
            .unwrap_or_else(|e| panic!("plan {call}: {e}"))
            .collect()
            .await
            .unwrap_or_else(|e| panic!("scan {call}: {e}"));
    }
}

/// Pin the TABLES/tvf_calls coverage to the registered surface so a
/// new table or TVF added to `create_context` without a smoke-test
/// entry fails this check.
#[tokio::test]
#[serial_test::serial(gage_home)]
async fn coverage_matches_registered_surface() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: see above.
    unsafe {
        std::env::set_var("GAGE_HOME", tmp.path());
    }
    // create_context opens the gage db through a connection pool that
    // does not create the file; ensure it exists first.
    drop(gage_db::db::open_db().unwrap());
    let ctx = gage_query::create_context(&common::testdata(), &tmp.path().join("cache")).await;

    let registered_tables: std::collections::BTreeSet<String> = ctx
        .catalog("datafusion")
        .unwrap()
        .schema("public")
        .unwrap()
        .table_names()
        .into_iter()
        .collect();
    let covered: std::collections::BTreeSet<String> =
        TABLES.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        registered_tables, covered,
        "TABLES in this test is out of sync with the registered tables. \
         Add new entries to TABLES (and remove any that were dropped)."
    );

    let registered_tvfs: std::collections::BTreeSet<String> = gage_query::tables::registered_tvfs()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let covered_tvfs: std::collections::BTreeSet<String> = tvf_calls("anything")
        .iter()
        .map(|s| s.split('(').next().unwrap().to_string())
        .collect();
    assert_eq!(
        registered_tvfs, covered_tvfs,
        "tvf_calls in this test is out of sync with `registered_tvfs()`. \
         Add a fixture-backed call expression for every new TVF."
    );
}
