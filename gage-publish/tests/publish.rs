//! The publish engine over a Claude session added to a temporary
//! store: filters at init, the definition edited by hand and
//! refreshed, and the staged content and `changes.json` they
//! produce.

use std::fs;

use std::path::PathBuf;

use gage_publish::{
    Allow, FilterEntry, InitArgs, PublishDefinition, PublishError, Publisher, Refreshed, TextEntry,
    TitleEntry, candidate, format_lines,
};
use gage_registry::driver::DriverRegistry;
use gage_registry::filter::FilterRegistry;
use gage_session::SourceOptions;
use gage_store::{ChangeKind, ChangeRecord, SessionRecord, SessionStore, Store, parse_changes};
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

/// A session in which the harness sent a task notification as a user
/// turn between the prompt and the reply.
const TASK_NOTIFICATION_SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"Hello"}}"#,
    "\n",
    r#"{"type":"user","uuid":"u2","timestamp":"2025-01-01T00:00:01Z","promptSource":"system","origin":{"kind":"task-notification","producer":"session-task","runId":"r1"},"message":{"role":"user","content":"<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n</task-notification>"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:02Z","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Hi"}],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
    "\n",
);

/// A session whose environment snapshot names the scratchpad directory
/// and the OS version, and whose reply names a sibling of the
/// scratchpad.
const SNAPSHOT_SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"Hello"}}"#,
    "\n",
    r#"{"type":"attachment","uuid":"at1","timestamp":"2025-01-01T00:00:00Z","attachment":{"type":"environment_snapshot","snapshot":{"scratchpadDirectory":"/tmp/claude-1000/-home-garrett-proj/s1/scratchpad","platform":"linux","osVersion":"Linux 6.17.0-1032-oem"}}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"The log is at /tmp/claude-1000/-home-garrett-proj/s1/tasks/a.out"}],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
    "\n",
);

/// A session whose context entries, the request shape and blob, an
/// attachment, and a meta user entry, sit between the conversation and
/// an API request record.
const CONTEXT_SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/home/garrett/proj","message":{"role":"user","content":"Hello"}}"#,
    "\n",
    r#"{"type":"attachment","uuid":"at1","timestamp":"2025-01-01T00:00:00Z","attachment":{"type":"skill_listing","content":"- publish: Publish a session"}}"#,
    "\n",
    r#"{"type":"user","uuid":"u2","isMeta":true,"timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"Base directory for this skill: /home/garrett/.gage"}}"#,
    "\n",
    r#"{"type":"api-request-shape","id":"s1","shape":{"system":[{"type":"text","text":"You are Claude Code"}],"tools":[{"name":"Bash","description":"Run a command"}]}}"#,
    "\n",
    r#"{"type":"api-request-blob","id":"b1","message":{"role":"user","content":[{"type":"text","text":"<system-reminder>rules</system-reminder>"}]}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Hi"}],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
    "\n",
    r#"{"type":"api-request","id":"r1","params":{"model":"m"}}"#,
    "\n",
);

/// A session whose first user turn carries a leading IDE tag block
/// prepended to the real prompt, the kind the harness injects inside a
/// conversation entry.
const IDE_SESSION: &str = concat!(
    r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"/home/garrett/proj","message":{"role":"user","content":"<ide_opened_file>The user opened the file notes.md</ide_opened_file> please review the change"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:01Z","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"output_tokens":1}}}"#,
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
    let store = Store::open_with_cache_root(&store_path, &home.path().join("cache")).unwrap();
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
    fs::read_to_string(staged_file(root, id))
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

/// The staged `session.jsonl` of the pending publish `id`.
fn staged_file(root: &std::path::Path, id: &str) -> std::path::PathBuf {
    root.join(id)
        .join("session")
        .join("files.d")
        .join("session.jsonl")
}

fn staged_attrs(root: &std::path::Path, id: &str) -> Value {
    let path = root.join(id).join("session").join("attrs.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn staged_summary(root: &std::path::Path, id: &str) -> Value {
    staged_attrs(root, id)["summary"].clone()
}

fn changes(root: &std::path::Path, id: &str) -> Vec<ChangeRecord> {
    let path = root.join(id).join("session").join("changes.json");
    parse_changes(&fs::read(path).unwrap()).unwrap()
}

/// The registry over the filters this repository ships.
fn registry() -> FilterRegistry {
    let builtin = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../filters"));
    FilterRegistry::load_from_roots(&builtin, vec![builtin.clone()])
}

/// The filter `name` with no params.
fn filter(name: &str) -> FilterEntry {
    FilterEntry {
        name: name.into(),
        params: None,
    }
}

/// Stage `filters` over the fixture's original with no segment.
async fn stage(
    publisher: &Publisher<'_>,
    original: &SessionRecord,
    filters: &[FilterEntry],
) -> String {
    publisher
        .init(original, filters, &[], "session:test", &InitArgs::default())
        .await
        .unwrap()
        .id
}

fn write_definition(publisher: &Publisher<'_>, id: &str, definition: &PublishDefinition) {
    fs::write(
        publisher.staging().definition_file(id),
        definition.to_jsonc_text(None),
    )
    .unwrap();
}

/// Edit the definition of `id` the way a hand edit does: read the
/// file, change it, write it back, and refresh as `user:test`.
async fn edit(
    publisher: &Publisher<'_>,
    id: &str,
    change: impl FnOnce(&mut PublishDefinition),
) -> Refreshed {
    let mut definition = publisher.definition(id).unwrap();
    change(&mut definition);
    write_definition(publisher, id, &definition);
    publisher.refresh(id, "user:test").await.unwrap()
}

/// The instance count of the redaction whose literal is `literal`.
async fn instances_of(publisher: &Publisher<'_>, id: &str, literal: &str) -> usize {
    let definition = publisher.definition(id).unwrap();
    let counts = publisher.instances(id).await.unwrap();
    let index = definition
        .redact
        .iter()
        .position(|t| t.literal == literal)
        .unwrap_or_else(|| panic!("no redaction {literal:?}: {definition:#?}"));
    *counts.get(&index).unwrap()
}

#[tokio::test]
async fn init_runs_the_named_filters_and_records_their_instances() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let filters = [filter("pii"), filter("secret")];
    let id = stage(&publisher, &f.original, &filters).await;

    let definition = publisher.definition(&id).unwrap();
    assert_eq!(definition.session, f.original.id);
    let by_author = |author: &str| {
        definition
            .redact
            .iter()
            .filter(|t| t.author == author)
            .count()
    };
    // pii: the email pattern, plus the username typed literal
    assert_eq!(by_author("filter:pii"), 2, "{definition:#?}");
    // secret: one key, found by pattern in two places
    assert_eq!(by_author("filter:secret"), 1, "{definition:#?}");
    let counts = publisher.instances(&id).await.unwrap();
    assert_eq!(counts.len(), 3);
    assert!(counts.values().all(|n| *n > 0), "{counts:?}");

    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 5);
    assert!(lines[0].contains("Mail [REDACTED]"), "{}", lines[0]);
    // The home directory is the system filter's; pii redacts the
    // username inside it
    assert!(
        lines[0].contains(r#""cwd":"/home/[REDACTED]/proj""#),
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

    let records = changes(&f.publish_root, &id);
    assert!(
        records
            .iter()
            .any(|r| r.kind == ChangeKind::Text && r.author == "filter:pii"),
        "{records:#?}"
    );
    // The span in the command sits after "echo " in the published value
    assert!(
        records.iter().any(|r| r.kind == ChangeKind::Text
            && r.reference.as_deref() == Some("/message/content/0/input/command")
            && (r.span_start, r.span_end) == (Some(5), Some(15))),
        "{records:#?}"
    );
}

/// The file `init` writes names the session with its title, records
/// the init arguments, and reads back; a refresh rewrites it with the
/// title kept.
#[tokio::test]
async fn init_writes_the_annotated_file_with_the_title_and_init_args() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let init = InitArgs {
        groups: vec!["public".into(), "reading".into()],
        notes: Some("For the blog".into()),
    };
    let id = publisher
        .init(&f.original, &[], &[], "session:test", &init)
        .await
        .unwrap()
        .id;
    let path = publisher.staging().definition_file(&id);
    assert!(path.extension().is_some_and(|e| e == "jsonc"), "{path:?}");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("// A pending publish."), "{text}");
    assert!(
        text.contains(&format!("\"session\": \"{}\", // title: ", f.original.id)),
        "{text}"
    );
    assert!(
        text.contains(r#""init": {"groups":["public","reading"],"notes":"For the blog"}"#),
        "{text}"
    );
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(definition.init, init);
    assert!(definition.filters.is_empty() && definition.redact.is_empty());

    publisher.refresh(&id, "user:test").await.unwrap();
    let again = fs::read_to_string(&path).unwrap();
    assert_eq!(
        again, text,
        "a refresh without a change rewrites the same file"
    );
}

