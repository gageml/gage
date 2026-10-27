//! The publish engine over a Claude session added to a temporary
//! store: filters at init, text and span redactions, and the staged
//! content and `filtered.json` they produce.

use std::fs;

use gage_publish::{FilterSpec, PublishError, Publisher, RedactionInput};
use gage_registry::driver::DriverRegistry;
use gage_session::SourceOptions;
use gage_store::{
    FilteredKind, FilteredRecord, SessionRecord, SessionStore, Store, parse_filtered,
};
use serde_json::Value;

const SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/home/garrett/proj","message":{"role":"user","content":"Hello, what the fucking hell is this? Mail alice@example.com"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"id":"m1","role":"assistant","model":"m","content":[{"type":"text","text":"Use key sk-ant-api03-PtYgjmUhBel31iEl2hpChYgCfrL1spNxnyVmihA-2O76UMFxFkM-R5Kjp1vRt_1fjORS-6ilI8ihN5KXSc7Tvo-hBKqFYAA for alice@example.com"}],"usage":{"input_tokens":10,"output_tokens":7}}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a2","timestamp":"2025-01-01T00:00:02Z","message":{"id":"m2","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo sk-ant-api03-PtYgjmUhBel31iEl2hpChYgCfrL1spNxnyVmihA-2O76UMFxFkM-R5Kjp1vRt_1fjORS-6ilI8ihN5KXSc7Tvo-hBKqFYAA"}}],"usage":{"input_tokens":10,"output_tokens":3}}}"#,
    "\n",
    r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"toolUseResult":{"stdout":"ok"}}"#,
    "\n",
    r#"{"type":"api-request","id":"r1","params":{"safeguards":[{"classifier_context":{"user_identity":"garrett","home_dir":"/home/garrett"}}]}}"#,
    "\n",
);

/// A session whose working directory, classifier context, tool
/// result, and prose all carry the machine's paths and host.
const SYSTEM_SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/home/garrett/proj","gitBranch":"main","message":{"role":"user","content":"Hello"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls /home/garrett/proj"}}],"usage":{"input_tokens":10,"output_tokens":3}}}"#,
    "\n",
    r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a.txt"}]},"toolUseResult":{"stdout":"/home/garrett/proj/a.txt"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a2","timestamp":"2025-01-01T00:00:03Z","message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"The file is at /home/garrett/proj/a.txt"}],"usage":{"input_tokens":10,"output_tokens":7}}}"#,
    "\n",
    r#"{"type":"api-request","id":"r1","params":{"safeguards":[{"classifier_context":{"user_identity":"garrett","home_dir":"/home/garrett","live_cwd":"/home/garrett/proj","git_state":{"root":"/home/garrett/proj","branch":"main","visibility":{"origin":{"host":"git.corp.example","visibility":"private"}}}}}]}}"#,
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
    fixture_with(SESSION)
}

fn fixture_with(session: &str) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let claude = tempfile::tempdir().unwrap();
    let native_id = "11111111-2222-3333-4444-555555555555";
    let project_dir = claude.path().join("projects").join("-home-garrett-proj");
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(project_dir.join(format!("{native_id}.jsonl")), session).unwrap();

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

fn staged_attrs(root: &std::path::Path, id: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(root.join(id).join("attrs.json")).unwrap()).unwrap()
}

fn staged_summary(root: &std::path::Path, id: &str) -> Value {
    staged_attrs(root, id)["summary"].clone()
}

fn filtered(root: &std::path::Path, id: &str) -> Vec<FilteredRecord> {
    parse_filtered(&fs::read(root.join(id).join("filtered.json")).unwrap()).unwrap()
}

