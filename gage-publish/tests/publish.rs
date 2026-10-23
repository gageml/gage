//! The publish engine over a Claude session added to a temporary
//! store: filters at init, text and span redactions, and the staged
//! content and `redacted.json` they produce.

use std::fs;

use gage_publish::{FilterSpec, PublishError, Publisher, RedactionInput};
use gage_registry::driver::DriverRegistry;
use gage_session::SourceOptions;
use gage_store::{SessionRecord, SessionStore, Store};
use serde_json::Value;

const SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/home/garrett/proj","message":{"role":"user","content":"Hello, what the fucking hell is this? Mail alice@example.com"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"id":"m1","role":"assistant","model":"m","content":[{"type":"text","text":"Use key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123 for alice@example.com"}],"usage":{"input_tokens":10,"output_tokens":7}}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a2","timestamp":"2025-01-01T00:00:02Z","message":{"id":"m2","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123"}}],"usage":{"input_tokens":10,"output_tokens":3}}}"#,
    "\n",
    r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"toolUseResult":{"stdout":"ok"}}"#,
    "\n",
    r#"{"type":"api-request","id":"r1","params":{"safeguards":[{"classifier_context":{"user_identity":"garrett","home_dir":"/home/garrett"}}]}}"#,
    "\n",
);

struct Fixture {
    _home: tempfile::TempDir,
    _claude: tempfile::TempDir,
    store: Store,
    original: SessionRecord,
    publish_root: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let claude = tempfile::tempdir().unwrap();
    let native_id = "11111111-2222-3333-4444-555555555555";
    let project_dir = claude.path().join("projects").join("-home-garrett-proj");
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(project_dir.join(format!("{native_id}.jsonl")), SESSION).unwrap();

    let store_path = home.path().join("store.git");
    gage_store::init(&store_path).unwrap();
    let store = Store::open(&store_path).unwrap();
    let spec = format!("claude:{}", claude.path().display());
    let registry = DriverRegistry::builtin();
    let driver = registry.driver_for(&spec).unwrap();
    let source = driver
        .open_source(SourceOptions::new(&spec).cache_root(claude.path().join("cache")))
        .unwrap();
    let mut native = source.open_native(native_id).unwrap();
    let outcome = SessionStore::from(&store)
        .add(driver.as_ref(), native.as_mut())
        .unwrap();
    let original = SessionStore::from(&store)
        .at_commit(&outcome.commit_sha)
        .unwrap();
    Fixture {
        publish_root: home.path().join("publish"),
        _home: home,
        _claude: claude,
        store,
        original,
    }
}