/// A redaction row added by hand takes effect everywhere its text
/// appears, a row without an author takes the refresh's, a row that
/// matches nothing is reported by position and kept, and a changed
/// `Publisher::edit` sets the title and adds redactions in one
/// rebuild, fills the author it is given into a row left without one,
/// keeps the marker when mark is true, and skips an exact duplicate
/// on a later call.
#[tokio::test]
async fn edit_sets_the_title_and_adds_a_redaction_once() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[]).await;

    let row = TextEntry {
        literal: "garrett".into(),
        reason: "pii".into(),
        author: String::new(),
        mark: true,
    };
    let refreshed = publisher
        .edit(
            &id,
            "session:s",
            Some("A shared title"),
            std::slice::from_ref(&row),
        )
        .await
        .unwrap();
    assert_eq!(refreshed.instances, 3);

    let definition = publisher.definition(&id).unwrap();
    let title = definition.title.as_ref().unwrap();
    assert_eq!(title.text, "A shared title");
    assert_eq!(title.author, "session:s");
    assert_eq!(definition.redact.len(), 1);
    assert_eq!(definition.redact[0].author, "session:s");
    assert!(definition.redact[0].mark);

    let lines = staged_lines(&f.publish_root, &id);
    assert!(
        lines[0].contains(r#""cwd":"/home/[REDACTED]/proj""#),
        "{}",
        lines[0]
    );

    let filled = TextEntry {
        literal: "garrett".into(),
        reason: "pii".into(),
        author: "session:s".into(),
        mark: true,
    };
    publisher
        .edit(&id, "session:s", None, std::slice::from_ref(&filled))
        .await
        .unwrap();
    assert_eq!(publisher.definition(&id).unwrap().redact.len(), 1);
}

/// reason or mark takes effect.
#[tokio::test]
async fn refresh_applies_redact_rows_and_reports_an_unmatched_row() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[]).await;
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);

    let refreshed = edit(&publisher, &id, |d| {
        d.push_redact("garrett", "pii", "user:hand", true);
        d.push_redact("fucking", "language", "", false);
        d.push_redact("nothing-like-this", "secret", "user:hand", true);
    })
    .await;
    // "garrett" is in cwd on line 1 and in the username and home_dir on
    // line 5
    assert_eq!(
        refreshed,
        Refreshed {
            instances: 4,
            authored: 1,
            unmatched: vec![(3, "nothing-like-this".into())],
        }
    );
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(definition.redact.len(), 3);
    assert_eq!(definition.redact[1].author, "user:test");
    assert_eq!(instances_of(&publisher, &id, "garrett").await, 3);
    assert_eq!(instances_of(&publisher, &id, "fucking").await, 1);
    assert_eq!(instances_of(&publisher, &id, "nothing-like-this").await, 0);

    let lines = staged_lines(&f.publish_root, &id);
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

    let records = changes(&f.publish_root, &id);
    assert!(records.iter().all(|r| r.kind == ChangeKind::Text));
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

    // A changed reason and mark take effect; a changed literal is a
    // new match
    edit(&publisher, &id, |d| {
        d.redact.truncate(1);
        d.redact[0].reason = "privacy".into();
        d.redact[0].mark = false;
    })
    .await;
    let line = staged_lines(&f.publish_root, &id).remove(0);
    assert!(line.contains("the fucking hell"), "{line}");
    assert!(
        line.contains(r#""cwd":"/home//proj""#),
        "removed without a mark: {line}"
    );
    let records = changes(&f.publish_root, &id);
    assert!(
        records
            .iter()
            .all(|r| r.reason.as_deref() == Some("privacy")),
        "{records:#?}"
    );
}