#[test]
fn init_runs_the_named_filters_and_records_their_instances() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let filters = [
        "pii".parse::<FilterSpec>().unwrap(),
        "secret".parse::<FilterSpec>().unwrap(),
    ];
    let init = publisher.init(&f.original, &filters).unwrap();

    let definition = publisher.definition(&init.id).unwrap();
    let by_author = |author: &str| {
        definition
            .text
            .iter()
            .filter(|t| t.author == author)
            .count()
    };
    // pii: the email pattern, plus the username and home_dir typed literals
    assert_eq!(by_author("filter:pii"), 3, "{definition:#?}");
    // secret: one key, found by pattern in two places
    assert_eq!(by_author("filter:secret"), 1, "{definition:#?}");
    let counts = publisher.instances(&init.id).unwrap();
    assert_eq!(counts.len(), 4);
    assert!(counts.values().all(|n| *n > 0), "{counts:?}");

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
    // The key inside the tool input is a span like any other
    assert!(
        lines[2].contains(r#""command":"echo [REDACTED]""#),
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

    let records = filtered(&f.publish_root, &init.id);
    assert!(
        records
            .iter()
            .any(|r| r.kind == FilteredKind::Text && r.author == "filter:pii"),
        "{records:#?}"
    );
    // The span in the command sits after "echo " in the published value
    assert!(
        records.iter().any(|r| r.kind == FilteredKind::Text
            && r.reference.as_deref() == Some("/message/content/0/input/command")
            && (r.span_start, r.span_end) == (Some(5), Some(15))),
        "{records:#?}"
    );
}

#[test]
fn redact_applies_text_everywhere_and_rejects_a_miss() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();
    assert_eq!(staged_lines(&f.publish_root, &init.id).len(), 5);

    let applied = publisher
        .redact(
            &init.id,
            &[
                RedactionInput::parse_text("garrett", "pii", true).unwrap(),
                RedactionInput::parse_text("fucking", "language", false).unwrap(),
            ],
            "user:test",
        )
        .unwrap();
    assert_eq!(applied.len(), 2);
    // "garrett" is in cwd on line 1 and in the username and home_dir on
    // line 5
    assert_eq!(applied[0].instances, 3, "{applied:#?}");
    assert_eq!(applied[1].instances, 1);

    let lines = staged_lines(&f.publish_root, &init.id);
    assert_eq!(lines.len(), 5);
    assert!(lines[0].contains("what the hell is this?"), "{}", lines[0]);
    assert!(
        lines[0].contains(r#""cwd":"/home/[REDACTED]/proj""#),
        "{}",
        lines[0]
    );
    assert!(
        lines[4].contains(r#""home_dir":"/home/[REDACTED]""#),
        "{}",
        lines[4]
    );

    let records = filtered(&f.publish_root, &init.id);
    assert!(records.iter().all(|r| r.author == "user:test"));
    assert!(records.iter().all(|r| r.kind == FilteredKind::Text));
    // Span offsets are positions in the published value: the marker in
    // the cwd, and an empty span where "fucking " was removed
    let span = |pointer: &str| {
        let r = records
            .iter()
            .find(|r| r.line == 1 && r.reference.as_deref() == Some(pointer))
            .unwrap_or_else(|| panic!("no record at {pointer}: {records:#?}"));
        (r.span_start.unwrap(), r.span_end.unwrap())
    };
    assert_eq!(span("/cwd"), (6, 16));
    assert_eq!(span("/message/content"), (16, 16));

    let definition = publisher.definition(&init.id).unwrap();
    assert_eq!(definition.text.len(), 2);
    assert_eq!(definition.next_text_id, 3);

    let miss = publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("nothing-like-this", "secret", true).unwrap()],
            "user:test",
        )
        .unwrap_err();
    assert!(matches!(miss, PublishError::NoMatch(_)), "{miss}");
    assert_eq!(publisher.definition(&init.id).unwrap().text.len(), 2);
}

/// A filter is a name, and `custom` takes its prompt as its whole
/// instruction: `init` refuses it without one.
#[test]
fn custom_requires_a_prompt_and_any_filter_takes_one() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let bare = FilterSpec {
        name: "custom".into(),
        prompt: None,
    };
    let err = publisher.init(&f.original, &[bare]).unwrap_err();
    assert!(err.to_string().contains("requires a prompt"), "{err}");
    assert!(publisher.store().list().unwrap().is_empty());

    let custom = FilterSpec::new("custom", Some("Remove every duck.".into())).unwrap();
    let pii = FilterSpec::new("pii", Some("Watch for street addresses.".into())).unwrap();
    let init = publisher.init(&f.original, &[custom, pii]).unwrap();
    let definition = publisher.definition(&init.id).unwrap();
    assert_eq!(definition.filters[0].name, "custom");
    assert_eq!(
        definition.filters[0].prompt.as_deref(),
        Some("Remove every duck.")
    );
    assert_eq!(
        definition.filters[1].prompt.as_deref(),
        Some("Watch for street addresses.")
    );
    let text = fs::read_to_string(publisher.store().definition_file(&init.id)).unwrap();
    assert!(
        text.contains(r#"{"name":"custom","prompt":"Remove every duck."}"#),
        "{text}"
    );
    let removed = publisher.filter_remove(&init.id, "custom").unwrap();
    assert_eq!(removed.name, "custom");
    let none = publisher.filter_remove(&init.id, "secret").unwrap_err();
    assert!(matches!(none, PublishError::Input(_)), "{none}");
    assert_eq!(publisher.definition(&init.id).unwrap().filters.len(), 1);
}

