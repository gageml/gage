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
fn rich_summary(store: &IndexStore, root: &Path) -> Option<gage_claude::index::ClaudeSessionAttrs> {
    let mtime = std::fs::metadata(rich_session_path(root))
        .map(|m| m.modified().unwrap())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    store.session_attrs(RICH_SESSION, mtime)
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

/// The cache of a corpus is `summary/<slug>` under the cache root, a
/// flat directory of one file per session beside the manifest, which
/// names the projects directory it caches.
#[test]
fn summary_dir_is_the_slug_under_the_summary_category() {
    let (store, root, cache) = store_with_tempdirs();
    store.reconcile(LockMode::Wait).unwrap();
    let dir = store.summary_dir();
    assert_eq!(dir.parent().unwrap(), cache.path().join("summary"));
    assert!(dir.join(RICH_SESSION).is_file());
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["source"],
        std::fs::canonicalize(root.path())
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(manifest["sessions"].as_object().unwrap().len(), 4);
}

/// A directory whose manifest records another format version holds
/// files of unknown shape. Ensuring the directory discards it, and the
/// next reconcile rebuilds it in the current format.
#[test]
fn a_manifest_of_another_format_discards_the_directory() {
    let (store, root, _cache) = store_with_tempdirs();
    let dir = store.summary_dir();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        r#"{"format_version":1,"source":"/elsewhere","last_reconcile_ms":5,"sessions":{}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("stale-file"), b"old format").unwrap();

    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.indexed, 4);
    assert!(!dir.join("stale-file").exists());
    assert!(rich_summary(&store, root.path()).is_some());
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_ne!(manifest["format_version"], 1);
    assert_ne!(manifest["source"], "/elsewhere");
}

/// A summary a table read cached for a session the walk does not
/// list, as for an agent session read while it ran, has no manifest
/// entry. Reconcile removes it once the session is gone from disk.
#[test]
fn reconcile_removes_orphan_summaries() {
    let (store, _root, _cache) = store_with_tempdirs();
    store.reconcile(LockMode::Wait).unwrap();
    let orphan = "01234567-89ab-cdef-0123-456789abcdef";
    store
        .put_session_attrs(orphan, &gage_claude::index::ClaudeSessionAttrs::default())
        .unwrap();
    assert!(
        store
            .session_attrs(orphan, SystemTime::UNIX_EPOCH)
            .is_some()
    );

    let outcome = store.reconcile(LockMode::Wait).unwrap();
    assert_eq!(outcome.removed, 1);
    assert!(
        store
            .session_attrs(orphan, SystemTime::UNIX_EPOCH)
            .is_none()
    );
}
