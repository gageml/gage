//! `GAGE_SHOW_PUBLISHING=1` includes a pending publish working tree in
//! the `session`, `entry`, `message`, and `tool_use` tables. The test
//! runs in its own process, so its environment mutation does not race
//! the library tests.

use std::sync::{Arc, Mutex};

use datafusion::arrow::array::{Array, StringArray};
use datafusion::prelude::SessionContext;
use gage_query::ContextBuilder;
use gage_registry::driver::DriverRegistry;
use gage_session::SourceOptions;
use gage_store::{PublishStore, SessionStore, Store};

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
async fn pending_publish_appears_in_row_tables_under_env() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this integration test owns its process; no other test
    // reads these variables concurrently. Both are removed at the end.
    unsafe {
        std::env::set_var("GAGE_HOME", home.path());
        std::env::set_var("GAGE_SHOW_PUBLISHING", "1");
    }

    let claude_root = tempfile::tempdir().unwrap();
    let native_id = "11111111-2222-3333-4444-555555555555";
    let project_dir = claude_root.path().join("projects").join("-w-proj");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join(format!("{native_id}.jsonl")), SESSION).unwrap();

    let store_path = home.path().join("store.git");
    gage_store::init(&store_path).unwrap();
    let store = Store::open(&store_path).unwrap();

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
    let pending = PublishStore::from(&store).init(&original).unwrap();

    let ctx = ContextBuilder::new(Some(Arc::new(Mutex::new(store))))
        .build()
        .await;
    let id = &pending.id;

    let sess = strings(&ctx, &format!("SELECT id FROM session WHERE id = '{id}'")).await;
    assert_eq!(sess, vec![id.clone()], "pending session row");

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

    unsafe {
        std::env::remove_var("GAGE_SHOW_PUBLISHING");
        std::env::remove_var("GAGE_HOME");
    }
}
