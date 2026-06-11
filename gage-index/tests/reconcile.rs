#![allow(clippy::indexing_slicing)]

use std::path::{Path, PathBuf};

use gage_index::{IndexStore, LockMode, derive_session, text_search_mask};

/// The gage-query session fixtures, shared across crates.
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("gage-query")
        .join("testdata")
}

/// Copy the fixtures into a temp root so tests can mutate the corpus.
fn copy_fixtures(dst: &Path) {
    for project in std::fs::read_dir(fixture_root()).unwrap().flatten() {
        if !project.path().is_dir() {
            continue;
        }
        let dst_project = dst.join(project.file_name());
        std::fs::create_dir_all(&dst_project).unwrap();
        for session in std::fs::read_dir(project.path()).unwrap().flatten() {
            std::fs::copy(session.path(), dst_project.join(session.file_name())).unwrap();
        }
    }
}

fn store_with_tempdirs() -> (IndexStore, tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    copy_fixtures(root.path());
    let store = IndexStore::new(root.path(), cache.path());
    (store, root, cache)
}

const RICH_SESSION: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

#[test]
fn reconcile_lifecycle() {
    let (store, root, _cache) = store_with_tempdirs();

    // First pass derives the full corpus.
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert!(!outcome.skipped);
    assert_eq!(outcome.discovered, 4);
    assert_eq!(outcome.derived, 4);
    assert_eq!(store.session_files().len(), 4);

    // Aggregates were consolidated.
    let aggregates = store.load_aggregates().unwrap();
    assert_eq!(aggregates.len(), 4);
    let rich = &aggregates[RICH_SESSION];
    assert_eq!(rich.title.as_deref(), Some("Read and explain main.rs"));
    assert_eq!(rich.model.as_deref(), Some("claude-sonnet-4-20250514"));
    assert_eq!(rich.message_count, 6);
    assert_eq!(rich.input_tokens, 500);

    // The index serves coordinate search.
    let coords = store.search("main").unwrap();
    assert!(coords.iter().any(|(id, _)| id == RICH_SESSION));

    // Steady state: nothing dirty.
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.derived, 0);
    assert_eq!(outcome.removed, 0);

    let status = store.status();
    assert_eq!(status.discovered, 4);
    assert_eq!(status.cached, 4);
    assert_eq!(status.indexed, 4);
    assert_eq!(status.dirty, 0);
    assert!(status.last_reconcile_ms.is_some());

    // Append a line: the session re-derives and the index sees it.
    let session_path = root
        .path()
        .join("-home-test-project")
        .join(format!("{RICH_SESSION}.jsonl"));
    let mut contents = std::fs::read_to_string(&session_path).unwrap();
    contents.push_str(
        r#"{"type":"user","uuid":"u9999","timestamp":"2025-03-20T11:00:00.000Z","message":{"content":"zanzibar flotsam"}}"#,
    );
    contents.push('\n');
    std::fs::write(&session_path, contents).unwrap();

    assert!(store.status().dirty >= 1);
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.derived, 1);
    let coords = store.search("zanzibar").unwrap();
    assert_eq!(coords.len(), 1);
    assert_eq!(coords[0].0, RICH_SESSION);

    // Remove a session: artifacts are garbage-collected.
    std::fs::remove_file(&session_path).unwrap();
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.removed, 1);
    assert_eq!(store.session_files().len(), 3);
    assert!(store.search("zanzibar").unwrap().is_empty());
    assert!(!store.load_aggregates().unwrap().contains_key(RICH_SESSION));
}

#[test]
fn rebuild_resets_artifacts() {
    let (store, _root, _cache) = store_with_tempdirs();
    store.reconcile(LockMode::Wait).unwrap();
    let outcome = store.rebuild().unwrap();
    assert_eq!(outcome.derived, 4);
    assert_eq!(store.session_files().len(), 4);
    assert!(store.search("main").unwrap().iter().any(|(id, _)| id == RICH_SESSION));
}

#[test]
fn search_on_empty_cache_is_empty() {
    let cache = tempfile::tempdir().unwrap();
    let store = IndexStore::new(fixture_root(), cache.path());
    assert!(store.search("anything").unwrap().is_empty());
}

/// Differential test for tokenizer integrity: the persistent index
/// (accelerated path) and the per-batch transient index (row-wise
/// semantics) must produce identical match sets over real session
/// text. The result set under acceleration is the intersection of the
/// two, so divergence silently drops rows — this test catches what
/// version discipline cannot see, e.g. a Tantivy upgrade that changes
/// tokenization.
#[test]
fn accelerated_and_rowwise_match_sets_agree() {
    let (store, _root, _cache) = store_with_tempdirs();
    store.reconcile(LockMode::Wait).unwrap();

    // Row corpus: every message row, via the same derivation the
    // store uses.
    let mut rows: Vec<(String, i64, String)> = Vec::new();
    for project in std::fs::read_dir(fixture_root()).unwrap().flatten() {
        if !project.path().is_dir() {
            continue;
        }
        for session in std::fs::read_dir(project.path()).unwrap().flatten() {
            let name = session.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let derived = derive_session(id, &session.path()).unwrap();
            let batch = derived.batch;
            let lines = batch
                .column(1)
                .as_any()
                .downcast_ref::<arrow::array::Int64Array>()
                .unwrap();
            let texts = batch
                .column(7)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .unwrap();
            for i in 0..batch.num_rows() {
                if arrow::array::Array::is_valid(texts, i) {
                    rows.push((id.to_string(), lines.value(i), texts.value(i).to_string()));
                }
            }
        }
    }
    assert!(!rows.is_empty());

    let battery = [
        "main",
        "read AND main",
        "read OR hello",
        "\"src main\"",
        "main AND NOT hello",
        "rs",
        "tool",
        "nonexistentterm",
    ];

    for query in battery {
        let mut accelerated: Vec<(String, i64)> = store.search(query).unwrap();
        accelerated.sort();

        let mask =
            text_search_mask(rows.iter().map(|(_, _, t)| Some(t.as_str())), query).unwrap();
        let mut rowwise: Vec<(String, i64)> = rows
            .iter()
            .zip(&mask)
            .filter(|(_, m)| **m == Some(true))
            .map(|((id, line, _), _)| (id.clone(), *line))
            .collect();
        rowwise.sort();

        assert_eq!(accelerated, rowwise, "match sets diverge for {query:?}");
    }
}