/// A filter row records the params the filter ran under, and a
/// filter the registry does not know is refused at init and at
/// refresh, with the file left as written.
#[tokio::test]
async fn params_are_recorded_and_an_unknown_filter_is_refused() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let err = publisher
        .init(
            &f.original,
            &[filter("piii")],
            &[],
            "session:test",
            &InitArgs::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PublishError::Input(_)), "{err}");
    assert!(err.to_string().contains("piii"), "{err}");
    assert!(publisher.staging().list().unwrap().is_empty());

    let pii = FilterEntry {
        name: "pii".into(),
        params: serde_json::json!({ "prompt": "Watch for street addresses." })
            .as_object()
            .cloned(),
    };
    let id = stage(&publisher, &f.original, &[pii, filter("secret")]).await;
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(definition.filters[0].name, "pii");
    assert_eq!(
        definition.filters[0].params.as_ref().unwrap()["prompt"],
        "Watch for street addresses."
    );
    assert_eq!(definition.filters[1], filter("secret"));
    let text = fs::read_to_string(publisher.staging().definition_file(&id)).unwrap();
    assert!(
        text.contains(r#"    ["pii", {"prompt":"Watch for street addresses."}],"#),
        "{text}"
    );
    assert!(text.contains("    \"secret\",\n"), "{text}");
    let instructions = publisher.instructions(&id).await.unwrap();
    assert!(
        instructions.contains("Watch for street addresses."),
        "{instructions}"
    );

    let mut definition = publisher.definition(&id).unwrap();
    definition.filters[0].name = "piii".into();
    write_definition(&publisher, &id, &definition);
    let err = publisher.refresh(&id, "user:test").await.unwrap_err();
    assert!(err.to_string().contains("piii"), "{err}");
    let text = fs::read_to_string(publisher.staging().definition_file(&id)).unwrap();
    assert!(
        text.contains("\"piii\""),
        "an invalid file is left as written"
    );
}

/// The `harness` filter leaves out the entries the harness kept about
/// the run: the API request on line 5 goes, with one line record under
/// the filter, and the descriptive fields of the entries that stay go
/// with it.
#[tokio::test]
async fn harness_filter_leaves_out_run_records_and_descriptive_fields() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("harness")]).await;
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert!(
        !lines.iter().any(|l| l.contains("api-request")),
        "{lines:#?}"
    );
    let records = changes(&f.publish_root, &id);
    let dropped: Vec<(u32, Option<u32>, Option<&str>, &str)> = records
        .iter()
        .filter(|r| r.kind == ChangeKind::Entry)
        .map(|r| (r.line, r.line_end, r.reason.as_deref(), r.author.as_str()))
        .collect();
    assert_eq!(dropped, [(5, Some(5), Some("scope"), "filter:harness")]);
    // The token usage is a descriptive field and goes; the working
    // directory is the system filter's and stays
    assert!(!lines[1].contains("\"usage\""), "{}", lines[1]);
    assert!(
        lines[0].contains("\"cwd\":\"/home/garrett/proj\""),
        "{}",
        lines[0]
    );
    let values: Vec<&str> = records
        .iter()
        .filter(|r| r.kind == ChangeKind::Value)
        .map(|r| r.reference.as_deref().unwrap())
        .collect();
    assert!(values.contains(&"$.message.usage"), "{values:?}");
    assert!(
        records
            .iter()
            .filter(|r| r.kind == ChangeKind::Value)
            .all(|r| r.reason.as_deref() == Some("scope") && r.author == "filter:harness")
    );
    let summary = staged_summary(&f.publish_root, &id);
    assert_eq!(summary["line_count"], 4, "{summary}");
    publisher.check(&id).await.unwrap();

    // A literal only on a dropped line matches nothing
    let refreshed = edit(&publisher, &id, |d| {
        d.push_redact("home_dir", "system", "user:test", true);
    })
    .await;
    assert_eq!(refreshed.unmatched, [(1, "home_dir".to_string())]);
}

/// The `harness` filter removes a leading IDE tag block from a user
/// turn as an unmarked span, leaving the real prompt, and records the
/// removal as a `text` change under the filter.
#[tokio::test]
async fn harness_filter_removes_an_ide_tag_block_from_a_user_turn() {
    let f = fixture_with(IDE_SESSION);
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("harness")]).await;
    let lines = staged_lines(&f.publish_root, &id);
    assert!(
        !lines[0].contains("ide_opened_file"),
        "the IDE block should be gone: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("please review the change"),
        "the prompt should stay: {}",
        lines[0]
    );
    assert!(
        !lines[0].contains("[REDACTED]"),
        "the removal is unmarked: {}",
        lines[0]
    );
    let records = changes(&f.publish_root, &id);
    assert!(
        records.iter().any(|r| r.kind == ChangeKind::Text
            && r.reason.as_deref() == Some("scope")
            && r.author == "filter:harness"),
        "{records:#?}"
    );
    publisher.check(&id).await.unwrap();
}

/// The `harness` filter leaves out what the harness put in the model's
/// context and the records it kept about the run: the attachment, the
/// meta user entry, and the request shape and blob go as one run of
/// lines, the API request record goes as another, and the token usage
/// of the reply that stays goes as a value.
#[tokio::test]
async fn harness_filter_leaves_out_context_and_record_entries() {
    let f = fixture_with(CONTEXT_SESSION);
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("harness")]).await;
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(lines[0].contains("\"content\":\"Hello\""), "{}", lines[0]);
    assert!(lines[1].contains("\"text\":\"Hi\""), "{}", lines[1]);
    assert!(!lines[1].contains("\"usage\""), "{}", lines[1]);
    let records = changes(&f.publish_root, &id);
    let dropped: Vec<(u32, Option<u32>, Option<&str>, &str)> = records
        .iter()
        .filter(|r| r.kind == ChangeKind::Entry)
        .map(|r| (r.line, r.line_end, r.reason.as_deref(), r.author.as_str()))
        .collect();
    assert_eq!(
        dropped,
        [
            (2, Some(5), Some("scope"), "filter:harness"),
            (7, Some(7), Some("scope"), "filter:harness"),
        ]
    );
    assert!(
        records
            .iter()
            .all(|r| matches!(r.kind, ChangeKind::Entry | ChangeKind::Value)),
        "{records:#?}"
    );
    publisher.check(&id).await.unwrap();
}

/// The `harness` filter leaves out a task notification the harness
/// sent as a user turn, as context, rather than leaving an empty user
/// entry behind.
#[tokio::test]
async fn harness_filter_leaves_out_a_task_notification_user_turn() {
    let f = fixture_with(TASK_NOTIFICATION_SESSION);
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("harness")]).await;
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(lines[0].contains("\"content\":\"Hello\""), "{}", lines[0]);
    assert!(lines[1].contains("\"text\":\"Hi\""), "{}", lines[1]);
    let records = changes(&f.publish_root, &id);
    let dropped: Vec<(u32, Option<u32>, Option<&str>, &str)> = records
        .iter()
        .filter(|r| r.kind == ChangeKind::Entry)
        .map(|r| (r.line, r.line_end, r.reason.as_deref(), r.author.as_str()))
        .collect();
    assert_eq!(dropped, [(2, Some(2), Some("scope"), "filter:harness")]);
    publisher.check(&id).await.unwrap();
}

