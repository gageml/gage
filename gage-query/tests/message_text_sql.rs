#![allow(clippy::indexing_slicing)]

mod common;

use common::{col_strings, test_ctx};
use datafusion::arrow::array::{Float32Array, Int64Array};

const RICH: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

/// The motivating shape: `message_text('term')` returns ranked hits
/// with snippets, no join required.
#[tokio::test]
async fn term_search_returns_scored_snippets() {
    let ctx = test_ctx().await;
    let batches = ctx
        .sql("SELECT session_id, line, score, snippet FROM message_text('main')")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert!(!batches.is_empty());
    let batch = &batches[0];
    assert!(batch.num_rows() >= 1);
    let scores = batch
        .column(2)
        .as_any()
        .downcast_ref::<Float32Array>()
        .unwrap();
    assert!((0..scores.len()).all(|i| scores.value(i) > 0.0));
    let snippets = col_strings(batch, 3);
    assert!(snippets.iter().any(|s| s.contains('«')));
    let ids = col_strings(batch, 0);
    assert!(ids.iter().any(|id| id == RICH));
}

/// Bare terms are AND-conjoined.
#[tokio::test]
async fn and_is_default_for_bare_terms() {
    let ctx = test_ctx().await;
    let and_hits = ctx
        .sql("SELECT count(*) AS n FROM message_text('read main')")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let or_hits = ctx
        .sql("SELECT count(*) AS n FROM message_text('read OR main')")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let and_n = and_hits[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(0);
    let or_n = or_hits[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(0);
    assert!(
        or_n >= and_n,
        "OR ({or_n}) should produce at least as many hits as AND ({and_n})",
    );
}

/// LIMIT pushes down to TopDocs.
#[tokio::test]
async fn limit_pushdown() {
    let ctx = test_ctx().await;
    let batches = ctx
        .sql("SELECT line FROM message_text('the') LIMIT 1")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total, 1);
}

/// Joining with `message` gives back the full row.
#[tokio::test]
async fn join_to_message_for_full_row() {
    let ctx = test_ctx().await;
    let batches = ctx
        .sql(
            "SELECT s.score, m.type, m.text \
             FROM message_text('hello') s \
             JOIN message m USING (session_id, line) \
             ORDER BY s.score DESC LIMIT 10",
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert!(total >= 1, "hello should match at least once");
}

/// Bad query strings surface as DataFusion errors, not empty results.
#[tokio::test]
async fn invalid_query_is_an_error() {
    let ctx = test_ctx().await;
    let result = ctx
        .sql("SELECT line FROM message_text('\"unterminated')")
        .await
        .unwrap()
        .collect()
        .await;
    assert!(result.is_err());
}

/// No hits returns zero rows, not an error.
#[tokio::test]
async fn no_match() {
    let ctx = test_ctx().await;
    let batches = ctx
        .sql("SELECT count(*) AS n FROM message_text('qwertyuiopasdfx')")
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