/// The `harness` filter leaves out the entries the harness wrote for
/// itself: the API request on line 5 goes, with one line record under
/// the filter, and a literal found only there is a miss.
#[test]
fn harness_filter_leaves_out_bookkeeping_entries() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher
        .init(&f.original, &["harness".parse::<FilterSpec>().unwrap()])
        .unwrap();
    let lines = staged_lines(&f.publish_root, &init.id);
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert!(
        !lines.iter().any(|l| l.contains("api-request")),
        "{lines:#?}"
    );
    let records = filtered(&f.publish_root, &init.id);
    let dropped: Vec<(u32, Option<u32>, &str, &str)> = records
        .iter()
        .filter(|r| r.kind == FilteredKind::Line)
        .map(|r| (r.line, r.line_end, r.reason.as_str(), r.author.as_str()))
        .collect();
    assert_eq!(dropped, [(5, Some(5), "harness", "filter:harness")]);
    let summary = staged_summary(&f.publish_root, &init.id);
    assert_eq!(summary["line_count"], 4, "{summary}");
    publisher.check(&init.id).unwrap();

    let err = publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("home_dir", "system", true).unwrap()],
            "user:test",
        )
        .unwrap_err();
    assert!(matches!(err, PublishError::NoMatch(_)), "{err}");
}

/// `init` rejects a filter given twice.
#[test]
fn init_rejects_a_duplicate_filter() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let pii = "pii".parse::<FilterSpec>().unwrap();
    let dup = publisher
        .init(&f.original, &[pii.clone(), pii])
        .unwrap_err();
    assert!(matches!(dup, PublishError::Input(_)), "{dup}");
    assert!(publisher.store().list().unwrap().is_empty());
}

/// `filter_add` runs the new filter and keeps everything else;
/// `filter_remove` takes the filter's redactions with it and leaves
/// the user's. A filter already on the list is rejected.
#[test]
fn filter_add_and_remove_change_one_filters_redactions() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher
        .init(&f.original, &["secret".parse::<FilterSpec>().unwrap()])
        .unwrap();
    publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("fucking", "language", true).unwrap()],
            "user:test",
        )
        .unwrap();

    let definition = publisher
        .filter_add(&init.id, &"pii".parse::<FilterSpec>().unwrap())
        .unwrap();
    let authors = |d: &gage_publish::PublishDefinition| -> Vec<String> {
        d.text.iter().map(|t| t.author.clone()).collect()
    };
    let added = authors(&definition);
    assert!(added.contains(&"user:test".to_string()), "{added:?}");
    assert!(added.contains(&"filter:pii".to_string()), "{added:?}");
    assert!(added.contains(&"filter:secret".to_string()), "{added:?}");
    assert_eq!(definition.filters.len(), 2);

    let dup = publisher
        .filter_add(&init.id, &"pii".parse::<FilterSpec>().unwrap())
        .unwrap_err();
    assert!(matches!(dup, PublishError::Input(_)), "{dup}");
    assert_eq!(publisher.definition(&init.id).unwrap().filters.len(), 2);

    let removed = publisher.filter_remove(&init.id, "secret").unwrap();
    assert_eq!(removed.name, "secret");
    let definition = publisher.definition(&init.id).unwrap();
    let left = authors(&definition);
    assert!(left.contains(&"user:test".to_string()), "{left:?}");
    assert!(left.contains(&"filter:pii".to_string()), "{left:?}");
    assert!(!left.contains(&"filter:secret".to_string()), "{left:?}");
    assert_eq!(definition.named_filters(), vec!["pii".to_string()]);
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
    assert!(
        lines[0].contains("what the [REDACTED] hell"),
        "the user's redaction stays: {}",
        lines[0]
    );
}