/// The `system` filter redacts the scratchpad directory's parent, so a
/// sibling of the scratchpad is covered, and a filter's own row that
/// matches nothing because the `harness` filter left its source entry
/// out is not reported, while a hand-written row that matches nothing
/// still is.
#[tokio::test]
async fn system_filter_covers_the_scratchpad_parent_and_a_filter_row_miss_is_not_reported() {
    let f = fixture_with(SNAPSHOT_SESSION);
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(
        &publisher,
        &f.original,
        &[filter("system"), filter("harness")],
    )
    .await;
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(
        lines[1].contains("The log is at [REDACTED]/tasks/a.out"),
        "{}",
        lines[1]
    );
    let definition = publisher.definition(&id).unwrap();
    let system_rows: Vec<&str> = definition
        .redact
        .iter()
        .filter(|t| t.author == "filter:system")
        .map(|t| t.literal.as_str())
        .collect();
    assert!(
        system_rows.contains(&"Linux 6.17.0-1032-oem"),
        "{system_rows:?}"
    );
    assert!(
        system_rows.contains(&"/tmp/claude-1000/-home-garrett-proj/s1"),
        "{system_rows:?}"
    );

    let refreshed = edit(&publisher, &id, |d| {
        d.push_redact("nothing-like-this", "custom", "user:test", true);
    })
    .await;
    let definition = publisher.definition(&id).unwrap();
    let position = definition
        .redact
        .iter()
        .position(|t| t.literal == "nothing-like-this")
        .unwrap()
        + 1;
    assert_eq!(
        refreshed.unmatched,
        [(position, "nothing-like-this".to_string())]
    );
    publisher.check(&id).await.unwrap();
}

/// `init` rejects a filter given twice.
#[tokio::test]
async fn init_rejects_a_duplicate_filter() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let pii = filter("pii");
    let dup = publisher
        .init(
            &f.original,
            &[pii.clone(), pii],
            &[],
            "session:test",
            &InitArgs::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(dup, PublishError::Input(_)), "{dup}");
    assert!(publisher.staging().list().unwrap().is_empty());
}

/// A filter added to the file runs on refresh and keeps everything
/// else; a filter removed from the file takes its redactions with it
/// and leaves the user's; a filter listed twice is rejected.
#[tokio::test]
async fn refresh_runs_an_added_filter_and_drops_a_removed_filters_rows() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("secret")]).await;
    edit(&publisher, &id, |d| {
        d.push_redact("fucking", "language", "user:test", true);
    })
    .await;

    edit(&publisher, &id, |d| {
        d.filters.push(filter("pii"));
    })
    .await;
    let authors = |d: &PublishDefinition| -> Vec<String> {
        d.redact.iter().map(|t| t.author.clone()).collect()
    };
    let definition = publisher.definition(&id).unwrap();
    let added = authors(&definition);
    assert!(added.contains(&"user:test".to_string()), "{added:?}");
    assert!(added.contains(&"filter:pii".to_string()), "{added:?}");
    assert!(added.contains(&"filter:secret".to_string()), "{added:?}");
    assert_eq!(definition.filters.len(), 2);

    let mut dup = definition.clone();
    dup.filters.push(dup.filters[0].clone());
    write_definition(&publisher, &id, &dup);
    let err = publisher.refresh(&id, "user:test").await.unwrap_err();
    assert!(err.to_string().contains("more than once"), "{err}");
    write_definition(&publisher, &id, &definition);

    edit(&publisher, &id, |d| {
        d.filters.retain(|f| f.name != "secret");
    })
    .await;
    let definition = publisher.definition(&id).unwrap();
    let left = authors(&definition);
    assert!(left.contains(&"user:test".to_string()), "{left:?}");
    assert!(left.contains(&"filter:pii".to_string()), "{left:?}");
    assert!(!left.contains(&"filter:secret".to_string()), "{left:?}");
    assert_eq!(definition.named_filters(), vec!["pii".to_string()]);
    let lines = staged_lines(&f.publish_root, &id);
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

/// A refresh re-runs every named filter: a filter's row stays where it
/// is, a filter's row deleted by hand comes back at the end, and the
/// user's rows keep their place.
#[tokio::test]
async fn refresh_reruns_filters_and_keeps_rows_in_place() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let filters = [filter("pii"), filter("secret")];
    let id = stage(&publisher, &f.original, &filters).await;
    let before = publisher.definition(&id).unwrap();
    assert_eq!(before.redact.len(), 3);
    let literals = |d: &PublishDefinition| -> Vec<(String, String)> {
        d.redact
            .iter()
            .map(|t| (t.literal.clone(), t.author.clone()))
            .collect()
    };

    edit(&publisher, &id, |d| {
        d.redact.insert(
            1,
            gage_publish::TextEntry {
                literal: "fucking".into(),
                reason: "language".into(),
                author: "user:test".into(),
                mark: true,
            },
        );
    })
    .await;
    let after = publisher.definition(&id).unwrap();
    let mut expected = literals(&before);
    expected.insert(1, ("fucking".into(), "user:test".into()));
    assert_eq!(literals(&after), expected, "{after:#?}");

    let key = before
        .redact
        .iter()
        .find(|t| t.author == "filter:secret")
        .unwrap()
        .literal
        .clone();
    edit(&publisher, &id, |d| {
        d.redact.retain(|t| t.literal != key);
    })
    .await;
    let back = publisher.definition(&id).unwrap();
    assert_eq!(back.redact.len(), 4, "{back:#?}");
    assert_eq!(back.redact[3].literal, key, "the filter's row came back");
    assert!(staged_lines(&f.publish_root, &id)[1].contains("Use key [REDACTED]"));
}

/// The title stays the original's: the driver takes it from the first
/// prompt, and redacting that prompt does not retitle the publication.
/// Every summary field, the title included, is the driver's reading
/// of the staged content: redacting the first prompt retitles the
/// publication.
#[tokio::test]
async fn the_staged_summary_is_computed_from_the_staged_content() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[]).await;

    let before = staged_summary(&f.publish_root, &id);
    assert!(
        before["title"]
            .as_str()
            .is_some_and(|t| t.contains("Hello")),
        "{before}"
    );

    edit(&publisher, &id, |d| {
        d.push_redact("Hello", "privacy", "user:test", true);
    })
    .await;
    let after = staged_summary(&f.publish_root, &id);
    assert!(
        after["title"]
            .as_str()
            .is_some_and(|t| t.starts_with("[REDACTED], what")),
        "{after}"
    );
    assert_eq!(after["line_count"], 5, "{after}");
    assert!(
        staged_lines(&f.publish_root, &id)[0].contains("[REDACTED], what"),
        "the prompt itself is redacted"
    );
}

