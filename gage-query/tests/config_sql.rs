#![allow(clippy::indexing_slicing)]

#[path = "common/mod.rs"]
#[allow(dead_code)]
mod common;

use std::fs;
use std::path::Path;
use std::sync::Arc;

use common::col_strings;
use datafusion::arrow::array::{Array, Int64Array, StringArray, TimestampMillisecondArray};
use datafusion::prelude::{SessionConfig, SessionContext};
use gage_claude::session::encode_project_dir;
use gage_query::tables::ConfigTable;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Build a fake claude-home dir plus one resolvable project. Returns
/// the tempdir (drop it last) and a `SessionContext` with the `config`
/// table pointed at the tempdir.
fn fixture() -> (tempfile::TempDir, SessionContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path();

    // User scope
    write(&claude.join("settings.json"), r#"{"theme":"dark"}"#);
    write(&claude.join("CLAUDE.md"), "user memory");
    write(&claude.join("skills/rust/SKILL.md"), "---\n---\n");
    write(&claude.join("skills/rust/rules/clippy.md"), "rule");
    write(&claude.join("commands/summary.md"), "cmd");
    write(&claude.join("agents/explorer.md"), "agent");

    // Project scope: a project rooted at <claude>/projsrc registered
    // in <claude>/.claude.json
    let project_root = claude.join("projsrc");
    fs::create_dir_all(&project_root).unwrap();
    write(&project_root.join("CLAUDE.md"), "project memory");
    write(&project_root.join(".claude/settings.json"), "{}");
    write(&project_root.join(".claude/settings.local.json"), "{}");
    write(
        &project_root.join(".claude/skills/local/SKILL.md"),
        "---\n---\n",
    );

    let claude_json = format!(
        r#"{{"projects":{{"{}":{{}}}}}}"#,
        project_root.to_string_lossy()
    );
    write(&claude.join(".claude.json"), &claude_json);
    // A sessions dir under the encoded project name has to exist for
    // `ClaudeHome::projects()` to surface this project.
    let encoded = encode_project_dir(&project_root);
    fs::create_dir_all(claude.join("projects").join(&encoded)).unwrap();

    let cfg = SessionConfig::new().with_information_schema(true);
    let ctx = SessionContext::new_with_config(cfg);
    ctx.register_table("config", Arc::new(ConfigTable::new(claude)))
        .unwrap();
    (tmp, ctx)
}

async fn run(ctx: &SessionContext, sql: &str) -> Vec<datafusion::arrow::record_batch::RecordBatch> {
    ctx.sql(sql).await.unwrap().collect().await.unwrap()
}

#[tokio::test]
async fn user_scope_returns_expected_types_and_names() {
    let (_tmp, ctx) = fixture();
    let batches = run(
        &ctx,
        "SELECT type, name FROM config WHERE scope = 'user' \
         ORDER BY type, name",
    )
    .await;
    let batch = &batches[0];
    let types = col_strings(batch, 0);
    let names = col_strings(batch, 1);

    let pairs: Vec<(String, String)> = types.into_iter().zip(names).collect();
    assert!(pairs.contains(&("settings".into(), String::new())));
    assert!(pairs.contains(&("memory".into(), String::new())));
    assert!(pairs.contains(&("skill".into(), "rust".into())));
    assert!(pairs.contains(&("skill-rule".into(), "rust::clippy".into())));
    assert!(pairs.contains(&("command".into(), "summary".into())));
    assert!(pairs.contains(&("agent".into(), "explorer".into())));
}

#[tokio::test]
async fn project_scope_uses_path_and_includes_local() {
    let (tmp, ctx) = fixture();
    let expected_project = tmp.path().join("projsrc").to_string_lossy().into_owned();
    let batches = run(
        &ctx,
        &format!(
            "SELECT scope, project, type FROM config \
             WHERE scope IN ('project', 'local') AND project = '{expected_project}' \
             ORDER BY scope, type"
        ),
    )
    .await;
    let batch = &batches[0];
    let scopes = col_strings(batch, 0);
    let projects = col_strings(batch, 1);
    let types = col_strings(batch, 2);

    assert!(!scopes.is_empty());
    for p in &projects {
        assert_eq!(p, &expected_project);
    }
    assert!(scopes.contains(&"local".into()));
    assert!(scopes.contains(&"project".into()));
    assert!(types.iter().any(|t| t == "settings"));
    assert!(types.iter().any(|t| t == "memory"));
}

#[tokio::test]
async fn type_filter_prunes_rows() {
    let (_tmp, ctx) = fixture();
    let batches = run(
        &ctx,
        "SELECT name FROM config WHERE type = 'skill' ORDER BY name",
    )
    .await;
    let batch = &batches[0];
    let names = col_strings(batch, 0);
    // user skill 'rust' and project skill 'local'
    assert_eq!(names, vec!["local".to_string(), "rust".to_string()]);
}

#[tokio::test]
async fn text_is_read_when_projected() {
    let (_tmp, ctx) = fixture();
    let batches = run(
        &ctx,
        "SELECT text FROM config \
         WHERE scope = 'user' AND type = 'settings'",
    )
    .await;
    let batch = &batches[0];
    let texts = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts.value(0), r#"{"theme":"dark"}"#);
}

#[tokio::test]
async fn size_and_mtime_populated() {
    let (_tmp, ctx) = fixture();
    let batches = run(
        &ctx,
        "SELECT size, mtime FROM config \
         WHERE scope = 'user' AND type = 'settings'",
    )
    .await;
    let batch = &batches[0];
    let sizes = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let mtimes = batch
        .column(1)
        .as_any()
        .downcast_ref::<TimestampMillisecondArray>()
        .unwrap();
    assert_eq!(sizes.value(0), r#"{"theme":"dark"}"#.len() as i64);
    assert!(mtimes.value(0) > 0);
}

/// Build a fixture where the project's `.claude/commands` path is a
/// regular FILE rather than a directory. The walker's `read_dir` on it
/// errors with `NotADirectory` — a non-`NotFound` error that
/// propagates through the iterator and fails the SQL query. A query
/// that prunes the commands phase (via the `type` filter) should
/// succeed; one that runs it should fail.
fn fixture_with_broken_commands() -> (tempfile::TempDir, SessionContext) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path();
    // Minimal user scope so user_files has something to walk without
    // hitting the broken path (which lives under the project).
    write(&claude.join("settings.json"), "{}");

    let project_root = claude.join("proj");
    fs::create_dir_all(project_root.join(".claude")).unwrap();
    write(&project_root.join("CLAUDE.md"), "memory");
    write(&project_root.join(".claude/settings.json"), "{}");
    // `commands` is a regular file — readdir on it errors
    write(&project_root.join(".claude/commands"), "not a dir");

    let claude_json = format!(
        r#"{{"projects":{{"{}":{{}}}}}}"#,
        project_root.to_string_lossy()
    );
    write(&claude.join(".claude.json"), &claude_json);
    let encoded = encode_project_dir(&project_root);
    fs::create_dir_all(claude.join("projects").join(&encoded)).unwrap();

    let cfg = SessionConfig::new().with_information_schema(true);
    let ctx = SessionContext::new_with_config(cfg);
    ctx.register_table("config", Arc::new(ConfigTable::new(claude)))
        .unwrap();
    (tmp, ctx)
}

#[tokio::test]
async fn type_filter_skips_unreadable_phase() {
    // `type='memory'` turns off the commands phase, so the broken
    // `.claude/commands` file never gets read_dir'd — query succeeds
    let (_tmp, ctx) = fixture_with_broken_commands();
    let batches = run(
        &ctx,
        "SELECT type FROM config WHERE type = 'memory' AND scope = 'project'",
    )
    .await;
    let types = col_strings(&batches[0], 0);
    assert!(types.iter().all(|t| t == "memory"));
    assert!(!types.is_empty());
}

#[tokio::test]
async fn no_filter_hits_unreadable_phase() {
    // Without a type filter, the commands phase runs and the bogus
    // file produces a fs error that fails the SQL query.
    let (_tmp, ctx) = fixture_with_broken_commands();
    let result = ctx
        .sql("SELECT count(*) FROM config WHERE scope = 'project'")
        .await
        .unwrap()
        .collect()
        .await;
    assert!(result.is_err(), "expected error from unreadable commands");
}
