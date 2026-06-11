#![allow(clippy::indexing_slicing)]

use std::path::{Path, PathBuf};

use gage_index::{IndexStore, LockMode};

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

    // First pass indexes the full corpus
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert!(!outcome.skipped);
    assert_eq!(outcome.discovered, 4);
    assert_eq!(outcome.indexed, 4);

    // The index serves search with scores and snippets
    let hits = store
        .search("main", 50, gage_index::DEFAULT_SNIPPET_CHARS)
        .unwrap();
    assert!(hits.iter().any(|h| h.session_id == RICH_SESSION));
    assert!(hits.iter().all(|h| h.score > 0.0));
    assert!(hits.iter().any(|h| h.snippet.contains("«")));

    // Steady state: nothing dirty
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.indexed, 0);
    assert_eq!(outcome.removed, 0);

    let status = store.status();
    assert_eq!(status.discovered, 4);
    assert_eq!(status.indexed, 4);
    assert_eq!(status.dirty, 0);
    assert!(status.last_reconcile_ms.is_some());

    // Append a line: the session re-indexes
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
    assert_eq!(outcome.indexed, 1);
    let hits = store
        .search("zanzibar", 10, gage_index::DEFAULT_SNIPPET_CHARS)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, RICH_SESSION);

    // Remove a session: index entries are garbage-collected
    std::fs::remove_file(&session_path).unwrap();
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.removed, 1);
    assert!(
        store
            .search("zanzibar", 10, gage_index::DEFAULT_SNIPPET_CHARS)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn rebuild_resets_artifacts() {
    let (store, _root, _cache) = store_with_tempdirs();
    store.reconcile(LockMode::Wait).unwrap();
    let outcome = store.rebuild().unwrap();
    assert_eq!(outcome.indexed, 4);
    assert!(
        store
            .search("main", 50, gage_index::DEFAULT_SNIPPET_CHARS)
            .unwrap()
            .iter()
            .any(|h| h.session_id == RICH_SESSION)
    );
}

#[test]
fn search_on_empty_cache_is_empty() {
    let cache = tempfile::tempdir().unwrap();
    let store = IndexStore::new(fixture_root(), cache.path());
    assert!(
        store
            .search("anything", 10, gage_index::DEFAULT_SNIPPET_CHARS)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn snippet_highlights_use_guillemets() {
    let (store, _root, _cache) = store_with_tempdirs();
    store.reconcile(LockMode::Wait).unwrap();
    let hits = store
        .search("main", 10, gage_index::DEFAULT_SNIPPET_CHARS)
        .unwrap();
    let snippet = hits
        .iter()
        .find(|h| h.snippet.contains('«'))
        .expect("at least one hit has a highlight");
    let s = &snippet.snippet;
    assert!(
        s.contains("«main»") || s.contains("«Main»"),
        "snippet should wrap `main` in guillemets: {s:?}",
    );
}

#[test]
fn limit_is_honored() {
    let (store, _root, _cache) = store_with_tempdirs();
    store.reconcile(LockMode::Wait).unwrap();
    let hits = store
        .search("the", 2, gage_index::DEFAULT_SNIPPET_CHARS)
        .unwrap();
    assert!(hits.len() <= 2);
}