/// A title in the definition is written as the driver's title entry
/// after the last staged line, recorded as a title change at that
/// line with the title's author, and read back as the summary title.
/// A title written by hand without an author takes the refresh's. The
/// staged session passes the removal-only check with the entry in
/// place and is refused with it altered.
#[tokio::test]
async fn a_title_in_the_definition_adds_an_entry_after_the_last_line() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("pii")]).await;
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);
    let before = publisher.changes(&id).await.unwrap();

    edit(&publisher, &id, |d| {
        d.title = Some(TitleEntry {
            text: "Ducks \"quoted\"".into(),
            author: "user:test".into(),
        });
    })
    .await;
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 6);
    assert_eq!(
        lines[5],
        r#"{"type":"custom-title","customTitle":"Ducks \"quoted\""}"#
    );
    let records = changes(&f.publish_root, &id);
    let titles: Vec<&ChangeRecord> = records
        .iter()
        .filter(|r| r.kind == ChangeKind::Title)
        .collect();
    assert_eq!(titles.len(), 1, "{records:#?}");
    assert_eq!(
        (
            titles[0].line,
            titles[0].reason.as_deref(),
            titles[0].author.as_str()
        ),
        (6, None, "user:test")
    );
    assert_eq!(records.last().unwrap().kind, ChangeKind::Title);
    let summary = staged_summary(&f.publish_root, &id);
    assert_eq!(summary["title"], "Ducks \"quoted\"", "{summary}");
    assert_eq!(summary["line_count"], 6, "{summary}");
    publisher.check(&id).await.unwrap();
    assert_eq!(
        publisher.changes(&id).await.unwrap(),
        before + 1,
        "the title counts as one change"
    );

    // A title written by hand without an author takes the refresh's
    let path = publisher.staging().definition_file(&id);
    fs::write(
        &path,
        format!(
            "{{\n  \"session\": \"{}\",\n  \"title\": [\"Geese\"],\n  \"filters\": [\"pii\"],\n}}\n",
            f.original.id
        ),
    )
    .unwrap();
    let refreshed = publisher.refresh(&id, "session:hand").await.unwrap();
    assert_eq!(refreshed.authored, 1);
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(
        definition.title,
        Some(TitleEntry {
            text: "Geese".into(),
            author: "session:hand".into(),
        })
    );
    assert_eq!(
        staged_lines(&f.publish_root, &id)[5],
        r#"{"type":"custom-title","customTitle":"Geese"}"#
    );
    assert_eq!(staged_summary(&f.publish_root, &id)["title"], "Geese");

    // The title entry is checked as written: an altered one is refused
    let staged = staged_file(&f.publish_root, &id);
    let text = fs::read_to_string(&staged).unwrap();
    fs::write(&staged, text.replace("Geese", "Swans")).unwrap();
    let err = publisher.apply(&id, false).await.unwrap_err();
    assert!(
        matches!(err, PublishError::Refused(ref m) if m.contains("line 6 is not the title entry")),
        "{err}"
    );
    fs::write(&staged, text).unwrap();

    let commit = publisher.apply(&id, false).await.unwrap();
    let published = SessionStore::from(&f.store).at_commit(&commit).unwrap();
    assert_eq!(published.attrs.summary.title.as_deref(), Some("Geese"));
    let committed = SessionStore::from(&f.store).changes(&commit).unwrap();
    assert_eq!(committed.last().unwrap().kind, ChangeKind::Title);
}

/// A user's row deleted by hand is gone; a filter's row deleted by
/// hand comes back unless its literal is on the allow list, where a
/// refresh leaves it out.
#[tokio::test]
async fn an_allowed_literal_is_not_recorded_again() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("secret")]).await;
    edit(&publisher, &id, |d| {
        d.push_redact("garrett", "pii", "user:test", true);
    })
    .await;
    let cwd = |publish_root: &std::path::Path| {
        let line = staged_lines(publish_root, &id).remove(0);
        line.contains("/home/[REDACTED]/proj")
    };
    assert!(cwd(&f.publish_root));

    edit(&publisher, &id, |d| {
        d.redact.retain(|t| t.literal != "garrett");
    })
    .await;
    assert!(!cwd(&f.publish_root), "the username is back");
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(definition.redact.len(), 1, "{definition:#?}");
    let key = definition.redact[0].literal.clone();
    assert_eq!(definition.redact[0].author, "filter:secret");

    edit(&publisher, &id, |d| {
        d.redact.clear();
    })
    .await;
    let definition = publisher.definition(&id).unwrap();
    assert_eq!(
        definition.redact.len(),
        1,
        "the key came back: {definition:#?}"
    );
    assert!(!staged_lines(&f.publish_root, &id)[1].contains("sk-ant-api03"));

    edit(&publisher, &id, |d| {
        d.redact.clear();
        d.allow.push(Allow {
            literal: key.to_uppercase(),
            author: "user:test".into(),
        });
    })
    .await;
    let definition = publisher.definition(&id).unwrap();
    assert!(
        definition.redact.is_empty(),
        "the allowed key is not recorded again: {definition:#?}"
    );
    assert!(staged_lines(&f.publish_root, &id)[1].contains("sk-ant-api03"));
}

