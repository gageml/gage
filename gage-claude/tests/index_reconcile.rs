#![allow(clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gage_claude::index::{IndexStore, LockMode};

/// The claude session fixtures
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
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

fn rich_session_path(root: &Path) -> PathBuf {
    root.join("-home-test-project")
        .join(format!("{RICH_SESSION}.jsonl"))
}

/// The cached summary for the rich session, validated against its
/// current JSONL mtime.
fn rich_summary(store: &IndexStore, root: &Path) -> Option<gage_claude::index::SessionSummary> {
    let mtime = std::fs::metadata(rich_session_path(root))
        .map(|m| m.modified().unwrap())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    store.session_summary(RICH_SESSION, mtime)
}

#[test]
fn reconcile_lifecycle() {
    let (store, root, _cache) = store_with_tempdirs();

    // First pass caches the full corpus
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert!(!outcome.skipped);
    assert_eq!(outcome.discovered, 4);
    assert_eq!(outcome.indexed, 4);
    let summary = rich_summary(&store, root.path()).expect("summary cached after reconcile");
    let message_count = summary.message_count;

    // Steady state: nothing dirty
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.indexed, 0);
    assert_eq!(outcome.removed, 0);

    let status = store.status();
    assert_eq!(status.discovered, 4);
    assert_eq!(status.indexed, 4);
    assert_eq!(status.dirty, 0);
    assert!(status.last_reconcile_ms.is_some());

    // Append a message: the session re-derives
    let session_path = rich_session_path(root.path());
    let mut contents = std::fs::read_to_string(&session_path).unwrap();
    contents.push_str(
        r#"{"type":"user","uuid":"u9999","timestamp":"2025-03-20T11:00:00.000Z","message":{"content":"zanzibar flotsam"}}"#,
    );
    contents.push('\n');
    std::fs::write(&session_path, contents).unwrap();

    assert!(store.status().dirty >= 1);
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.indexed, 1);
    let summary = rich_summary(&store, root.path()).expect("summary refreshed after append");
    assert_eq!(summary.message_count, message_count + 1);

    // Remove a session: its cache file is garbage-collected
    std::fs::remove_file(&session_path).unwrap();
    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.removed, 1);
    assert!(rich_summary(&store, root.path()).is_none());
    assert_eq!(store.status().indexed, 3);
}

#[test]
fn rebuild_resets_artifacts() {
    let (store, root, _cache) = store_with_tempdirs();
    store.reconcile(LockMode::Wait).unwrap();
    let outcome = store.rebuild().unwrap();
    assert_eq!(outcome.indexed, 4);
    assert!(rich_summary(&store, root.path()).is_some());
}

#[test]
fn status_on_empty_cache_has_no_reconcile() {
    let cache = tempfile::tempdir().unwrap();
    let store = IndexStore::new(fixture_root(), cache.path());
    let status = store.status();
    assert_eq!(status.indexed, 0);
    assert_eq!(status.discovered, 4);
    assert!(status.last_reconcile_ms.is_none());
}

#[test]
fn reconcile_removes_legacy_text_index() {
    let (store, _root, cache) = store_with_tempdirs();
    let legacy = cache.path().join("text").join("v3");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("meta.json"), "{}").unwrap();
    store.reconcile(LockMode::Wait).unwrap();
    assert!(!cache.path().join("text").exists());
}