/// `filter_run` re-runs one named filter, or every named filter, under
/// new ids; a filter not on the list is an error.
#[test]
fn filter_run_reruns_one_or_every_named_filter() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let filters = [
        "pii".parse::<FilterSpec>().unwrap(),
        "secret".parse::<FilterSpec>().unwrap(),
    ];
    let init = publisher.init(&f.original, &filters).unwrap();
    let before = publisher.definition(&init.id).unwrap();

    let missing = publisher
        .filter_run(&init.id, Some("language"))
        .unwrap_err();
    assert!(matches!(missing, PublishError::Input(_)), "{missing}");

    let one = publisher.filter_run(&init.id, Some("secret")).unwrap();
    assert_eq!(one.text.len(), before.text.len());
    let pii_ids = |d: &gage_publish::PublishDefinition| -> Vec<u64> {
        d.text
            .iter()
            .filter(|t| t.author == "filter:pii")
            .map(|t| t.id)
            .collect()
    };
    assert_eq!(pii_ids(&one), pii_ids(&before), "pii did not run");
    assert!(
        one.text.iter().any(|t| t.author == "filter:secret"),
        "{one:#?}"
    );
    assert_eq!(
        one.next_text_id,
        before.next_text_id + 1,
        "the re-run key took a new id"
    );

    let all = publisher.filter_run(&init.id, None).unwrap();
    assert_eq!(all.text.len(), before.text.len());
    assert_ne!(pii_ids(&all), pii_ids(&before), "pii ran this time");
}

/// The title stays the original's: the driver takes it from the first
/// prompt, and redacting that prompt does not retitle the publication.
#[test]
fn the_staged_summary_keeps_the_original_title() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();

    let before = staged_summary(&f.publish_root, &init.id);
    let title = before["title"].clone();
    assert!(
        title.as_str().is_some_and(|t| t.contains("Hello")),
        "{before}"
    );

    publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("Hello", "privacy", true).unwrap()],
            "user:test",
        )
        .unwrap();
    let after = staged_summary(&f.publish_root, &init.id);
    assert_eq!(after["title"], title, "{after}");
    assert_eq!(after["line_count"], 5, "{after}");
    assert!(
        staged_lines(&f.publish_root, &init.id)[0].contains("[REDACTED], what"),
        "the prompt itself is redacted"
    );
}

/// `redact_remove` removes redactions by id and rebuilds; an unknown
/// id changes nothing; a filter's removed literal is allowed on re-run.
#[test]
fn redact_remove_removes_redactions_and_allows_a_filters_literal() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher
        .init(&f.original, &["secret".parse::<FilterSpec>().unwrap()])
        .unwrap();
    let applied = publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("garrett", "pii", true).unwrap()],
            "user:test",
        )
        .unwrap();
    let cwd = |publish_root: &std::path::Path| {
        let line = staged_lines(publish_root, &init.id).remove(0);
        line.contains("/home/[REDACTED]/proj")
    };
    assert!(cwd(&f.publish_root));

    let miss = publisher
        .redact_remove(&init.id, &[applied[0].id, 99], "user:test")
        .unwrap_err();
    assert!(miss.to_string().contains("99"), "{miss}");
    assert!(cwd(&f.publish_root), "nothing changed");

    let removed = publisher
        .redact_remove(&init.id, &[applied[0].id], "user:test")
        .unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].literal, "garrett");
    assert!(!cwd(&f.publish_root), "the username is back");
    let definition = publisher.definition(&init.id).unwrap();
    assert!(
        definition.allow.is_empty(),
        "a user's own literal is not allowed: {definition:#?}"
    );
    assert_eq!(
        definition.next_text_id,
        applied[0].id + 1,
        "ids are not reused"
    );

    let key = definition
        .text
        .iter()
        .find(|t| t.author == "filter:secret")
        .unwrap()
        .clone();
    publisher
        .redact_remove(&init.id, &[key.id], "user:test")
        .unwrap();
    let definition = publisher.definition(&init.id).unwrap();
    assert_eq!(definition.allow.len(), 1, "{definition:#?}");
    assert_eq!(definition.allow[0].literal, key.literal);
    assert_eq!(definition.allow[0].author, "user:test");
    assert!(staged_lines(&f.publish_root, &init.id)[1].contains("sk-ant-api03"));

    let rerun = publisher.filter_run(&init.id, Some("secret")).unwrap();
    assert!(
        !rerun.text.iter().any(|t| t.author == "filter:secret"),
        "the allowed key is not recorded again: {rerun:#?}"
    );
    assert!(staged_lines(&f.publish_root, &init.id)[1].contains("sk-ant-api03"));
}