/// `refresh` re-applies a file edited by hand, comments and trailing
/// commas included: a text redaction takes effect, a segment drops
/// every line outside it and the staged summary follows, and an
/// invalid file is rejected without touching the staged session. A
/// file naming another session is refused.
#[tokio::test]
async fn refresh_reapplies_a_hand_edited_file() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[]).await;
    let path = publisher.staging().definition_file(&id);
    let before = staged_summary(&f.publish_root, &id);
    assert_eq!(before["line_count"], 5, "{before}");
    assert_eq!(before["output_tokens"], 10, "{before}");

    let file = |body: &str| {
        format!(
            "// edited by hand\n{{\n  \"session\": \"{}\", // the original\n{body}\n}}\n",
            f.original.id
        )
    };
    fs::write(
        &path,
        file("  \"redact\": [\n    [\"garrett\", \"pii\", true], // no author\n  ],"),
    )
    .unwrap();
    let refreshed = publisher.refresh(&id, "session:hand").await.unwrap();
    assert_eq!(refreshed.instances, 3);
    assert_eq!(refreshed.authored, 1);
    assert!(staged_lines(&f.publish_root, &id)[0].contains("/home/[REDACTED]/proj"));
    let rewritten = fs::read_to_string(&path).unwrap();
    assert!(
        rewritten.contains(r#"["garrett", "pii", "session:hand", true]"#),
        "the author is filled in: {rewritten}"
    );
    assert!(
        !rewritten.contains("edited by hand"),
        "the file is regenerated: {rewritten}"
    );

    // A segment keeps lines 2-3 and drops the rest, with a line record
    // per run attributed to the segment's author
    fs::write(
        &path,
        file(
            "  \"segments\": [[\"2-3\", \"The reply\", \"session:hand\"]],\n  \"redact\": [[\"garrett\", \"pii\", \"user:hand\", true]],",
        ),
    )
    .unwrap();
    let refreshed = publisher.refresh(&id, "session:hand").await.unwrap();
    assert_eq!(
        refreshed.instances, 0,
        "the username is only on dropped lines"
    );
    assert_eq!(refreshed.unmatched, [(1, "garrett".to_string())]);
    let lines = staged_lines(&f.publish_root, &id);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(lines[0].contains(r#""uuid":"a1""#), "{}", lines[0]);
    assert!(lines[1].contains(r#""uuid":"a2""#), "{}", lines[1]);
    let records = changes(&f.publish_root, &id);
    let dropped: Vec<(u32, Option<u32>, Option<&str>, &str)> = records
        .iter()
        .filter(|r| r.kind == ChangeKind::Entry)
        .map(|r| (r.line, r.line_end, r.reason.as_deref(), r.author.as_str()))
        .collect();
    assert_eq!(
        dropped,
        [
            (1, Some(1), Some("scope"), "session:hand"),
            (4, Some(5), Some("scope"), "session:hand"),
        ]
    );
    let after = staged_summary(&f.publish_root, &id);
    assert_eq!(after["line_count"], 2, "{after}");
    assert_eq!(after["output_tokens"], 10, "{after}");
    assert!(
        after["message_count"].as_u64().unwrap() < before["message_count"].as_u64().unwrap(),
        "{before} -> {after}"
    );
    // The kept lines hold no prompt, so the content yields no title
    assert_eq!(after["title"], Value::Null, "{after}");
    publisher.check(&id).await.unwrap();

    fs::write(
        &path,
        file("  \"redact\": [[\"x\", \"bogus\", \"user:hand\", true]],"),
    )
    .unwrap();
    let err = publisher.refresh(&id, "session:hand").await.unwrap_err();
    assert!(err.to_string().starts_with("4:14: "), "{err}");
    assert!(err.to_string().contains("bogus"), "{err}");
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 2);

    fs::write(&path, file("  \"segments\": [[\"9-2\", \"d\", \"u\"]],")).unwrap();
    let err = publisher.refresh(&id, "session:hand").await.unwrap_err();
    assert!(err.to_string().starts_with("4:16: "), "{err}");
    assert!(err.to_string().contains("9-2"), "{err}");

    fs::write(
        &path,
        "{\n  \"session\": \"other\",\n  \"filters\": [\"pii\"],\n}\n",
    )
    .unwrap();
    let err = publisher.refresh(&id, "session:hand").await.unwrap_err();
    assert!(
        err.to_string().starts_with("2:3: session \"other\""),
        "{err}"
    );
    let err = publisher.apply(&id, true).await.unwrap_err();
    assert!(matches!(err, PublishError::Definition(_)), "{err}");
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 2);
}

/// A segment without lines drops nothing; once it has lines, every
/// line outside it is dropped; a changed or removed segment rebuilds.
#[tokio::test]
async fn segments_drop_the_lines_outside_them() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[]).await;

    edit(&publisher, &id, |d| {
        d.push_segment("The reply", &[], "session:s");
    })
    .await;
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);

    edit(&publisher, &id, |d| d.segments[0].lines = vec![(2, 3)]).await;
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 2);
    let records = changes(&f.publish_root, &id);
    assert_eq!(
        records
            .iter()
            .filter(|r| r.kind == ChangeKind::Entry)
            .count(),
        2,
        "{records:#?}"
    );

    edit(&publisher, &id, |d| {
        d.segments[0].description = "The whole exchange".into();
        d.segments[0].lines = vec![(1, 5)];
    })
    .await;
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);
    assert_eq!(
        publisher.definition(&id).unwrap().segments[0].description,
        "The whole exchange"
    );

    edit(&publisher, &id, |d| d.segments.clear()).await;
    assert!(publisher.definition(&id).unwrap().segments.is_empty());
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);
}

/// The candidates are every text value of the kept lines that no
/// filter leaves out, in document order, each with the spans redacted
/// in it merged.
#[tokio::test]
async fn candidates_are_the_disclosed_text_values_with_their_spans() {
    let f = fixture_with(SYSTEM_SESSION);
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("system")]).await;
    edit(&publisher, &id, |d| {
        d.push_redact("/home/garrett", "system", "user:test", true);
        d.push_segment("The lookup", &[(2, 4)], "user:test");
    })
    .await;

    let candidates = publisher.candidates(&id).await.unwrap();
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
    // The system filter's working-directory literal and the home
    // directory redaction merge into one span
    let command = &candidates[0];
    assert_eq!(command.text, "ls /home/garrett/proj");
    assert_eq!(command.spans, [(3, 20, true)]);
    assert!(candidates[1].spans.is_empty(), "{candidates:#?}");
    assert_eq!(candidates[3].spans, [(15, 32, true)]);
}

/// `apply` commits the staged session as a new session object with the
/// redacted content and `origin`, and clears the staging directory and
/// the definition.
#[tokio::test]
async fn apply_commits_the_derived_session_and_clears_the_staging() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let filters = [filter("pii"), filter("secret")];
    let init = publisher
        .init(
            &f.original,
            &filters,
            &[],
            "session:test",
            &InitArgs::default(),
        )
        .await
        .unwrap();
    edit(&publisher, &init.id, |d| {
        d.push_redact("fucking", "language", "user:test", false);
    })
    .await;

    let commit = publisher.apply(&init.id, false).await.unwrap();

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
    assert!(!publisher.staging().definition_file(&init.id).exists());
    assert!(publisher.staging().list().unwrap().is_empty());
}

