#![allow(clippy::indexing_slicing)]

mod common;

use common::{col_strings, test_ctx};
use datafusion::arrow::array::{BooleanArray, Int64Array};

const RICH: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

/// The motivating query shape: an accelerated term search over
/// `message.text`.
#[tokio::test]
async fn term_search_finds_messages() {
    let ctx = test_ctx().await;
    let batches = ctx
        .sql("SELECT DISTINCT session_id FROM message WHERE text_search(text, 'main')")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let ids = col_strings(&batches[0], 0);
    assert_eq!(ids, vec![RICH]);
}

#[tokio::test]
async fn boolean_and_phrase_queries() {
    let ctx = test_ctx().await;

    // AND across terms appearing in the same message
    let batches = ctx
        .sql("SELECT line FROM message WHERE text_search(text, 'read AND main') ORDER BY line")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert!(
        total >= 2,
        "user prompt and thinking both match, got {total}"
    );

    // Phrase query: tokenizer splits src/main.rs into src, main, rs
    let batches = ctx
        .sql("SELECT count(*) AS n FROM message WHERE text_search(text, '\"src main rs\"')")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let n = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(0);
    assert!(n >= 1);

    // No match
    let batches = ctx
        .sql("SELECT count(*) AS n FROM message WHERE text_search(text, 'qwertyuiopasdf')")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let n = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(0);
    assert_eq!(n, 0);
}

/// The predicate has real row-wise semantics, so it composes in
/// shapes the index cannot accelerate: OR with other predicates and
/// select-list use.
#[tokio::test]
async fn rowwise_composition() {
    let ctx = test_ctx().await;

    let batches = ctx
        .sql(
            "SELECT DISTINCT session_id FROM message \
             WHERE text_search(text, 'main') OR type = 'user' ORDER BY session_id",
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let ids = col_strings(&batches[0], 0);
    // Every session has a user message except none — all four appear
    assert_eq!(ids.len(), 4);

    let batches = ctx
        .sql(
            "SELECT text_search(text, 'hello') AS hit FROM message \
             WHERE session_id = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee' ORDER BY line",
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let hits: Vec<Option<bool>> = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .unwrap()
        .iter()
        .collect();
    assert_eq!(hits, vec![Some(true), Some(false)]);
}

/// Unaccelerated columns evaluate through the row-wise path.
#[tokio::test]
async fn search_over_entry_raw() {
    let ctx = test_ctx().await;
    let batches = ctx
        .sql(
            "SELECT DISTINCT session_id FROM entry \
             WHERE text_search(raw, 'aiTitle')",
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let ids = col_strings(&batches[0], 0);
    assert_eq!(ids, vec![RICH]);
}

/// Acceleration composes with exact session pruning.
#[tokio::test]
async fn search_with_session_filter() {
    let ctx = test_ctx().await;
    let batches = ctx
        .sql(&format!(
            "SELECT line FROM message \
             WHERE session_id = '{RICH}' AND text_search(text, 'hello') ORDER BY line"
        ))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    // tool_result (line 5) and final text (line 7) both contain hello
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert!(total >= 1);

    let batches = ctx
        .sql(
            "SELECT line FROM message \
             WHERE session_id = 'nonexistent' AND text_search(text, 'hello')",
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 0);
}

#[tokio::test]
async fn invalid_query_is_an_error() {
    let ctx = test_ctx().await;
    let result = ctx
        .sql("SELECT line FROM message WHERE text_search(text, '\"unterminated')")
        .await
        .unwrap()
        .collect()
        .await;
    assert!(result.is_err());
}