/// `refresh` re-applies a definition edited by hand: a text redaction
/// takes effect, a segment drops every line outside it and the staged
/// summary follows, and an invalid definition is rejected without
/// touching the staged session.
#[test]
fn refresh_reapplies_a_hand_edited_definition() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();
    let path = publisher.store().definition_file(&init.id);
    let before = staged_summary(&f.publish_root, &init.id);
    assert_eq!(before["line_count"], 5, "{before}");
    assert_eq!(before["output_tokens"], 10, "{before}");

    let mut json: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    json["text"] = serde_json::json!([[1, "garrett", "pii", "user:hand", true]]);
    json["next_text_id"] = serde_json::json!(2);
    fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

    let instances = publisher.refresh(&init.id).unwrap();
    assert_eq!(instances, 3);
    assert!(staged_lines(&f.publish_root, &init.id)[0].contains("/home/[REDACTED]/proj"));

    // A segment keeps lines 2-3 and drops the rest, with a line record
    // per run attributed to the segment's author
    json["segments"] = serde_json::json!([[1, "The reply", [[2, 3]], "session:hand"]]);
    json["next_segment_id"] = serde_json::json!(2);
    fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    let instances = publisher.refresh(&init.id).unwrap();
    assert_eq!(instances, 0, "the username is only on dropped lines");
    let lines = staged_lines(&f.publish_root, &init.id);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(lines[0].contains(r#""uuid":"a1""#), "{}", lines[0]);
    assert!(lines[1].contains(r#""uuid":"a2""#), "{}", lines[1]);
    let records = filtered(&f.publish_root, &init.id);
    let dropped: Vec<(u32, Option<u32>, &str, &str)> = records
        .iter()
        .filter(|r| r.kind == FilteredKind::Line)
        .map(|r| (r.line, r.line_end, r.reason.as_str(), r.author.as_str()))
        .collect();
    assert_eq!(
        dropped,
        [
            (1, Some(1), "scope", "session:hand"),
            (4, Some(5), "scope", "session:hand"),
        ]
    );
    let after = staged_summary(&f.publish_root, &init.id);
    assert_eq!(after["line_count"], 2, "{after}");
    assert_eq!(after["output_tokens"], 10, "{after}");
    assert!(
        after["message_count"].as_u64().unwrap() < before["message_count"].as_u64().unwrap(),
        "{before} -> {after}"
    );
    assert_eq!(after["title"], before["title"], "{after}");
    publisher.check(&init.id).unwrap();

    json["text"][0][2] = serde_json::json!("bogus");
    fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    let err = publisher.refresh(&init.id).unwrap_err();
    assert!(err.to_string().contains("redaction 1"), "{err}");
    assert!(err.to_string().contains("bogus"), "{err}");
    assert_eq!(staged_lines(&f.publish_root, &init.id).len(), 2);
}

/// A segment is added without lines and drops nothing; once it has
/// lines, every line outside it is dropped; editing and removing it
/// rebuild; an unknown id is an error.
#[test]
fn segments_are_added_edited_and_removed() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();

    let id = publisher
        .segment_add(&init.id, "The reply", &[], "session:s")
        .unwrap();
    assert_eq!(id, 1);
    assert_eq!(staged_lines(&f.publish_root, &init.id).len(), 5);

    let edited = publisher
        .segment_edit(&init.id, id, None, Some(&[(2, 3)]))
        .unwrap();
    assert_eq!(edited.description, "The reply");
    assert_eq!(edited.lines, [(2, 3)]);
    assert_eq!(staged_lines(&f.publish_root, &init.id).len(), 2);
    let records = filtered(&f.publish_root, &init.id);
    assert_eq!(
        records
            .iter()
            .filter(|r| r.kind == FilteredKind::Line)
            .count(),
        2,
        "{records:#?}"
    );

    let edited = publisher
        .segment_edit(&init.id, id, Some("The whole exchange"), Some(&[(1, 5)]))
        .unwrap();
    assert_eq!(edited.description, "The whole exchange");
    assert_eq!(staged_lines(&f.publish_root, &init.id).len(), 5);
    let missing = publisher.segment_edit(&init.id, 9, None, None).unwrap_err();
    assert!(matches!(missing, PublishError::Input(_)), "{missing}");
    let reversed = publisher
        .segment_edit(&init.id, id, None, Some(&[(4, 2)]))
        .unwrap_err();
    assert!(reversed.to_string().contains("segment 1"), "{reversed}");

    let removed = publisher.segment_remove(&init.id, id).unwrap();
    assert_eq!(removed.id, id);
    let definition = publisher.definition(&init.id).unwrap();
    assert!(definition.segments.is_empty());
    assert_eq!(definition.next_segment_id, 2, "ids are not reused");
    assert_eq!(staged_lines(&f.publish_root, &init.id).len(), 5);
}

/// `redact_edit` changes the literal, the reason, or the mark of one
/// redaction and rebuilds; a literal that matches nothing changes
/// nothing.
#[test]
fn redact_edit_changes_one_redaction() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();
    let applied = publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("fucking", "language", true).unwrap()],
            "user:test",
        )
        .unwrap();
    let id = applied[0].id;
    assert!(staged_lines(&f.publish_root, &init.id)[0].contains("what the [REDACTED] hell"));

    let edited = publisher
        .redact_edit(&init.id, id, None, Some("privacy"), Some(false))
        .unwrap();
    assert_eq!(
        (edited.reason.as_str(), edited.mark, edited.instances),
        ("privacy", false, 1)
    );
    assert!(staged_lines(&f.publish_root, &init.id)[0].contains("what the hell is this?"));
    let definition = publisher.definition(&init.id).unwrap();
    assert_eq!(definition.text[0].reason, "privacy");

    let edited = publisher
        .redact_edit(&init.id, id, Some("garrett"), None, None)
        .unwrap();
    assert_eq!(edited.instances, 3);
    let line = staged_lines(&f.publish_root, &init.id).remove(0);
    assert!(line.contains("the fucking hell"), "{line}");
    assert!(
        line.contains(r#""cwd":"/home//proj""#),
        "removed without a mark: {line}"
    );

    let miss = publisher
        .redact_edit(&init.id, id, Some("nothing-like-this"), None, None)
        .unwrap_err();
    assert!(matches!(miss, PublishError::NoMatch(_)), "{miss}");
    assert_eq!(
        publisher.definition(&init.id).unwrap().text[0].literal,
        "garrett"
    );
    let bad = publisher
        .redact_edit(&init.id, id, None, Some("bogus"), None)
        .unwrap_err();
    assert!(bad.to_string().contains("bogus"), "{bad}");
    let missing = publisher
        .redact_edit(&init.id, 9, None, None, None)
        .unwrap_err();
    assert!(matches!(missing, PublishError::Input(_)), "{missing}");
}

