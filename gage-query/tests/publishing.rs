//! An external-scoped context serves a session directory in the
//! `session`, `entry`, `message`, `tool_use`, `tool_use_file`, and
//! `session_change` tables,
//! and nothing else; a rewritten directory is re-read by the same
//! context.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{Array, StringArray};
use datafusion::prelude::SessionContext;
use gage_core::uuid::new_uuid;
use gage_query::ContextBuilder;
use gage_registry::driver::DriverRegistry;
use gage_session::SourceOptions;
use gage_store::{ExternalSession, SessionAttrsRecord, SessionDirLayout, SessionStore, Store};

const SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/w/proj","message":{"role":"user","content":"hello there"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi"}]}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a2","timestamp":"2025-01-01T00:00:02Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
    "\n",
    r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"out"}]}}"#,
    "\n",
);

async fn strings(ctx: &SessionContext, sql: &str) -> Vec<String> {
    let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
    let mut out = Vec::new();
    for batch in &batches {
        let col = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        out.extend((0..batch.num_rows()).map(|i| col.value(i).to_string()));
    }
    out
}

#[tokio::test]
async fn external_scope_serves_session_directories_in_the_session_tables() {
    let home = tempfile::tempdir().unwrap();

    let claude_root = tempfile::tempdir().unwrap();
    let native_id = "11111111-2222-3333-4444-555555555555";
    let project_dir = claude_root.path().join("projects").join("-w-proj");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join(format!("{native_id}.jsonl")), SESSION).unwrap();

    let store_path = home.path().join("store.git");
    gage_store::init(&store_path).unwrap();
    let store = Store::open_with_cache_root(&store_path, &home.path().join("cache")).unwrap();

    let spec = format!("claude:{}", claude_root.path().display());
    let registry = DriverRegistry::builtin();
    let driver = registry.driver_for(&spec).unwrap();
    let source = driver
        .open_source(SourceOptions::new(&spec).cache_root(claude_root.path().join("cache")))
        .unwrap();
    let mut native = source.open_native(native_id).unwrap();
    let outcome = SessionStore::from(&store)
        .add(driver.as_ref(), native.as_mut())
        .unwrap();
    let original = SessionStore::from(&store)
        .at_commit(&outcome.commit_sha)
        .unwrap();
    // A derived copy of the original as a session directory
    let derived = new_uuid();
    let dir = home.path().join("external").join(&derived);
    let layout = SessionDirLayout::new(&dir);
    std::fs::create_dir_all(layout.files_dir()).unwrap();
    let attrs = SessionAttrsRecord {
        origin: format!("session:{}@{}", original.id, original.commit_sha),
        ..original.attrs.clone()
    };
    let mut attrs_bytes = serde_json::to_vec(&serde_json::to_value(&attrs).unwrap()).unwrap();
    attrs_bytes.push(b'\n');
    std::fs::write(layout.attrs_file(), attrs_bytes).unwrap();
    std::fs::write(layout.changes_file(), "[]\n").unwrap();
    std::fs::write(layout.files_dir().join("session.jsonl"), SESSION).unwrap();

    let store = Arc::new(Mutex::new(store));
    let ctx = ContextBuilder::new(Some(Arc::clone(&store)))
        .external(vec![ExternalSession {
            id: derived.clone(),
            dir: dir.clone(),
        }])
        .build()
        .await;
    let id = &derived;

    // The scope holds the external session and not its stored original
    let sess = strings(&ctx, "SELECT id FROM session").await;
    assert_eq!(sess, vec![id.clone()], "external session row");
    let tables = strings(&ctx, "SHOW TABLES").await;
    assert!(!tables.is_empty());
    for absent in ["note", "issue", "dataset", "scan"] {
        let names = strings(
            &ctx,
            &format!(
                "SELECT table_name FROM information_schema.tables WHERE table_name = '{absent}'"
            ),
        )
        .await;
        assert!(names.is_empty(), "{absent} is not an external scope table");
    }

    // The store-wide context does not hold it
    let wide = ContextBuilder::new(Some(store)).build().await;
    let stored = strings(&wide, "SELECT id FROM session").await;
    assert_eq!(stored, vec![original.id.clone()], "store-wide rows");

    let types = strings(
        &ctx,
        &format!("SELECT type FROM entry WHERE session_id = '{id}' ORDER BY line"),
    )
    .await;
    assert_eq!(types, ["user", "assistant", "assistant", "user"], "entries");

    let texts = strings(
        &ctx,
        &format!("SELECT text FROM message WHERE session_id = '{id}' ORDER BY line"),
    )
    .await;
    assert!(texts.contains(&"hello there".to_string()), "{texts:?}");
    assert!(texts.contains(&"hi".to_string()), "{texts:?}");

    let tools = strings(
        &ctx,
        &format!("SELECT name FROM tool_use WHERE session_id = '{id}'"),
    )
    .await;
    assert_eq!(tools, ["Bash"], "tool_use");

    // A rewrite replaces the content and `changes.json` in place and
    // rewrites `attrs.json`. The row cache keys an external session by
    // the `attrs.json` mtime, so the same context serves the new
    // content on the next query, which is what the session view's
    // reload relies on.
    let empty = strings(
        &ctx,
        &format!("SELECT type FROM session_change WHERE session_id = '{id}'"),
    )
    .await;
    assert!(
        empty.is_empty(),
        "a fresh directory has no records: {empty:?}"
    );

    let staged = layout.files_dir().join("session.jsonl");
    let rewritten = std::fs::read_to_string(&staged)
        .unwrap()
        .replace("hello there", "hello [REDACTED]");
    std::fs::write(&staged, rewritten).unwrap();
    std::fs::write(
        layout.changes_file(),
        r#"[["text", 1, "/message/content", 6, 16, "pii", "filter:pii"]]"#,
    )
    .unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(90);
    std::fs::File::options()
        .write(true)
        .open(layout.attrs_file())
        .unwrap()
        .set_modified(later)
        .unwrap();
    let reread = strings(
        &ctx,
        &format!("SELECT text FROM message WHERE session_id = '{id}' AND line = 1"),
    )
    .await;
    assert_eq!(reread, ["hello [REDACTED]"], "rewritten content is re-read");

    let records = strings(
        &ctx,
        &format!(
            "SELECT type || ' ' || ref || ' ' || reason FROM session_change \
             WHERE session_id = '{id}'"
        ),
    )
    .await;
    assert_eq!(
        records,
        ["text /message/content pii"],
        "rewritten records are re-read"
    );
}
