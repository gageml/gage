#![allow(clippy::indexing_slicing)]

use datafusion::arrow::array::{Array, Int64Array, StringArray};
use gage_db::note::{self, Note, NoteValue};
use gage_db::target::{NoteTarget, SessionTarget};

/// The note table reflects `value` as JSON text, so the registered JSON
/// functions can reach into a structured value. Point `GAGE_HOME` at a
/// tempdir, write a note whose value is an object, then query a nested
/// field through DataFusion.
#[tokio::test]
#[serial_test::serial(gage_home)]
async fn value_object_is_queryable_through_json_functions() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: the note provider resolves its DB via `GAGE_HOME`; this
    // test binary is the sole process reading it, set before any access.
    unsafe {
        std::env::set_var("GAGE_HOME", tmp.path());
    }

    let conn = gage_db::db::open_db().unwrap();
    let note = Note::new(
        NoteTarget::Session(SessionTarget::new("550e8400-e29b-41d4-a716-446655440000")),
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

/// Regression: the `note.metadata` column is declared `TEXT` but is
/// `NULL` for most rows. The upstream sqlite table provider infers
/// schemas by sampling the first row's runtime cell type, which yields
/// `DataType::Null` for an all-NULL sampled column. A later batch with
/// a real text value then carries `Utf8` and panics in the arrow
/// coalescer with `expected Null but found Utf8`.
///
/// Our fork of `datafusion-table-providers` reads the declared types
/// via `PRAGMA table_info` instead. This test pins that behavior: if
/// the patch is dropped or the fork is replaced with a version that
/// reverts to row sampling, `metadata` here would resolve to `Null`
/// and the assertion fails.
#[tokio::test]
#[serial_test::serial(gage_home)]
async fn note_metadata_resolves_to_utf8_not_null() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: see `value_object_is_queryable_through_json_functions`.
    unsafe {
        std::env::set_var("GAGE_HOME", tmp.path());
    }

    let conn = gage_db::db::open_db().unwrap();
    let note = Note::new(
        NoteTarget::Session(SessionTarget::new("550e8400-e29b-41d4-a716-446655440001")),
        "any.name",
        NoteValue::from(serde_json::json!({"x": 1})),
        "user:test",
    );
    note::insert(&conn, &note).unwrap();
    drop(conn);

    let ctx = gage_query::create_context(tmp.path(), &tmp.path().join("cache")).await;
    let batches = ctx
        .sql(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 'note' AND column_name = 'metadata'",
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(batches.len(), 1);
    let types = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(
        types.value(0),
        "Utf8",
        "note.metadata must resolve to Utf8; got `{}`. The \
         `datafusion-table-providers` fork has likely reverted to \
         row-sampling schema inference",
        types.value(0)
    );

    // The original panic shape: select `metadata` from a row whose
    // metadata is NULL. With the buggy schema this stream coalesces a
    // `Null` array against the schema-declared `Utf8` and panics.
    let rows = ctx
        .sql("SELECT id, metadata FROM note ORDER BY created DESC LIMIT 5")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let total: usize = rows.iter().map(|b| b.num_rows()).sum();
    assert!(total >= 1, "expected at least one note row");
}