/// The candidates are every text value of the kept lines that no
/// filter leaves out, in document order, each with the spans redacted
/// in it merged.
#[test]
fn candidates_are_the_disclosed_text_values_with_their_spans() {
    let f = fixture_with(SYSTEM_SESSION);
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher
        .init(&f.original, &["system".parse::<FilterSpec>().unwrap()])
        .unwrap();
    publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("/home/garrett", "system", true).unwrap()],
            "user:test",
        )
        .unwrap();
    publisher
        .segment_add(&init.id, "The lookup", &[(2, 4)], "user:test")
        .unwrap();

    let candidates = publisher.candidates(&init.id).unwrap();
    let keys: Vec<(u32, &str)> = candidates
        .iter()
        .map(|c| (c.line, c.pointer.as_str()))
        .collect();
    // Lines 1 and 5 are dropped by the segment; the command, the tool
    // result's content and stdout, and the reply remain
    assert_eq!(
        keys,
        [
            (2, "/message/content/0/input/command"),
            (3, "/message/content/0/content"),
            (3, "/toolUseResult/stdout"),
            (4, "/message/content/0/text"),
        ],
        "{candidates:#?}"
    );
    let command = &candidates[0];
    assert_eq!(command.text, "ls /home/garrett/proj");
    assert_eq!(command.spans, [(3, 15, true)]);
    assert!(candidates[1].spans.is_empty(), "{candidates:#?}");
    assert_eq!(candidates[3].spans, [(15, 27, true)]);
}

/// `apply` commits the staged session as a new session object with the
/// redacted content and `origin`, and clears the staging directory and
/// the definition.
#[test]
fn apply_commits_the_derived_session_and_clears_the_staging() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let filters = [
        "pii".parse::<FilterSpec>().unwrap(),
        "secret".parse::<FilterSpec>().unwrap(),
    ];
    let init = publisher.init(&f.original, &filters).unwrap();
    publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("fucking", "language", false).unwrap()],
            "user:test",
        )
        .unwrap();

    let commit = publisher.apply(&init.id).unwrap();

    let sessions = SessionStore::from(&f.store);
    let published = sessions.at_commit(&commit).unwrap();
    assert_eq!(published.id, init.id);
    assert_eq!(published.attrs.origin, init.origin);
    assert_eq!(sessions.iter().unwrap().count(), 2);
    let mut text = String::new();
    sessions
        .content(&commit)
        .open("session.jsonl")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text.lines().count(), 5);
    assert!(text.contains("[REDACTED]"), "{text}");
    assert!(!text.contains("sk-ant-api03"), "{text}");
    assert!(!text.contains("alice@example.com"), "{text}");
    assert!(!text.contains("fucking"), "{text}");

    assert!(!f.publish_root.join(&init.id).exists());
    assert!(!publisher.store().definition_file(&init.id).exists());
    assert!(publisher.store().list().unwrap().is_empty());
}