fn staged_lines(root: &std::path::Path, id: &str) -> Vec<String> {
    fs::read_to_string(root.join(id).join("files.d").join("session.jsonl"))
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

fn staged_summary(root: &std::path::Path, id: &str) -> Value {
    let attrs: Value =
        serde_json::from_str(&fs::read_to_string(root.join(id).join("attrs.json")).unwrap())
            .unwrap();
    attrs["summary"].clone()
}

fn redacted(root: &std::path::Path, id: &str) -> Vec<Value> {
    serde_json::from_str(&fs::read_to_string(root.join(id).join("redacted.json")).unwrap()).unwrap()
}

#[test]
fn init_runs_the_named_filters_and_records_their_instances() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let filters = [
        FilterSpec::from_text("pii"),
        FilterSpec::from_text("secret"),
    ];
    let init = publisher.init(&f.original, &filters).unwrap();

    let definition = publisher.definition(&init.id).unwrap();
    let by_author = |author: &str| {
        definition
            .redactions
            .iter()
            .filter(|r| r.author == author)
            .count()
    };
    // pii: the email pattern, plus the username and home_dir typed literals
    assert_eq!(by_author("filter:pii"), 3, "{definition:#?}");
    // secret: one key, found by pattern in two places
    assert_eq!(by_author("filter:secret"), 1, "{definition:#?}");
    assert!(definition.redactions.iter().all(|r| r.instances > 0));

    let lines = staged_lines(&f.publish_root, &init.id);
    assert_eq!(lines.len(), 5);
    assert!(lines[0].contains("Mail [REDACTED]"), "{}", lines[0]);
    assert!(
        lines[0].contains(r#""cwd":"[REDACTED]/proj""#),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("Use key [REDACTED] for [REDACTED]"),
        "{}",
        lines[1]
    );
    // The tool_use block is atomic: the whole command value is replaced
    assert!(
        lines[2].contains(r#""command":"[REDACTED]""#),
        "{}",
        lines[2]
    );
    assert!(
        lines[4].contains(r#""user_identity":"[REDACTED]""#),
        "{}",
        lines[4]
    );
    // Key order of the original line is kept
    assert!(
        lines[0].starts_with(r#"{"type":"user","uuid":"u1""#),
        "{}",
        lines[0]
    );

    let records = redacted(&f.publish_root, &init.id);
    assert!(
        records
            .iter()
            .any(|r| r[0] == "span" && r[6] == "filter:pii"),
        "{records:#?}"
    );
    assert!(
        records
            .iter()
            .any(|r| r[0] == "value" && r[2] == "/message/content/0/input/command"),
        "{records:#?}"
    );
    assert!(records.iter().all(|r| r.as_array().unwrap().len() >= 5));
}

#[test]
fn redact_applies_lines_text_and_spans_and_rejects_a_miss() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();
    assert_eq!(staged_lines(&f.publish_root, &init.id).len(), 5);

    let applied = publisher
        .redact(
            &init.id,
            &[
                RedactionInput::parse_lines("5", "opaque").unwrap(),
                RedactionInput::parse_text("garrett", "pii", true).unwrap(),
                // Line 1's text values in document order: cwd, then
                // message.content
                RedactionInput::Span {
                    line: 1,
                    seq: 1,
                    reason: "language".into(),
                    literal: "fucking".into(),
                    mark: false,
                },
            ],
            "user:test",
        )
        .unwrap();
    assert_eq!(applied.len(), 3);
    assert_eq!(applied[0].instances, 1);
    // "garrett" is in cwd on line 1 only; line 5 is dropped
    assert_eq!(applied[1].instances, 1, "{applied:#?}");
    assert_eq!(applied[2].instances, 1);

    let lines = staged_lines(&f.publish_root, &init.id);
    assert_eq!(lines.len(), 4, "line 5 is dropped");
    assert!(lines[0].contains("what the hell is this?"), "{}", lines[0]);
    assert!(
        lines[0].contains(r#""cwd":"/home/[REDACTED]/proj""#),
        "{}",
        lines[0]
    );

    let records = redacted(&f.publish_root, &init.id);
    assert!(
        records
            .iter()
            .any(|r| r[0] == "line" && r[1] == 5 && r[3] == "opaque"),
        "{records:#?}"
    );
    assert!(
        records
            .iter()
            .all(|r| r.as_array().unwrap().last().unwrap() == "user:test")
    );

    let definition = publisher.definition(&init.id).unwrap();
    assert_eq!(definition.redactions.len(), 3);
    assert_eq!(definition.next_id, 4);

    let miss = publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("nothing-like-this", "secret", true).unwrap()],
            "user:test",
        )
        .unwrap_err();
    assert!(matches!(miss, PublishError::NoMatch(_)), "{miss}");
    assert_eq!(publisher.definition(&init.id).unwrap().redactions.len(), 3);
}

#[test]
fn reinit_replaces_the_filters_and_their_redactions() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher
        .init(&f.original, &[FilterSpec::from_text("secret")])
        .unwrap();
    publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_lines("5", "scope").unwrap()],
            "user:test",
        )
        .unwrap();

    let definition = publisher
        .reinit(&init.id, &[FilterSpec::from_text("pii")])
        .unwrap();
    let authors: Vec<&str> = definition
        .redactions
        .iter()
        .map(|r| r.author.as_str())
        .collect();
    assert!(authors.contains(&"user:test"), "{authors:?}");
    assert!(authors.contains(&"filter:pii"), "{authors:?}");
    assert!(!authors.contains(&"filter:secret"), "{authors:?}");
    let lines = staged_lines(&f.publish_root, &init.id);
    assert!(
        lines[1].contains("sk-ant-api03"),
        "the key is no longer redacted: {}",
        lines[1]
    );
    assert!(
        lines[1].contains("[REDACTED]"),
        "the email still is: {}",
        lines[1]
    );
}

