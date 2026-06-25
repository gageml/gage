#![allow(clippy::indexing_slicing)]

#[allow(dead_code)]
mod common;

use datafusion::arrow::array::{Int64Array, StringArray};
use gage_db::note::{self, Note, NoteValue};
use gage_db::target::{NoteTarget, SessionTarget};

const SESSION_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

/// Regression: `note_message_context` declares its schema via
/// `message_schema()` (which renames the derived `message_subtype`
/// column to `subtype`) but used to return the raw derived batch
/// after `project(MESSAGE_PROJECTION)`, whose schema still carried
/// `message_subtype`. DataFusion rejected the mismatched batch with
/// "Mismatch between schema and batches". This test exercises the
/// rename path by selecting the `subtype` column.
#[tokio::test]
#[serial_test::serial(gage_home)]
async fn returns_window_with_renamed_subtype_column() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: this test binary is the sole reader of GAGE_HOME, set
    // before any access.
    unsafe {
        std::env::set_var("GAGE_HOME", tmp.path());
    }

    let conn = gage_db::db::open_db().unwrap();
    let note = Note::new(
        NoteTarget::Session(SessionTarget::new(SESSION_B).with_line(3)),
        "anchor",
        NoteValue::from(serde_json::json!({})),
        "user:test",
    );
    let note_id = note.id.clone();
    note::insert(&conn, &note).unwrap();
    drop(conn);

    let ctx = gage_query::create_context(&common::testdata(), &tmp.path().join("cache")).await;
    let sql = format!(
        "SELECT line, type, subtype FROM note_message_context('{note_id}', 1, 1) \
         ORDER BY line"
    );
    let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();

    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert!(total >= 1, "expected at least the anchor row");

    let batch = &batches[0];
    let lines = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let values: Vec<i64> = (0..lines.len()).map(|i| lines.value(i)).collect();
    assert!(values.contains(&3), "anchor line 3 missing from {values:?}");
    // `subtype` resolves: schema rename succeeded.
    assert!(
        batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .is_some()
    );
}

/// Whole-session notes (no `line`) produce no rows, per the TVF's
/// documented contract.
#[tokio::test]
#[serial_test::serial(gage_home)]
async fn whole_session_note_yields_empty() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: see above.
    unsafe {
        std::env::set_var("GAGE_HOME", tmp.path());
    }

    let conn = gage_db::db::open_db().unwrap();
    let note = Note::new(
        NoteTarget::Session(SessionTarget::new(SESSION_B)),
        "whole",
        NoteValue::from(serde_json::json!({})),
        "user:test",
    );
    let note_id = note.id.clone();
    note::insert(&conn, &note).unwrap();
    drop(conn);

    let ctx = gage_query::create_context(&common::testdata(), &tmp.path().join("cache")).await;
    let sql = format!("SELECT line FROM note_message_context('{note_id}', 2, 2)");
    let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 0);
}