/// `apply` refuses a staged session edited by hand: added text, a
/// removed entry, or a change outside the text values. Nothing is
/// committed and the staging directory stays.
#[test]
fn apply_refuses_a_hand_edited_staged_session() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();
    let path = f
        .publish_root
        .join(&init.id)
        .join("files.d")
        .join("session.jsonl");
    let staged = fs::read_to_string(&path).unwrap();

    let refused = |edited: String, expect: &str| {
        fs::write(&path, edited).unwrap();
        let err = publisher.apply(&init.id).unwrap_err();
        assert!(matches!(err, PublishError::Refused(_)), "{err}");
        assert!(err.to_string().contains(expect), "{err}");
        assert_eq!(SessionStore::from(&f.store).iter().unwrap().count(), 1);
        assert!(f.publish_root.join(&init.id).is_dir());
    };
    refused(staged.replace("Hello, what", "Hello there, what"), "line 1");
    refused(
        staged.lines().skip(1).map(|l| format!("{l}\n")).collect(),
        "entries staged",
    );
    refused(
        staged.replace("\"uuid\":\"a1\"", "\"uuid\":\"a9\""),
        "outside its text values",
    );
    refused(
        staged.replace("\"cwd\":\"/home/garrett/proj\",", ""),
        "outside its text values",
    );

    fs::write(&path, &staged).unwrap();
    publisher.check(&init.id).unwrap();
}