/// The staged summary follows the staged content: a dropped assistant
/// entry lowers the line count, the message count, and the token
/// totals, and the title stays the original's.
#[test]
fn the_staged_summary_follows_the_derived_content() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();

    let before = staged_summary(&f.publish_root, &init.id);
    assert_eq!(before["line_count"], 5, "{before}");
    assert_eq!(before["output_tokens"], 10, "{before}");
    let title = before["title"].clone();
    assert!(title.is_string(), "{before}");

    publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_lines("1 2", "scope").unwrap()],
            "user:test",
        )
        .unwrap();
    let after = staged_summary(&f.publish_root, &init.id);
    assert_eq!(after["line_count"], 3, "{after}");
    assert_eq!(after["output_tokens"], 3, "{after}");
    assert!(
        after["message_count"].as_u64().unwrap() < before["message_count"].as_u64().unwrap(),
        "{before} -> {after}"
    );
    assert_eq!(after["title"], title, "{after}");
}

/// `undo` removes redactions by id and rebuilds; an unknown id changes
/// nothing; a filter's undone redaction is suppressed on re-run.
#[test]
fn undo_removes_redactions_and_suppresses_a_filters() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher
        .init(&f.original, &[FilterSpec::from_text("secret")])
        .unwrap();
    let applied = publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_lines("5", "scope").unwrap()],
            "user:test",
        )
        .unwrap();
    assert_eq!(staged_lines(&f.publish_root, &init.id).len(), 4);

    let miss = publisher.undo(&init.id, &[applied[0].id, 99]).unwrap_err();
    assert!(miss.to_string().contains("99"), "{miss}");
    assert_eq!(
        staged_lines(&f.publish_root, &init.id).len(),
        4,
        "nothing changed"
    );

    let removed = publisher.undo(&init.id, &[applied[0].id]).unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(
        staged_lines(&f.publish_root, &init.id).len(),
        5,
        "line 5 is back"
    );
    let definition = publisher.definition(&init.id).unwrap();
    assert!(
        definition.suppressions.is_empty(),
        "a user's undo leaves no suppression"
    );
    assert_eq!(definition.next_id, applied[0].id + 1, "ids are not reused");

    let secret_id = definition
        .redactions
        .iter()
        .find(|r| r.author == "filter:secret")
        .unwrap()
        .id;
    publisher.undo(&init.id, &[secret_id]).unwrap();
    let definition = publisher.definition(&init.id).unwrap();
    assert_eq!(definition.suppressions.len(), 1);
    assert!(staged_lines(&f.publish_root, &init.id)[1].contains("sk-ant-api03"));

    let rerun = publisher
        .reinit(&init.id, &[FilterSpec::from_text("secret")])
        .unwrap();
    assert!(
        !rerun.redactions.iter().any(|r| r.author == "filter:secret"),
        "the suppressed key is not restored: {rerun:#?}"
    );
    assert!(staged_lines(&f.publish_root, &init.id)[1].contains("sk-ant-api03"));
}

/// `refresh` re-applies a definition edited by hand and rejects an
/// invalid one without touching the staged session.
#[test]
fn refresh_reapplies_a_hand_edited_definition() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();
    let path = publisher.store().definition_file(&init.id);

    let mut json: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    json["redactions"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "id": 1, "author": "user:hand", "type": "text", "reason": "pii", "literal": "garrett"
        }));
    json["next_id"] = serde_json::json!(2);
    fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

    let rebuilt = publisher.refresh(&init.id).unwrap();
    assert_eq!(rebuilt.redactions.len(), 1);
    assert_eq!(rebuilt.redactions[0].instances, 3, "{rebuilt:#?}");
    assert!(staged_lines(&f.publish_root, &init.id)[0].contains("/home/[REDACTED]/proj"));

    json["redactions"][0]["reason"] = serde_json::json!("bogus");
    fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    let err = publisher.refresh(&init.id).unwrap_err();
    assert!(err.to_string().contains("redaction 1"), "{err}");
    assert!(err.to_string().contains("bogus"), "{err}");
    assert!(staged_lines(&f.publish_root, &init.id)[0].contains("/home/[REDACTED]/proj"));
}
