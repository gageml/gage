#![allow(clippy::indexing_slicing)]

use datafusion::arrow::array::{Array, Int64Array, StringArray};
use gage_db::note::{self, Note, NoteValue};
use gage_db::target::{NoteTarget, SessionTarget};

/// The note table reflects `value` as JSON text, so the registered JSON
/// functions can reach into a structured value. Point `GAGE_HOME` at a
/// tempdir, write a note whose value is an object, then query a nested
/// field through DataFusion.
#[tokio::test]
async fn value_object_is_queryable_through_json_functions() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: the note provider resolves its DB via `GAGE_HOME`; this
    // test binary is the sole process reading it, set before any access.
    unsafe {
        std::env::set_var("GAGE_HOME", tmp.path());
    }

    let conn = gage_db::db::open_db().unwrap();
    let note = Note::new(
        NoteTarget::Session(SessionTarget::new("sess-1")),
        "fast-mode.summary",
        NoteValue::from(serde_json::json!({"fast": {"count": 5}})),
        "user:test",
    );
    note::insert(&conn, &note).unwrap();
    drop(conn);

    let ctx = gage_query::create_context(tmp.path(), &tmp.path().join("cache")).await;
    let batches = ctx
        .sql(
            "SELECT name, json_get_int(value, 'fast', 'count') AS count \
             FROM note WHERE name = 'fast-mode.summary'",
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    let names = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let counts = batch
        .column(1)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(names.value(0), "fast-mode.summary");
    assert_eq!(counts.value(0), 5);
}