/// `apply` refuses a staged session edited by hand: added text, a
/// removed entry, or a change outside the text values. Nothing is
/// committed and the staging directory stays.
#[tokio::test]
async fn apply_refuses_a_hand_edited_staged_session() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let filters = [filter("language")];
    let id = stage(&publisher, &f.original, &filters).await;
    let path = staged_file(&f.publish_root, &id);
    let staged = fs::read_to_string(&path).unwrap();

    let refused: [(String, &str); 4] = [
        (staged.replace("Hello, what", "Hello there, what"), "line 1"),
        (
            staged.lines().skip(1).map(|l| format!("{l}\n")).collect(),
            "entries staged",
        ),
        (
            staged.replace("\"uuid\":\"a1\"", "\"uuid\":\"a9\""),
            "outside its text values",
        ),
        (
            staged.replace("\"cwd\":\"/home/garrett/proj\",", ""),
            "outside its text values",
        ),
    ];
    for (edited, expect) in refused {
        fs::write(&path, edited).unwrap();
        let err = publisher.apply(&id, true).await.unwrap_err();
        assert!(matches!(err, PublishError::Refused(_)), "{err}");
        assert!(err.to_string().contains(expect), "{err}");
        assert_eq!(SessionStore::from(&f.store).iter().unwrap().count(), 1);
        assert!(f.publish_root.join(&id).is_dir());
    }

    fs::write(&path, &staged).unwrap();
    publisher.check(&id).await.unwrap();
}

/// The candidate text is each distinct disclosed value once, in order
/// of first occurrence, divided into splits that `--split-files`
/// writes one per file.
#[tokio::test]
async fn candidate_texts_are_each_distinct_value_once_in_splits() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[]).await;
    let candidates = publisher.candidates(&id).await.unwrap();
    let texts = publisher.candidate_texts(&id).await.unwrap();
    // "ok" is the tool result's content and the harness's stdout copy
    assert_eq!(candidates.iter().filter(|c| c.text == "ok").count(), 2);
    assert_eq!(texts.iter().filter(|t| *t == "ok").count(), 1);
    let mut expected: Vec<&str> = Vec::new();
    for c in &candidates {
        if !expected.contains(&c.text.as_str()) {
            expected.push(&c.text);
        }
    }
    assert_eq!(texts, expected);
    assert!(
        texts.contains(&"Hello, what the fucking hell is this? Mail alice@example.com".to_string())
    );

    let total: usize = texts.iter().map(|t| candidate::encode_line(t).len()).sum();
    let splits = candidate::splits(&texts, total / 2);
    assert!(splits.len() >= 2, "{splits:#?}");
    assert_eq!(splits.iter().flatten().count(), texts.len());
    let dir = f.publish_root.join("candidate");
    let paths = candidate::write_files(&dir, &splits).unwrap();
    assert_eq!(paths.len(), splits.len());
    let back: Vec<String> = paths
        .iter()
        .flat_map(|p| {
            fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str::<String>(l).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(back, texts);
}

/// `init` adds a segment per description, without lines, attributed
/// to the author; the staged session is unchanged until a segment has
/// lines.
#[tokio::test]
async fn init_adds_segments_by_description() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let segments = ["The question".to_string(), "The reply".to_string()];
    let id = publisher
        .init(
            &f.original,
            &[],
            &segments,
            "session:s",
            &InitArgs::default(),
        )
        .await
        .unwrap()
        .id;
    let definition = publisher.definition(&id).unwrap();
    let added: Vec<(&str, String, &str)> = definition
        .segments
        .iter()
        .map(|s| {
            (
                s.description.as_str(),
                format_lines(&s.lines),
                s.author.as_str(),
            )
        })
        .collect();
    assert_eq!(
        added,
        [
            ("The question", String::new(), "session:s"),
            ("The reply", String::new(), "session:s"),
        ]
    );
    let text = fs::read_to_string(publisher.staging().definition_file(&id)).unwrap();
    assert!(
        text.contains(r#"    [null, "The question", "session:s"],"#),
        "{text}"
    );
    assert_eq!(staged_lines(&f.publish_root, &id).len(), 5);
    assert_eq!(publisher.changes(&id).await.unwrap(), 0);
}

/// `apply` refuses a definition that changed nothing, with or without
/// filters, unless forced, and a forced apply publishes the session as
/// it is. A definition whose only change is the title is applied.
#[tokio::test]
async fn apply_requires_a_change_unless_forced() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let sessions = SessionStore::from(&f.store);

    let empty = stage(&publisher, &f.original, &[]).await;
    let err = publisher.apply(&empty, false).await.unwrap_err();
    assert!(matches!(err, PublishError::Unchanged), "{err}");
    assert!(f.publish_root.join(&empty).is_dir());
    assert_eq!(sessions.iter().unwrap().count(), 1);

    edit(&publisher, &empty, |d| {
        d.title = Some(TitleEntry {
            text: "Retitled".into(),
            author: "user:test".into(),
        });
    })
    .await;
    assert_eq!(publisher.changes(&empty).await.unwrap(), 1);
    let commit = publisher.apply(&empty, false).await.unwrap();
    let retitled = sessions.at_commit(&commit).unwrap();
    assert_eq!(retitled.attrs.summary.title.as_deref(), Some("Retitled"));
    assert_eq!(sessions.iter().unwrap().count(), 2);

    let filters = [filter("language")];
    let unchanged = stage(&publisher, &f.original, &filters).await;
    assert_eq!(publisher.changes(&unchanged).await.unwrap(), 0);
    let err = publisher.apply(&unchanged, false).await.unwrap_err();
    assert!(matches!(err, PublishError::Unchanged), "{err}");
    assert!(f.publish_root.join(&unchanged).is_dir());
    assert_eq!(sessions.iter().unwrap().count(), 2);

    let commit = publisher.apply(&unchanged, true).await.unwrap();
    let published = sessions.at_commit(&commit).unwrap();
    assert_eq!(published.id, unchanged);
    let mut text = String::new();
    sessions
        .content(&commit)
        .open("session.jsonl")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, SESSION);
    assert!(!f.publish_root.join(&unchanged).exists());
}