#[test]
fn system_filter_leaves_out_system_values_and_drops_the_project_attribute() {
    let f = fixture_with(SYSTEM_SESSION);
    assert_eq!(
        f.original.attrs.project.as_deref(),
        Some("-home-garrett-proj")
    );
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let filters = [
        "system".parse::<FilterSpec>().unwrap(),
        "pii".parse::<FilterSpec>().unwrap(),
    ];
    let init = publisher.init(&f.original, &filters).unwrap();

    // The system filter has no patterns and no literal paths: what it
    // leaves out is derived at build time, not recorded as text
    let definition = publisher.definition(&init.id).unwrap();
    assert!(
        definition.text.iter().all(|t| t.author == "filter:pii"),
        "{definition:#?}"
    );

    let lines = staged_lines(&f.publish_root, &init.id);
    // A left-out value goes with its name, and the rest of the line is
    // as written
    assert!(
        lines[0].starts_with(
            r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","gitBranch":"main""#
        ),
        "{}",
        lines[0]
    );
    assert!(!lines[0].contains("cwd"), "{}", lines[0]);
    for field in ["home_dir", "live_cwd", "root", "host"] {
        assert!(
            !lines[4].contains(&format!("\"{field}\"")),
            "{field}: {}",
            lines[4]
        );
    }
    assert!(lines[4].contains(r#""branch":"main""#), "{}", lines[4]);
    // The username is not a system field; the pii literal redacts it
    assert!(
        lines[4].contains(r#""user_identity":"[REDACTED]""#),
        "{}",
        lines[4]
    );
    // The home directory is a pii literal everywhere it is text: in a
    // tool input, a tool result, and prose
    assert!(
        lines[1].contains(r#""command":"ls [REDACTED]/proj""#),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].contains(r#""stdout":"[REDACTED]/proj/a.txt""#),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].contains("at [REDACTED]/proj/a.txt"),
        "{}",
        lines[3]
    );

    let records = filtered(&f.publish_root, &init.id);
    let attrs: Vec<&FilteredRecord> = records
        .iter()
        .filter(|r| r.kind == FilteredKind::Attr)
        .collect();
    assert_eq!(attrs.len(), 1, "{records:#?}");
    assert_eq!(attrs[0].reference.as_deref(), Some("project"));
    assert_eq!(
        (attrs[0].reason.as_str(), attrs[0].author.as_str()),
        ("system", "filter:system")
    );
    // One value rule per path, on no line, however many values it
    // selects; no text record is on a left-out value
    let values: Vec<&FilteredRecord> = records
        .iter()
        .filter(|r| r.kind == FilteredKind::Value)
        .collect();
    assert_eq!(values.len(), 5, "{records:#?}");
    assert!(
        values
            .iter()
            .all(|r| r.line == 0 && r.reason == "system" && r.author == "filter:system"),
        "{records:#?}"
    );
    assert!(
        values
            .iter()
            .any(|r| r.reference.as_deref() == Some("$.cwd")),
        "{records:#?}"
    );
    assert!(
        !records
            .iter()
            .any(|r| r.kind == FilteredKind::Text && r.reference.as_deref() == Some("/cwd")),
        "{records:#?}"
    );
    let pii_on_5: Vec<&FilteredRecord> = records
        .iter()
        .filter(|r| r.line == 5 && r.author == "filter:pii")
        .collect();
    assert_eq!(pii_on_5.len(), 1, "{records:#?}");
    assert!(
        pii_on_5[0]
            .reference
            .as_deref()
            .unwrap()
            .ends_with("/user_identity"),
        "{records:#?}"
    );

    let attrs = staged_attrs(&f.publish_root, &init.id);
    assert!(attrs.get("project").is_none(), "{attrs}");

    // A literal found only inside a left-out value changes nothing,
    // and is reported as such
    let err = publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("git.corp.example", "system", true).unwrap()],
            "user:test",
        )
        .unwrap_err();
    assert!(
        matches!(err, PublishError::NoMatch(ref m) if m.contains("git.corp.example")),
        "{err}"
    );

    publisher.filter_remove(&init.id, "system").unwrap();
    let attrs = staged_attrs(&f.publish_root, &init.id);
    assert_eq!(attrs["project"], "-home-garrett-proj");
    let lines = staged_lines(&f.publish_root, &init.id);
    // The cwd is back, with the pii literal redacted inside it
    assert!(
        lines[0].contains(r#""cwd":"[REDACTED]/proj""#),
        "{}",
        lines[0]
    );
    let records = filtered(&f.publish_root, &init.id);
    assert!(
        !records
            .iter()
            .any(|r| r.kind == FilteredKind::Attr || r.kind == FilteredKind::Value),
        "{records:#?}"
    );
}

#[test]
fn a_literal_inside_a_tool_input_is_a_span() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root);
    let init = publisher.init(&f.original, &[]).unwrap();
    let key = "sk-ant-api03-PtYgjmUhBel31iEl2hpChYgCfrL1spNxnyVmihA-2O76UMFxFkM-R5Kjp1vRt_1fjORS-6ilI8ihN5KXSc7Tvo-hBKqFYAA";
    let command = "/message/content/0/input/command";

    let applied = publisher
        .redact(
            &init.id,
            &[
                RedactionInput::parse_text(key, "secret", true).unwrap(),
                RedactionInput::parse_text("api03", "secret", true).unwrap(),
            ],
            "user:test",
        )
        .unwrap();
    // The key: a span on line 2 and one in the command on line 3. The
    // substring: a span inside each of them
    assert_eq!(applied[0].instances, 2, "{applied:#?}");
    assert_eq!(applied[1].instances, 2, "{applied:#?}");

    let records = filtered(&f.publish_root, &init.id);
    let on_line = |line: u32| -> Vec<&FilteredRecord> {
        records
            .iter()
            .filter(|r| r.kind == FilteredKind::Text && r.line == line)
            .collect()
    };
    let spans = on_line(2);
    assert_eq!(
        spans.len(),
        2,
        "overlapping spans are two records: {records:#?}"
    );
    // Both record the one merged marker's position
    assert_eq!(
        (spans[0].span_start, spans[0].span_end),
        (spans[1].span_start, spans[1].span_end)
    );
    assert_eq!(
        spans[0].span_end.unwrap() - spans[0].span_start.unwrap(),
        10
    );
    let on_command = on_line(3);
    assert_eq!(on_command.len(), 2, "{records:#?}");
    assert!(
        on_command
            .iter()
            .all(|r| r.reference.as_deref() == Some(command)
                && (r.span_start, r.span_end) == (Some(5), Some(15))),
        "the merged marker sits after \"echo \": {records:#?}"
    );
    let lines = staged_lines(&f.publish_root, &init.id);
    assert!(
        lines[1].contains("Use key [REDACTED] for alice@example.com"),
        "one marker for the merged range: {}",
        lines[1]
    );
    assert_eq!(lines[1].matches("[REDACTED]").count(), 1, "{}", lines[1]);
    assert!(
        lines[2].contains(r#""command":"echo [REDACTED]""#),
        "{}",
        lines[2]
    );

    // A literal next to a redacted span is its own marker
    let applied = publisher
        .redact(
            &init.id,
            &[RedactionInput::parse_text("echo", "secret", true).unwrap()],
            "user:test",
        )
        .unwrap();
    assert_eq!(applied[0].instances, 1, "{applied:#?}");
    let lines = staged_lines(&f.publish_root, &init.id);
    assert!(
        lines[2].contains(r#""command":"[REDACTED] [REDACTED]""#),
        "{}",
        lines[2]
    );
}