#[tokio::test]
async fn system_filter_leaves_out_system_values_and_drops_the_project_attribute() {
    let f = fixture_with(SYSTEM_SESSION);
    assert_eq!(
        f.original.attrs.project.as_deref(),
        Some("-home-garrett-proj")
    );
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let filters = [filter("system"), filter("pii")];
    let id = stage(&publisher, &f.original, &filters).await;

    // The system filter has no patterns; its literal paths record the
    // working directory and the home directory as its text redactions.
    // The username is the pii filter's.
    let definition = publisher.definition(&id).unwrap();
    let literals = |author: &str| -> Vec<&str> {
        definition
            .redact
            .iter()
            .filter(|t| t.author == author)
            .map(|t| t.literal.as_str())
            .collect()
    };
    assert_eq!(
        literals("filter:system"),
        ["/home/garrett", "/home/garrett/proj"],
        "{definition:#?}"
    );
    assert_eq!(literals("filter:pii"), ["garrett"], "{definition:#?}");

    let lines = staged_lines(&f.publish_root, &id);
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
    // The working directory is a system literal everywhere it is text:
    // in a tool input, a tool result, and prose. The home directory
    // literal inside it merges into the one marker. The username
    // inside both is the pii filter's and merges as well.
    assert!(
        lines[1].contains(r#""command":"ls [REDACTED]""#),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].contains(r#""stdout":"[REDACTED]/a.txt""#),
        "{}",
        lines[2]
    );
    assert!(lines[3].contains("at [REDACTED]/a.txt"), "{}", lines[3]);

    let records = changes(&f.publish_root, &id);
    let attrs: Vec<&ChangeRecord> = records
        .iter()
        .filter(|r| r.kind == ChangeKind::Attr)
        .collect();
    assert_eq!(attrs.len(), 1, "{records:#?}");
    assert_eq!(attrs[0].reference.as_deref(), Some("project"));
    assert_eq!(
        (attrs[0].reason.as_deref(), attrs[0].author.as_str()),
        (Some("system"), "filter:system")
    );
    // One value rule per path, on no line, however many values it
    // selects; no text record is on a left-out value
    let values: Vec<&ChangeRecord> = records
        .iter()
        .filter(|r| r.kind == ChangeKind::Value)
        .collect();
    assert_eq!(values.len(), 5, "{records:#?}");
    assert!(
        values.iter().all(|r| r.line == 0
            && r.reason.as_deref() == Some("system")
            && r.author == "filter:system"),
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
            .any(|r| r.kind == ChangeKind::Text && r.reference.as_deref() == Some("/cwd")),
        "{records:#?}"
    );
    let pii_on_5: Vec<&ChangeRecord> = records
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

    let attrs = staged_attrs(&f.publish_root, &id);
    assert!(attrs.get("project").is_none(), "{attrs}");

    // A literal found only inside a left-out value changes nothing,
    // and is reported as such
    let refreshed = edit(&publisher, &id, |d| {
        d.push_redact("git.corp.example", "system", "user:test", true);
    })
    .await;
    assert_eq!(refreshed.unmatched, [(4, "git.corp.example".to_string())]);

    edit(&publisher, &id, |d| {
        d.filters.retain(|f| f.name != "system");
        d.redact.retain(|t| t.literal != "git.corp.example");
    })
    .await;
    let attrs = staged_attrs(&f.publish_root, &id);
    assert_eq!(attrs["project"], "-home-garrett-proj");
    let lines = staged_lines(&f.publish_root, &id);
    // The cwd is back, with the pii username redacted inside it; the
    // system literals went with the filter
    assert!(
        lines[0].contains(r#""cwd":"/home/[REDACTED]/proj""#),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains(r#""command":"ls /home/[REDACTED]/proj""#),
        "{}",
        lines[1]
    );
    let records = changes(&f.publish_root, &id);
    assert!(
        !records
            .iter()
            .any(|r| r.kind == ChangeKind::Attr || r.kind == ChangeKind::Value),
        "{records:#?}"
    );
}

/// The name on the reminder's git user line is a pii pattern literal:
/// found in the attachment, redacted there and in a quoted copy.
#[tokio::test]
async fn the_git_user_name_is_redacted_from_the_reminder_and_its_quotes() {
    let session = concat!(
        r#"{"type":"attachment","uuid":"x1","timestamp":"2025-01-01T00:00:00Z","attachment":{"type":"session_context","context":{"gitStatus":"Current branch: main\n\nGit user: Alice Carol\n\nStatus:\n(clean)"}}}"#,
        "\n",
        r#"{"type":"user","uuid":"u1","timestamp":"2025-01-01T00:00:01Z","message":{"role":"user","content":"Hello"}}"#,
        "\n",
        r#"{"type":"assistant","uuid":"a1","timestamp":"2025-01-01T00:00:02Z","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Commits are by Alice Carol."}],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        "\n",
    );
    let f = fixture_with(session);
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[filter("pii")]).await;
    let definition = publisher.definition(&id).unwrap();
    let pii: Vec<(&str, &str)> = definition
        .redact
        .iter()
        .map(|t| (t.literal.as_str(), t.author.as_str()))
        .collect();
    assert_eq!(pii, [("Alice Carol", "filter:pii")], "{definition:#?}");
    let lines = staged_lines(&f.publish_root, &id);
    assert!(
        lines[0].contains(r#"Git user: [REDACTED]\n"#),
        "{}",
        lines[0]
    );
    assert!(
        lines[2].contains("Commits are by [REDACTED]."),
        "{}",
        lines[2]
    );
}

#[tokio::test]
async fn a_literal_inside_a_tool_input_is_a_span() {
    let f = fixture();
    let publisher = Publisher::with_root(&f.store, &f.publish_root, registry());
    let id = stage(&publisher, &f.original, &[]).await;
    let key = "sk-ant-api03-PtYgjmUhBel31iEl2hpChYgCfrL1spNxnyVmihA-2O76UMFxFkM-R5Kjp1vRt_1fjORS-6ilI8ihN5KXSc7Tvo-hBKqFYAA";
    let command = "/message/content/0/input/command";

    edit(&publisher, &id, |d| {
        d.push_redact(key, "secret", "user:test", true);
        d.push_redact("api03", "secret", "user:test", true);
    })
    .await;
    // The key: a span on line 2 and one in the command on line 3. The
    // substring: a span inside each of them
    assert_eq!(instances_of(&publisher, &id, key).await, 2);
    assert_eq!(instances_of(&publisher, &id, "api03").await, 2);

    let records = changes(&f.publish_root, &id);
    let on_line = |line: u32| -> Vec<&ChangeRecord> {
        records
            .iter()
            .filter(|r| r.kind == ChangeKind::Text && r.line == line)
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
    let lines = staged_lines(&f.publish_root, &id);
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
    edit(&publisher, &id, |d| {
        d.push_redact("echo", "secret", "user:test", true);
    })
    .await;
    assert_eq!(instances_of(&publisher, &id, "echo").await, 1);
    let lines = staged_lines(&f.publish_root, &id);
    assert!(
        lines[2].contains(r#""command":"[REDACTED] [REDACTED]""#),
        "{}",
        lines[2]
    );
}
